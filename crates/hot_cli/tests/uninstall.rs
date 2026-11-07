//! `hot uninstall` against temporary installs. The PATH lines come from the
//! real `add_to_path` in `install.sh`, so the two can't drift apart.
#![cfg(unix)]

use std::io::{ErrorKind, Write};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::Duration;

const CLEAN_PATH: &str = "/usr/bin:/bin";

struct Install {
    _root: tempfile::TempDir,
    home: PathBuf,
    bin: PathBuf,
    resources: PathBuf,
}

/// A user install in `<home>/.local`, laid out as `install.sh` leaves it
fn user_install() -> Install {
    let target_tmp = Path::new(env!("CARGO_TARGET_TMPDIR"));
    std::fs::create_dir_all(target_tmp).unwrap();
    let root = tempfile::tempdir_in(target_tmp).unwrap();
    let home = root.path().join("home");
    let bin = home.join(".local/bin");
    let resources = home.join(".local/share/hot");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::create_dir_all(resources.join("pkg/hot-std")).unwrap();
    std::fs::write(
        resources.join("install-receipt.json"),
        "{\"method\": \"archive\"}\n",
    )
    .unwrap();
    // A copy, not a hard link: macOS can kill a fresh build run through
    // several links at once while it checks the code signature
    std::fs::copy(env!("CARGO_BIN_EXE_hot"), bin.join("hot")).unwrap();
    Install {
        _root: root,
        home,
        bin,
        resources,
    }
}

/// Runs a fresh copy of a binary. Linux won't run a file that's open for
/// writing, and for a moment this one can be: a test on another thread may fork
/// while the copy is open, and that child holds the handle until it execs. The
/// refused run never started, so trying again is safe.
fn run_copy(command: &mut Command) -> Output {
    for _ in 0..500 {
        match command.output() {
            Err(error) if error.kind() == ErrorKind::ExecutableFileBusy => {
                std::thread::sleep(Duration::from_millis(10));
            }
            result => return result.unwrap(),
        }
    }
    panic!("the copy stayed busy: {command:?}");
}

/// Runs the real `add_to_path` for `install`, as `shell`'s user
fn add_to_path(install: &Install, shell: &str) {
    let installer =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../resources/installer/install.sh");
    let source = std::fs::read_to_string(installer).unwrap();
    let (functions, _) = source.rsplit_once("\nmain \"$@\"").unwrap();
    let harness = format!("{functions}\nadd_to_path \"$TEST_BIN\"\n");
    let output = Command::new("sh")
        .args(["-c", &harness])
        .env("HOME", &install.home)
        .env("SHELL", shell)
        .env("PATH", CLEAN_PATH)
        .env("TEST_BIN", &install.bin)
        .env_remove("ZDOTDIR")
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
}

fn hot_uninstall(install: &Install, args: &[&str]) -> Output {
    run_copy(
        Command::new(install.bin.join("hot"))
            .arg("uninstall")
            .args(args)
            .env("HOME", &install.home)
            .env("PATH", CLEAN_PATH)
            .env_remove("ZDOTDIR")
            .env_remove("HOT_HOME")
            .stdin(Stdio::null()),
    )
}

fn assert_removed(install: &Install, output: &Output) {
    assert!(output.status.success(), "{output:?}");
    assert!(!install.bin.join("hot").exists());
    assert!(!install.resources.exists());
}

/// The startup file each shell's install writes to, and what it held before
fn round_trip(shell: &str, file: &str, before: Option<&str>) {
    let install = user_install();
    let file = install.home.join(file);
    if let Some(before) = before {
        std::fs::write(&file, before).unwrap();
    }

    add_to_path(&install, shell);
    let added = std::fs::read_to_string(&file).unwrap();
    assert!(added.contains("# Added by the Hot installer"), "{added}");

    let output = hot_uninstall(&install, &["--yes"]);

    assert_removed(&install, &output);
    match before {
        Some(before) => assert_eq!(std::fs::read_to_string(&file).unwrap(), before),
        // A startup file is never deleted, even one the installer created
        None => assert_eq!(std::fs::read_to_string(&file).unwrap(), ""),
    }
}

/// On Linux a plain `output()` is refused here, as it was on CI, on a binary
/// another thread's fork had open.
#[test]
fn a_copy_that_is_busy_for_a_moment_still_runs() {
    std::fs::create_dir_all(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let root = tempfile::tempdir_in(env!("CARGO_TARGET_TMPDIR")).unwrap();
    let program = root.path().join("program");
    let mut handle = std::fs::File::create(&program).unwrap();
    handle.write_all(b"#!/bin/sh\nexit 0\n").unwrap();
    std::fs::set_permissions(&program, std::fs::Permissions::from_mode(0o755)).unwrap();
    let closes = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(100));
        drop(handle);
    });

    let output = run_copy(&mut Command::new(&program));

    closes.join().unwrap();
    assert!(output.status.success(), "{output:?}");
}

#[test]
fn zsh_startup_file_is_restored_byte_for_byte() {
    round_trip(
        "/bin/zsh",
        ".zshrc",
        Some("export EDITOR=vim\n\nalias l=ls\n"),
    );
}

#[test]
fn a_startup_file_the_installer_created_ends_up_empty() {
    round_trip("/bin/zsh", ".zshrc", None);
}

#[test]
fn profile_of_an_unknown_shell_is_restored() {
    round_trip("/bin/ksh", ".profile", Some("umask 022\n"));
}

#[test]
fn fish_file_is_removed() {
    let install = user_install();
    std::fs::create_dir_all(install.home.join(".config/fish/conf.d")).unwrap();
    add_to_path(&install, "/usr/bin/fish");
    let file = install.home.join(".config/fish/conf.d/hot.fish");
    assert!(file.exists());

    let output = hot_uninstall(&install, &["--yes"]);

    assert_removed(&install, &output);
    assert!(!file.exists());
}

#[test]
fn a_bin_folder_with_other_programs_stays_on_path() {
    let install = user_install();
    std::fs::write(install.bin.join("uv"), "another tool").unwrap();
    add_to_path(&install, "/bin/zsh");
    let zshrc = install.home.join(".zshrc");
    let added = std::fs::read_to_string(&zshrc).unwrap();

    let output = hot_uninstall(&install, &["--yes"]);

    assert_removed(&install, &output);
    assert_eq!(std::fs::read_to_string(&zshrc).unwrap(), added);
    assert!(install.bin.join("uv").exists());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("holds other programs"), "{stdout}");
}

#[test]
fn a_dry_run_removes_nothing() {
    let install = user_install();
    add_to_path(&install, "/bin/zsh");
    let zshrc = install.home.join(".zshrc");
    let added = std::fs::read_to_string(&zshrc).unwrap();

    let output = hot_uninstall(&install, &["--dry-run"]);

    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("share/hot"), "{stdout}");
    assert!(stdout.contains(".zshrc"), "{stdout}");
    assert!(install.bin.join("hot").exists());
    assert!(install.resources.exists());
    assert_eq!(std::fs::read_to_string(&zshrc).unwrap(), added);
}

#[test]
fn without_a_terminal_it_needs_yes() {
    let install = user_install();

    let output = hot_uninstall(&install, &[]);

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--yes"), "{stderr}");
    assert!(install.bin.join("hot").exists());
    assert!(install.resources.exists());
}

#[test]
fn hot_home_inside_the_install_stops_it() {
    let install = user_install();

    let output = run_copy(
        Command::new(install.bin.join("hot"))
            .args(["uninstall", "--yes"])
            .env("HOME", &install.home)
            .env("PATH", CLEAN_PATH)
            .env("HOT_HOME", &install.resources)
            .stdin(Stdio::null()),
    );

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("HOT_HOME"), "{stderr}");
    assert!(install.bin.join("hot").exists());
    assert!(install.resources.exists());
}

#[test]
fn a_binary_no_installer_owns_is_left_alone() {
    let output = Command::new(env!("CARGO_BIN_EXE_hot"))
        .args(["uninstall", "--yes"])
        .env("PATH", CLEAN_PATH)
        .stdin(Stdio::null())
        .output()
        .unwrap();

    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("No installer owns"), "{stderr}");
    assert!(Path::new(env!("CARGO_BIN_EXE_hot")).exists());
}
