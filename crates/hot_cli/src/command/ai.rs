//! `hot ai` — install / list / update AGENTS.md and the hot-language skill.

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
            let project_skills = std::path::Path::new(".skills/hot-language");
            let global_skills = home.join(".skills/hot-language");
            let skills_status = if project_skills.exists() {
                "(installed - project)"
            } else if global_skills.exists() {
                "(installed - global)"
            } else {
                ""
            };
            println!("  .skills/      - Hot language skill  {}", skills_status);

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
            let project_skills = std::path::Path::new(".skills/hot-language");
            let global_skills = home.join(".skills/hot-language");
            if project_skills.exists() {
                setup_agent_skills(false)?;
                updated_count += 1;
            } else if global_skills.exists() {
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

/// Setup AGENTS.md with the canonical Hot section from resources/ai/AGENTS.md.
fn setup_agents_md() -> Result<(), String> {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    const HOT_SECTION_START: &str = "<!-- HOT_LANGUAGE_SECTION_START -->";
    const HOT_SECTION_END: &str = "<!-- HOT_LANGUAGE_SECTION_END -->";

    fn content_hash(content: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        hasher.finish()
    }

    fn extract_section_hash(content: &str, section_start: &str) -> Option<u64> {
        content.find(section_start).and_then(|start| {
            let after_marker = &content[start + section_start.len()..];
            after_marker
                .lines()
                .next()
                .and_then(|line| line.strip_prefix(" hash:"))
                .and_then(|hash_str| hash_str.trim().parse::<u64>().ok())
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

/// Install/refresh the `hot-language` skill under `.skills/` (project) or
/// `~/.skills/` (global).
fn setup_agent_skills(global: bool) -> Result<(), String> {
    use ahash::AHashSet;
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    fn content_hash(content: &str) -> u64 {
        let mut hasher = DefaultHasher::new();
        content.hash(&mut hasher);
        hasher.finish()
    }

    let source_skill_dir = hot::resources::get_skill_path("hot-language")?;

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
        let content_with_hash = if is_markdown {
            format!("<!-- hot-skill-hash:{} -->\n{}", hash, content)
        } else {
            format!("// hot-skill-hash:{}\n{}", hash, content)
        };

        if path.exists() {
            let existing = fs::read_to_string(path)
                .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

            let existing_hash = existing.lines().next().and_then(|line| {
                line.strip_prefix("<!-- hot-skill-hash:")
                    .and_then(|rest| rest.strip_suffix(" -->"))
                    .or_else(|| line.strip_prefix("// hot-skill-hash:"))
                    .and_then(|hash_str| hash_str.trim().parse::<u64>().ok())
            });

            if existing_hash == Some(hash) {
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

    let skill_dir = skills_base.join("hot-language");

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
        println!("  .skills/hot-language/ is up to date");
    }

    Ok(())
}
