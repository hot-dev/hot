//! An installed skill written by an older `hot` must be migrated, not skipped.
//!
//! The previous installer stamped its content hash as line 1, which invalidates
//! the YAML frontmatter beneath it — agent runtimes report "missing YAML
//! frontmatter delimited by ---" and skip the skill entirely. Because the skip
//! decision compared *hashes*, such a file matched its own stamp and was never
//! rewritten: every `hot ai update` left it broken.

use std::path::Path;
use std::process::Command;

fn hot_binary() -> &'static Path {
    Path::new(env!("CARGO_BIN_EXE_hot"))
}

fn run(project: &Path, args: &[&str]) {
    let status = Command::new(hot_binary())
        .args(args)
        .current_dir(project)
        .status()
        .unwrap_or_else(|e| panic!("run hot {args:?}: {e}"));
    assert!(status.success(), "hot {args:?} should succeed");
}

/// Rewrite a current install into the layout the previous version produced:
/// the very same content and hash, with the marker hoisted to line 1.
///
/// Fabricating a stale body instead would not reproduce the bug — the old code
/// rewrote whenever the hash differed. Only a file whose stamp still *matches*
/// its content exercises the early return that stranded broken installs.
fn regress_to_legacy_layout(skill_md: &Path) {
    let current = std::fs::read_to_string(skill_md).expect("read installed skill");
    let marker_start = current
        .find("<!-- hot-skill-hash:")
        .expect("installed skill carries a provenance marker");
    let marker_end = current[marker_start..]
        .find('\n')
        .map(|i| marker_start + i + 1)
        .expect("marker line ends");
    let marker = &current[marker_start..marker_end];
    let without = format!("{}{}", &current[..marker_start], &current[marker_end..]);
    std::fs::write(skill_md, format!("{marker}{without}")).expect("write legacy layout");
}

#[test]
fn hot_ai_repairs_a_legacy_stamped_skill() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path();

    run(project, &["ai", "add"]);
    let skill_md = project
        .join(".skills")
        .join("hot-language")
        .join("SKILL.md");
    assert!(skill_md.is_file(), "hot ai add installs the skill");
    assert!(
        project
            .join(".skills")
            .join("hot-ai-agents")
            .join("SKILL.md")
            .is_file(),
        "hot ai add installs every bundled skill"
    );

    regress_to_legacy_layout(&skill_md);
    let legacy = std::fs::read_to_string(&skill_md).unwrap();
    assert!(
        legacy.starts_with("<!-- hot-skill-hash:"),
        "precondition: the install is in the broken legacy layout"
    );

    run(project, &["ai", "update"]);

    let repaired = std::fs::read_to_string(&skill_md).expect("read repaired skill");
    assert!(
        repaired.starts_with("---\n"),
        "frontmatter must be restored to line 1, got:\n{}",
        repaired.lines().take(3).collect::<Vec<_>>().join("\n")
    );
    let close = repaired.find("\n---\n").expect("closing delimiter");
    let marker = repaired
        .find("<!-- hot-skill-hash:")
        .expect("provenance marker is still stamped");
    assert!(
        marker > close,
        "marker must sit below the frontmatter block"
    );

    // The migration has to converge, or every update rewrites every skill.
    run(project, &["ai", "update"]);
    assert_eq!(
        std::fs::read_to_string(&skill_md).unwrap(),
        repaired,
        "a second update must not rewrite an already-migrated skill"
    );
}

/// `hot ai update` must not clobber a user's local edits.
///
/// The stamp covers the *shipped source*, not the installed body, so an
/// unchanged skill is left alone whatever the user did to it. A byte-for-byte
/// comparison would repair layout correctly and silently destroy
/// customizations — this pins the distinction.
#[test]
fn hot_ai_update_preserves_local_edits_to_an_unchanged_skill() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path();

    run(project, &["ai", "add"]);
    let skill_md = project
        .join(".skills")
        .join("hot-language")
        .join("SKILL.md");

    let customized = format!(
        "{}\n<!-- MY LOCAL CUSTOMIZATION -->\n",
        std::fs::read_to_string(&skill_md).unwrap().trim_end()
    );
    std::fs::write(&skill_md, &customized).unwrap();

    run(project, &["ai", "update"]);

    let after = std::fs::read_to_string(&skill_md).unwrap();
    assert!(
        after.contains("MY LOCAL CUSTOMIZATION"),
        "a local edit must survive update while the shipped skill is unchanged"
    );
    assert_eq!(
        after, customized,
        "an unchanged skill must not be rewritten"
    );
}

#[test]
fn hot_ai_update_preserves_content_prepended_before_a_valid_marker() {
    let temp = tempfile::tempdir().expect("tempdir");
    let project = temp.path();

    run(project, &["ai", "add"]);
    let reference = project
        .join(".skills")
        .join("hot-language")
        .join("references")
        .join("flows.md");
    let customized = format!(
        "<!-- MY PREPENDED CUSTOMIZATION -->\n{}",
        std::fs::read_to_string(&reference).expect("read installed reference")
    );
    std::fs::write(&reference, &customized).expect("customize reference");

    run(project, &["ai", "update"]);

    assert_eq!(
        std::fs::read_to_string(&reference).expect("read updated reference"),
        customized,
        "moving a valid marker below prepended local content must not trigger a rewrite"
    );
}
