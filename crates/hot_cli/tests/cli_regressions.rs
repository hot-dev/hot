//! Black-box regressions for CLI commands: daemon configuration, the
//! project each command uses, `hot init`, `hot deps`, `hot run`/`hot eval`,
//! and file-backed settings.
#![cfg(unix)]

use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

fn hot_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_hot"))
}

/// A `HOT_HOME` for one test directory, holding only the checkout's
/// packages: the checkout's hot-std wins over an installed release, and the
/// default database (`$HOT_HOME/db`) belongs to that directory's commands.
fn hot_home(dir: &Path) -> String {
    let home = dir.join(".test-hot-home");
    if !home.join("pkg").exists() {
        std::fs::create_dir_all(&home).unwrap();
        let pkg = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hot/pkg");
        std::os::unix::fs::symlink(pkg.canonicalize().unwrap(), home.join("pkg")).unwrap();
    }
    home.display().to_string()
}

fn write(path: &Path, content: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, content).unwrap();
}

/// Runs `hot` in `dir` with extra environment variables, killing it if it
/// outlives the deadline (a respawn loop never exits on its own).
fn run_hot_env(dir: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let mut cmd = Command::new(hot_binary());
    cmd.args(args)
        .current_dir(dir)
        .env("HOT_HOME", hot_home(dir))
        .env("NO_COLOR", "1")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (k, v) in envs {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn().expect("run hot");
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

fn run_hot(dir: &Path, args: &[&str]) -> Output {
    run_hot_env(dir, args, &[])
}

fn text(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

/// Two projects, alpha (the default) and beta, each with one source file.
fn two_projects(extra_conf: &str) -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("hot.hot"),
        &format!(
            "hot.project.alpha.src.paths [\"./alpha/src\"]\n\
             hot.project.beta.src.paths [\"./beta/src\"]\n\
             hot.set.project \"alpha\"\n{}",
            extra_conf
        ),
    );
    write(
        &dir.path().join("alpha/src/alpha/m.hot"),
        "::alpha::m ns\n\nok 1\n",
    );
    write(
        &dir.path().join("beta/src/beta/m.hot"),
        "::beta::m ns\n\nbroken fn (): Int { no-such-fn(1) }\n",
    );
    dir
}

#[test]
fn daemon_configuration_does_not_respawn_other_commands() {
    let dir = two_projects("hot.daemon true\n");
    let output = run_hot(dir.path(), &["check"]);
    let out = text(&output);
    assert_eq!(output.status.code(), Some(0), "{out}");
    assert!(!out.contains("Started in daemon mode"), "{out}");
    assert!(out.contains("No issues found"), "{out}");

    let output = run_hot_env(
        dir.path(),
        &["eval", "add(1, 2)"],
        &[("HOT_DAEMON", "true")],
    );
    let out = text(&output);
    assert!(!out.contains("Started in daemon mode"), "{out}");
    assert!(out.contains('3'), "{out}");
}

#[test]
fn compile_uses_its_positional_project() {
    let dir = two_projects("");
    let output = run_hot(dir.path(), &["compile", "beta"]);
    let out = text(&output);
    assert_ne!(output.status.code(), Some(0), "{out}");
    assert!(!out.contains("compiled successfully"), "{out}");

    let output = run_hot(dir.path(), &["compile", "nosuch"]);
    let out = text(&output);
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(out.contains("Project 'nosuch' is not defined"), "{out}");

    let output = run_hot(dir.path(), &["compile", "beta", "-p", "alpha"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
}

#[test]
fn build_and_deploy_refuse_an_undefined_project() {
    let dir = two_projects("");
    for args in [
        &["build", "-p", "alpah"][..],
        &["deploy", "--local", "-p", "alpah"][..],
    ] {
        let output = run_hot(dir.path(), args);
        let out = text(&output);
        assert_eq!(output.status.code(), Some(1), "{args:?}: {out}");
        assert!(out.contains("Project 'alpah' is not defined"), "{out}");
        assert!(!out.contains("Build created"), "{out}");
    }
}

#[test]
fn dev_rejects_a_project_flag_for_another_project() {
    let dir = two_projects("");
    let output = run_hot(dir.path(), &["dev", "-p", "beta"]);
    let out = text(&output);
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(out.contains("does not take -p"), "{out}");

    let undefined = tempfile::tempdir().unwrap();
    write(
        &undefined.path().join("hot.hot"),
        "hot.project.drafting.src.paths [\"./hot/src\"]\n",
    );
    let output = run_hot(undefined.path(), &["dev"]);
    let out = text(&output);
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(out.contains("hot.set.project \"drafting\""), "{out}");
    assert!(!out.contains("-p <name>"), "{out}");
}

#[test]
fn init_derives_a_valid_project_name_and_ignores_the_dev_key() {
    let parent = tempfile::tempdir().unwrap();
    let dir = parent.path().join("2024 app.v2");
    std::fs::create_dir_all(&dir).unwrap();
    let output = run_hot(&dir, &["init"]);
    let out = text(&output);
    assert_eq!(output.status.code(), Some(0), "{out}");
    assert!(out.contains("Project name: app-2024-app-v2"), "{out}");
    assert!(dir.join("hot/src/app-2024-app-v2/hi.hot").exists(), "{out}");
    let gitignore = std::fs::read_to_string(dir.join(".gitignore")).unwrap();
    assert!(gitignore.lines().any(|l| l == "hot/dev.key"), "{gitignore}");

    let output = run_hot(&dir, &["check"]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));

    let named = parent.path().join("named");
    std::fs::create_dir_all(&named).unwrap();
    let output = run_hot(&named, &["init", "--name", "my.app"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
    assert!(text(&output).contains("not a valid project name"));
    let output = run_hot(&named, &["init", "--name", "chosen"]);
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert!(named.join("hot/src/chosen/hi.hot").exists());
}

#[test]
fn deps_add_and_remove_fail_instead_of_pretending() {
    let dir = two_projects("");
    let output = run_hot(dir.path(), &["deps", "add", "hot.dev/x", "--local", "./x"]);
    let out = text(&output);
    assert_eq!(output.status.code(), Some(1), "{out}");
    assert!(
        out.contains("hot.project.alpha.deps {\"hot.dev/x\": {\"local\": \"./x\"}}"),
        "{out}"
    );
    let output = run_hot(dir.path(), &["deps", "remove", "hot.dev/x"]);
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
}

#[test]
fn run_of_a_bare_file_name_loads_sibling_namespaces() {
    let dir = tempfile::tempdir().unwrap();
    write(&dir.path().join("sb.hot"), "::sb ns\n\nanswer 42\n");
    write(&dir.path().join("main.hot"), "::main ns\n\n::sb/answer\n");
    let output = run_hot(dir.path(), &["run", "main.hot"]);
    let out = text(&output);
    assert_eq!(output.status.code(), Some(0), "{out}");
    assert!(out.contains("42"), "{out}");
}

#[test]
fn eval_can_use_the_store() {
    let dir = two_projects("");
    let store = "::hot::store/Map({name: \"eval-test\"})";
    let output = run_hot(
        dir.path(),
        &["eval", &format!("::hot::store/put({store}, \"k\", \"v\")")],
    );
    let out = text(&output);
    assert!(!out.contains("Store not configured"), "{out}");
    let output = run_hot(
        dir.path(),
        &["eval", &format!("::hot::store/get({store}, \"k\")")],
    );
    let out = text(&output);
    assert!(!out.contains("Store not configured"), "{out}");
    assert!(out.contains("\"v\""), "{out}");
}

#[test]
fn unrelated_file_variables_do_not_abort_commands() {
    let dir = tempfile::tempdir().unwrap();
    let output = run_hot_env(
        dir.path(),
        &["eval", "add(1, 2)"],
        &[("HOT_PUBLIC_REF_FILE", "/nonexistent/ref")],
    );
    let out = text(&output);
    assert_eq!(output.status.code(), Some(0), "{out}");
    assert!(out.contains('3'), "{out}");

    let output = run_hot_env(
        dir.path(),
        &["eval", "add(1, 2)"],
        &[("HOT_DB_URI_FILE", "/nonexistent/uri")],
    );
    assert_eq!(output.status.code(), Some(1), "{}", text(&output));
}

#[test]
fn conf_redacts_values_from_files_and_env_get_reads_them() {
    let dir = two_projects("hot.custom.secret ::hot::env/get(\"HOT_CUSTOM_SECRET\", \"unset\")\n");
    let secret = dir.path().join("secret.txt");
    std::fs::write(&secret, "AKIAFILEVALUE000\n").unwrap();
    let output = run_hot_env(
        dir.path(),
        &["conf"],
        &[
            ("HOT_FILE_AWS_ACCESS_KEY_ID_FILE", secret.to_str().unwrap()),
            ("HOT_CUSTOM_SECRET_FILE", secret.to_str().unwrap()),
        ],
    );
    let out = text(&output);
    assert_eq!(output.status.code(), Some(0), "{out}");
    assert!(!out.contains("AKIAFILEVALUE000"), "{out}");

    let output = run_hot_env(
        dir.path(),
        &["eval", "::hot::env/get(\"HOT_CUSTOM_SECRET\", \"unset\")"],
        &[("HOT_CUSTOM_SECRET_FILE", secret.to_str().unwrap())],
    );
    let out = text(&output);
    assert!(out.contains("AKIAFILEVALUE000"), "{out}");
}

#[test]
fn ai_add_global_does_not_write_agents_md_here() {
    let dir = tempfile::tempdir().unwrap();
    let home = tempfile::tempdir().unwrap();
    let output = run_hot_env(
        dir.path(),
        &["ai", "add", "--global"],
        &[("HOME", home.path().to_str().unwrap())],
    );
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert!(!dir.path().join("AGENTS.md").exists());
}

#[test]
fn key_create_works_on_a_fresh_project_and_errors_go_to_stderr() {
    let dir = two_projects("");
    let output = run_hot(dir.path(), &["key", "create"]);
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_eq!(output.status.code(), Some(0), "{}", text(&output));
    assert!(stdout.trim().starts_with("hot_"), "{stdout}");
    assert_eq!(stdout.trim().lines().count(), 1, "{stdout}");

    let output = run_hot_env(
        dir.path(),
        &["key", "create"],
        &[("HOT_DB_URI", "postgres://nobody@127.0.0.1:1/none")],
    );
    assert_ne!(output.status.code(), Some(0), "{}", text(&output));
    assert!(
        String::from_utf8_lossy(&output.stdout).trim().is_empty(),
        "{}",
        text(&output)
    );
}

/// A project and its dependency package may each declare a same-named type
/// and an arrow to it from `HttpResponse`, the shape nearly every API package
/// has. The project's arrow used to run for the package's type, or the other
/// way round, so a redacting arrow could be skipped.
#[test]
fn a_dependency_keeps_its_own_arrow_to_a_same_named_type() {
    const API: &str = r#"HttpResponse ::hot::http/HttpResponse
HttpError type { status: Int, source: Str, body: Any }
HttpResponse -> HttpError fn (r: HttpResponse): HttpError {
    HttpError({status: r.status, source: OWNER, body: "redacted"})
}
fail fn (): Any {
    HttpError(HttpResponse({status: 500, headers: {}, body: {ssn: "123"}}))
}
take fn (e: HttpError): Any { e.source }
take-any fn (x: Any): Any { take(x) }
response fn (): Any { HttpResponse({status: 500, headers: {}, body: {ssn: "123"}}) }
"#;
    let lib = tempfile::tempdir().unwrap();
    write(
        &lib.path().join("hot/src/lib/api.hot"),
        &format!("::lib::api ns\nOWNER \"lib\"\n{API}"),
    );
    let dir = tempfile::tempdir().unwrap();
    write(
        &dir.path().join("hot.hot"),
        &format!(
            "hot.project.app.deps {{\"example.com/lib\": {{\"local\": \"{}\"}}}}\n\
             hot.project.app.src.paths [\"./hot/src\"]\n\
             hot.set.project \"app\"\n",
            lib.path().display()
        ),
    );
    write(
        &dir.path().join("hot/src/app/api.hot"),
        &format!("::app::api ns\nOWNER \"app\"\n{API}"),
    );

    let code = r#"println(to-json(untype(::app::api/fail())))
println(to-json(untype(::lib::api/fail())))
println(::app::api/take-any(::app::api/response()))
println(::lib::api/take-any(::lib::api/response()))
1"#;
    for threshold in ["100", "1"] {
        let output = run_hot(dir.path(), &["eval", "--jit.threshold", threshold, code]);
        assert_eq!(output.status.code(), Some(0), "{}", text(&output));
        let out = String::from_utf8_lossy(&output.stdout);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(
            lines[..4],
            [
                r#"{"status":500,"source":"app","body":"redacted"}"#,
                r#"{"status":500,"source":"lib","body":"redacted"}"#,
                "app",
                "lib",
            ],
            "threshold {threshold}: {out}"
        );
    }
}
