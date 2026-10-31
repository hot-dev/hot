//! `hot check` and `hot watch`: the secret-shape scan honours the build
//! allowlist and is reported next to the real diagnostics, output formats
//! and exit codes, and a watch that re-runs on change.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn hot_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_hot"))
}

fn hot_home() -> String {
    format!("{}/../../hot", env!("CARGO_MANIFEST_DIR"))
}

const AWS_KEY_SOURCE: &str = "::sec::a ns\n\nkey \"AKIAIOSFODNN7EXAMPLE\"\n";
const BROKEN_SOURCE: &str = "::sec::b ns\n\nbroken fn (): Int { no-such-fn(1) }\n";

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// A project `sec` with the given extra hot.hot lines and source files.
fn project(extra_conf: &str, files: &[(&str, &str)]) -> tempfile::TempDir {
    let dir = tempfile::tempdir().expect("temporary project");
    write(
        &dir.path().join("hot.hot"),
        &format!(
            "hot.project.sec.src.paths [\"./hot/src\"]\n\
             hot.project.sec.resources.paths [\"./hot/resources\"]\n\
             hot.set.project \"sec\"\n{}",
            extra_conf
        ),
    );
    for (path, content) in files {
        write(&dir.path().join(path), content);
    }
    dir
}

fn command(dir: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(hot_binary());
    cmd.args(args)
        .current_dir(dir)
        .env("HOT_HOME", hot_home())
        .env("NO_COLOR", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

fn run_hot(dir: &Path, args: &[&str]) -> Output {
    command(dir, args).output().expect("run hot")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).to_string()
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

fn json_diagnostics(output: &Output) -> Vec<serde_json::Value> {
    serde_json::from_str(stdout(output).trim()).unwrap_or_else(|e| {
        panic!("stdout is not a JSON array ({e}): {}", text(output));
    })
}

#[test]
fn allowlisted_secret_shape_does_not_hide_real_errors() {
    let dir = project(
        "hot.build.allow-secret-shape [\"hot/src/sec/a.hot\"]\n",
        &[
            ("hot/src/sec/a.hot", AWS_KEY_SOURCE),
            ("hot/src/sec/b.hot", BROKEN_SOURCE),
        ],
    );

    let output = run_hot(dir.path(), &["check"]);
    let out = text(&output);
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(out.contains("no-such-fn"), "{out}");
    assert!(!out.contains("potential secret"), "{out}");

    let output = run_hot(dir.path(), &["check", "--check.format", "json-min"]);
    let diagnostics = json_diagnostics(&output);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0]["code"], "unresolved-variable");
}

#[test]
fn allowlisted_resource_and_allow_all_pass() {
    let dir = project(
        "hot.build.allow-secret-shape [\"resources/fixtures/*.json\"]\n",
        &[
            ("hot/src/sec/m.hot", "::sec::m ns\n\nok 1\n"),
            (
                "hot/resources/fixtures/sample.json",
                "{\"key\": \"AKIAIOSFODNN7EXAMPLE\"}\n",
            ),
        ],
    );
    let output = run_hot(dir.path(), &["check"]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));

    let dir = project(
        "hot.build.allow-secret-shape true\n",
        &[("hot/src/sec/a.hot", AWS_KEY_SOURCE)],
    );
    let output = run_hot(dir.path(), &["check", "--check.format", "json-min"]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert!(json_diagnostics(&output).is_empty());
}

#[test]
fn allowlisted_dependency_fixture_passes() {
    let lib = tempfile::tempdir().unwrap();
    write(
        &lib.path().join("hot/src/lib/fixtures.hot"),
        "::lib::fixtures ns\n\nkey \"AKIAIOSFODNN7EXAMPLE\"\n",
    );
    let dir = project(
        &format!(
            "hot.project.sec.deps {{\"example.com/lib\": {{\"local\": \"{}\"}}}}\n\
             hot.build.allow-secret-shape [\"hot/pkg/**\"]\n",
            lib.path().display()
        ),
        &[("hot/src/sec/m.hot", "::sec::m ns\n\nok 1\n")],
    );
    let output = run_hot(dir.path(), &["check"]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
}

#[test]
fn unallowed_secret_shape_is_a_diagnostic_in_every_format() {
    let dir = project("", &[("hot/src/sec/a.hot", AWS_KEY_SOURCE)]);

    let output = run_hot(dir.path(), &["check", "--check.format", "json"]);
    let diagnostics = json_diagnostics(&output);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert_eq!(diagnostics.len(), 1, "{diagnostics:?}");
    assert_eq!(diagnostics[0]["code"], "secret-shape");
    assert_eq!(diagnostics[0]["range"]["start"]["line"], 2);
    assert!(
        diagnostics[0]["file"]
            .as_str()
            .unwrap()
            .ends_with("hot/src/sec/a.hot")
    );

    let output = run_hot(dir.path(), &["check", "--check.format", "simple"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(
        out.lines()
            .any(|l| l.contains("a.hot:3:1: error [secret-shape]")),
        "{out}"
    );

    let output = run_hot(dir.path(), &["check"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(out.contains("found 1 potential secret(s)"), "{out}");

    let output = run_hot(dir.path(), &["check", "--allow-secret-shape"]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
}

#[test]
fn simple_format_is_one_line_per_diagnostic() {
    let dir = project("", &[("hot/src/sec/b.hot", BROKEN_SOURCE)]);
    let output = run_hot(dir.path(), &["check", "--check.format", "simple"]);
    let out = stdout(&output);
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert_eq!(out.lines().count(), 1, "{out}");
    assert!(
        out.contains("b.hot:3:21: error [unresolved-variable]"),
        "{out}"
    );
    assert!(!out.contains('\u{1b}'), "{out}");
}

#[test]
fn check_fails_on_a_missing_path_or_no_sources() {
    let dir = project("", &[("hot/src/sec/m.hot", "::sec::m ns\n\nok 1\n")]);
    let output = run_hot(dir.path(), &["check", "nope.hot"]);
    let err = String::from_utf8_lossy(&output.stderr).to_string();
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(err.contains("Path not found: nope.hot"), "{err}");
    assert!(!stdout(&output).contains("No issues found"));

    let empty = project("", &[]);
    let output = run_hot(empty.path(), &["check"]);
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(text(&output).contains("No .hot files found"));

    let output = run_hot(dir.path(), &["check", "--check.format", "jsonl"]);
    assert_eq!(output.status.code(), Some(2), "{}", text(&output));
    assert!(text(&output).contains("Unknown check format 'jsonl'"));
}

#[test]
fn package_directory_check_explains_how_to_check_the_package() {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("pkg.hot"),
        "hot.pkg.lib {name: \"example.com/lib\"}\n",
    );
    write(
        &dir.path().join("src/lib/m.hot"),
        "::lib::m ns\n\nbroken fn (): Int { no-such-fn(1) }\n",
    );
    let output = run_hot(dir.path(), &["check"]);
    let out = text(&output);
    assert_eq!(output.status.code(), Some(2), "{out}");
    assert!(out.contains("hot check src"), "{out}");

    let output = run_hot(dir.path(), &["check", "src"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
}

/// `hot watch` (pretty output) used to panic with "Cannot start a runtime
/// from within a runtime" on the first change.
#[test]
fn watch_reruns_after_a_change() {
    use std::io::BufRead;

    let dir = project("", &[("hot/src/sec/m.hot", "::sec::m ns\n\nok 1\n")]);
    let mut child = command(dir.path(), &["watch", "--watch.debounce", "50"])
        .spawn()
        .expect("start hot watch");
    let stdout = child.stdout.take().unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::BufReader::new(stdout)
            .lines()
            .map_while(Result::ok)
        {
            if tx.send(line).is_err() {
                break;
            }
        }
    });
    let mut out = String::new();
    let mut wait_for = |needle: &str| {
        let deadline = Instant::now() + Duration::from_secs(60);
        while Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(line) => {
                    out.push_str(&line);
                    out.push('\n');
                    if line.contains(needle) {
                        return true;
                    }
                }
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(_) => return false,
            }
        }
        false
    };

    let first = wait_for("No issues found");
    // Give the watcher a moment to register before the change.
    std::thread::sleep(Duration::from_millis(500));
    write(
        &dir.path().join("hot/src/sec/m.hot"),
        "::sec::m ns\n\nok no-such-fn(1)\n",
    );
    let second = first && wait_for("no-such-fn");
    let exited = child.try_wait().unwrap();
    child.kill().ok();
    let mut err = String::new();
    if let Some(mut e) = child.stderr.take() {
        e.read_to_string(&mut err).ok();
    }
    child.wait().ok();
    assert!(first && second, "{out}\n{err}");
    assert!(exited.is_none(), "watch exited: {exited:?}\n{out}\n{err}");
}
