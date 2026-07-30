//! Installed skills keep their native file formats and migrate older stamps.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::path::Path;
use std::process::{Command, ExitStatus};

const MANIFEST_FILE: &str = ".hot-skill-manifest.json";

fn hot_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_hot"))
}

fn run_status(project: &Path, args: &[&str]) -> ExitStatus {
    Command::new(hot_binary())
        .args(args)
        .current_dir(project)
        .status()
        .unwrap_or_else(|e| panic!("run hot {args:?}: {e}"))
}

fn run(project: &Path, args: &[&str]) {
    let status = run_status(project, args);
    assert!(status.success(), "hot {args:?} should succeed");
}

fn legacy_hash(content: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    content.hash(&mut hasher);
    hasher.finish()
}

fn regress_to_legacy_layout(path: &Path, markdown: bool) {
    let content = std::fs::read_to_string(path).expect("read installed skill file");
    let hash = legacy_hash(&content);
    let marker = if markdown {
        format!("<!-- hot-skill-hash:{hash} -->")
    } else {
        format!("// hot-skill-hash:{hash}")
    };
    std::fs::write(path, format!("{marker}\n{content}")).expect("write legacy layout");
}

fn source_skill_file(skill: &str, relative: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../resources/ai/skills")
        .join(skill)
        .join(relative)
}

#[test]
fn hot_ai_installs_raw_files_and_repairs_legacy_stamps() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path();

    run(project, &["ai", "add"]);

    let language_dir = project.join(".skills/hot-language");
    let agent_dir = project.join(".skills/hot-ai-agents");
    let language_skill = language_dir.join("SKILL.md");
    let agent_skill = agent_dir.join("SKILL.md");
    let language_yaml = language_dir.join("agents/openai.yaml");
    let agent_yaml = agent_dir.join("agents/openai.yaml");

    for (skill, installed_yaml) in [
        ("hot-language", &language_yaml),
        ("hot-ai-agents", &agent_yaml),
    ] {
        assert_eq!(
            std::fs::read(installed_yaml).expect("read installed YAML"),
            std::fs::read(source_skill_file(skill, "agents/openai.yaml"))
                .expect("read source YAML"),
            "format-sensitive files must be installed byte-for-byte"
        );
        assert!(
            !std::fs::read_to_string(installed_yaml)
                .unwrap()
                .starts_with("//"),
            "YAML must never receive a Hot line comment"
        );
    }

    std::fs::remove_file(language_dir.join(MANIFEST_FILE)).expect("remove new manifest");
    std::fs::remove_file(agent_dir.join(MANIFEST_FILE)).expect("remove new manifest");
    regress_to_legacy_layout(&language_skill, true);
    regress_to_legacy_layout(&agent_skill, true);
    regress_to_legacy_layout(&agent_yaml, false);

    run(project, &["ai", "update"]);

    assert_eq!(
        std::fs::read(&language_skill).unwrap(),
        std::fs::read(source_skill_file("hot-language", "SKILL.md")).unwrap(),
        "legacy Markdown marker must be removed"
    );
    assert_eq!(
        std::fs::read(&agent_yaml).unwrap(),
        std::fs::read(source_skill_file("hot-ai-agents", "agents/openai.yaml")).unwrap(),
        "legacy YAML corruption must be repaired"
    );
    assert!(language_dir.join(MANIFEST_FILE).is_file());
    assert!(agent_dir.join(MANIFEST_FILE).is_file());

    let repaired = std::fs::read(&agent_yaml).unwrap();
    run(project, &["ai", "update"]);
    assert_eq!(
        std::fs::read(&agent_yaml).unwrap(),
        repaired,
        "migration must converge"
    );
}

#[test]
fn hot_ai_update_preserves_local_edits_when_shipped_source_is_unchanged() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path();

    run(project, &["ai", "add"]);
    let skill_md = project.join(".skills/hot-language/SKILL.md");
    let customized = format!(
        "{}\n<!-- MY LOCAL CUSTOMIZATION -->\n",
        std::fs::read_to_string(&skill_md).unwrap().trim_end()
    );
    std::fs::write(&skill_md, &customized).unwrap();

    run(project, &["ai", "update"]);

    assert_eq!(
        std::fs::read_to_string(&skill_md).unwrap(),
        customized,
        "the sidecar hash tracks shipped source without clobbering local edits"
    );
}

#[test]
fn hot_ai_update_does_not_reinstall_a_removed_skill() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path();

    run(project, &["ai", "add"]);
    let removed = project.join("removed-hot-ai-agents");
    std::fs::rename(project.join(".skills/hot-ai-agents"), &removed)
        .expect("move one installed skill out of .skills");

    run(project, &["ai", "update"]);

    assert!(
        !project.join(".skills/hot-ai-agents").exists(),
        "update must touch only skills that remain installed"
    );
    assert!(project.join(".skills/hot-language").is_dir());
}

#[test]
fn hot_ai_list_and_empty_update_tolerate_a_missing_skill_bundle() {
    let fake_install = tempfile::tempdir().expect("fake install");
    std::fs::create_dir(fake_install.path().join("resources")).unwrap();
    let project = tempfile::tempdir().expect("project");
    let isolated_home = project.path().join("home");
    std::fs::create_dir(&isolated_home).unwrap();

    for args in [&["ai", "list"][..], &["ai", "update"][..]] {
        let status = Command::new(hot_binary())
            .args(args)
            .current_dir(project.path())
            .env("HOT_HOME", fake_install.path())
            .env("HOME", &isolated_home)
            .env("USERPROFILE", &isolated_home)
            .status()
            .unwrap_or_else(|e| panic!("run hot {args:?}: {e}"));
        assert!(
            status.success(),
            "hot {args:?} should not require resources when nothing is installed"
        );
    }
}
