//! `hot uninstall`: removes the install this binary runs from, the way it was
//! installed. Like `hot update`, it never touches another install.
//!
//! `update::detect_install_method` treats anything it doesn't recognize as a
//! package, which is harmless for an update. Here that would delete a dev
//! build, so every install has to be positively identified first.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use hot::lang::cache::paths as cache_paths;

use crate::update::{
    ARCHIVE_RECEIPT, archive_install_prefix, is_windows_user_install, normalize_windows_path,
    powershell_literal, windows_registered_uninstaller, windows_user_uninstaller,
};

type Error = Box<dyn std::error::Error + Send + Sync>;

const DOCS_URL: &str = "https://hot.dev/docs/uninstalling";

/// The comment `install.sh` and the per-user macOS package write above the
/// line that adds Hot to PATH
const INSTALLER_COMMENT: &str = "# Added by the Hot installer";

const MACOS_PKG_ID: &str = "dev.hot.hot.pkg";
const MACOS_USER_PKG_ID: &str = "dev.hot.hot.user.pkg";
const MACOS_PKG_BINARY: &str = "/usr/local/bin/hot";
const MACOS_PKG_RESOURCES: &str = "/usr/local/share/hot";
/// cargo-deb derives `hot-cli` from the crate name. `hot` covers a package
/// built with the name set, so the name to remove always comes from dpkg.
const DEB_PKG_NAMES: [&str; 2] = ["hot-cli", "hot"];
const DEB_PKG_RESOURCES: &str = "/usr/share/hot";

/// What Hot creates under `HOT_HOME` and no release ships. Finding one in an
/// install means someone's data lives there.
const HOT_HOME_DATA: [&str; 3] = ["cache", "file", "db/hot.sqlite.db"];

/// The one startup file Hot named itself, so the only one it ever deletes
const FISH_PATH_FILE: &str = ".config/fish/conf.d/hot.fish";

/// What `scripts/install-completions.sh` writes for the macOS package. The
/// Debian package removes its own in `postrm`.
const MACOS_SYSTEM_COMPLETIONS: [&str; 3] = [
    "/usr/local/share/zsh/site-functions/_hot",
    "/usr/local/etc/bash_completion.d/hot",
    "/usr/local/share/fish/vendor_completions.d/hot.fish",
];

#[derive(Debug, PartialEq)]
enum Install {
    /// `<prefix>/bin/hot` and `<prefix>/share/hot`, from `install.sh` or the
    /// per-user macOS package
    Prefix {
        prefix: PathBuf,
        user_pkg: bool,
    },
    MacosPackage,
    DebPackage {
        name: String,
    },
    /// The `brew` of the Homebrew prefix that holds this binary. Another
    /// Homebrew earlier on PATH would remove its own Hot instead.
    Homebrew {
        brew: PathBuf,
    },
    /// Either NSIS installer, which leaves `uninstall.exe` beside `hot.exe`
    Windows {
        uninstaller: PathBuf,
        user_install: bool,
    },
}

impl Install {
    fn describe(&self) -> String {
        match self {
            Install::Prefix { prefix, .. } => format!("the Hot install in {}", prefix.display()),
            Install::MacosPackage => "the macOS package install of Hot".to_string(),
            Install::DebPackage { .. } => "the Debian package install of Hot".to_string(),
            Install::Homebrew { .. } => "the Homebrew install of Hot".to_string(),
            Install::Windows { uninstaller, .. } => format!(
                "the Hot install in {}",
                uninstaller.parent().unwrap_or(uninstaller).display()
            ),
        }
    }

    /// The folder of Hot's own files that goes with this install. Homebrew's
    /// is its versioned Cellar folder, which nothing writes to.
    fn resources(&self) -> Option<PathBuf> {
        match self {
            Install::Prefix { prefix, .. } => Some(prefix.join("share").join("hot")),
            Install::MacosPackage => Some(PathBuf::from(MACOS_PKG_RESOURCES)),
            Install::DebPackage { .. } => Some(PathBuf::from(DEB_PKG_RESOURCES)),
            Install::Homebrew { .. } => None,
            Install::Windows { uninstaller, .. } => uninstaller.parent().map(Path::to_path_buf),
        }
    }
}

/// The `method` in the receipt of the install under `prefix`
fn receipt_method(prefix: &Path) -> Option<String> {
    let receipt = std::fs::read(prefix.join("share").join("hot").join(ARCHIVE_RECEIPT)).ok()?;
    let receipt: serde_json::Value = serde_json::from_slice(&receipt).ok()?;
    Some(receipt.get("method")?.as_str()?.to_string())
}

fn prefix_install(exe: &Path) -> Option<Install> {
    // A renamed copy beside the install isn't the install
    if exe.file_name()? != "hot" {
        return None;
    }
    let prefix = archive_install_prefix(exe)?;
    let user_pkg = match receipt_method(&prefix)?.as_str() {
        "archive" => false,
        "user-pkg" => true,
        _ => return None,
    };
    Some(Install::Prefix { prefix, user_pkg })
}

/// The Homebrew prefix of a binary at `<prefix>/Cellar/hot/<version>/bin/hot`.
/// On Intel Macs `/usr/local/bin/hot` can be Homebrew's symlink or the
/// package's own file, so this needs the resolved path.
fn homebrew_prefix(exe: &Path) -> Option<&Path> {
    let bin_dir = exe.parent()?;
    let formula_dir = bin_dir.parent()?.parent()?;
    let cellar = formula_dir.parent()?;
    let is_cellar = exe.file_name()? == "hot"
        && bin_dir.file_name()? == "bin"
        && formula_dir.file_name()? == "hot"
        && cellar.file_name()? == "Cellar";
    if is_cellar { cellar.parent() } else { None }
}

/// The package named by `dpkg-query -S` output: `hot-cli: /usr/bin/hot`, or
/// `hot-cli:amd64: /usr/bin/hot` for a multiarch package
fn dpkg_query_owner(output: &str) -> Option<&str> {
    output.lines().next()?.split(':').next()
}

/// The name of the Hot package that owns `exe`
fn deb_package(exe: &Path) -> Option<String> {
    let output = Command::new("dpkg-query")
        .arg("-S")
        .arg(exe)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let owner = dpkg_query_owner(&stdout)?;
    DEB_PKG_NAMES.contains(&owner).then(|| owner.to_string())
}

fn is_macos_package(exe: &Path) -> bool {
    exe == Path::new(MACOS_PKG_BINARY)
        && Command::new("pkgutil")
            .args(["--pkg-info", MACOS_PKG_ID])
            .output()
            .is_ok_and(|output| output.status.success())
}

/// Whether `uninstaller` is Hot's. The name proves nothing: a `hot.exe` copied
/// into another program's folder sits beside that program's `uninstall.exe`,
/// which must never run. Hot's installers write `install-scope` beside it, and
/// older ones can be told by their uninstall registration.
fn is_hot_uninstaller(
    uninstaller: &Path,
    install_scope: Option<&str>,
    registered: &[Option<String>],
) -> bool {
    if matches!(install_scope.map(str::trim), Some("user" | "system")) {
        return true;
    }
    let uninstaller = normalize_windows_path(&uninstaller.to_string_lossy());
    registered
        .iter()
        .flatten()
        .any(|path| normalize_windows_path(path) == uninstaller)
}

fn detect_windows_install(exe: &Path) -> Option<Install> {
    if !exe.file_name()?.eq_ignore_ascii_case("hot.exe") {
        return None;
    }
    let dir = exe.parent()?;
    let uninstaller = dir.join("uninstall.exe");
    if !uninstaller.is_file() {
        return None;
    }
    let user_registration = windows_user_uninstaller();
    let is_hot = is_hot_uninstaller(
        &uninstaller,
        std::fs::read_to_string(dir.join("install-scope"))
            .ok()
            .as_deref(),
        &[
            user_registration.clone(),
            windows_registered_uninstaller("LocalMachine"),
        ],
    );
    is_hot.then(|| Install::Windows {
        user_install: is_windows_user_install(
            exe,
            std::env::var_os("LOCALAPPDATA").as_deref().map(Path::new),
            user_registration.as_deref(),
        ),
        uninstaller,
    })
}

/// How the binary at `exe` was installed, or `None` when nothing owns it
fn detect_install(exe: &Path) -> Option<Install> {
    if cfg!(windows) {
        return detect_windows_install(exe);
    }
    let exe = exe.canonicalize().ok()?;
    if archive_install_prefix(&exe).is_some() {
        return prefix_install(&exe);
    }
    if let Some(prefix) = homebrew_prefix(&exe) {
        let brew = prefix.join("bin").join("brew");
        return brew.is_file().then_some(Install::Homebrew { brew });
    }
    if cfg!(target_os = "macos") && is_macos_package(&exe) {
        return Some(Install::MacosPackage);
    }
    if cfg!(target_os = "linux") {
        return deb_package(&exe).map(|name| Install::DebPackage { name });
    }
    None
}

/// Refuse when uninstalling would delete data along with `resources`, because
/// `HOT_HOME` holds a database and stored files. It may be set only for a
/// service, so this also looks for what it leaves behind.
fn check_no_data_inside(resources: &Path, hot_home: Option<&Path>) -> Result<(), Error> {
    let resolved = resources
        .canonicalize()
        .unwrap_or_else(|_| resources.to_path_buf());
    let home_inside = hot_home.is_some_and(|home| {
        home.canonicalize()
            .unwrap_or_else(|_| home.to_path_buf())
            .starts_with(&resolved)
    });
    let found = HOT_HOME_DATA
        .iter()
        .map(|name| resources.join(name))
        .find(|path| path.exists());
    if home_inside {
        return Err(format!(
            "HOT_HOME is inside {}, so uninstalling would delete its data. Nothing was removed. \
             Move your data out and change HOT_HOME first.",
            resources.display()
        )
        .into());
    }
    if let Some(found) = found {
        return Err(format!(
            "{} looks like data from a HOT_HOME set to {}, so nothing was removed. \
             Move it out of that folder first.",
            found.display(),
            resources.display()
        )
        .into());
    }
    Ok(())
}

/// Every other `hot` on PATH, for the message after uninstalling
fn other_hots_on_path(exe: &Path) -> Vec<PathBuf> {
    let name = if cfg!(windows) { "hot.exe" } else { "hot" };
    let exe = exe.canonicalize().unwrap_or_else(|_| exe.to_path_buf());
    let mut found: Vec<PathBuf> = Vec::new();
    for dir in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        let candidate = dir.join(name);
        if !candidate.is_file() {
            continue;
        }
        let resolved = candidate.canonicalize().unwrap_or(candidate);
        if resolved != exe && !found.contains(&resolved) {
            found.push(resolved);
        }
    }
    found
}

/// The folder a PATH line from the installers adds, as written in the line
fn path_line_folder(line: &str) -> Option<&str> {
    line.strip_prefix("export PATH=\"")
        .and_then(|rest| rest.strip_suffix(":$PATH\""))
        .or_else(|| {
            line.strip_prefix("fish_add_path \"")
                .and_then(|rest| rest.strip_suffix('"'))
        })
}

/// Whether `line` is an installer's PATH line for `bin_dir`. The line may spell
/// the folder differently from the resolved `bin_dir`: `--prefix` is written as
/// given, and a home folder can sit behind a symlink.
fn is_path_line_for(line: &str, bin_dir: &Path, home: &Path) -> bool {
    let Some(folder) = path_line_folder(line) else {
        return false;
    };
    let folder = match folder.strip_prefix("$HOME/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(folder),
    };
    folder.canonicalize().unwrap_or(folder) == bin_dir
}

/// `content` without the installer's comment and the PATH line under it,
/// along with the blank line the installer writes above them. `None` when
/// there's no such block: a hand-edited line isn't ours to remove. Startup
/// files work on bytes, because they needn't be UTF-8.
fn without_path_block(content: &[u8], is_ours: impl Fn(&str) -> bool) -> Option<Vec<u8>> {
    let lines: Vec<&[u8]> = content.split_inclusive(|byte| *byte == b'\n').collect();
    let mut kept: Vec<&[u8]> = Vec::with_capacity(lines.len());
    let mut index = 0;
    while index < lines.len() {
        let is_block = lines[index].trim_ascii_end() == INSTALLER_COMMENT.as_bytes()
            && lines
                .get(index + 1)
                .is_some_and(|line| std::str::from_utf8(line.trim_ascii_end()).is_ok_and(&is_ours));
        if is_block {
            if kept.last().is_some_and(|line| line.trim_ascii().is_empty()) {
                kept.pop();
            }
            index += 2;
        } else {
            kept.push(lines[index]);
            index += 1;
        }
    }
    (kept.len() != lines.len()).then(|| kept.concat())
}

/// `content` without any line equal to `target`
fn without_line(content: &[u8], target: &str) -> Option<Vec<u8>> {
    let kept: Vec<&[u8]> = content
        .split_inclusive(|byte| *byte == b'\n')
        .filter(|line| line.trim_ascii_end() != target.as_bytes())
        .collect();
    (kept.concat().len() != content.len()).then(|| kept.concat())
}

/// Every file an installer may have added a PATH line to. This scans them all
/// rather than asking the current shell, which may have changed since.
fn shell_startup_files(home: &Path, zdotdir: Option<&Path>) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = zdotdir.map(|dir| dir.join(".zshrc")).into_iter().collect();
    for name in [
        ".zshrc",
        ".bashrc",
        ".bash_profile",
        ".bash_login",
        ".profile",
    ] {
        files.push(home.join(name));
    }
    files.push(home.join(FISH_PATH_FILE));
    files.dedup();
    files
}

/// Whether `bin_dir` holds anything but Hot. `~/.local/bin` is shared, and other
/// programs may have come to rely on the PATH entry the installer added.
fn holds_other_programs(bin_dir: &Path) -> bool {
    std::fs::read_dir(bin_dir).is_ok_and(|mut entries| {
        entries.any(|entry| entry.is_ok_and(|entry| entry.file_name() != "hot"))
    })
}

const PATH_LINE: &str = "the PATH line";
const COMPLETIONS_LINE: &str = "the completions line";

struct ShellEdit {
    file: PathBuf,
    content: Vec<u8>,
    what: &'static str,
}

/// The user's shell setup, read once so a plan can be tested without touching
/// a real home folder
struct ShellEnv {
    home: Option<PathBuf>,
    zdotdir: Option<PathBuf>,
    /// Sudo is unavailable in many containers, where root doesn't need it
    is_root: bool,
    /// Another `hot` still uses the shell completions
    keep_completions: bool,
}

#[derive(Default)]
struct Plan {
    /// Run first, so a refused sudo prompt changes nothing
    commands: Vec<Vec<String>>,
    /// Deleted in order
    paths: Vec<PathBuf>,
    /// Applied once Hot is gone, so a failed delete leaves PATH working
    shell_edits: Vec<ShellEdit>,
    /// Run last. A failure leaves stale package data, not an install.
    cleanup_commands: Vec<Vec<String>>,
    notes: Vec<String>,
}

fn command_line(env: &ShellEnv, sudo: bool, args: &[&str]) -> Vec<String> {
    let sudo = sudo && !env.is_root;
    sudo.then(|| "sudo".to_string())
        .into_iter()
        .chain(args.iter().map(|arg| arg.to_string()))
        .collect()
}

/// The content of a startup file. One that exists but can't be read may still
/// hold a line of ours, so the plan says so.
fn read_shell_file(plan: &mut Plan, file: &Path) -> Option<Vec<u8>> {
    match std::fs::read(file) {
        Ok(content) => Some(content),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => {
            let note = format!(
                "Could not read {} ({e}). Check it for Hot lines by hand.",
                file.display()
            );
            if !plan.notes.contains(&note) {
                plan.notes.push(note);
            }
            None
        }
    }
}

fn add_path_edits(plan: &mut Plan, bin_dir: &Path, home: &Path, zdotdir: Option<&Path>) {
    let mut kept_in = Vec::new();
    for file in shell_startup_files(home, zdotdir) {
        let Some(content) = read_shell_file(plan, &file) else {
            continue;
        };
        let Some(content) =
            without_path_block(&content, |line| is_path_line_for(line, bin_dir, home))
        else {
            continue;
        };
        if holds_other_programs(bin_dir) {
            kept_in.push(file);
        } else {
            plan.shell_edits.push(ShellEdit {
                file,
                content,
                what: PATH_LINE,
            });
        }
    }
    for file in kept_in {
        plan.notes.push(format!(
            "Keeping the PATH line in {}: {} holds other programs.",
            file.display(),
            bin_dir.display()
        ));
    }
}

/// Whether `path` is a completion file `hot completions` generated. Another
/// program called hot could own the same name.
fn is_hot_completion(path: &Path) -> bool {
    std::fs::read_to_string(path).is_ok_and(|content| {
        ["#compdef hot", "_hot()", "complete -c hot"]
            .iter()
            .any(|marker| content.contains(marker))
    })
}

/// `install-completions.sh` is run by hand for a per-user setup
fn add_user_completions(plan: &mut Plan, home: &Path) {
    for path in [
        ".zsh/completions/_hot",
        ".bash_completion.d/hot",
        ".config/fish/completions/hot.fish",
    ] {
        let path = home.join(path);
        if is_hot_completion(&path) {
            // First: a failure here leaves the install whole
            plan.paths.insert(0, path);
        }
    }
    let source_line = format!(". {}/.bash_completion.d/hot", home.display());
    for name in [".bash_profile", ".bashrc"] {
        let file = home.join(name);
        // The PATH edit may already have planned a new version of this file
        let planned = plan.shell_edits.iter().position(|edit| edit.file == file);
        let content = match planned {
            Some(index) => Some(plan.shell_edits[index].content.clone()),
            None => read_shell_file(plan, &file),
        };
        let Some(content) = content.and_then(|content| without_line(&content, &source_line)) else {
            continue;
        };
        match planned {
            Some(index) => {
                plan.shell_edits[index].content = content;
                plan.shell_edits[index].what = "the PATH and completions lines";
            }
            None => plan.shell_edits.push(ShellEdit {
                file,
                content,
                what: COMPLETIONS_LINE,
            }),
        }
    }
}

/// `hot update` rewrites the receipt of a per-user package install, so the
/// package's own record is the only sign left of it
fn has_user_pkg_receipt(prefix: &Path, home: &Path) -> bool {
    cfg!(target_os = "macos")
        && home
            .join(".local")
            .canonicalize()
            .is_ok_and(|local| local == prefix)
        && Command::new("pkgutil")
            .arg("--volume")
            .arg(home)
            .args(["--pkg-info", MACOS_USER_PKG_ID])
            .output()
            .is_ok_and(|output| output.status.success())
}

fn build_plan(install: &Install, env: &ShellEnv) -> Plan {
    let mut plan = Plan::default();
    match install {
        Install::Prefix { prefix, user_pkg } => {
            let bin_dir = prefix.join("bin");
            if let Some(home) = &env.home {
                add_path_edits(&mut plan, &bin_dir, home, env.zdotdir.as_deref());
            }
            // The receipt in share/hot is how a retry finds this install,
            // so that folder goes after the binary
            plan.paths.push(bin_dir.join("hot"));
            plan.paths.push(prefix.join("share").join("hot"));
            if let Some(home) = &env.home
                && (*user_pkg || has_user_pkg_receipt(prefix, home))
            {
                plan.cleanup_commands.push(vec![
                    "pkgutil".to_string(),
                    "--volume".to_string(),
                    home.display().to_string(),
                    "--forget".to_string(),
                    MACOS_USER_PKG_ID.to_string(),
                ]);
            }
        }
        Install::MacosPackage => {
            let mut remove_files = vec!["rm", "-f", MACOS_PKG_BINARY];
            if !env.keep_completions {
                remove_files.extend(
                    MACOS_SYSTEM_COMPLETIONS
                        .into_iter()
                        .filter(|path| is_hot_completion(Path::new(path))),
                );
            }
            plan.commands
                .push(command_line(env, true, &["rm", "-rf", MACOS_PKG_RESOURCES]));
            plan.commands.push(command_line(env, true, &remove_files));
            plan.commands.push(command_line(
                env,
                true,
                &["pkgutil", "--forget", MACOS_PKG_ID],
            ));
        }
        Install::DebPackage { name } => {
            plan.commands
                .push(command_line(env, true, &["dpkg", "--purge", name]));
        }
        Install::Homebrew { brew } => {
            let brew = brew.to_string_lossy();
            plan.commands
                .push(command_line(env, false, &[&brew, "uninstall", "hot"]));
        }
        // The Windows uninstaller does the rest itself, but skips a file in use
        Install::Windows { .. } => {
            plan.notes
                .push("Stop any running hot commands first.".to_string());
        }
    }
    if !matches!(install, Install::Homebrew { .. } | Install::Windows { .. })
        && !env.keep_completions
        && let Some(home) = &env.home
    {
        add_user_completions(&mut plan, home);
    }
    plan
}

/// Start the uninstaller after a pause, so this process has exited by the
/// time it deletes `hot.exe`. `Start-Process` is what can raise the UAC prompt
/// the machine-wide uninstaller needs.
fn windows_uninstall_command(uninstaller: &Path, user_install: bool) -> String {
    format!(
        "Start-Sleep -Seconds 1; Start-Process -FilePath {} -ArgumentList '/S'{}",
        powershell_literal(&uninstaller.to_string_lossy()),
        if user_install { "" } else { " -Verb RunAs" }
    )
}

fn print_plan(install: &Install, plan: &Plan) {
    println!("This removes {}:", install.describe());
    println!();
    for command in plan.commands.iter().chain(&plan.cleanup_commands) {
        println!("  Run {}", command.join(" "));
    }
    if let Install::Windows { uninstaller, .. } = install {
        println!("  Run {}", uninstaller.display());
    }
    for path in &plan.paths {
        println!("  Delete {}", path.display());
    }
    for edit in &plan.shell_edits {
        println!("  Remove {} from {}", edit.what, edit.file.display());
    }
    if !plan.notes.is_empty() {
        println!();
        for note in &plan.notes {
            println!("{note}");
        }
    }
}

fn confirm(yes: bool) -> Result<(), Error> {
    use is_terminal::IsTerminal;
    if yes {
        return Ok(());
    }
    if !std::io::stdin().is_terminal() {
        return Err("Nothing was removed. Run `hot uninstall --yes` to remove it.".into());
    }
    println!();
    print!("Remove it? [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    if matches!(answer.trim(), "y" | "Y" | "yes" | "Yes") {
        Ok(())
    } else {
        Err("Nothing was removed.".into())
    }
}

/// Fail before anything is removed when a folder can't be changed. That
/// includes a folder being deleted: `sudo hot update` leaves a root-owned
/// `share/hot` in a folder the user can still write to.
fn check_writable(plan: &Plan, exe: &Path) -> Result<(), Error> {
    for path in &plan.paths {
        let is_folder = path.symlink_metadata().is_ok_and(|meta| meta.is_dir());
        let folders = path
            .parent()
            .into_iter()
            .chain(is_folder.then_some(path.as_path()));
        for dir in folders {
            if tempfile::tempfile_in(dir).is_err() {
                return Err(format!(
                    "You can't write to {}, so nothing was removed. Try:\n  sudo {} uninstall",
                    dir.display(),
                    exe.display()
                )
                .into());
            }
        }
    }
    Ok(())
}

/// Write `content` over `file`. The content only ever shrinks, so this can't
/// run out of disk space part way. Writing in place, rather than renaming a
/// new file over it, keeps symlinks, hard links, the owner and the mode, and
/// respects a read-only file.
fn write_shell_file(file: &Path, content: &[u8]) -> std::io::Result<()> {
    if content.is_empty() && file.ends_with(FISH_PATH_FILE) {
        return std::fs::remove_file(file);
    }
    let mut opened = std::fs::OpenOptions::new().write(true).open(file)?;
    opened.write_all(content)?;
    opened.set_len(content.len() as u64)
}

fn remove_path(path: &Path) -> std::io::Result<()> {
    let result = match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() => std::fs::remove_dir_all(path),
        Ok(_) => std::fs::remove_file(path),
        Err(e) => Err(e),
    };
    match result {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

fn run_command(command: &[String]) -> Result<(), Error> {
    let (program, args) = command.split_first().ok_or("Empty command")?;
    let status = Command::new(program)
        .args(args)
        .status()
        .map_err(|e| format!("Could not run {program}: {e}"))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{}` failed", command.join(" ")).into())
    }
}

/// Run the plan. Shell files are the user's, and a read-only one (as under
/// some dotfile managers) shouldn't stop Hot from being removed, so those
/// failures are only reported.
fn execute(plan: &Plan, exe: &Path) -> Result<(), Error> {
    check_writable(plan, exe)?;
    for command in &plan.commands {
        run_command(command)?;
    }
    for path in &plan.paths {
        remove_path(path).map_err(|e| {
            format!(
                "Could not delete {}: {e}. To finish, delete what's left of it by hand.",
                path.display()
            )
        })?;
    }
    for edit in &plan.shell_edits {
        if let Err(e) = write_shell_file(&edit.file, &edit.content) {
            eprintln!(
                "Could not update {} ({e}). Remove {} by hand.",
                edit.file.display(),
                edit.what
            );
        }
    }
    for command in &plan.cleanup_commands {
        if let Err(e) = run_command(command) {
            eprintln!("{e}");
        }
    }
    Ok(())
}

fn print_left_behind(others: &[PathBuf]) {
    println!();
    println!("Your projects and their .hot folders were left alone.");
    let cache = cache_paths::get_system_cache_dir();
    if cache.exists() {
        println!(
            "Downloaded packages and caches are still in {}.",
            cache.display()
        );
    }
    if let Some(home) = std::env::var_os("HOT_HOME") {
        println!("HOT_HOME is still set to {}.", Path::new(&home).display());
    }
    if !others.is_empty() {
        println!();
        println!("Another hot is still on your PATH:");
        for other in others {
            println!("  {}", other.display());
        }
        println!(
            "Run `{} uninstall` to remove that one too.",
            others[0].display()
        );
    }
}

fn is_root() -> bool {
    !cfg!(windows)
        && Command::new("id")
            .arg("-u")
            .output()
            .is_ok_and(|output| String::from_utf8_lossy(&output.stdout).trim() == "0")
}

pub fn run_uninstall(yes: bool, dry_run: bool) -> Result<(), Error> {
    let exe = std::env::current_exe()?;
    let Some(install) = detect_install(&exe) else {
        return Err(format!(
            "No installer owns {}, so `hot uninstall` left it alone. If you built or copied it \
             yourself, delete it by hand. For other installs, see {DOCS_URL}",
            exe.display()
        )
        .into());
    };

    if let Some(resources) = install.resources() {
        let hot_home = std::env::var_os("HOT_HOME").map(PathBuf::from);
        check_no_data_inside(&resources, hot_home.as_deref())?;
    }

    let others = other_hots_on_path(&exe);
    let env = ShellEnv {
        home: std::env::var_os("HOME")
            .filter(|home| !home.is_empty())
            .map(PathBuf::from)
            .or_else(dirs::home_dir),
        zdotdir: std::env::var_os("ZDOTDIR")
            .filter(|dir| !dir.is_empty())
            .map(PathBuf::from),
        is_root: is_root(),
        keep_completions: !others.is_empty(),
    };
    let plan = build_plan(&install, &env);

    print_plan(&install, &plan);
    if dry_run {
        return Ok(());
    }
    confirm(yes)?;

    if let Install::Windows {
        uninstaller,
        user_install,
    } = &install
    {
        // Windows can't delete a running executable, so the uninstaller
        // runs in a separate process once this one exits.
        Command::new("powershell")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-Command"])
            .arg(windows_uninstall_command(uninstaller, *user_install))
            .spawn()?;
        println!();
        println!("The Hot uninstaller has started, and finishes after this command exits.");
        println!("If Windows asks for permission and you refuse, Hot stays installed.");
    } else {
        // Planned again, so a startup file edited during the prompt keeps its edit
        let plan = build_plan(&install, &env);
        println!();
        execute(&plan, &exe)?;
        if exe.exists() {
            return Err(format!("{} is still there after uninstalling.", exe.display()).into());
        }
        println!("Hot is uninstalled.");
    }
    print_left_behind(&others);
    Ok(())
}

// Windows paths are covered by the string tests, which run everywhere else
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    const BLOCK: &str = "\n# Added by the Hot installer\nexport PATH=\"$HOME/.local/bin:$PATH\"\n";

    fn strip(content: &str, is_ours: impl Fn(&str) -> bool) -> Option<String> {
        without_path_block(content.as_bytes(), is_ours).map(|kept| String::from_utf8(kept).unwrap())
    }

    fn is_local_bin(line: &str) -> bool {
        line == "export PATH=\"$HOME/.local/bin:$PATH\""
    }

    fn env(home: &Path) -> ShellEnv {
        ShellEnv {
            home: Some(home.to_path_buf()),
            zdotdir: None,
            is_root: false,
            keep_completions: false,
        }
    }

    /// A user install under `<home>/.local`, as `install.sh` lays it out
    fn user_install(home: &Path, method: &str) -> PathBuf {
        let prefix = home.join(".local");
        std::fs::create_dir_all(prefix.join("bin")).unwrap();
        std::fs::create_dir_all(prefix.join("share/hot")).unwrap();
        std::fs::write(prefix.join("bin/hot"), "binary").unwrap();
        std::fs::write(
            prefix.join("share/hot").join(ARCHIVE_RECEIPT),
            format!("{{\"method\": \"{method}\"}}\n"),
        )
        .unwrap();
        prefix.canonicalize().unwrap()
    }

    #[test]
    fn removing_the_block_restores_the_file() {
        let before = "export EDITOR=vim\n";
        let after = strip(&format!("{before}{BLOCK}"), is_local_bin).unwrap();
        assert_eq!(after, before);
    }

    #[test]
    fn a_file_the_installer_created_ends_up_empty() {
        assert_eq!(strip(BLOCK, is_local_bin).unwrap(), "");
    }

    #[test]
    fn lines_written_after_the_block_stay_on_their_own_lines() {
        let content = format!("first\n{BLOCK}last\n");
        assert_eq!(strip(&content, is_local_bin).unwrap(), "first\nlast\n");
        // Without the blank line, the lines around the block must not join
        let content = format!("first{BLOCK}last\n");
        assert_eq!(strip(&content, is_local_bin).unwrap(), "first\nlast\n");
    }

    #[test]
    fn every_copy_of_the_block_goes() {
        let content = format!("first\n{BLOCK}{BLOCK}");
        assert_eq!(strip(&content, is_local_bin).unwrap(), "first\n");
    }

    #[test]
    fn an_edited_line_or_a_lone_comment_is_left_alone() {
        let edited = "# Added by the Hot installer\nexport PATH=\"$HOME/.local/bin:/x:$PATH\"\n";
        assert_eq!(strip(edited, is_local_bin), None);
        assert_eq!(strip("# Added by the Hot installer\n", is_local_bin), None);
        assert_eq!(
            strip("export PATH=\"$HOME/.local/bin:$PATH\"\n", is_local_bin),
            None
        );
    }

    #[test]
    fn a_path_line_matches_only_its_own_folder() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        std::fs::create_dir_all(home.join(".local/bin")).unwrap();
        std::fs::create_dir_all(home.join("other/bin")).unwrap();
        let bin = home.join(".local/bin");
        let absolute = format!("export PATH=\"{}:$PATH\"", bin.display());

        assert!(is_path_line_for(
            "export PATH=\"$HOME/.local/bin:$PATH\"",
            &bin,
            &home
        ));
        assert!(is_path_line_for(&absolute, &bin, &home));
        assert!(is_path_line_for(
            "fish_add_path \"$HOME/.local/bin\"",
            &bin,
            &home
        ));
        assert!(!is_path_line_for(
            "export PATH=\"$HOME/other/bin:$PATH\"",
            &bin,
            &home
        ));
        assert!(!is_path_line_for("export PATH=\"$PATH:/x\"", &bin, &home));
    }

    #[test]
    fn a_path_line_written_through_a_symlink_matches() {
        let root = tempfile::tempdir().unwrap();
        let root = root.path().canonicalize().unwrap();
        std::fs::create_dir_all(root.join("real/bin")).unwrap();
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(root.join("real"), root.join("link")).unwrap();
            let line = format!("export PATH=\"{}/link/bin:$PATH\"", root.display());
            assert!(is_path_line_for(&line, &root.join("real/bin"), &root));
        }
    }

    #[test]
    fn dpkg_output_names_the_owner() {
        assert_eq!(dpkg_query_owner("hot-cli: /usr/bin/hot\n"), Some("hot-cli"));
        assert_eq!(
            dpkg_query_owner("hot-cli:amd64: /usr/bin/hot\n"),
            Some("hot-cli")
        );
        assert_eq!(dpkg_query_owner("hot: /usr/bin/hot\n"), Some("hot"));
        assert_eq!(dpkg_query_owner("hotter: /usr/bin/hot\n"), Some("hotter"));
        assert_eq!(dpkg_query_owner(""), None);
    }

    #[test]
    fn the_cellar_names_the_homebrew_prefix() {
        for (exe, prefix) in [
            ("/opt/homebrew/Cellar/hot/3.0.3/bin/hot", "/opt/homebrew"),
            ("/usr/local/Cellar/hot/3.0.3/bin/hot", "/usr/local"),
            (
                "/home/linuxbrew/.linuxbrew/Cellar/hot/3.0.3/bin/hot",
                "/home/linuxbrew/.linuxbrew",
            ),
        ] {
            assert_eq!(homebrew_prefix(Path::new(exe)), Some(Path::new(prefix)));
        }
        for exe in [
            "/usr/local/bin/hot",
            "/opt/homebrew/hot/bin/hot",
            "/x/Cellar/hot/3.0.3/bin/hot-copy",
            "/x/Cellar/hot/3.0.3/libexec/hot",
            "/x/Cellar/other/3.0.3/bin/hot",
            "/x/Cellar/hot/backup/3.0.3/bin/hot",
        ] {
            assert_eq!(homebrew_prefix(Path::new(exe)), None, "{exe}");
        }
    }

    #[test]
    fn a_receipt_names_the_install() {
        let home = tempfile::tempdir().unwrap();
        let prefix = user_install(home.path(), "archive");
        assert_eq!(
            prefix_install(&prefix.join("bin/hot")),
            Some(Install::Prefix {
                prefix: prefix.clone(),
                user_pkg: false
            })
        );

        let home = tempfile::tempdir().unwrap();
        let prefix = user_install(home.path(), "user-pkg");
        assert_eq!(
            prefix_install(&prefix.join("bin/hot")),
            Some(Install::Prefix {
                prefix: prefix.clone(),
                user_pkg: true
            })
        );
    }

    #[test]
    fn a_binary_nothing_owns_is_refused() {
        // A dev build has no receipt
        let root = tempfile::tempdir().unwrap();
        let exe = root.path().join("target/debug/hot");
        std::fs::create_dir_all(exe.parent().unwrap()).unwrap();
        std::fs::write(&exe, "binary").unwrap();
        assert_eq!(detect_install(&exe), None);

        // A receipt this version doesn't understand
        let home = tempfile::tempdir().unwrap();
        let prefix = user_install(home.path(), "something-new");
        assert_eq!(detect_install(&prefix.join("bin/hot")), None);
    }

    #[test]
    fn a_user_install_plan_removes_the_install_and_its_path_line() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let prefix = user_install(&home, "archive");
        std::fs::write(home.join(".zshrc"), format!("first\n{BLOCK}")).unwrap();
        std::fs::write(home.join(".bashrc"), "untouched\n").unwrap();

        let plan = build_plan(
            &Install::Prefix {
                prefix: prefix.clone(),
                user_pkg: false,
            },
            &env(&home),
        );

        assert_eq!(
            plan.paths,
            vec![prefix.join("bin/hot"), prefix.join("share/hot")]
        );
        assert_eq!(plan.shell_edits.len(), 1);
        assert_eq!(plan.shell_edits[0].file, home.join(".zshrc"));
        assert_eq!(plan.shell_edits[0].content, b"first\n");
        assert!(plan.commands.is_empty());
        assert!(plan.cleanup_commands.is_empty());
    }

    #[test]
    fn a_shared_bin_folder_keeps_its_path_line() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let prefix = user_install(&home, "archive");
        std::fs::write(prefix.join("bin/uv"), "another tool").unwrap();
        std::fs::write(home.join(".zshrc"), format!("first\n{BLOCK}")).unwrap();

        let plan = build_plan(
            &Install::Prefix {
                prefix,
                user_pkg: false,
            },
            &env(&home),
        );

        assert!(plan.shell_edits.is_empty());
        assert_eq!(plan.notes.len(), 1);
        assert!(plan.notes[0].contains("holds other programs"));
    }

    #[test]
    fn user_completions_go_unless_another_hot_needs_them() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let prefix = user_install(&home, "archive");
        std::fs::create_dir_all(home.join(".zsh/completions")).unwrap();
        std::fs::write(home.join(".zsh/completions/_hot"), "#compdef hot\n").unwrap();
        let source_line = format!(". {}/.bash_completion.d/hot\n", home.display());
        // install-completions.sh adds the line again each time it runs
        std::fs::write(
            home.join(".bashrc"),
            format!("first\n{source_line}{source_line}{BLOCK}"),
        )
        .unwrap();
        let install = Install::Prefix {
            prefix,
            user_pkg: false,
        };

        let plan = build_plan(&install, &env(&home));
        assert!(plan.paths.contains(&home.join(".zsh/completions/_hot")));
        assert_eq!(plan.paths[0], home.join(".zsh/completions/_hot"));
        assert_eq!(plan.shell_edits.len(), 1);
        assert_eq!(plan.shell_edits[0].content, b"first\n");
        assert_eq!(plan.shell_edits[0].what, "the PATH and completions lines");

        let plan = build_plan(
            &install,
            &ShellEnv {
                keep_completions: true,
                ..env(&home)
            },
        );
        assert!(!plan.paths.contains(&home.join(".zsh/completions/_hot")));
        let kept = String::from_utf8_lossy(&plan.shell_edits[0].content);
        assert!(kept.contains(".bash_completion.d"));
    }

    #[test]
    fn a_user_package_forgets_its_receipt_last() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let prefix = user_install(&home, "user-pkg");
        let plan = build_plan(
            &Install::Prefix {
                prefix,
                user_pkg: true,
            },
            &env(&home),
        );
        assert!(plan.commands.is_empty());
        assert_eq!(
            plan.cleanup_commands,
            vec![vec![
                "pkgutil".to_string(),
                "--volume".to_string(),
                home.display().to_string(),
                "--forget".to_string(),
                MACOS_USER_PKG_ID.to_string()
            ]]
        );
    }

    #[test]
    fn packages_use_sudo_unless_already_root() {
        let home = tempfile::tempdir().unwrap();
        let deb = Install::DebPackage {
            name: "hot-cli".to_string(),
        };
        let plan = build_plan(&deb, &env(home.path()));
        assert_eq!(
            plan.commands,
            vec![vec!["sudo", "dpkg", "--purge", "hot-cli"]]
        );

        let root = ShellEnv {
            is_root: true,
            ..env(home.path())
        };
        let plan = build_plan(&deb, &root);
        assert_eq!(plan.commands, vec![vec!["dpkg", "--purge", "hot-cli"]]);

        let plan = build_plan(&Install::MacosPackage, &env(home.path()));
        assert_eq!(
            plan.commands[0],
            ["sudo", "rm", "-rf", "/usr/local/share/hot"]
        );
        assert_eq!(
            plan.commands[1][..4],
            ["sudo", "rm", "-f", "/usr/local/bin/hot"]
        );
        assert_eq!(
            plan.commands[2],
            ["sudo", "pkgutil", "--forget", MACOS_PKG_ID]
        );
    }

    #[test]
    fn homebrew_only_runs_its_own_brew() {
        let home = tempfile::tempdir().unwrap();
        let install = Install::Homebrew {
            brew: PathBuf::from("/usr/local/bin/brew"),
        };
        let plan = build_plan(&install, &env(home.path()));
        assert_eq!(
            plan.commands,
            vec![vec!["/usr/local/bin/brew", "uninstall", "hot"]]
        );
        assert!(plan.paths.is_empty());
        assert!(plan.shell_edits.is_empty());
    }

    #[test]
    fn only_a_machine_install_asks_for_administrator_rights() {
        let path = Path::new(r"C:\Users\O'Brien\Hot Dev\uninstall.exe");
        assert_eq!(
            windows_uninstall_command(path, true),
            r"Start-Sleep -Seconds 1; Start-Process -FilePath 'C:\Users\O''Brien\Hot Dev\uninstall.exe' -ArgumentList '/S'"
        );
        assert!(windows_uninstall_command(path, false).ends_with(" -Verb RunAs"));
    }

    #[cfg(unix)]
    #[test]
    fn a_symlinked_startup_file_stays_a_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("dotfiles-zshrc");
        std::fs::write(&target, "old\n").unwrap();
        std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o600)).unwrap();
        let link = root.path().join(".zshrc");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        write_shell_file(&link, b"new\n").unwrap();

        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_to_string(&target).unwrap(), "new\n");
        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_renamed_copy_is_not_the_install() {
        let home = tempfile::tempdir().unwrap();
        let prefix = user_install(home.path(), "archive");
        let copy = prefix.join("bin/hot-old");
        std::fs::write(&copy, "binary").unwrap();
        assert_eq!(detect_install(&copy), None);
    }

    #[test]
    fn another_programs_uninstaller_is_never_hot() {
        let path = Path::new(r"C:\Program Files\Hot Dev\uninstall.exe");
        let elsewhere = Some(r"C:\Tools\Hot Dev\uninstall.exe".to_string());

        assert!(!is_hot_uninstaller(path, None, &[None, None]));
        assert!(!is_hot_uninstaller(path, None, &[elsewhere.clone(), None]));
        assert!(!is_hot_uninstaller(path, Some("other"), &[None, None]));

        assert!(is_hot_uninstaller(path, Some("system\n"), &[None, None]));
        assert!(is_hot_uninstaller(path, Some("user"), &[None, None]));
        let quoted = Some(r#""c:\program files\hot dev\uninstall.exe""#.to_string());
        assert!(is_hot_uninstaller(path, None, &[elsewhere, quoted]));
    }

    #[test]
    fn data_inside_the_install_stops_the_uninstall() {
        let root = tempfile::tempdir().unwrap();
        let resources = root.path().join("share/hot");
        std::fs::create_dir_all(resources.join("data")).unwrap();

        // HOT_HOME elsewhere, or a parent of the install, is safe
        assert!(check_no_data_inside(&resources, None).is_ok());
        assert!(check_no_data_inside(&resources, Some(&root.path().join("data"))).is_ok());
        assert!(check_no_data_inside(&resources, Some(root.path())).is_ok());

        // HOT_HOME set to the install, or to a folder in it
        assert!(check_no_data_inside(&resources, Some(&resources)).is_err());
        assert!(check_no_data_inside(&resources, Some(&resources.join("data"))).is_err());

        // HOT_HOME isn't set here, but was once
        std::fs::create_dir_all(resources.join("cache")).unwrap();
        let error = check_no_data_inside(&resources, None).unwrap_err();
        assert!(error.to_string().contains("nothing was removed"), "{error}");
    }

    #[test]
    fn a_stranger_named_hot_keeps_its_completions() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let prefix = user_install(&home, "archive");
        std::fs::create_dir_all(home.join(".bash_completion.d")).unwrap();
        let stranger = home.join(".bash_completion.d/hot");
        std::fs::write(&stranger, "complete -F _other_tool hot\n").unwrap();

        let plan = build_plan(
            &Install::Prefix {
                prefix,
                user_pkg: false,
            },
            &env(&home),
        );

        assert!(!plan.paths.contains(&stranger));
    }

    #[test]
    fn deletes_stay_inside_the_install() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let prefix = user_install(&home, "archive");
        std::fs::write(prefix.join("bin/uv"), "another tool").unwrap();
        std::fs::create_dir_all(prefix.join("share/other")).unwrap();
        let install = Install::Prefix {
            prefix: prefix.clone(),
            user_pkg: false,
        };

        let plan = build_plan(&install, &env(&home));
        execute(&plan, &prefix.join("bin/hot")).unwrap();

        assert!(!prefix.join("bin/hot").exists());
        assert!(!prefix.join("share/hot").exists());
        assert!(prefix.join("bin/uv").exists());
        assert!(prefix.join("share/other").exists());
    }

    #[test]
    fn a_symlinked_install_folder_only_loses_the_link() {
        let root = tempfile::tempdir().unwrap();
        let elsewhere = root.path().join("elsewhere");
        std::fs::create_dir_all(&elsewhere).unwrap();
        std::fs::write(elsewhere.join("keep"), "data").unwrap();
        let link = root.path().join("hot");
        std::os::unix::fs::symlink(&elsewhere, &link).unwrap();

        remove_path(&link).unwrap();

        assert!(!link.symlink_metadata().is_ok());
        assert!(elsewhere.join("keep").exists());
    }

    #[test]
    fn a_folder_it_cant_empty_stops_the_uninstall_before_any_delete() {
        use std::os::unix::fs::PermissionsExt;
        if is_root() {
            return;
        }
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let prefix = user_install(&home, "archive");
        let resources = prefix.join("share/hot");
        let read_only = std::fs::Permissions::from_mode(0o555);
        std::fs::set_permissions(&resources, read_only).unwrap();
        let install = Install::Prefix {
            prefix: prefix.clone(),
            user_pkg: false,
        };

        let plan = build_plan(&install, &env(&home));
        let result = execute(&plan, &prefix.join("bin/hot"));

        std::fs::set_permissions(&resources, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("nothing was removed")
        );
        assert!(prefix.join("bin/hot").exists());
    }

    #[test]
    fn a_hard_linked_startup_file_stays_linked() {
        let root = tempfile::tempdir().unwrap();
        let original = root.path().join("dotfiles-zshrc");
        std::fs::write(&original, "old content\n").unwrap();
        let linked = root.path().join(".zshrc");
        std::fs::hard_link(&original, &linked).unwrap();

        write_shell_file(&linked, b"new\n").unwrap();

        assert_eq!(std::fs::read(&original).unwrap(), b"new\n");
    }

    #[test]
    fn a_startup_file_is_never_deleted_but_hot_fish_is() {
        let root = tempfile::tempdir().unwrap();
        let zshrc = root.path().join(".zshrc");
        std::fs::write(&zshrc, "export PATH=x\n").unwrap();
        write_shell_file(&zshrc, b"").unwrap();
        assert_eq!(std::fs::read(&zshrc).unwrap(), b"");

        let fish = root.path().join(FISH_PATH_FILE);
        std::fs::create_dir_all(fish.parent().unwrap()).unwrap();
        std::fs::write(&fish, "fish_add_path \"$HOME/.local/bin\"\n").unwrap();
        write_shell_file(&fish, b"").unwrap();
        assert!(!fish.exists());
    }

    #[test]
    fn a_read_only_startup_file_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let file = root.path().join(".zshrc");
        std::fs::write(&file, "old\n").unwrap();
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o444)).unwrap();

        // Root can write to anything, so this means nothing in a container
        if write_shell_file(&file, b"new\n").is_ok() {
            assert!(is_root());
        } else {
            assert_eq!(std::fs::read(&file).unwrap(), b"old\n");
        }
    }

    #[test]
    fn a_startup_file_that_isnt_utf8_is_still_edited() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let prefix = user_install(&home, "archive");
        let mut content = b"# caf\xe9\n".to_vec();
        content
            .extend_from_slice(format!(". {}/.bash_completion.d/hot\n", home.display()).as_bytes());
        content.extend_from_slice(BLOCK.as_bytes());
        std::fs::write(home.join(".bashrc"), content).unwrap();

        let plan = build_plan(
            &Install::Prefix {
                prefix,
                user_pkg: false,
            },
            &env(&home),
        );

        assert_eq!(plan.shell_edits.len(), 1);
        assert_eq!(plan.shell_edits[0].content, b"# caf\xe9\n");
    }

    #[test]
    fn an_unreadable_startup_file_is_reported() {
        let home = tempfile::tempdir().unwrap();
        let home = home.path().canonicalize().unwrap();
        let prefix = user_install(&home, "archive");
        // A folder can't be read as a file, even by root
        std::fs::create_dir(home.join(".zshrc")).unwrap();

        let plan = build_plan(
            &Install::Prefix {
                prefix,
                user_pkg: false,
            },
            &env(&home),
        );

        assert_eq!(plan.notes.len(), 1);
        assert!(plan.notes[0].contains(".zshrc"), "{}", plan.notes[0]);
    }
}
