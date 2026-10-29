//! A command whose project `hot.hot` doesn't define must fail, not succeed
//! on no project files.

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn hot_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_hot"))
}

const BROKEN_SOURCE: &str = "::broken ns\n\nbroken fn (): Int { no-such-local(1) }\n";

/// A project directory with `hot.hot` and one source file with an error.
fn project_with(hot_hot: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temporary project");
    std::fs::write(dir.path().join("hot.hot"), hot_hot).unwrap();
    let src = dir.path().join("hot/src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("broken.hot"), BROKEN_SOURCE).unwrap();
    dir
}

/// Defines a project but sets no default with `hot.set.project`.
fn project_without_default() -> tempfile::TempDir {
    project_with("hot.project.drafting.src.paths [\"./hot/src\"]\n")
}

/// Runs `hot` in `dir`, killing it if it outlives the deadline (a `hot watch`
/// that didn't refuse would watch forever).
fn run_hot(dir: &Path, args: &[&str]) -> Output {
    let mut child = Command::new(hot_binary())
        .args(args)
        .current_dir(dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("run hot");
    let deadline = Instant::now() + Duration::from_secs(120);
    while child.try_wait().expect("wait for hot").is_none() {
        if Instant::now() > deadline {
            child.kill().ok();
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    child.wait_with_output().expect("hot output")
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[test]
fn check_refuses_an_undefined_default_project() {
    let dir = project_without_default();
    let output = run_hot(dir.path(), &["check"]);
    let out = text(&output);

    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(!out.contains("No issues found"), "{out}");
    assert!(
        out.contains(
            "Project 'my-hot-project' (the default when hot.set.project is not set) is not defined"
        ),
        "{out}"
    );
    assert!(out.contains("hot.hot defines: drafting."), "{out}");
    assert!(out.contains("-p <name>"), "{out}");
    assert!(out.contains("hot.set.project \"drafting\""), "{out}");
}

#[test]
fn check_refuses_a_misspelled_project() {
    let dir = project_without_default();
    let output = run_hot(dir.path(), &["check", "-p", "draftin"]);
    let out = text(&output);

    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(!out.contains("No issues found"), "{out}");
    assert!(
        out.contains("Project 'draftin' is not defined. hot.hot defines: drafting."),
        "{out}"
    );
}

#[test]
fn check_json_reports_a_misspelled_project_as_a_diagnostic() {
    let dir = project_without_default();
    let output = run_hot(
        dir.path(),
        &["check", "-p", "draftin", "--check.format", "json-min"],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);

    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(stdout.trim_start().starts_with('['), "{stdout}");
    assert!(
        stdout.contains("Project 'draftin' is not defined"),
        "{stdout}"
    );
}

#[test]
fn check_of_a_defined_project_reports_its_errors() {
    let dir = project_without_default();
    let output = run_hot(dir.path(), &["check", "-p", "drafting"]);
    let out = text(&output);

    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(out.contains("no-such-local"), "{out}");
    assert!(!out.contains("is not defined. hot.hot defines"), "{out}");
}

#[test]
fn check_keeps_the_default_project_when_hot_hot_defines_none() {
    let dir = project_with("hot.check.with-tests true\n");
    let output = run_hot(dir.path(), &["check"]);
    let out = text(&output);

    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(out.contains("no-such-local"), "{out}");

    std::fs::write(
        dir.path().join("hot/src/broken.hot"),
        "::fine ns\n\nfine fn (): Int { 1 }\n",
    )
    .unwrap();
    let output = run_hot(dir.path(), &["check"]);
    let out = text(&output);
    assert_eq!(output.status.code(), Some(0), "{out}");
    assert!(out.contains("No issues found"), "{out}");
}

#[test]
fn watch_test_and_run_refuse_an_undefined_project() {
    let dir = project_without_default();
    let commands: [&[&str]; 5] = [
        &["watch"],
        &["test"],
        &["test", "-p", "draftin"],
        &["run", "hot/src/broken.hot"],
        &["eval", "-p", "draftin", "1"],
    ];
    for args in commands {
        let output = run_hot(dir.path(), args);
        let out = text(&output);
        assert_eq!(output.status.code(), Some(1), "hot {args:?}: {out}");
        assert!(
            out.contains("is not defined. hot.hot defines: drafting."),
            "hot {args:?}: {out}"
        );
    }
}
