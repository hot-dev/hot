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

    let language_dir = project.join(".agents/skills/hot-language");
    let agent_dir = project.join(".agents/skills/hot-ai-agents");
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
    let skill_md = project.join(".agents/skills/hot-language/SKILL.md");
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
    let notes = project.join(".agents/skills/hot-language/my-notes.md");
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
        project.join(".agents/skills/hot-language/linked-outside"),
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
    let managed = project.join(".agents/skills/hot-language/SKILL.md");
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
    let managed = project.join(".agents/skills/hot-language/references");
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

    std::fs::remove_dir_all(project.join(".agents/skills/hot-ai-agents")).unwrap();
    std::fs::remove_dir_all(home.join(".agents/skills/hot-language")).unwrap();
    let project_manifest = project
        .join(".agents/skills/hot-language")
        .join(MANIFEST_FILE);
    let global_manifest = home
        .join(".agents/skills/hot-ai-agents")
        .join(MANIFEST_FILE);
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
    let skill_md = project.join(".agents/skills/hot-language/SKILL.md");
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
    let skill_dir = project.join(".agents/skills/hot-language");
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
    std::fs::rename(project.join(".agents/skills/hot-ai-agents"), &removed)
        .expect("move one installed skill out of .agents/skills");

    run(project, &["ai", "update"]);

    assert!(
        !project.join(".agents/skills/hot-ai-agents").exists(),
        "update must touch only skills that remain installed"
    );
    assert!(project.join(".agents/skills/hot-language").is_dir());
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

fn hot_output(project: &Path, home: &Path, args: &[&str]) -> (bool, String) {
    let output = Command::new(hot_binary())
        .args(args)
        .current_dir(project)
        .env("HOME", home)
        .env("USERPROFILE", home)
        .output()
        .unwrap_or_else(|e| panic!("run hot {args:?}: {e}"));
    // Windows prints paths with `\`; compare them in one spelling.
    (
        output.status.success(),
        String::from_utf8(output.stdout)
            .expect("UTF-8 stdout")
            .replace('\\', "/"),
    )
}

/// Every file under `dir`, relative to it, with its bytes.
fn tree(dir: &Path) -> Vec<(String, Vec<u8>)> {
    fn walk(dir: &Path, base: &Path, out: &mut Vec<(String, Vec<u8>)>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, base, out);
            } else {
                let rel = path
                    .strip_prefix(base)
                    .unwrap()
                    .to_string_lossy()
                    .replace('\\', "/");
                out.push((rel, std::fs::read(&path).unwrap()));
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// Turn a fresh project install into what Hot 3.0.11 and earlier left: one
/// managed copy of each skill under `.skills/`.
fn regress_to_dot_skills(root: &Path) {
    std::fs::create_dir_all(root.join(".skills")).unwrap();
    for skill in ["hot-language", "hot-ai-agents"] {
        std::fs::rename(
            root.join(".agents/skills").join(skill),
            root.join(".skills").join(skill),
        )
        .unwrap();
    }
    std::fs::remove_dir_all(root.join(".agents")).unwrap();
    std::fs::remove_dir_all(root.join(".claude")).unwrap();
}

#[test]
fn hot_ai_add_installs_each_skill_where_agents_read_it() {
    let project = tempfile::tempdir().expect("project");
    run(project.path(), &["ai", "add"]);

    for skill in ["hot-language", "hot-ai-agents"] {
        let shared = project.path().join(".agents/skills").join(skill);
        let claude = project.path().join(".claude/skills").join(skill);
        assert!(shared.join(MANIFEST_FILE).is_file());
        assert_eq!(tree(&shared), tree(&claude), "both copies must match");
    }
    assert!(!project.path().join(".skills").exists());
    let agents = std::fs::read_to_string(project.path().join("AGENTS.md")).unwrap();
    assert!(agents.contains("`.agents/skills/hot-language/`"));
}

#[test]
fn hot_ai_add_global_installs_to_both_home_skill_folders() {
    let project = tempfile::tempdir().expect("project");
    let home = project.path().join("home");
    std::fs::create_dir(&home).unwrap();

    run_with_home(project.path(), &home, &["ai", "add", "--global"]);

    for base in [".agents/skills", ".claude/skills"] {
        assert!(home.join(base).join("hot-language/SKILL.md").is_file());
    }
    assert!(!home.join(".skills").exists());
    assert!(!project.path().join("AGENTS.md").exists());
    assert!(!project.path().join(".agents").exists());
}

#[test]
fn hot_ai_update_moves_skills_out_of_dot_skills() {
    let project = tempfile::tempdir().expect("project");
    let home = project.path().join("home");
    std::fs::create_dir(&home).unwrap();
    run_with_home(project.path(), &home, &["ai", "add"]);
    run_with_home(project.path(), &home, &["ai", "add", "--global"]);
    regress_to_dot_skills(project.path());
    regress_to_dot_skills(&home);

    let (ok, listed) = hot_output(project.path(), &home, &["ai", "list"]);
    assert!(ok);
    assert!(
        listed.contains(".skills/hot-language/  old location, agents don't read it"),
        "list must flag the old folder:\n{listed}"
    );

    run_with_home(project.path(), &home, &["ai", "update"]);

    for root in [project.path(), home.as_path()] {
        assert!(
            !root.join(".skills").exists(),
            "the old folder must be removed once empty"
        );
        for base in [".agents/skills", ".claude/skills"] {
            for skill in ["hot-language", "hot-ai-agents"] {
                assert_eq!(
                    std::fs::read(root.join(base).join(skill).join("SKILL.md")).unwrap(),
                    std::fs::read(source_skill_file(skill, "SKILL.md")).unwrap()
                );
            }
        }
    }
}

#[test]
fn hot_ai_update_keeps_a_changed_dot_skills_copy() {
    let project = tempfile::tempdir().expect("project");
    run(project.path(), &["ai", "add"]);
    regress_to_dot_skills(project.path());
    let edited = project.path().join(".skills/hot-language/SKILL.md");
    std::fs::write(&edited, "my edits\n").unwrap();
    let notes = project.path().join(".skills/hot-ai-agents/notes.md");
    std::fs::write(&notes, "mine\n").unwrap();

    run(project.path(), &["ai", "update"]);

    assert_eq!(std::fs::read_to_string(&edited).unwrap(), "my edits\n");
    assert_eq!(std::fs::read_to_string(&notes).unwrap(), "mine\n");
    assert!(
        project
            .path()
            .join(".agents/skills/hot-language/SKILL.md")
            .is_file(),
        "the new copy is installed even when the old one stays"
    );
}

#[test]
fn hot_ai_update_global_leaves_the_project_alone() {
    let project = tempfile::tempdir().expect("project");
    let home = project.path().join("home");
    std::fs::create_dir(&home).unwrap();
    run_with_home(project.path(), &home, &["ai", "add"]);
    run_with_home(project.path(), &home, &["ai", "add", "--global"]);

    let agents_path = project.path().join("AGENTS.md");
    let stale_agents =
        std::fs::read_to_string(&agents_path)
            .unwrap()
            .replacen("hash:", "hash:stale", 1);
    std::fs::write(&agents_path, &stale_agents).unwrap();
    let project_manifest = project
        .path()
        .join(".agents/skills/hot-language")
        .join(MANIFEST_FILE);
    let global_manifest = home.join(".agents/skills/hot-language").join(MANIFEST_FILE);
    std::fs::write(&project_manifest, "{not valid json").unwrap();
    std::fs::write(&global_manifest, "{not valid json").unwrap();

    run_with_home(project.path(), &home, &["ai", "update", "--global"]);

    assert_eq!(std::fs::read_to_string(&agents_path).unwrap(), stale_agents);
    assert_eq!(
        std::fs::read_to_string(&project_manifest).unwrap(),
        "{not valid json"
    );
    serde_json::from_str::<serde_json::Value>(&std::fs::read_to_string(&global_manifest).unwrap())
        .expect("global manifest refreshed");
}

#[test]
fn hot_ai_list_reports_out_of_date_files() {
    let project = tempfile::tempdir().expect("project");
    let home = project.path().join("home");
    std::fs::create_dir(&home).unwrap();
    run_with_home(project.path(), &home, &["ai", "add"]);

    let (ok, listed) = hot_output(project.path(), &home, &["ai", "list"]);
    assert!(ok);
    assert!(listed.contains("AGENTS.md  up to date"), "{listed}");
    assert!(
        listed.contains(".agents/skills/hot-language/  up to date"),
        "{listed}"
    );
    assert!(!listed.contains("hot ai update"), "{listed}");

    // A manifest from another release records other shipped hashes.
    let manifest_path = project
        .path()
        .join(".claude/skills/hot-language")
        .join(MANIFEST_FILE);
    let mut manifest: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
    manifest["files"]["SKILL.md"] = serde_json::Value::from("0".repeat(64));
    std::fs::write(&manifest_path, manifest.to_string()).unwrap();
    let agents_path = project.path().join("AGENTS.md");
    let stale_agents =
        std::fs::read_to_string(&agents_path)
            .unwrap()
            .replacen("hash:", "hash:stale", 1);
    std::fs::write(&agents_path, stale_agents).unwrap();

    let (ok, listed) = hot_output(project.path(), &home, &["ai", "list"]);
    assert!(ok);
    assert!(listed.contains("AGENTS.md  out of date"), "{listed}");
    assert!(
        listed.contains(".claude/skills/hot-language/  out of date"),
        "{listed}"
    );
    assert!(
        listed.contains(".agents/skills/hot-language/  up to date"),
        "{listed}"
    );
    assert!(listed.contains("Use 'hot ai update'"), "{listed}");
}

#[test]
fn hot_ai_list_labels_external_skill_installs_honestly() {
    let project = tempfile::tempdir().expect("project");
    let isolated_home = project.path().join("home");
    let skill_dir = project.path().join(".agents/skills/hot-language");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::create_dir(&isolated_home).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: hot-language\ndescription: External install.\n---\n",
    )
    .unwrap();

    let (ok, stdout) = hot_output(project.path(), &isolated_home, &["ai", "list"]);
    assert!(ok);
    assert!(
        stdout.contains(".agents/skills/hot-language/  managed outside Hot"),
        "external skill status should be explicit:\n{stdout}"
    );
    assert!(
        !stdout.contains("hot-language/  up to date"),
        "external skill must not be reported as managed by hot ai:\n{stdout}"
    );
}

#[test]
fn hot_ai_add_skips_an_external_skill_but_fills_the_other_folder() {
    let project = tempfile::tempdir().expect("project");
    let skill_dir = project.path().join(".agents/skills/hot-language");
    std::fs::create_dir_all(&skill_dir).unwrap();
    let skill_md = skill_dir.join("SKILL.md");
    let external = "---\nname: hot-language\ndescription: External install.\n---\n";
    std::fs::write(&skill_md, external).unwrap();

    run(project.path(), &["ai", "add"]);

    assert_eq!(std::fs::read_to_string(&skill_md).unwrap(), external);
    assert!(!skill_dir.join(MANIFEST_FILE).exists());
    assert!(
        project
            .path()
            .join(".claude/skills/hot-language")
            .join(MANIFEST_FILE)
            .is_file()
    );
}

#[test]
fn hot_ai_add_refuses_to_overwrite_or_adopt_an_external_skill() {
    let project = tempfile::tempdir().expect("project");
    let external = "---\nname: hot-language\ndescription: External install.\n---\n";
    let mut skill_dirs = Vec::new();
    for base in [".agents/skills", ".claude/skills"] {
        let skill_dir = project.path().join(base).join("hot-language");
        std::fs::create_dir_all(&skill_dir).unwrap();
        std::fs::write(skill_dir.join("SKILL.md"), external).unwrap();
        skill_dirs.push(skill_dir);
    }

    let status = run_status(project.path(), &["ai", "add"]);

    assert!(
        !status.success(),
        "a skill no folder can take must reject add"
    );
    for skill_dir in skill_dirs {
        assert_eq!(
            std::fs::read_to_string(skill_dir.join("SKILL.md")).unwrap(),
            external
        );
        assert!(!skill_dir.join(MANIFEST_FILE).exists());
    }
    assert!(
        !project.path().join("AGENTS.md").exists(),
        "preflight failure must not leave a partial AGENTS.md install"
    );
}

#[test]
fn hot_ai_add_uses_an_empty_preexisting_skill_directory() {
    let project = tempfile::tempdir().expect("project");
    let skill_dir = project.path().join(".agents/skills/hot-language");
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

    let skill_dir = project.path().join(".agents/skills/hot-language");
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
        .join(".agents/skills/hot-language")
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
fn hot_ai_add_skips_a_symlinked_skills_folder() {
    use std::os::unix::fs::symlink;

    // A common setup: ~/.claude/skills links to ~/.agents/skills.
    let project = tempfile::tempdir().expect("project");
    let home = project.path().join("home");
    std::fs::create_dir_all(home.join(".agents/skills")).unwrap();
    std::fs::create_dir_all(home.join(".claude")).unwrap();
    symlink(home.join(".agents/skills"), home.join(".claude/skills")).unwrap();

    let (ok, stdout) = hot_output(project.path(), &home, &["ai", "add", "--global"]);

    assert!(ok, "{stdout}");
    assert!(stdout.contains("it is a symlink"), "{stdout}");
    assert!(home.join(".agents/skills/hot-language/SKILL.md").is_file());
    assert!(
        std::fs::symlink_metadata(home.join(".claude/skills"))
            .unwrap()
            .file_type()
            .is_symlink(),
        "the link itself must be left alone"
    );
}

#[cfg(unix)]
#[test]
fn hot_ai_add_rejects_symlinked_skills_folders_before_writing_agents() {
    use std::os::unix::fs::symlink;

    let project = tempfile::tempdir().expect("project");
    let outside = project.path().join("outside");
    std::fs::create_dir(&outside).unwrap();
    for parent in [".agents", ".claude"] {
        std::fs::create_dir(project.path().join(parent)).unwrap();
        symlink(&outside, project.path().join(parent).join("skills")).unwrap();
    }

    let status = run_status(project.path(), &["ai", "add"]);

    assert!(!status.success(), "with no folder to write, add must fail");
    assert_eq!(std::fs::read_dir(outside).unwrap().count(), 0);
    assert!(
        !project.path().join("AGENTS.md").exists(),
        "skills preflight must happen before AGENTS.md is written"
    );
}

#[cfg(unix)]
#[test]
fn hot_ai_update_never_writes_through_a_symlinked_skills_folder() {
    use std::os::unix::fs::symlink;

    let project = tempfile::tempdir().expect("project");
    run(project.path(), &["ai", "add"]);

    let real_skills = project.path().join("skills-real");
    std::fs::rename(project.path().join(".claude/skills"), &real_skills).unwrap();
    let manifest = real_skills.join("hot-language").join(MANIFEST_FILE);
    std::fs::write(&manifest, "{not valid json").unwrap();
    symlink(&real_skills, project.path().join(".claude/skills")).unwrap();

    run(project.path(), &["ai", "update"]);

    assert_eq!(
        std::fs::read_to_string(&manifest).unwrap(),
        "{not valid json",
        "update must not write through the symlinked folder"
    );
}
