//! `hot ai` — install / list / update AGENTS.md and bundled Hot skills.

use std::fs;
use tracing::info;

use crate::cli::AiAction;

pub(crate) fn run_ai(action: &AiAction) -> Result<(), String> {
    match action {
        AiAction::Add { global } => {
            info!("Adding AI coding support using AGENTS.md + SKILL.md standards...");

            setup_agents_md()?;
            setup_agent_skills(*global)?;

            Ok(())
        }
        AiAction::List => {
            println!("AI Coding Support Status:\n");

            let agents_exists = std::path::Path::new("AGENTS.md").exists();
            let agents_status = if agents_exists { "(installed)" } else { "" };
            println!("  AGENTS.md     - AI agent instructions {}", agents_status);

            let home = dirs::home_dir().unwrap_or_default();
            for skill_name in bundled_skill_names()? {
                let project_skill = std::path::Path::new(".skills").join(&skill_name);
                let global_skill = home.join(".skills").join(&skill_name);
                let status = if project_skill.exists() {
                    "(installed - project)"
                } else if global_skill.exists() {
                    "(installed - global)"
                } else {
                    ""
                };
                println!("  .skills/{skill_name}/  {status}");
            }

            let legacy_files = [
                ("CLAUDE.md", "Old Claude Code file"),
                (".cursor/rules/hot.mdc", "Old Cursor rules"),
                (
                    ".github/copilot-instructions.md",
                    "Old Copilot instructions",
                ),
                (".windsurfrules", "Old Windsurf rules"),
                (".claude/skills/hot-language", "Old Claude skills location"),
                (".codex/skills/hot-language", "Old Codex skills location"),
            ];
            let mut has_legacy = false;
            for (path, _desc) in &legacy_files {
                if std::path::Path::new(path).exists() {
                    if !has_legacy {
                        println!("\n  Legacy files (can be removed):");
                        has_legacy = true;
                    }
                    println!("    {}", path);
                }
            }

            println!("\nUse 'hot ai add' to add AI support to this project.");
            println!("Use 'hot ai add --global' to install skills to ~/.skills/");
            println!("Use 'npx skills add hot-dev/hot-skills' for the public skills.sh source.");
            Ok(())
        }
        AiAction::Update => {
            info!("Updating AI support files...");
            let mut updated_count = 0;

            if std::path::Path::new("AGENTS.md").exists() {
                setup_agents_md()?;
                updated_count += 1;
            }

            let home = dirs::home_dir().unwrap_or_default();
            let skill_names = bundled_skill_names()?;
            let project_has_skills = skill_names
                .iter()
                .any(|name| std::path::Path::new(".skills").join(name).exists());
            let global_has_skills = skill_names
                .iter()
                .any(|name| home.join(".skills").join(name).exists());
            if project_has_skills {
                setup_agent_skills(false)?;
                updated_count += 1;
            } else if global_has_skills {
                setup_agent_skills(true)?;
                updated_count += 1;
            }

            if updated_count == 0 {
                println!("No AI support files found to update.");
                println!("Use 'hot ai add' to add AI support to this project.");
            } else {
                println!("\nUpdated {} location(s).", updated_count);
            }
            Ok(())
        }
    }
}

/// Discover every skill bundled under `resources/ai/skills/`.
///
/// A directory is installable when it contains `SKILL.md`. Sorting keeps
/// command output and installation behavior deterministic across filesystems.
fn bundled_skill_names() -> Result<Vec<String>, String> {
    let skills_dir = hot::resources::get_ai_path()?.join("skills");
    let entries = fs::read_dir(&skills_dir).map_err(|e| {
        format!(
            "Failed to read bundled skills {}: {}",
            skills_dir.display(),
            e
        )
    })?;
    let mut names = Vec::new();

    for entry in entries {
        let entry = entry.map_err(|e| format!("Failed to read bundled skill entry: {}", e))?;
        let path = entry.path();
        if path.is_dir() && path.join("SKILL.md").is_file() {
            let name = entry
                .file_name()
                .into_string()
                .map_err(|name| format!("Bundled skill name is not valid UTF-8: {name:?}"))?;
            names.push(name);
        }
    }

    names.sort();
    if names.is_empty() {
        return Err(format!(
            "No bundled skills found in {}",
            skills_dir.display()
        ));
    }
    Ok(names)
}

/// Setup AGENTS.md with the canonical Hot section from resources/ai/AGENTS.md.
fn setup_agents_md() -> Result<(), String> {
    use sha2::{Digest, Sha256};

    const HOT_SECTION_START: &str = "<!-- HOT_LANGUAGE_SECTION_START -->";
    const HOT_SECTION_END: &str = "<!-- HOT_LANGUAGE_SECTION_END -->";

    fn content_hash(content: &str) -> String {
        format!("{:x}", Sha256::digest(content.as_bytes()))[..12].to_string()
    }

    fn extract_section_hash(content: &str, section_start: &str) -> Option<String> {
        content.find(section_start).and_then(|start| {
            let after_marker = &content[start + section_start.len()..];
            after_marker
                .lines()
                .next()
                .and_then(|line| line.strip_prefix(" hash:"))
                .map(|hash_str| hash_str.trim().to_string())
        })
    }

    fn create_section(template: &str, section_start: &str, section_end: &str) -> String {
        let hash = content_hash(template);
        format!(
            "{} hash:{}\n{}\n{}",
            section_start, hash, template, section_end
        )
    }

    fn update_shared_file(
        path: &std::path::Path,
        template_content: &str,
        file_desc: &str,
        section_start: &str,
        section_end: &str,
    ) -> Result<bool, String> {
        let new_hash = content_hash(template_content);
        let new_section = create_section(template_content, section_start, section_end);

        if path.exists() {
            let existing = fs::read_to_string(path)
                .map_err(|e| format!("Failed to read {}: {}", file_desc, e))?;

            if let Some(start_idx) = existing.find(section_start) {
                if let Some(existing_hash) = extract_section_hash(&existing, section_start)
                    && existing_hash == new_hash
                {
                    return Ok(false);
                }

                if let Some(end_idx) = existing.find(section_end) {
                    let before = &existing[..start_idx];
                    let after = &existing[end_idx + section_end.len()..];
                    let separator = if before.is_empty() { "" } else { "\n\n" };
                    let updated =
                        format!("{}{}{}{}", before.trim_end(), separator, new_section, after);
                    fs::write(path, updated)
                        .map_err(|e| format!("Failed to update {}: {}", file_desc, e))?;
                    return Ok(true);
                }
            }

            let separator = if existing.ends_with('\n') {
                "\n"
            } else {
                "\n\n"
            };
            let updated = format!("{}{}{}\n", existing, separator, new_section);
            fs::write(path, updated)
                .map_err(|e| format!("Failed to append to {}: {}", file_desc, e))?;
            Ok(true)
        } else {
            fs::write(path, format!("{}\n", new_section))
                .map_err(|e| format!("Failed to create {}: {}", file_desc, e))?;
            Ok(true)
        }
    }

    let agents_template = hot::resources::read_agents_md()?;

    let agents_md_path = std::path::Path::new("AGENTS.md");
    let agents_existed = agents_md_path.exists();
    match update_shared_file(
        agents_md_path,
        &agents_template,
        "AGENTS.md",
        HOT_SECTION_START,
        HOT_SECTION_END,
    )? {
        true if agents_existed => println!("  Updated AGENTS.md"),
        true => println!("  Added AGENTS.md"),
        false => println!("  AGENTS.md is up to date"),
    }

    Ok(())
}

/// The hash stamped into an installed file, wherever the marker sits.
fn extract_skill_hash(content: &str) -> Option<u64> {
    content.lines().take(64).find_map(|line| {
        let line = line.trim();
        line.strip_prefix("<!-- hot-skill-hash:")
            .and_then(|rest| rest.strip_suffix("-->"))
            .or_else(|| line.strip_prefix("// hot-skill-hash:"))
            .and_then(|hash_str| hash_str.trim().parse::<u64>().ok())
    })
}

/// Whether an old Markdown stamp is hiding YAML frontmatter on line 2.
///
/// A leading marker is valid for Markdown without frontmatter and for
/// non-Markdown files. Only this exact legacy shape is broken; treating every
/// moved marker as stale would overwrite local content prepended by a user.
fn has_broken_legacy_frontmatter_layout(content: &str, is_markdown: bool) -> bool {
    if !is_markdown {
        return false;
    }
    let Some((marker, rest)) = content.split_once('\n') else {
        return false;
    };
    let marker = marker.trim_end_matches('\r').trim();
    let is_hash_marker = marker
        .strip_prefix("<!-- hot-skill-hash:")
        .and_then(|rest| rest.strip_suffix("-->"))
        .is_some();
    is_hash_marker && (rest.starts_with("---\n") || rest.starts_with("---\r\n"))
}

/// Stamp the content hash into a file without breaking its format.
///
/// A YAML frontmatter block must begin on line 1. Prepending the marker
/// invalidated every skill that has frontmatter — Codex reports
/// "missing YAML frontmatter delimited by ---" and skips the skill — so
/// for those files the marker goes immediately after the closing `---`.
fn stamp_skill_hash(content: &str, hash: u64, is_markdown: bool) -> String {
    if !is_markdown {
        return format!("// hot-skill-hash:{}\n{}", hash, content);
    }

    let marker = format!("<!-- hot-skill-hash:{} -->", hash);
    if let Some(rest) = content.strip_prefix("---\n")
        && let Some(close) = rest.find("\n---")
    {
        // `close` indexes the newline before the closing delimiter; step
        // past that delimiter line to land just after the block.
        let after_delim = close + 1;
        let tail = &rest[after_delim..];
        return match tail.find('\n') {
            Some(offset) => {
                let split = after_delim + offset + 1;
                format!("---\n{}{}\n{}", &rest[..split], marker, &rest[split..])
            }
            // Frontmatter closes the file with no trailing newline.
            None => format!("---\n{}\n{}\n", rest, marker),
        };
    }

    format!("{}\n{}", marker, content)
}

fn setup_agent_skills(global: bool) -> Result<(), String> {
    for skill_name in bundled_skill_names()? {
        setup_agent_skill(global, &skill_name)?;
    }
    Ok(())
}

/// Install/refresh one bundled skill under `.skills/` (project) or
/// `~/.skills/` (global).
fn setup_agent_skill(global: bool, skill_name: &str) -> Result<(), String> {
    use ahash::AHashSet;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn content_hash(content: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        hasher.finish()
    }

    let source_skill_dir = hot::resources::get_skill_path(skill_name)?;

    let (skills_base, location_desc) = if global {
        let home = dirs::home_dir().ok_or("Could not determine home directory")?;
        (home.join(".skills"), "global")
    } else {
        (std::path::PathBuf::from(".skills"), "project")
    };

    fn collect_source_files(
        dir: &std::path::Path,
        base: &std::path::Path,
        files: &mut Vec<(std::path::PathBuf, String)>,
    ) -> Result<(), String> {
        let entries = fs::read_dir(dir)
            .map_err(|e| format!("Failed to read directory {}: {}", dir.display(), e))?;

        for entry in entries {
            let entry = entry.map_err(|e| format!("Failed to read directory entry: {}", e))?;
            let path = entry.path();

            if path.is_dir() {
                collect_source_files(&path, base, files)?;
            } else if path.is_file() {
                let rel_path = path
                    .strip_prefix(base)
                    .map_err(|e| format!("Failed to get relative path: {}", e))?;
                let content = fs::read_to_string(&path)
                    .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
                files.push((rel_path.to_path_buf(), content));
            }
        }
        Ok(())
    }

    let mut skill_files: Vec<(std::path::PathBuf, String)> = Vec::new();
    collect_source_files(&source_skill_dir, &source_skill_dir, &mut skill_files)?;

    let expected_files: AHashSet<String> = skill_files
        .iter()
        .map(|(path, _)| path.to_string_lossy().to_string())
        .collect();

    fn write_skill_file(
        path: &std::path::Path,
        content: &str,
        is_markdown: bool,
    ) -> Result<bool, String> {
        let hash = content_hash(content);
        let content_with_hash = stamp_skill_hash(content, hash, is_markdown);

        if path.exists() {
            let existing = fs::read_to_string(path)
                .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

            // Rewrite when the source content changed, or when the stamp sits
            // in the exact broken layout an older installer produced: a
            // Markdown marker on line 1 hiding frontmatter on line 2. Those
            // installs matched their own hash and so were never repaired.
            //
            // Deliberately *not* a byte comparison of the whole file: the
            // hash covers the source, not the installed body, so a user's
            // local edits to a skill survive `hot ai update` as long as the
            // shipped content is unchanged. Comparing bytes would silently
            // start clobbering those edits.
            if extract_skill_hash(&existing) == Some(hash)
                && !has_broken_legacy_frontmatter_layout(&existing, is_markdown)
            {
                return Ok(false);
            }
        }

        if let Some(parent) = path.parent()
            && !parent.exists()
        {
            fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directory {}: {}", parent.display(), e))?;
        }

        fs::write(path, &content_with_hash)
            .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;
        Ok(true)
    }

    fn collect_files(
        dir: &std::path::Path,
        base: &std::path::Path,
        files: &mut Vec<std::path::PathBuf>,
    ) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    collect_files(&path, base, files);
                } else if path.is_file()
                    && let Ok(rel) = path.strip_prefix(base)
                {
                    files.push(rel.to_path_buf());
                }
            }
        }
    }

    fn cleanup_empty_dirs(path: &std::path::Path, skill_root: &std::path::Path) {
        let mut current = path.parent();
        while let Some(dir) = current {
            if dir == skill_root || dir.parent().is_none() {
                break;
            }
            if fs::remove_dir(dir).is_err() {
                break;
            }
            current = dir.parent();
        }
    }

    let skill_dir = skills_base.join(skill_name);

    let mut any_updated = false;
    let mut any_removed = false;

    for (rel_path, content) in &skill_files {
        let full_path = skill_dir.join(rel_path);
        let rel_path_str = rel_path.to_string_lossy();
        let is_markdown = rel_path_str.ends_with(".md");
        if write_skill_file(&full_path, content, is_markdown)? {
            any_updated = true;
        }
    }

    if skill_dir.exists() {
        let mut existing_files = Vec::new();
        collect_files(&skill_dir, &skill_dir, &mut existing_files);

        for rel_path in existing_files {
            let rel_str = rel_path.to_string_lossy().to_string();
            if !expected_files.contains(&rel_str) {
                let full_path = skill_dir.join(&rel_path);
                if fs::remove_file(&full_path).is_ok() {
                    any_removed = true;
                    cleanup_empty_dirs(&full_path, &skill_dir);
                }
            }
        }
    }

    if any_updated || any_removed {
        println!(
            "  Updated {} ({} directory)",
            skill_dir.display(),
            location_desc
        );
    } else {
        println!("  .skills/{skill_name}/ is up to date");
    }

    Ok(())
}

#[cfg(test)]
mod skill_stamp_tests {
    use super::{has_broken_legacy_frontmatter_layout, stamp_skill_hash};

    const FRONTMATTER_SKILL: &str =
        "---\nname: hot-language\ndescription: >\n  Write Hot.\n---\n\n# Body\n";

    /// YAML frontmatter is only recognized when `---` is the very first line.
    /// Stamping must never displace it — a skill that loses its frontmatter is
    /// silently skipped by the agent runtimes that load it.
    #[test]
    fn stamping_preserves_leading_frontmatter() {
        let stamped = stamp_skill_hash(FRONTMATTER_SKILL, 42, true);
        assert!(
            stamped.starts_with("---\n"),
            "frontmatter must remain at line 1:\n{stamped}"
        );
        let close = stamped.find("\n---\n").expect("closing delimiter");
        let marker = stamped
            .find("<!-- hot-skill-hash:42 -->")
            .expect("marker must be present");
        assert!(marker > close, "marker must follow the frontmatter block");
        assert!(stamped.contains("name: hot-language") && stamped.contains("# Body"));
    }

    #[test]
    fn stamping_markdown_without_frontmatter_prepends() {
        let stamped = stamp_skill_hash("# Reference\n\ntext\n", 7, true);
        assert!(stamped.starts_with("<!-- hot-skill-hash:7 -->\n"));
        assert!(stamped.ends_with("# Reference\n\ntext\n"));
    }

    #[test]
    fn stamping_non_markdown_prepends_line_comment() {
        assert_eq!(
            stamp_skill_hash("::demo ns\n", 9, false),
            "// hot-skill-hash:9\n::demo ns\n"
        );
    }

    /// The installer compares its output byte for byte against the file on
    /// disk, so an unstable stamp would rewrite every skill on every run.
    #[test]
    fn stamping_is_deterministic() {
        assert_eq!(
            stamp_skill_hash(FRONTMATTER_SKILL, 42, true),
            stamp_skill_hash(FRONTMATTER_SKILL, 42, true)
        );
    }

    /// The layout an older version produced must never be what we write now,
    /// or the migration in `write_skill_file` could not converge.
    #[test]
    fn legacy_layout_is_not_reproduced() {
        let current = stamp_skill_hash(FRONTMATTER_SKILL, 1234, true);
        assert_ne!(
            current,
            format!("<!-- hot-skill-hash:1234 -->\n{FRONTMATTER_SKILL}")
        );
        assert!(!current.starts_with("<!-- hot-skill-hash"));
    }

    #[test]
    fn only_a_leading_marker_hiding_frontmatter_is_legacy() {
        let legacy = format!("<!-- hot-skill-hash:1234 -->\n{FRONTMATTER_SKILL}");
        assert!(has_broken_legacy_frontmatter_layout(&legacy, true));

        let plain_markdown = "<!-- hot-skill-hash:1234 -->\n# Reference\n\nLocally edited body\n";
        assert!(!has_broken_legacy_frontmatter_layout(plain_markdown, true));

        let prepended_edit = "<!-- LOCAL EDIT -->\n<!-- hot-skill-hash:1234 -->\n# Reference\n";
        assert!(!has_broken_legacy_frontmatter_layout(prepended_edit, true));
        assert!(!has_broken_legacy_frontmatter_layout(&legacy, false));
    }
}
