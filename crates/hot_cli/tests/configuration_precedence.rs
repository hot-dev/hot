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
        .output()
        .expect("run hot conf");

    assert!(output.status.success(), "hot conf should succeed");
    let stdout = String::from_utf8(output.stdout).expect("configuration output is UTF-8");
    assert!(
        stdout.contains("hot.jit.threshold 1"),
        "HOT_JIT_THRESHOLD must override project-resolved defaults:\n{stdout}"
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
