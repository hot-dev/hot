//! `hot ai` — install / list / update AGENTS.md and bundled Hot skills.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use serde::{Deserialize, Serialize};
use tracing::info;

use crate::cli::AiAction;

const SKILL_MANIFEST_FILE: &str = ".hot-skill-manifest.json";
const SKILL_OWNER_FILE: &str = ".hot-skill-owner";
const SKILL_OWNER_CONTENT: &[u8] = b"managed-by=hot-cli\n";

fn ai_home_dir() -> Option<std::path::PathBuf> {
    // Honor shell-provided home overrides before platform APIs. Besides making
    // CLI behavior predictable in Git Bash and similar environments, this is
    // what keeps global-install integration tests isolated on Windows, where
    // dirs::home_dir() otherwise ignores HOME/USERPROFILE and uses the real
    // profile known folder.
    ["HOME", "USERPROFILE"]
        .into_iter()
        .filter_map(std::env::var_os)
        .find(|value| !value.is_empty())
        .map(std::path::PathBuf::from)
        .or_else(dirs::home_dir)
}

#[derive(Debug, Default, Deserialize, Serialize)]
struct InstalledSkillManifest {
    version: u8,
    files: BTreeMap<String, String>,
}

#[derive(Debug)]
enum InstalledSkillManifestState {
    Missing,
    Valid(InstalledSkillManifest),
    Corrupt {
        path: std::path::PathBuf,
        error: String,
    },
}

pub(crate) fn run_ai(action: &AiAction) -> Result<(), String> {
    match action {
        AiAction::Add { global } => {
            info!("Adding AI coding support using AGENTS.md + SKILL.md standards...");

            // Validate skill ownership and the destination root before writing
            // AGENTS.md so a rejected skill install cannot leave a partial add.
            preflight_agent_skills(*global)?;
            setup_agents_md()?;
            setup_agent_skills(*global)?;

            Ok(())
        }
        AiAction::List => {
            println!("AI Coding Support Status:\n");

            let agents_exists = std::path::Path::new("AGENTS.md").exists();
            let agents_status = if agents_exists { "(installed)" } else { "" };
            println!("  AGENTS.md     - AI agent instructions {}", agents_status);

            let home = ai_home_dir().unwrap_or_default();
            let project_skills: BTreeSet<String> =
                installed_hot_skill_names(std::path::Path::new(".skills"))
                    .into_iter()
                    .collect();
            let global_skills: BTreeSet<String> = installed_hot_skill_names(&home.join(".skills"))
                .into_iter()
                .collect();
            let mut skill_names = BTreeSet::new();
            if let Ok(bundled) = bundled_skill_names() {
                skill_names.extend(bundled);
            }
            skill_names.extend(project_skills.iter().cloned());
            skill_names.extend(global_skills.iter().cloned());

            for skill_name in skill_names {
                let project_skill = std::path::Path::new(".skills").join(&skill_name);
                let global_skill = home.join(".skills").join(&skill_name);
                let status = if project_skills.contains(&skill_name) {
                    "(installed - project)"
                } else if global_skills.contains(&skill_name) {
                    "(installed - global)"
                } else if project_skill.exists() {
                    "(present - project, externally managed)"
                } else if global_skill.exists() {
                    "(present - global, externally managed)"
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

            let home = ai_home_dir().unwrap_or_default();
            let project_skills = installed_hot_skill_names(std::path::Path::new(".skills"));
            let global_skills = installed_hot_skill_names(&home.join(".skills"));

            if !project_skills.is_empty() || !global_skills.is_empty() {
                let bundled: BTreeSet<String> = bundled_skill_names()?.into_iter().collect();
                let project_targets: Vec<String> = project_skills
                    .into_iter()
                    .filter(|name| bundled.contains(name))
                    .collect();
                let global_targets: Vec<String> = global_skills
                    .into_iter()
                    .filter(|name| bundled.contains(name))
                    .collect();

                if !project_targets.is_empty() {
                    setup_selected_agent_skills(false, &project_targets)?;
                    updated_count += 1;
                }
                if !global_targets.is_empty() {
                    setup_selected_agent_skills(true, &global_targets)?;
                    updated_count += 1;
                }
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

fn installed_hot_skill_names(skills_dir: &std::path::Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(skills_dir) else {
        return Vec::new();
    };
    let mut names = Vec::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let has_manifest = path.join(SKILL_MANIFEST_FILE).is_file();
        let has_owner_marker = path.join(SKILL_OWNER_FILE).is_file();
        let has_legacy_stamp = fs::read_to_string(path.join("SKILL.md"))
            .ok()
            .and_then(|content| extract_skill_hash(&content))
            .is_some();
        if (has_manifest || has_owner_marker || has_legacy_stamp)
            && let Ok(name) = entry.file_name().into_string()
        {
            names.push(name);
        }
    }

    names.sort();
    names
}

fn setup_selected_agent_skills(global: bool, skill_names: &[String]) -> Result<(), String> {
    for skill_name in skill_names {
        setup_agent_skill(global, skill_name)?;
    }
    Ok(())
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

fn parse_legacy_skill_hash_line(line: &str) -> Option<u64> {
    let line = line.trim();
    line.strip_prefix("<!-- hot-skill-hash:")
        .and_then(|rest| rest.strip_suffix("-->"))
        .or_else(|| line.strip_prefix("// hot-skill-hash:"))
        .and_then(|hash_str| hash_str.trim().parse::<u64>().ok())
}

/// The hash stamped into a file by Hot versions before sidecar manifests.
fn extract_skill_hash(content: &str) -> Option<u64> {
    content
        .lines()
        .take(64)
        .find_map(parse_legacy_skill_hash_line)
}

/// Remove one legacy hash marker without otherwise normalizing the file.
fn strip_legacy_skill_hash(content: &str) -> String {
    let mut stripped = String::with_capacity(content.len());
    let mut removed = false;

    for (index, line) in content.split_inclusive('\n').enumerate() {
        let marker_candidate = line.trim_end_matches(['\n', '\r']);
        if !removed && index < 64 && parse_legacy_skill_hash_line(marker_candidate).is_some() {
            removed = true;
            continue;
        }
        stripped.push_str(line);
    }

    stripped
}

fn skill_content_hash(content: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    format!("{:x}", Sha256::digest(content))
}

fn legacy_skill_content_hash(content: &[u8]) -> Option<u64> {
    use siphasher::sip::SipHasher13;
    use std::hash::{Hash, Hasher};

    let content = std::str::from_utf8(content).ok()?;
    // Legacy Hot releases stamped `DefaultHasher::new()`, whose implementation
    // was SipHash 1-3 with fixed keys. Pin that algorithm so migrations remain
    // stable even if Rust changes DefaultHasher in a future toolchain.
    let mut hasher = SipHasher13::new();
    content.hash(&mut hasher);
    Some(hasher.finish())
}

fn skill_manifest_key(path: &std::path::Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

fn ensure_managed_path_is_not_symlinked(
    skills_base: &std::path::Path,
    path: &std::path::Path,
) -> Result<(), String> {
    let relative = path.strip_prefix(skills_base).map_err(|_| {
        format!(
            "Managed skill path {} is outside {}",
            path.display(),
            skills_base.display()
        )
    })?;
    let mut current = skills_base.to_path_buf();

    for component in std::iter::once(None).chain(relative.components().map(Some)) {
        if let Some(component) = component {
            let std::path::Component::Normal(component) = component else {
                return Err(format!(
                    "Managed skill path {} contains an unsupported component",
                    path.display()
                ));
            };
            current.push(component);
        }

        match fs::symlink_metadata(&current) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(format!(
                    "Refusing to manage skill path {} because {} is a symlink",
                    path.display(),
                    current.display()
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(format!(
                    "Failed to inspect managed skill path {}: {}",
                    current.display(),
                    error
                ));
            }
        }
    }

    Ok(())
}

fn read_installed_skill_manifest(
    skill_dir: &std::path::Path,
) -> Result<InstalledSkillManifestState, String> {
    let path = skill_dir.join(SKILL_MANIFEST_FILE);
    if !path.is_file() {
        return Ok(InstalledSkillManifestState::Missing);
    }

    let content = fs::read_to_string(&path)
        .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
    let manifest: InstalledSkillManifest = match serde_json::from_str(&content) {
        Ok(manifest) => manifest,
        Err(error) => {
            return Ok(InstalledSkillManifestState::Corrupt {
                path,
                error: error.to_string(),
            });
        }
    };
    if manifest.version != 1 {
        return Err(format!(
            "Unsupported installed skill manifest version {} in {}",
            manifest.version,
            path.display()
        ));
    }
    Ok(InstalledSkillManifestState::Valid(manifest))
}

fn write_if_changed(path: &std::path::Path, content: &[u8]) -> Result<bool, String> {
    write_atomic_if_changed(path, content)
}

fn write_atomic_if_changed(path: &std::path::Path, content: &[u8]) -> Result<bool, String> {
    use std::io::Write;
    use std::sync::atomic::{AtomicU64, Ordering};

    if path.is_file()
        && let Ok(existing) = fs::read(path)
        && existing == content
    {
        return Ok(false);
    }
    let parent = path
        .parent()
        .ok_or_else(|| format!("Path has no parent: {}", path.display()))?;
    fs::create_dir_all(parent)
        .map_err(|e| format!("Failed to create directory {}: {}", parent.display(), e))?;

    static TEMP_COUNTER: AtomicU64 = AtomicU64::new(0);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("hot-skill-manifest");
    let sequence = TEMP_COUNTER.fetch_add(1, Ordering::Relaxed);
    let temp_path = parent.join(format!(
        ".{file_name}.tmp-{}-{}",
        std::process::id(),
        sequence
    ));

    let result = (|| {
        let mut temp = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .map_err(|e| format!("Failed to create {}: {}", temp_path.display(), e))?;
        temp.write_all(content)
            .map_err(|e| format!("Failed to write {}: {}", temp_path.display(), e))?;
        temp.sync_all()
            .map_err(|e| format!("Failed to sync {}: {}", temp_path.display(), e))?;
        #[cfg(not(windows))]
        {
            fs::rename(&temp_path, path).map_err(|e| {
                format!(
                    "Failed to rename {} to {}: {}",
                    temp_path.display(),
                    path.display(),
                    e
                )
            })
        }

        // std::fs::rename does not replace an existing destination on Windows.
        // Keep the old file recoverable while installing the fully-synced temp
        // file, and restore it if the second rename fails.
        #[cfg(windows)]
        {
            if !path.exists() {
                return fs::rename(&temp_path, path).map_err(|e| {
                    format!(
                        "Failed to rename {} to {}: {}",
                        temp_path.display(),
                        path.display(),
                        e
                    )
                });
            }

            let backup_path = parent.join(format!(
                ".{file_name}.backup-{}-{}",
                std::process::id(),
                sequence
            ));
            fs::rename(path, &backup_path).map_err(|e| {
                format!(
                    "Failed to stage existing {} as {}: {}",
                    path.display(),
                    backup_path.display(),
                    e
                )
            })?;
            match fs::rename(&temp_path, path) {
                Ok(()) => {
                    fs::remove_file(&backup_path).map_err(|e| {
                        format!(
                            "Updated {} but failed to remove backup {}: {}",
                            path.display(),
                            backup_path.display(),
                            e
                        )
                    })?;
                    Ok(())
                }
                Err(error) => {
                    let restore_error = fs::rename(&backup_path, path).err();
                    Err(format!(
                        "Failed to install {} as {}: {}{}",
                        temp_path.display(),
                        path.display(),
                        error,
                        restore_error
                            .map(|restore| format!(
                                "; restoring the original also failed: {restore}"
                            ))
                            .unwrap_or_default()
                    ))
                }
            }
        }
    })();

    if result.is_err() {
        let _ = fs::remove_file(&temp_path);
    }
    result.map(|_| true)
}

fn setup_agent_skills(global: bool) -> Result<(), String> {
    for skill_name in bundled_skill_names()? {
        setup_agent_skill(global, &skill_name)?;
    }
    Ok(())
}

fn agent_skills_base(global: bool) -> Result<(std::path::PathBuf, &'static str), String> {
    if global {
        let home = ai_home_dir().ok_or("Could not determine home directory")?;
        Ok((home.join(".skills"), "global"))
    } else {
        Ok((std::path::PathBuf::from(".skills"), "project"))
    }
}

fn has_legacy_skill_stamp(skill_dir: &std::path::Path) -> bool {
    fs::read_to_string(skill_dir.join("SKILL.md"))
        .ok()
        .and_then(|content| extract_skill_hash(&content))
        .is_some()
}

fn validate_skill_ownership(skill_dir: &std::path::Path) -> Result<(), String> {
    if skill_dir.exists()
        && !skill_dir.join(SKILL_MANIFEST_FILE).is_file()
        && !skill_dir.join(SKILL_OWNER_FILE).is_file()
        && !has_legacy_skill_stamp(skill_dir)
    {
        if skill_dir.is_dir()
            && fs::read_dir(skill_dir)
                .map(|mut entries| entries.next().is_none())
                .unwrap_or(false)
        {
            return Ok(());
        }
        return Err(format!(
            "Refusing to overwrite externally managed skill {}. Remove it or choose a different installation scope before running hot ai add.",
            skill_dir.display()
        ));
    }
    Ok(())
}

fn preflight_agent_skills(global: bool) -> Result<(), String> {
    let (skills_base, _) = agent_skills_base(global)?;
    ensure_managed_path_is_not_symlinked(&skills_base, &skills_base)?;
    for skill_name in bundled_skill_names()? {
        let skill_dir = skills_base.join(skill_name);
        ensure_managed_path_is_not_symlinked(&skills_base, &skill_dir)?;
        validate_skill_ownership(&skill_dir)?;
    }
    Ok(())
}

/// Install/refresh one bundled skill under `.skills/` (project) or
/// `~/.skills/` (global).
fn setup_agent_skill(global: bool, skill_name: &str) -> Result<(), String> {
    use ahash::AHashSet;

    let source_skill_dir = hot::resources::get_skill_path(skill_name)?;

    let (skills_base, location_desc) = agent_skills_base(global)?;

    fn collect_source_files(
        dir: &std::path::Path,
        base: &std::path::Path,
        files: &mut Vec<(std::path::PathBuf, Vec<u8>)>,
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
                let content = fs::read(&path)
                    .map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
                files.push((rel_path.to_path_buf(), content));
            }
        }
        Ok(())
    }

    let mut skill_files: Vec<(std::path::PathBuf, Vec<u8>)> = Vec::new();
    collect_source_files(&source_skill_dir, &source_skill_dir, &mut skill_files)?;

    let skill_dir = skills_base.join(skill_name);
    ensure_managed_path_is_not_symlinked(&skills_base, &skill_dir)?;
    validate_skill_ownership(&skill_dir)?;
    ensure_managed_path_is_not_symlinked(&skills_base, &skill_dir.join(SKILL_MANIFEST_FILE))?;
    ensure_managed_path_is_not_symlinked(&skills_base, &skill_dir.join(SKILL_OWNER_FILE))?;
    for (rel_path, _) in &skill_files {
        ensure_managed_path_is_not_symlinked(&skills_base, &skill_dir.join(rel_path))?;
    }
    let installed_manifest_state = read_installed_skill_manifest(&skill_dir)?;
    let preserve_untracked_files = matches!(
        &installed_manifest_state,
        InstalledSkillManifestState::Corrupt { .. }
    ) || matches!(
        &installed_manifest_state,
        InstalledSkillManifestState::Missing
    ) && skill_dir.join(SKILL_OWNER_FILE).is_file();
    if let InstalledSkillManifestState::Corrupt { path, error } = &installed_manifest_state {
        println!(
            "  Warning: rebuilding corrupt skill manifest {} ({}) without overwriting existing files",
            path.display(),
            error
        );
    }
    let installed_manifest = match &installed_manifest_state {
        InstalledSkillManifestState::Valid(manifest) => Some(manifest),
        InstalledSkillManifestState::Missing | InstalledSkillManifestState::Corrupt { .. } => None,
    };
    let mut next_manifest = InstalledSkillManifest {
        version: 1,
        files: BTreeMap::new(),
    };
    let mut expected_files: AHashSet<String> = skill_files
        .iter()
        .map(|(path, _)| skill_manifest_key(path))
        .collect();
    expected_files.insert(SKILL_MANIFEST_FILE.to_string());
    expected_files.insert(SKILL_OWNER_FILE.to_string());
    next_manifest.files.insert(
        SKILL_OWNER_FILE.to_string(),
        skill_content_hash(SKILL_OWNER_CONTENT),
    );

    fn write_skill_file(
        path: &std::path::Path,
        content: &[u8],
        manifest_key: &str,
        installed_manifest: Option<&InstalledSkillManifest>,
        preserve_untracked_files: bool,
    ) -> Result<bool, String> {
        let hash = skill_content_hash(content);

        if path.exists() {
            let existing =
                fs::read(path).map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;

            if preserve_untracked_files {
                return Ok(false);
            }

            if installed_manifest.and_then(|manifest| manifest.files.get(manifest_key))
                == Some(&hash)
            {
                // The shipped source is unchanged. Preserve local edits while
                // opportunistically cleaning a legacy marker from a manifest
                // install that was only partially migrated.
                if let Ok(existing_text) = std::str::from_utf8(&existing)
                    && extract_skill_hash(existing_text).is_some()
                {
                    return write_if_changed(
                        path,
                        strip_legacy_skill_hash(existing_text).as_bytes(),
                    );
                }
                return Ok(false);
            }

            // Migrate marker-based installs. The old marker records the hash
            // of the shipped source, so a match means the source is unchanged
            // and any other differences are user customizations to preserve.
            if installed_manifest.is_none()
                && let Some(legacy_hash) = legacy_skill_content_hash(content)
                && let Ok(existing_text) = std::str::from_utf8(&existing)
                && extract_skill_hash(existing_text) == Some(legacy_hash)
            {
                return write_if_changed(path, strip_legacy_skill_hash(existing_text).as_bytes());
            }
        }

        write_if_changed(path, content)
    }

    fn collect_files(
        dir: &std::path::Path,
        base: &std::path::Path,
        files: &mut Vec<std::path::PathBuf>,
    ) {
        if let Ok(entries) = fs::read_dir(dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if file_type.is_dir() {
                    collect_files(&path, base, files);
                } else if (file_type.is_file() || file_type.is_symlink())
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

    let mut any_updated = false;
    let mut any_removed = false;

    for (rel_path, content) in &skill_files {
        let full_path = skill_dir.join(rel_path);
        // Repeat the component check immediately before the atomic write. The
        // rename replaces a leaf symlink rather than following it, while this
        // recheck protects parent components changed since preflight.
        ensure_managed_path_is_not_symlinked(&skills_base, &full_path)?;
        let manifest_key = skill_manifest_key(rel_path);
        next_manifest
            .files
            .insert(manifest_key.clone(), skill_content_hash(content));
        if write_skill_file(
            &full_path,
            content,
            &manifest_key,
            installed_manifest,
            preserve_untracked_files,
        )? {
            any_updated = true;
        }
    }

    if skill_dir.exists()
        && !preserve_untracked_files
        && let Some(previous_manifest) = installed_manifest
    {
        let mut existing_files = Vec::new();
        collect_files(&skill_dir, &skill_dir, &mut existing_files);

        for rel_path in existing_files {
            let rel_str = skill_manifest_key(&rel_path);
            if expected_files.contains(&rel_str) {
                continue;
            }
            let Some(previous_hash) = previous_manifest.files.get(&rel_str) else {
                continue;
            };

            let full_path = skill_dir.join(&rel_path);
            ensure_managed_path_is_not_symlinked(&skills_base, &full_path)?;
            let metadata = fs::symlink_metadata(&full_path)
                .map_err(|e| format!("Failed to inspect {}: {}", full_path.display(), e))?;
            if metadata.file_type().is_symlink() {
                println!(
                    "  Warning: preserving retired managed path {} because it is now a symlink",
                    full_path.display()
                );
                continue;
            }
            let existing = fs::read(&full_path)
                .map_err(|e| format!("Failed to read {}: {}", full_path.display(), e))?;
            if skill_content_hash(&existing) != *previous_hash {
                println!(
                    "  Warning: preserving locally modified retired skill file {}",
                    full_path.display()
                );
                continue;
            }

            fs::remove_file(&full_path)
                .map_err(|e| format!("Failed to remove {}: {}", full_path.display(), e))?;
            any_removed = true;
            cleanup_empty_dirs(&full_path, &skill_dir);
        }
    }

    let mut manifest_content = serde_json::to_vec_pretty(&next_manifest)
        .map_err(|e| format!("Failed to serialize installed skill manifest: {}", e))?;
    manifest_content.push(b'\n');
    let owner_path = skill_dir.join(SKILL_OWNER_FILE);
    ensure_managed_path_is_not_symlinked(&skills_base, &owner_path)?;
    if write_atomic_if_changed(&owner_path, SKILL_OWNER_CONTENT)? {
        any_updated = true;
    }
    let manifest_path = skill_dir.join(SKILL_MANIFEST_FILE);
    ensure_managed_path_is_not_symlinked(&skills_base, &manifest_path)?;
    if write_atomic_if_changed(&manifest_path, &manifest_content)? {
        any_updated = true;
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
mod skill_manifest_tests {
    use super::{extract_skill_hash, strip_legacy_skill_hash};

    const FRONTMATTER_SKILL: &str =
        "---\nname: hot-language\ndescription: >\n  Write Hot.\n---\n\n# Body\n";

    #[test]
    fn strips_marker_hiding_frontmatter() {
        let legacy = format!("<!-- hot-skill-hash:42 -->\n{FRONTMATTER_SKILL}");
        assert_eq!(strip_legacy_skill_hash(&legacy), FRONTMATTER_SKILL);
        assert_eq!(extract_skill_hash(&legacy), Some(42));
    }

    #[test]
    fn strips_markers_without_changing_other_content() {
        let markdown = "<!-- LOCAL EDIT -->\n<!-- hot-skill-hash:7 -->\n# Reference\n";
        assert_eq!(
            strip_legacy_skill_hash(markdown),
            "<!-- LOCAL EDIT -->\n# Reference\n"
        );

        let hot = "// hot-skill-hash:9\n::demo ns\n";
        assert_eq!(strip_legacy_skill_hash(hot), "::demo ns\n");
    }
}
