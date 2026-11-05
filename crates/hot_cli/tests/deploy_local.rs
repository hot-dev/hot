//! `hot deploy --local` and build storage: workers read a bundle from the
//! configured storage, wherever `hot build` and `hot deploy` first wrote it.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// A `HOT_HOME` holding only the checkout's packages, so the database is this
/// test's own.
fn hot_home(dir: &Path) -> PathBuf {
    let home = dir.join(".test-hot-home");
    std::fs::create_dir_all(&home).unwrap();
    let pkg = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hot/pkg");
    if !home.join("pkg").exists() {
        std::os::unix::fs::symlink(pkg.canonicalize().unwrap(), home.join("pkg")).unwrap();
    }
    home
}

/// A new project in `dir`, with build storage at `dir/builds`.
struct Project {
    root: tempfile::TempDir,
}

impl Project {
    fn new() -> Self {
        let project = Self {
            root: tempfile::tempdir().unwrap(),
        };
        let output = project.hot(project.root.path(), &["init", "demo"]);
        assert!(output.status.success(), "{}", text(&output));
        project
    }

    fn hot(&self, dir: &Path, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_hot"))
            .args(args)
            .current_dir(dir)
            .env("HOT_HOME", hot_home(self.root.path()))
            .env("HOT_BUILD_STORAGE_PATH", self.storage())
            .env("NO_COLOR", "1")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .unwrap()
    }

    fn run(&self, args: &[&str]) -> Output {
        self.hot(&self.root.path().join("demo"), args)
    }

    fn storage(&self) -> PathBuf {
        self.root.path().join("builds")
    }

    /// The bundles under `dir`.
    fn bundles(dir: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut pending = vec![dir.to_path_buf()];
        while let Some(dir) = pending.pop() {
            for entry in std::fs::read_dir(dir).into_iter().flatten().flatten() {
                let path = entry.path();
                if path.is_dir() {
                    pending.push(path);
                } else if path.to_string_lossy().ends_with(".hot.zip") {
                    found.push(path);
                }
            }
        }
        found
    }
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn a_local_deploy_reaches_the_configured_build_storage() {
    let project = Project::new();
    let output = project.run(&["deploy", "--local"]);
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(Project::bundles(&project.storage()).len(), 1);
}

#[test]
fn a_build_made_earlier_reaches_the_configured_build_storage() {
    let project = Project::new();
    let built = project.run(&["build"]);
    assert!(built.status.success(), "{}", text(&built));
    let stdout = String::from_utf8_lossy(&built.stdout);
    let id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Build: "))
        .and_then(|line| line.split_whitespace().next())
        .expect("hot build names the build");
    assert!(Project::bundles(&project.storage()).is_empty());

    let output = project.run(&["deploy", "--local", id]);
    assert!(output.status.success(), "{}", text(&output));
    assert_eq!(Project::bundles(&project.storage()).len(), 1);
}

/// Deployed from another directory, the bundle can't be found. The error says
/// where Hot looked.
#[test]
fn a_build_in_neither_place_is_reported() {
    let project = Project::new();
    let built = project.run(&["build"]);
    assert!(built.status.success(), "{}", text(&built));
    let stdout = String::from_utf8_lossy(&built.stdout);
    let id = stdout
        .lines()
        .find_map(|line| line.strip_prefix("Build: "))
        .and_then(|line| line.split_whitespace().next())
        .unwrap();
    for bundle in Project::bundles(&project.root.path().join("demo/.hot/build")) {
        std::fs::remove_file(bundle).unwrap();
    }
    let output = project.run(&["deploy", "--local", id]);
    assert!(!output.status.success());
    assert!(
        text(&output).contains("isn't in build storage"),
        "{}",
        text(&output)
    );
}
