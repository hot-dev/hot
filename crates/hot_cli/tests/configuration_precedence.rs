use std::path::Path;
use std::process::Command;

fn hot_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_hot"))
}

#[test]
fn hot_environment_overrides_survive_project_configuration_loading() {
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(hot_binary())
        .arg("conf")
        .current_dir(project_root)
        .env("HOT_JIT_THRESHOLD", "1")
        .env("HOT_JIT_MODE", "off")
        .output()
        .expect("run hot conf");

    assert!(output.status.success(), "hot conf should succeed");
    let stdout = String::from_utf8(output.stdout).expect("configuration output is UTF-8");
    assert!(
        stdout.lines().any(|line| line == "hot.jit.threshold 1"),
        "HOT_JIT_THRESHOLD must override project-resolved defaults:\n{stdout}"
    );
    assert!(
        stdout.lines().any(|line| line == "hot.jit.mode \"off\""),
        "HOT_JIT_MODE must override project-resolved defaults:\n{stdout}"
    );
}

#[test]
fn hot_environment_unknown_extension_cannot_destroy_a_scalar_setting() {
    // HOT_LOG_LEVEL_EXTRA maps to log.level.extra, which extends the existing
    // scalar log.level. Setting through it would replace the scalar with a
    // materialized map, silently wiping the configured value; such variables
    // are ignored instead.
    let project_root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let output = Command::new(hot_binary())
        .arg("conf")
        .current_dir(project_root)
        .env("HOT_LOG_LEVEL_EXTRA", "x")
        .output()
        .expect("run hot conf");

    assert!(output.status.success(), "hot conf should succeed");
    let stdout = String::from_utf8(output.stdout).expect("configuration output is UTF-8");
    assert!(
        stdout.lines().any(|line| line == "hot.log.level \"info\""),
        "existing scalar must survive an unknown extending env var:\n{stdout}"
    );
    assert!(
        !stdout.contains("hot.log.level.extra"),
        "the extending env var must be ignored, not applied:\n{stdout}"
    );
}

#[test]
fn hot_eval_returns_the_updated_collection_for_terminal_dynamic_assignment() {
    let project = tempfile::tempdir().expect("temporary eval project");
    let output = Command::new(hot_binary())
        .args(["eval", "record {}\nkey \"name\"\nrecord[key] \"Ada\""])
        .current_dir(project.path())
        .output()
        .expect("run hot eval");

    assert!(output.status.success(), "hot eval should succeed");
    let stdout = String::from_utf8(output.stdout).expect("eval output is UTF-8");
    assert!(
        stdout.contains("name: \"Ada\""),
        "unexpected eval output: {stdout}"
    );
    assert_ne!(
        stdout.trim(),
        "\"name\"",
        "eval must not echo the key operand"
    );
}

#[test]
fn hot_eval_does_not_substitute_a_preceding_binding_for_terminal_null() {
    let project = tempfile::tempdir().expect("temporary eval project");
    let output = Command::new(hot_binary())
        .args(["eval", "mapgz {}\nkeyz \"nokey\"\nmapgz[keyz]"])
        .current_dir(project.path())
        .output()
        .expect("run hot eval");

    assert!(output.status.success(), "hot eval should succeed");
    let stdout = String::from_utf8(output.stdout).expect("eval output is UTF-8");
    assert!(
        stdout.trim().is_empty(),
        "terminal null follows the CLI's normal no-output convention; got {stdout:?}"
    );
}
