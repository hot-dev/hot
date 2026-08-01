//! Installed skills keep their native file formats and migrate older stamps.

use std::hash::{Hash, Hasher};
use std::path::Path;
use std::process::{Command, ExitStatus};

use siphasher::sip::SipHasher13;

const MANIFEST_FILE: &str = ".hot-skill-manifest.json";
const OWNER_FILE: &str = ".hot-skill-owner";

fn hot_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_hot"))
}

fn run_status(project: &Path, args: &[&str]) -> ExitStatus {
    let home = project.join(".test-home");
    std::fs::create_dir_all(&home).expect("create isolated home");
    Command::new(hot_binary())
        .args(args)
        .current_dir(project)
        .env("HOME", &home)
        .env("USERPROFILE", &home)
        .status()
        .unwrap_or_else(|e| panic!("run hot {args:?}: {e}"))
}

fn run(project: &Path, args: &[&str]) {
    let status = run_status(project, args);
    assert!(status.success(), "hot {args:?} should succeed");
}

fn run_with_home(project: &Path, home: &Path, args: &[&str]) {
    let status = Command::new(hot_binary())
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .status()
        .unwrap_or_else(|e| panic!("run hot {args:?}: {e}"));
    assert!(status.success(), "hot {args:?} should succeed");
}

fn legacy_hash(content: &str) -> u64 {
    let mut hasher = SipHasher13::new();
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
    std::fs::remove_file(language_dir.join(OWNER_FILE)).expect("remove new owner marker");
    std::fs::remove_file(agent_dir.join(OWNER_FILE)).expect("remove new owner marker");
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
fn hot_ai_update_preserves_unmanaged_files() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path();

    run(project, &["ai", "add"]);
    let notes = project.join(".skills/hot-language/my-notes.md");
    std::fs::write(&notes, "keep me\n").unwrap();

    run(project, &["ai", "update"]);

    assert_eq!(
        std::fs::read_to_string(notes).unwrap(),
        "keep me\n",
        "update must not delete files absent from the managed manifest"
    );
}

#[cfg(unix)]
#[test]
fn hot_ai_update_does_not_follow_unmanaged_directory_symlinks() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path();
    let outside = project.join("outside");
    std::fs::create_dir(&outside).unwrap();
    let sentinel = outside.join("keep.txt");
    std::fs::write(&sentinel, "keep me\n").unwrap();

    run(project, &["ai", "add"]);
    symlink(
        &outside,
        project.join(".skills/hot-language/linked-outside"),
    )
    .unwrap();

    run(project, &["ai", "update"]);

    assert_eq!(
        std::fs::read_to_string(sentinel).unwrap(),
        "keep me\n",
        "skill cleanup must never traverse an unmanaged directory symlink"
    );
}

#[cfg(unix)]
#[test]
fn hot_ai_update_rejects_a_managed_file_symlink() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path();

    run(project, &["ai", "add"]);
    let agents_path = project.join("AGENTS.md");
    let agents_before = std::fs::read_to_string(&agents_path).unwrap();
    let stale_agents = agents_before.replacen("hash:", "hash:stale", 1);
    assert_ne!(stale_agents, agents_before);
    std::fs::write(&agents_path, &stale_agents).unwrap();
    let outside = project.join("outside-skill.md");
    std::fs::write(&outside, "do not replace\n").unwrap();
    let managed = project.join(".skills/hot-language/SKILL.md");
    std::fs::remove_file(&managed).unwrap();
    symlink(&outside, &managed).unwrap();

    let status = run_status(project, &["ai", "update"]);

    assert!(!status.success(), "managed symlinks must make update fail");
    assert_eq!(
        std::fs::read_to_string(outside).unwrap(),
        "do not replace\n",
        "skill update must never write through a managed file symlink"
    );
    assert_eq!(
        std::fs::read_to_string(agents_path).unwrap(),
        stale_agents,
        "descendant preflight must fail before AGENTS.md is refreshed"
    );
}

#[cfg(unix)]
#[test]
fn hot_ai_update_rejects_a_managed_directory_symlink() {
    use std::os::unix::fs::symlink;

    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path();

    run(project, &["ai", "add"]);
    let agents_path = project.join("AGENTS.md");
    let agents_before = std::fs::read_to_string(&agents_path).unwrap();
    let stale_agents = agents_before.replacen("hash:", "hash:stale", 1);
    assert_ne!(stale_agents, agents_before);
    std::fs::write(&agents_path, &stale_agents).unwrap();
    let outside = project.join("outside-references");
    std::fs::create_dir(&outside).unwrap();
    let sentinel = outside.join("keep.txt");
    std::fs::write(&sentinel, "keep me\n").unwrap();
    let managed = project.join(".skills/hot-language/references");
    std::fs::remove_dir_all(&managed).unwrap();
    symlink(&outside, &managed).unwrap();

    let status = run_status(project, &["ai", "update"]);

    assert!(
        !status.success(),
        "symlinked managed directories must make update fail"
    );
    assert_eq!(
        std::fs::read_to_string(sentinel).unwrap(),
        "keep me\n",
        "skill update must never write through a managed directory symlink"
    );
    assert_eq!(
        std::fs::read_dir(outside).unwrap().count(),
        1,
        "skill update must not create managed files outside the skill tree"
    );
    assert_eq!(
        std::fs::read_to_string(agents_path).unwrap(),
        stale_agents,
        "descendant preflight must fail before AGENTS.md is refreshed"
    );
}

#[test]
fn hot_ai_update_refreshes_project_and_global_managed_skills() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path();
    let home = project.join("home");
    std::fs::create_dir(&home).unwrap();

    run_with_home(project, &home, &["ai", "add"]);
    run_with_home(project, &home, &["ai", "add", "--global"]);

    std::fs::remove_dir_all(project.join(".skills/hot-ai-agents")).unwrap();
    std::fs::remove_dir_all(home.join(".skills/hot-language")).unwrap();
    let project_manifest = project.join(".skills/hot-language").join(MANIFEST_FILE);
    let global_manifest = home.join(".skills/hot-ai-agents").join(MANIFEST_FILE);
    std::fs::write(&project_manifest, "{not valid json").unwrap();
    std::fs::write(&global_manifest, "{not valid json").unwrap();

    run_with_home(project, &home, &["ai", "update"]);

    for manifest in [project_manifest, global_manifest] {
        let content = std::fs::read_to_string(&manifest).unwrap();
        serde_json::from_str::<serde_json::Value>(&content)
            .unwrap_or_else(|e| panic!("{} was not refreshed: {e}", manifest.display()));
    }
}

#[test]
fn hot_ai_update_repairs_a_managed_skill_missing_skill_md() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path();

    run(project, &["ai", "add"]);
    let skill_md = project.join(".skills/hot-language/SKILL.md");
    std::fs::remove_file(&skill_md).unwrap();

    run(project, &["ai", "update"]);

    assert!(skill_md.is_file(), "managed SKILL.md should be restored");
    assert_eq!(
        std::fs::read(&skill_md).unwrap(),
        std::fs::read(source_skill_file("hot-language", "SKILL.md")).unwrap()
    );
}

#[test]
fn hot_ai_update_repairs_a_corrupt_manifest_without_clobbering_files() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path();

    run(project, &["ai", "add"]);
    let skill_dir = project.join(".skills/hot-language");
    let skill_md = skill_dir.join("SKILL.md");
    let customized = format!(
        "{}\n<!-- MY LOCAL CUSTOMIZATION -->\n",
        std::fs::read_to_string(&skill_md).unwrap().trim_end()
    );
    std::fs::write(&skill_md, &customized).unwrap();
    std::fs::write(skill_dir.join(MANIFEST_FILE), "{not valid json").unwrap();

    run(project, &["ai", "update"]);

    assert_eq!(
        std::fs::read_to_string(&skill_md).unwrap(),
        customized,
        "manifest recovery must preserve existing skill files"
    );
    let repaired = std::fs::read_to_string(skill_dir.join(MANIFEST_FILE)).unwrap();
    let parsed: serde_json::Value =
        serde_json::from_str(&repaired).expect("manifest should be repaired as valid JSON");
    assert_eq!(parsed["version"], 1);
    assert!(
        std::fs::read_dir(&skill_dir).unwrap().all(|entry| {
            !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".hot-skill-manifest.json.tmp-")
        }),
        "atomic manifest write must not leave a temporary file"
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

#[test]
fn hot_ai_list_labels_external_skill_installs_honestly() {
    let project = tempfile::tempdir().expect("project");
    let isolated_home = project.path().join("home");
    let skill_dir = project.path().join(".skills/hot-language");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::create_dir(&isolated_home).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: hot-language\ndescription: External install.\n---\n",
    )
    .unwrap();

    let output = Command::new(hot_binary())
        .args(["ai", "list"])
        .current_dir(project.path())
        .env("HOME", &isolated_home)
        .env("USERPROFILE", &isolated_home)
        .output()
        .expect("run hot ai list");
    assert!(output.status.success());

    let stdout = String::from_utf8(output.stdout).expect("UTF-8 stdout");
    assert!(
        stdout.contains(".skills/hot-language/  (present - project, externally managed)"),
        "external skill status should be explicit:\n{stdout}"
    );
    assert!(
        !stdout.contains(".skills/hot-language/  (installed - project)"),
        "external skill must not be reported as managed by hot ai:\n{stdout}"
    );
}

#[test]
fn hot_ai_add_refuses_to_overwrite_or_adopt_an_external_skill() {
    let project = tempfile::tempdir().expect("project");
    let skill_dir = project.path().join(".skills/hot-language");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let skill_md = skill_dir.join("SKILL.md");
    let external = "---\nname: hot-language\ndescription: External install.\n---\n";
    std::fs::write(&skill_md, external).unwrap();

    let status = run_status(project.path(), &["ai", "add"]);

    assert!(
        !status.success(),
        "external skill collision must reject add"
    );
    assert_eq!(std::fs::read_to_string(skill_md).unwrap(), external);
    assert!(!skill_dir.join(MANIFEST_FILE).exists());
    assert!(
        !project.path().join("AGENTS.md").exists(),
        "preflight failure must not leave a partial AGENTS.md install"
    );
}

#[test]
fn hot_ai_add_uses_an_empty_preexisting_skill_directory() {
    let project = tempfile::tempdir().expect("project");
    let skill_dir = project.path().join(".skills/hot-language");
    std::fs::create_dir_all(&skill_dir).unwrap();

    run(project.path(), &["ai", "add"]);

    assert!(skill_dir.join("SKILL.md").is_file());
    assert!(skill_dir.join(MANIFEST_FILE).is_file());
    assert!(skill_dir.join(OWNER_FILE).is_file());
}

#[test]
fn hot_ai_update_recovers_when_the_manifest_was_deleted() {
    let project = tempfile::tempdir().expect("project");
    run(project.path(), &["ai", "add"]);

    let skill_dir = project.path().join(".skills/hot-language");
    let skill_md = skill_dir.join("SKILL.md");
    let customized = format!(
        "{}\n<!-- LOCAL -->\n",
        std::fs::read_to_string(&skill_md).unwrap().trim_end()
    );
    std::fs::write(&skill_md, &customized).unwrap();
    std::fs::remove_file(skill_dir.join(MANIFEST_FILE)).unwrap();

    run(project.path(), &["ai", "update"]);

    assert_eq!(std::fs::read_to_string(skill_md).unwrap(), customized);
    assert!(skill_dir.join(MANIFEST_FILE).is_file());
    assert!(skill_dir.join(OWNER_FILE).is_file());
}

#[test]
fn hot_ai_update_rejects_an_unsupported_manifest_before_touching_agents() {
    let project = tempfile::tempdir().expect("project");
    run(project.path(), &["ai", "add"]);

    let agents_path = project.path().join("AGENTS.md");
    let agents_before = std::fs::read_to_string(&agents_path).unwrap();
    let stale_agents = agents_before.replacen("hash:", "hash:stale", 1);
    assert_ne!(stale_agents, agents_before);
    std::fs::write(&agents_path, &stale_agents).unwrap();

    let manifest_path = project
        .path()
        .join(".skills/hot-language")
        .join(MANIFEST_FILE);
    std::fs::write(&manifest_path, "{\"version\":2,\"files\":{}}\n").unwrap();

    let status = run_status(project.path(), &["ai", "update"]);

    assert!(
        !status.success(),
        "unsupported manifests must reject update"
    );
    assert_eq!(
        std::fs::read_to_string(agents_path).unwrap(),
        stale_agents,
        "manifest validation must happen before AGENTS.md is refreshed"
    );
}

#[cfg(unix)]
#[test]
fn hot_ai_add_rejects_a_symlinked_skills_root_before_writing_agents() {
    use std::os::unix::fs::symlink;

    let project = tempfile::tempdir().expect("project");
    let outside = project.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    symlink(&outside, project.path().join(".skills")).unwrap();

    let status = run_status(project.path(), &["ai", "add"]);

    assert!(!status.success(), "symlinked skills root must reject add");
    assert_eq!(std::fs::read_dir(outside).unwrap().count(), 0);
    assert!(
        !project.path().join("AGENTS.md").exists(),
        "skills preflight must happen before AGENTS.md is written"
    );
}

#[cfg(unix)]
#[test]
fn hot_ai_update_rejects_a_symlinked_skills_root_before_touching_agents() {
    use std::os::unix::fs::symlink;

    let project = tempfile::tempdir().expect("project");
    run(project.path(), &["ai", "add"]);

    // Make the AGENTS.md managed section stale so a half-completed update
    // would be observable, then swap .skills for a symlink.
    let agents_path = project.path().join("AGENTS.md");
    let agents_before = std::fs::read_to_string(&agents_path).expect("read AGENTS.md");
    let stale = agents_before.replacen("hash:", "hash:stale", 1);
    assert_ne!(stale, agents_before, "expected a section hash to perturb");
    std::fs::write(&agents_path, &stale).unwrap();

    let real_skills = project.path().join(".skills-real");
    std::fs::rename(project.path().join(".skills"), &real_skills).unwrap();
    symlink(&real_skills, project.path().join(".skills")).unwrap();

    let status = run_status(project.path(), &["ai", "update"]);

    assert!(
        !status.success(),
        "symlinked skills root must reject update"
    );
    assert_eq!(
        std::fs::read_to_string(&agents_path).expect("read AGENTS.md"),
        stale,
        "update must preflight skill paths before rewriting AGENTS.md"
    );
}
