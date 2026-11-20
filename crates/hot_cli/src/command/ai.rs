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

/// Skill folders agents read, relative to a project root or the home folder.
/// `.agents/skills` is the shared location: Codex, Cursor, Gemini CLI, GitHub
/// Copilot and OpenCode read it. Claude Code reads only `.claude/skills`.
pub(crate) const SKILL_DIRS: &[&str] = &[".agents/skills", ".claude/skills"];

/// Where Hot 3.0.12 and earlier installed skills. No agent reads it, so
/// `hot ai add` and `hot ai update` move managed skills out of it.
const LEGACY_SKILL_DIR: &str = ".skills";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Scope {
    Project,
    Global,
}

impl Scope {
    fn label(self) -> &'static str {
        match self {
            Scope::Project => "project",
            Scope::Global => "global",
        }
    }

    /// Project paths stay relative, so messages name them as the user sees them.
    fn root(self) -> Result<std::path::PathBuf, String> {
        match self {
            Scope::Project => Ok(std::path::PathBuf::new()),
            Scope::Global => {
                ai_home_dir().ok_or_else(|| "Could not determine home directory".into())
            }
        }
    }

    fn skill_bases(self) -> Result<Vec<std::path::PathBuf>, String> {
        let root = self.root()?;
        Ok(SKILL_DIRS.iter().map(|dir| root.join(dir)).collect())
    }

    fn legacy_base(self) -> Result<std::path::PathBuf, String> {
        Ok(self.root()?.join(LEGACY_SKILL_DIR))
    }
}

/// Who owns a skill folder, which decides whether Hot may write to it.
#[derive(Debug, PartialEq, Eq)]
enum SkillDirOwner {
    /// Missing, or an empty folder Hot may fill.
    Nobody,
    /// Installed by `hot ai add`.
    Hot,
    /// Anything else, including a symlink: Hot leaves it alone.
    Other,
}

fn skill_dir_owner(skill_dir: &std::path::Path) -> SkillDirOwner {
    match fs::symlink_metadata(skill_dir) {
        Err(_) => SkillDirOwner::Nobody,
        Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
            SkillDirOwner::Other
        }
        Ok(_) if validate_skill_ownership(skill_dir).is_err() => SkillDirOwner::Other,
        Ok(_) if is_hot_managed_skill_dir(skill_dir) => SkillDirOwner::Hot,
        Ok(_) => SkillDirOwner::Nobody,
    }
}

fn is_hot_managed_skill_dir(skill_dir: &std::path::Path) -> bool {
    skill_dir.join(SKILL_MANIFEST_FILE).is_file()
        || skill_dir.join(SKILL_OWNER_FILE).is_file()
        || has_legacy_skill_stamp(skill_dir)
}

/// One skill folder Hot will write.
struct SkillTarget {
    base: std::path::PathBuf,
    scope: Scope,
    skill: String,
}

impl SkillTarget {
    fn preflight(&self) -> Result<(), String> {
        preflight_agent_skill(&self.base, &self.skill)
    }

    fn install(&self) -> Result<(), String> {
        setup_agent_skill(&self.base, self.scope, &self.skill)
    }
}

fn print_skipped_external(skill_dir: &std::path::Path) {
    println!(
        "  Left {} alone: another tool manages it",
        skill_dir.display()
    );
}

/// The skill folders of a scope Hot may write. A folder that is itself a
/// symlink, such as `~/.claude/skills` linked to `~/.agents/skills`, is
/// skipped: Hot never writes through a link.
fn writable_skill_bases(scope: Scope) -> Result<Vec<std::path::PathBuf>, String> {
    let mut bases = Vec::new();
    for base in scope.skill_bases()? {
        if ensure_managed_path_is_not_symlinked(&base, &base).is_err() {
            println!("  Left {} alone: it is a symlink", base.display());
            continue;
        }
        bases.push(base);
    }
    Ok(bases)
}

/// Every skill folder `hot ai add` writes for one scope. A folder another
/// tool manages is skipped; a skill no folder can take is an error.
fn add_targets(scope: Scope, skills: &[String]) -> Result<Vec<SkillTarget>, String> {
    let bases = writable_skill_bases(scope)?;
    let mut targets = Vec::new();
    for skill in skills {
        let mut placed = false;
        for base in &bases {
            let skill_dir = base.join(skill);
            if skill_dir_owner(&skill_dir) == SkillDirOwner::Other {
                print_skipped_external(&skill_dir);
                continue;
            }
            placed = true;
            targets.push(SkillTarget {
                base: base.clone(),
                scope,
                skill: skill.clone(),
            });
        }
        if !placed {
            return Err(format!(
                "Refusing to overwrite externally managed skill {skill} in {}. Remove those copies or use a different scope before running hot ai add.",
                scope
                    .skill_bases()?
                    .iter()
                    .map(|base| base.display().to_string())
                    .collect::<Vec<_>>()
                    .join(" and ")
            ));
        }
    }
    Ok(targets)
}

/// The skill folders `hot ai update` refreshes for one scope: each managed
/// install of a bundled skill, plus every location for a skill still in the
/// old `.skills` folder. A skill the user removed from one location stays
/// removed there.
fn update_targets(scope: Scope, bundled: &BTreeSet<String>) -> Result<Vec<SkillTarget>, String> {
    if !has_managed_skills(scope) {
        return Ok(Vec::new());
    }
    let legacy: BTreeSet<String> = installed_hot_skill_names(&scope.legacy_base()?)
        .into_iter()
        .filter(|name| bundled.contains(name))
        .collect();
    let mut targets = Vec::new();
    for base in writable_skill_bases(scope)? {
        let installed: BTreeSet<String> = installed_hot_skill_names(&base)
            .into_iter()
            .filter(|name| bundled.contains(name))
            .collect();
        for skill in installed.union(&legacy) {
            let owner = skill_dir_owner(&base.join(skill));
            if owner == SkillDirOwner::Other {
                if legacy.contains(skill) {
                    print_skipped_external(&base.join(skill));
                }
                continue;
            }
            targets.push(SkillTarget {
                base: base.clone(),
                scope,
                skill: skill.clone(),
            });
        }
    }
    Ok(targets)
}

/// Remove the old `.skills` copy of each skill that now has a Hot-managed
/// copy where agents read it.
fn retire_legacy_skills(scope: Scope) -> Result<(), String> {
    let legacy_base = scope.legacy_base()?;
    let bases = scope.skill_bases()?;
    for skill in installed_hot_skill_names(&legacy_base) {
        let moved = bases
            .iter()
            .any(|base| skill_dir_owner(&base.join(&skill)) == SkillDirOwner::Hot);
        if moved {
            retire_legacy_skill(&legacy_base, &skill)?;
        }
    }
    // Only succeeds once the folder is empty.
    let _ = fs::remove_dir(&legacy_base);
    Ok(())
}

/// Delete one skill Hot installed under `.skills`, unless it holds anything
/// Hot didn't write: a changed or added file, or a symlink. Those copies stay
/// put with a note, so no local edit is lost.
fn retire_legacy_skill(legacy_base: &std::path::Path, skill: &str) -> Result<(), String> {
    let skill_dir = legacy_base.join(skill);
    let keep = |reason: &str| {
        println!(
            "  Left {} in place: {reason}. Agents don't read that folder; move anything you need and delete it.",
            skill_dir.display()
        );
        Ok(())
    };

    if ensure_managed_path_is_not_symlinked(legacy_base, &skill_dir).is_err() {
        return keep("it is a symlink");
    }
    let InstalledSkillManifestState::Valid(manifest) = read_installed_skill_manifest(&skill_dir)?
    else {
        return keep("it has no manifest to show it is unchanged");
    };

    let mut files = Vec::new();
    collect_skill_files(&skill_dir, &skill_dir, &mut files);
    for rel_path in &files {
        let full_path = skill_dir.join(rel_path);
        let key = skill_manifest_key(rel_path);
        if key == SKILL_MANIFEST_FILE {
            continue;
        }
        let unchanged = fs::symlink_metadata(&full_path)
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
            && fs::read(&full_path).ok().is_some_and(|content| {
                manifest.files.get(&key) == Some(&skill_content_hash(&content))
            });
        if !unchanged {
            return keep(&format!("{} was changed or added", full_path.display()));
        }
    }

    // The manifest goes last, so an interrupted removal can be retried.
    files.sort_by_key(|path| skill_manifest_key(path) == SKILL_MANIFEST_FILE);
    for rel_path in &files {
        let full_path = skill_dir.join(rel_path);
        fs::remove_file(&full_path)
            .map_err(|e| format!("Failed to remove {}: {}", full_path.display(), e))?;
    }
    remove_empty_dirs(&skill_dir);
    println!("  Removed old copy {}", skill_dir.display());
    Ok(())
}

/// Remove `dir` and the folders under it, bottom up, while they are empty.
fn remove_empty_dirs(dir: &std::path::Path) {
    if let Ok(entries) = fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|file_type| file_type.is_dir()) {
                remove_empty_dirs(&entry.path());
            }
        }
    }
    let _ = fs::remove_dir(dir);
}

fn collect_skill_files(
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
                collect_skill_files(&path, base, files);
            } else if (file_type.is_file() || file_type.is_symlink())
                && let Ok(rel) = path.strip_prefix(base)
            {
                files.push(rel.to_path_buf());
            }
        }
    }
}

pub(crate) fn run_ai(action: &AiAction) -> Result<(), String> {
    match action {
        AiAction::Add { global } => {
            info!("Adding AI coding support using AGENTS.md + SKILL.md standards...");
            let scope = if *global {
                Scope::Global
            } else {
                Scope::Project
            };

            // Check every destination before writing anything, AGENTS.md
            // included, so a rejected skill install cannot leave a partial add.
            let targets = add_targets(scope, &bundled_skill_names()?)?;
            for target in &targets {
                target.preflight()?;
            }
            // AGENTS.md belongs to a project; a global install only adds skills.
            if scope == Scope::Project {
                setup_agents_md()?;
            }
            for target in &targets {
                target.install()?;
            }
            retire_legacy_skills(scope)?;

            Ok(())
        }
        AiAction::List => {
            print_ai_status();
            Ok(())
        }
        AiAction::Update { global } => {
            info!("Updating AI support files...");
            let scopes: &[Scope] = if *global {
                &[Scope::Global]
            } else {
                &[Scope::Project, Scope::Global]
            };

            // Resolve the bundle lazily: an update with nothing installed
            // must not require the skill resources to be present.
            let any_installed = scopes.iter().any(|scope| has_managed_skills(*scope));
            let bundled: BTreeSet<String> = if any_installed {
                bundled_skill_names()?.into_iter().collect()
            } else {
                BTreeSet::new()
            };
            let mut targets = Vec::new();
            for scope in scopes {
                targets.extend(update_targets(*scope, &bundled)?);
            }

            // Validate every source, destination, and installed manifest this
            // update will touch before refreshing AGENTS.md. The write path
            // repeats the safety checks to narrow the TOCTOU window.
            for target in &targets {
                target.preflight()?;
            }

            let mut updated: BTreeSet<String> = BTreeSet::new();
            if !*global && std::path::Path::new("AGENTS.md").exists() {
                setup_agents_md()?;
                updated.insert("AGENTS.md".into());
            }
            for target in &targets {
                target.install()?;
                updated.insert(target.base.display().to_string());
            }
            for scope in scopes {
                retire_legacy_skills(*scope)?;
            }

            if updated.is_empty() {
                if *global {
                    println!("No global Hot skills found to update.");
                    println!("Use 'hot ai add --global' to install them.");
                } else {
                    println!("No AI support files found to update.");
                    println!("Use 'hot ai add' to add AI support to this project.");
                }
            } else {
                println!("\nUpdated {} location(s).", updated.len());
            }
            Ok(())
        }
    }
}

/// Whether Hot manages any skill for this scope, in a current location or
/// the old `.skills` folder.
fn has_managed_skills(scope: Scope) -> bool {
    let Ok(bases) = scope.skill_bases() else {
        return false;
    };
    let legacy = scope.legacy_base().ok();
    bases
        .iter()
        .chain(legacy.iter())
        .any(|base| !installed_hot_skill_names(base).is_empty())
}

/// Whether `hot update` should refresh global skills afterwards.
pub(crate) fn has_global_managed_skills() -> bool {
    has_managed_skills(Scope::Global)
}

/// How an installed AI file compares with what this Hot release bundles.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Freshness {
    Current,
    /// Installed from another Hot release.
    Stale,
    /// In the old `.skills` folder, which agents don't read.
    Legacy,
    /// Present but managed outside Hot.
    External,
}

impl Freshness {
    fn describe(self) -> &'static str {
        match self {
            Freshness::Current => "up to date",
            Freshness::Stale => "out of date",
            Freshness::Legacy => "old location, agents don't read it",
            Freshness::External => "managed outside Hot",
        }
    }

    fn needs_update(self) -> bool {
        matches!(self, Freshness::Stale | Freshness::Legacy)
    }
}

struct InstalledSkill {
    dir: std::path::PathBuf,
    freshness: Freshness,
}

/// The manifest a fresh install of this skill would write.
fn bundled_skill_manifest(skill: &str) -> Result<BTreeMap<String, String>, String> {
    let mut files: BTreeMap<String, String> = bundled_skill_files(skill)?
        .iter()
        .map(|(path, content)| (skill_manifest_key(path), skill_content_hash(content)))
        .collect();
    files.insert(
        SKILL_OWNER_FILE.to_string(),
        skill_content_hash(SKILL_OWNER_CONTENT),
    );
    Ok(files)
}

/// The installed copies of Hot skills in one scope, and how each compares
/// with this release. The manifest records what Hot shipped, so a copy with
/// local edits still counts as current.
fn installed_skills(scope: Scope) -> Vec<InstalledSkill> {
    let (Ok(bases), Ok(legacy_base)) = (scope.skill_bases(), scope.legacy_base()) else {
        return Vec::new();
    };
    let bundled: BTreeSet<String> = bundled_skill_names()
        .map(|names| names.into_iter().collect())
        .unwrap_or_default();
    let mut manifests: BTreeMap<String, Option<BTreeMap<String, String>>> = BTreeMap::new();
    let mut is_current = |skill: &str, dir: &std::path::Path| {
        let expected = manifests
            .entry(skill.to_string())
            .or_insert_with(|| bundled_skill_manifest(skill).ok());
        matches!(
            (expected, read_installed_skill_manifest(dir)),
            (Some(expected), Ok(InstalledSkillManifestState::Valid(installed)))
                if installed.files == *expected
        )
    };

    let mut found = Vec::new();
    for base in &bases {
        let managed: BTreeSet<String> = installed_hot_skill_names(base).into_iter().collect();
        for skill in bundled.union(&managed) {
            let dir = base.join(skill);
            let freshness = match skill_dir_owner(&dir) {
                SkillDirOwner::Nobody => continue,
                SkillDirOwner::Other => Freshness::External,
                SkillDirOwner::Hot if is_current(skill, &dir) => Freshness::Current,
                SkillDirOwner::Hot => Freshness::Stale,
            };
            found.push(InstalledSkill { dir, freshness });
        }
    }
    for skill in installed_hot_skill_names(&legacy_base) {
        found.push(InstalledSkill {
            dir: legacy_base.join(skill),
            freshness: Freshness::Legacy,
        });
    }
    found
}

/// The Hot section of AGENTS.md in the current folder, if it has one.
fn agents_md_freshness() -> Option<Freshness> {
    let existing = fs::read_to_string("AGENTS.md").ok()?;
    let installed = extract_section_hash(&existing, HOT_SECTION_START)?;
    let template = hot::resources::read_agents_md().ok()?;
    Some(if installed == agents_section_hash(&template) {
        Freshness::Current
    } else {
        Freshness::Stale
    })
}

fn print_ai_status() {
    println!("AI Coding Support Status:\n");

    let agents_status = match (
        std::path::Path::new("AGENTS.md").exists(),
        agents_md_freshness(),
    ) {
        (false, _) => "not installed".to_string(),
        (true, None) => "present, no Hot section".to_string(),
        (true, Some(freshness)) => freshness.describe().to_string(),
    };
    println!("  AGENTS.md  {agents_status}");

    let mut needs_update = agents_md_freshness().is_some_and(Freshness::needs_update);
    for scope in [Scope::Project, Scope::Global] {
        let skills = installed_skills(scope);
        println!("\n  Skills ({}):", scope.label());
        if skills.is_empty() {
            println!("    none installed");
        }
        for skill in &skills {
            needs_update |= skill.freshness.needs_update();
            println!(
                "    {}/  {}",
                skill.dir.display(),
                skill.freshness.describe()
            );
        }
    }

    let legacy_files = [
        ("CLAUDE.md", "Old Claude Code file"),
        (".cursor/rules/hot.mdc", "Old Cursor rules"),
        (
            ".github/copilot-instructions.md",
            "Old Copilot instructions",
        ),
        (".windsurfrules", "Old Windsurf rules"),
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

    println!();
    if needs_update {
        println!("Use 'hot ai update' to refresh out-of-date files and move old copies.");
    }
    println!("Use 'hot ai add' to add AI support to this project.");
    println!(
        "Use 'hot ai add --global' to install skills to ~/{}/",
        SKILL_DIRS.join("/ and ~/")
    );
    println!("Use 'npx skills add hot-dev/hot-skills' for the public skills.sh source.");
}

/// One-line hints about installed AI files this release would change, for
/// `hot dev` to print at startup. Empty when everything is current.
pub(crate) fn stale_ai_hints() -> Vec<String> {
    let mut hints = Vec::new();

    let mut project: Vec<String> = Vec::new();
    if agents_md_freshness().is_some_and(Freshness::needs_update) {
        project.push("AGENTS.md".into());
    }
    let project_skills = installed_skills(Scope::Project);
    project.extend(
        project_skills
            .iter()
            .filter(|skill| skill.freshness.needs_update())
            .map(|skill| skill.dir.display().to_string()),
    );
    if !project.is_empty() {
        hints.push(format!(
            "AI files in this project need updating for this Hot release ({}). Run 'hot ai update'.",
            project.join(", ")
        ));
    }

    let global_stale = installed_skills(Scope::Global)
        .iter()
        .any(|skill| skill.freshness.needs_update());
    if global_stale {
        hints.push(
            "Your global Hot skills need updating for this Hot release. Run 'hot ai update --global'."
                .into(),
        );
    }
    hints
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

const HOT_SECTION_START: &str = "<!-- HOT_LANGUAGE_SECTION_START -->";
const HOT_SECTION_END: &str = "<!-- HOT_LANGUAGE_SECTION_END -->";

/// The hash stamped after the start marker of the Hot section in AGENTS.md.
fn agents_section_hash(content: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(content.as_bytes()))[..12].to_string()
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

/// Setup AGENTS.md with the canonical Hot section from resources/ai/AGENTS.md.
fn setup_agents_md() -> Result<(), String> {
    use agents_section_hash as content_hash;

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
    hex::encode(Sha256::digest(content))
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
            let content =
                fs::read(&path).map_err(|e| format!("Failed to read {}: {}", path.display(), e))?;
            files.push((rel_path.to_path_buf(), content));
        }
    }
    Ok(())
}

fn bundled_skill_files(skill_name: &str) -> Result<Vec<(std::path::PathBuf, Vec<u8>)>, String> {
    let source_skill_dir = hot::resources::get_skill_path(skill_name)?;
    let mut skill_files = Vec::new();
    collect_source_files(&source_skill_dir, &source_skill_dir, &mut skill_files)?;
    Ok(skill_files)
}

fn manifest_relative_path(key: &str) -> Result<std::path::PathBuf, String> {
    use std::path::Component;

    let path = std::path::Path::new(key);
    if key.is_empty()
        || !path
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
    {
        return Err(format!("Invalid managed skill manifest path: {key}"));
    }
    Ok(path.to_path_buf())
}

fn preflight_agent_skill(skills_base: &std::path::Path, skill_name: &str) -> Result<(), String> {
    let skill_files = bundled_skill_files(skill_name)?;
    let skills_base = skills_base.to_path_buf();
    let skill_dir = skills_base.join(skill_name);

    ensure_managed_path_is_not_symlinked(&skills_base, &skills_base)?;
    ensure_managed_path_is_not_symlinked(&skills_base, &skill_dir)?;
    validate_skill_ownership(&skill_dir)?;
    ensure_managed_path_is_not_symlinked(&skills_base, &skill_dir.join(SKILL_MANIFEST_FILE))?;
    ensure_managed_path_is_not_symlinked(&skills_base, &skill_dir.join(SKILL_OWNER_FILE))?;
    for (rel_path, _) in &skill_files {
        ensure_managed_path_is_not_symlinked(&skills_base, &skill_dir.join(rel_path))?;
    }

    if let InstalledSkillManifestState::Valid(manifest) = read_installed_skill_manifest(&skill_dir)?
    {
        for key in manifest.files.keys() {
            let rel_path = manifest_relative_path(key)?;
            ensure_managed_path_is_not_symlinked(&skills_base, &skill_dir.join(rel_path))?;
        }
    }
    Ok(())
}

/// Install or refresh one bundled skill under one of [`SKILL_DIRS`].
fn setup_agent_skill(
    skills_base: &std::path::Path,
    scope: Scope,
    skill_name: &str,
) -> Result<(), String> {
    use ahash::AHashSet;

    let skills_base = skills_base.to_path_buf();
    let location_desc = scope.label();
    let skill_files = bundled_skill_files(skill_name)?;

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
        collect_skill_files(&skill_dir, &skill_dir, &mut existing_files);

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
        println!("  {}/ is up to date", skill_dir.display());
    }

    Ok(())
}

#[cfg(test)]
mod skill_manifest_tests {
    use super::{extract_skill_hash, skill_content_hash, strip_legacy_skill_hash};

    #[test]
    fn skill_content_hash_is_lowercase_sha256_hex() {
        // Stored in skill manifests. A format change would mark every
        // installed skill as user-modified.
        assert_eq!(
            skill_content_hash(b"skill"),
            "9c53c074d7ac6a2728b638ac1f376c5fa9eb8f71603017c3ea638c2fd40548df"
        );
    }

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
