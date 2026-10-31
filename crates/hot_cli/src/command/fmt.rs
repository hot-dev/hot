//! `hot fmt` — format `.hot` source files.
//!
//! The formatter itself is `hot::lang::fmt`; it refuses any change that
//! would alter the program. This command finds the files, writes or checks
//! them, and counts every file it could not read, format or write as a
//! failure, so `hot fmt --check` is a reliable CI gate.
//!
//! It runs before `hot.hot` is evaluated (see `async_main`): formatting
//! needs no project configuration.

use crate::cli::GlobalOptions;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

/// A file to format and how messages name it.
struct Target {
    path: PathBuf,
    display: String,
}

/// Run `hot fmt` and return the process exit code: 0 when every file is
/// formatted (or was formatted), 1 when `--check` finds an unformatted file
/// or any file cannot be read, formatted or written.
pub(crate) fn run_fmt(global: &GlobalOptions, paths: &[String], check: bool) -> i32 {
    hot::discovery::set_no_gitignore_global(global.no_gitignore);

    let mut failures = 0usize;
    let mut targets: Vec<Target> = Vec::new();
    if paths.is_empty() {
        let conf = crate::create_default_conf();
        let mut roots = vec![".".to_string()];
        roots.extend(crate::get_merged_src_paths(
            &conf,
            global.project.as_deref(),
            &global.src_paths,
        ));
        for root in &roots {
            targets.extend(find_hot_files(root));
        }
    } else {
        for arg in paths {
            let path = Path::new(arg);
            if path.is_dir() {
                targets.extend(find_hot_files(arg));
            } else if !path.exists() {
                eprintln!("error: {arg}: no such file or directory");
                failures += 1;
            } else if path.extension().and_then(|e| e.to_str()) != Some("hot") {
                eprintln!("error: {arg}: not a .hot file");
                failures += 1;
            } else {
                targets.push(Target {
                    path: path.to_path_buf(),
                    display: arg.clone(),
                });
            }
        }
    }

    // The same file can be reached from several roots.
    let mut seen = HashSet::new();
    targets.retain(|t| seen.insert(t.path.canonicalize().unwrap_or_else(|_| t.path.clone())));

    if targets.is_empty() && failures == 0 {
        println!("No .hot files found to format");
        return 0;
    }

    let mut unformatted: Vec<&str> = Vec::new();
    let mut changed = 0usize;
    for target in &targets {
        let name = target.display.as_str();
        let content = match std::fs::read(&target.path) {
            Ok(bytes) => match String::from_utf8(bytes) {
                Ok(content) => content,
                Err(_) => {
                    eprintln!("error: {name}: cannot format: the file is not valid UTF-8");
                    failures += 1;
                    continue;
                }
            },
            Err(e) => {
                eprintln!("error: {name}: cannot read: {e}");
                failures += 1;
                continue;
            }
        };
        // A formatter bug must not stop the run: the other files still get
        // formatted, and this one counts as a failure.
        let result = std::panic::catch_unwind(|| hot::lang::fmt::format_str(&content, 4))
            .unwrap_or_else(|_| Err("internal formatter error (panic)".to_string()));
        let formatted = match result {
            Ok(formatted) => formatted,
            Err(e) => {
                eprintln!("error: {name}: {e}; the file was left unchanged");
                failures += 1;
                continue;
            }
        };
        if formatted == content {
            continue;
        }
        if check {
            unformatted.push(name);
            continue;
        }
        match std::fs::write(&target.path, &formatted) {
            Ok(()) => {
                println!("formatted {name}");
                changed += 1;
            }
            Err(e) => {
                eprintln!("error: {name}: cannot write: {e}");
                failures += 1;
            }
        }
    }

    if check {
        if !unformatted.is_empty() {
            println!("The following files are not formatted:");
            for name in &unformatted {
                println!("  {name}");
            }
            println!("{} file(s) need formatting", unformatted.len());
        } else if failures == 0 {
            println!("All files are properly formatted");
        }
    } else if changed > 0 {
        println!("{changed} file(s) formatted");
    }
    if failures > 0 {
        eprintln!(
            "{failures} file(s) could not be {}",
            if check { "checked" } else { "formatted" }
        );
    }

    if failures > 0 || !unformatted.is_empty() {
        1
    } else {
        0
    }
}

/// Discover `.hot` files under `root`.
///
/// Routes through `hot::discovery::discover` so `hot fmt` honors `.gitignore`
/// / `.hotignore` / default hard-excludes consistently with the rest of the
/// toolchain. It also skips `.skills/`, the skill snapshots `hot ai add`
/// manages; name a directory inside `.skills/` to format it anyway.
fn find_hot_files(root: &str) -> Vec<Target> {
    let path = Path::new(root);
    if !path.exists() {
        return Vec::new();
    }
    let opts = hot::discovery::DiscoveryOpts::for_extension("hot").exclude(".skills/");
    hot::discovery::discover(&[path], &opts)
        .into_iter()
        .map(|d| Target {
            display: if root == "." {
                d.rel_path
            } else {
                path.join(&d.rel_path).display().to_string()
            },
            path: d.abs_path,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const FORMATTED: &str = "::t::ok ns\n\nx 1\n";
    const UNFORMATTED: &str = "::t::dirty ns\n\nPoint type { x: Int, y: Int }   \n";
    const DIRTY_FORMATTED: &str = "::t::dirty ns\n\nPoint type {\n    x: Int,\n    y: Int\n}\n";

    fn fmt(paths: &[&std::path::Path], check: bool) -> i32 {
        let paths: Vec<String> = paths.iter().map(|p| p.display().to_string()).collect();
        run_fmt(&GlobalOptions::default(), &paths, check)
    }

    fn write(dir: &std::path::Path, name: &str, content: &[u8]) -> PathBuf {
        let path = dir.join(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, content).unwrap();
        path
    }

    #[test]
    fn check_passes_formatted_files_and_writes_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let ok = write(dir.path(), "ok.hot", FORMATTED.as_bytes());
        assert_eq!(fmt(&[dir.path()], true), 0);
        assert_eq!(fmt(&[&ok], true), 0);
    }

    #[test]
    fn check_fails_on_unformatted_unreadable_and_unparseable_files() {
        for (name, content) in [
            ("dirty.hot", UNFORMATTED.as_bytes()),
            ("latin1.hot", b"::t::b ns\n\xff\xfe 1\n".as_slice()),
            ("broken.hot", b"x (\n".as_slice()),
        ] {
            let dir = tempfile::tempdir().unwrap();
            write(dir.path(), "ok.hot", FORMATTED.as_bytes());
            let path = write(dir.path(), name, content);
            assert_eq!(fmt(&[dir.path()], true), 1, "{name}");
            assert_eq!(std::fs::read(&path).unwrap(), content, "{name} was changed");
        }
    }

    #[test]
    fn write_mode_formats_and_fails_on_refusals() {
        let dir = tempfile::tempdir().unwrap();
        let dirty = write(dir.path(), "dirty.hot", UNFORMATTED.as_bytes());
        assert_eq!(fmt(&[dir.path()], false), 0);
        assert_eq!(std::fs::read_to_string(&dirty).unwrap(), DIRTY_FORMATTED);

        let broken = write(dir.path(), "broken.hot", b"x (\n");
        assert_eq!(fmt(&[dir.path()], false), 1);
        assert_eq!(std::fs::read(&broken).unwrap(), b"x (\n");
    }

    #[cfg(unix)]
    #[test]
    fn write_errors_fail() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let readonly = write(dir.path(), "ro.hot", UNFORMATTED.as_bytes());
        std::fs::set_permissions(&readonly, std::fs::Permissions::from_mode(0o444)).unwrap();
        // A superuser can write the file anyway; there is no error to see.
        if std::fs::OpenOptions::new()
            .write(true)
            .open(&readonly)
            .is_ok()
        {
            return;
        }
        assert_eq!(fmt(&[&readonly], false), 1);
        assert_eq!(std::fs::read_to_string(&readonly).unwrap(), UNFORMATTED);
    }

    #[test]
    fn missing_and_non_hot_paths_fail() {
        let dir = tempfile::tempdir().unwrap();
        let ok = write(dir.path(), "ok.hot", FORMATTED.as_bytes());
        let txt = write(dir.path(), "notes.txt", b"x\n");
        assert_eq!(fmt(&[&ok, &dir.path().join("missing.hot")], true), 1);
        assert_eq!(fmt(&[&ok, &txt], true), 1);
    }

    #[test]
    fn directory_walks_skip_skill_snapshots() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "ok.hot", FORMATTED.as_bytes());
        let skill = write(
            dir.path(),
            ".skills/hot-ai-agents/examples/agent.hot",
            UNFORMATTED.as_bytes(),
        );
        assert_eq!(fmt(&[dir.path()], true), 0);
        // Naming the snapshot directory formats it.
        assert_eq!(fmt(&[&dir.path().join(".skills")], true), 1);
        assert_eq!(fmt(&[&skill], true), 1);
    }

    #[test]
    fn crlf_files_stay_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "crlf.hot",
            UNFORMATTED.replace('\n', "\r\n").as_bytes(),
        );
        assert_eq!(fmt(&[&path], false), 0);
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            DIRTY_FORMATTED.replace('\n', "\r\n")
        );
        assert_eq!(fmt(&[&path], true), 0);
    }
}
