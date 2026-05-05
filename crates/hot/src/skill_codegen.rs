//! Skill-stub codegen.
//!
//! Walks the project's resource roots for `*.skill.md` files and emits a
//! matching `*.skill.hot` stub in a configurable output directory. Each
//! stub declares a `::ai::skill/Skill` value, so a Markdown-authored
//! skill can be passed directly to `::ai::skill/for-agent` without a
//! no-op function wrapper.
//!
//! ## Authoring shape
//!
//! ```text
//! resources/
//!   skills/
//!     customer-tone.skill.md      # YAML frontmatter + Markdown body
//!     refunds/
//!       escalate.skill.md
//! ```
//!
//! ## Generated shape
//!
//! ```text
//! hot/src/_skills/
//!   skills/
//!     customer-tone.skill.hot     # ::skills::skills/customer-tone
//!     refunds/
//!       escalate.skill.hot        # ::skills::skills::refunds/escalate
//! ```
//!
//! Generated paths mirror the source directory structure so a project
//! with hundreds of skills stays organized. The namespace is derived
//! from the configured root namespace (default `::skills`) plus the
//! intermediate directories joined with `::`.
//!
//! ## Ownership marker
//!
//! Every stub starts with a one-line marker:
//!
//! ```text
//! // HOT-CODEGEN: skill-stub from <rel-md-path> [hash:<blake3-12>]
//! ```
//!
//! The codegen only overwrites a file when this marker is intact and the
//! recorded source-hash differs (or the file is absent). Removing the
//! marker — or any user edit that displaces the first line — gives the
//! user full ownership: subsequent codegen runs leave that file alone.
//! When a `.skill.md` source disappears, the corresponding marker-bearing
//! stub is deleted so stale skills don't linger in the registry.

use crate::discovery::{DiscoveryOpts, discover};
use crate::lang::hot::md::parse_frontmatter_str;
use crate::val::Val;
use indexmap::IndexMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Marker the codegen writes as the first line of every generated stub.
/// We grep for the prefix when deciding whether a file is owned by the
/// generator (overwrite ok) or the user (leave it alone).
pub const MARKER_PREFIX: &str = "// HOT-CODEGEN: skill-stub from ";

/// Default values for [`SkillCodegenOpts`] when the project hasn't
/// overridden them in `hot.hot`.
const DEFAULT_OUT_DIR: &str = "hot/src/_skills";
const DEFAULT_NAMESPACE: &str = "::skills";

/// Per-project skill codegen configuration. Sourced from
/// `hot.project.<name>.skills.codegen.{enabled,out-dir,namespace}`; see
/// [`opts_from_conf`].
#[derive(Debug, Clone)]
pub struct SkillCodegenOpts {
    /// When `false`, [`run_skill_codegen`] returns an empty report
    /// without scanning anything. Mirrors `skills.codegen.enabled`.
    pub enabled: bool,
    /// Optional resource roots used only by skill codegen. When unset,
    /// codegen scans the project's normal `resources.paths`.
    pub resource_paths: Option<Vec<PathBuf>>,
    /// Directory (relative to project root) where stubs are written.
    /// Defaults to `hot/src/_skills`.
    pub out_dir: PathBuf,
    /// Root namespace prepended to every generated stub. Subdirectory
    /// path components get appended via `::`. Default `::skills`.
    pub root_namespace: String,
}

impl Default for SkillCodegenOpts {
    fn default() -> Self {
        Self {
            enabled: true,
            resource_paths: None,
            out_dir: PathBuf::from(DEFAULT_OUT_DIR),
            root_namespace: DEFAULT_NAMESPACE.to_string(),
        }
    }
}

/// Summary returned by [`run_skill_codegen`] so the CLI can log a
/// concise one-line status (and tests can assert the right files were
/// touched).
#[derive(Debug, Default, Clone)]
pub struct SkillCodegenReport {
    /// Stubs newly created on disk.
    pub created: Vec<PathBuf>,
    /// Stubs already up to date — no write performed.
    pub unchanged: Vec<PathBuf>,
    /// Existing stubs whose content was rewritten because the source
    /// `.skill.md` changed.
    pub updated: Vec<PathBuf>,
    /// Files that exist at the target path but lack the codegen marker;
    /// the user has taken ownership and we left them alone.
    pub user_owned: Vec<PathBuf>,
    /// Marker-bearing stubs deleted because the source `.skill.md` was
    /// removed.
    pub removed: Vec<PathBuf>,
    /// Per-file errors encountered during scan/write. The runner does
    /// not abort on a single failure — we log and continue so one bad
    /// file can't break `hot dev`.
    pub errors: Vec<(PathBuf, String)>,
}

impl SkillCodegenReport {
    /// True iff anything other than `unchanged` happened. Used by the
    /// CLI to decide whether to emit a one-line summary.
    pub fn any_changes(&self) -> bool {
        !self.created.is_empty()
            || !self.updated.is_empty()
            || !self.removed.is_empty()
            || !self.errors.is_empty()
    }

    /// Concise summary suitable for a single info-level log line.
    pub fn summary(&self) -> String {
        format!(
            "skills: {} created, {} updated, {} unchanged, {} user-owned, {} removed, {} errors",
            self.created.len(),
            self.updated.len(),
            self.unchanged.len(),
            self.user_owned.len(),
            self.removed.len(),
            self.errors.len()
        )
    }
}

/// Read [`SkillCodegenOpts`] from a project's resolved configuration.
///
/// All fields are optional; missing values fall back to the defaults
/// (`enabled=true`, `paths=<resources.paths>`, `out-dir=hot/src/_skills`,
/// `namespace=::skills`).
pub fn opts_from_conf(conf: &Val, project_name: &str) -> SkillCodegenOpts {
    let mut opts = SkillCodegenOpts::default();
    let Some(project_conf) = crate::project::get_project_conf(conf, project_name) else {
        return opts;
    };
    let Some(skills) = project_conf.get("skills") else {
        return opts;
    };
    let Some(codegen) = skills.get("codegen") else {
        return opts;
    };

    if let Some(Val::Bool(b)) = codegen.get("enabled") {
        opts.enabled = b;
    }
    if let Some(Val::Vec(vec)) = codegen.get("paths") {
        opts.resource_paths = Some(
            vec.iter()
                .map(|v| match v {
                    Val::Str(s) => PathBuf::from(s.as_ref()),
                    _ => PathBuf::from(v.to_string().trim_matches('"').to_string()),
                })
                .collect(),
        );
    }
    if let Some(Val::Str(s)) = codegen.get("out-dir") {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            opts.out_dir = PathBuf::from(trimmed);
        }
    }
    if let Some(Val::Str(s)) = codegen.get("namespace") {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            opts.root_namespace = normalize_namespace(trimmed);
        }
    }
    opts
}

/// Convenience entry point used by the CLI: read the codegen opts from
/// `conf`, resolve the resource roots (project-config + any extra paths
/// from `--resource.path`), and run the codegen against them.
///
/// Disabling the codegen via `skills.codegen.enabled = false` returns
/// an empty report without touching the disk.
pub fn run_skill_codegen_from_conf(
    conf: &Val,
    project_name: &str,
    extra_resource_paths: &[String],
) -> SkillCodegenReport {
    let opts = opts_from_conf(conf, project_name);
    if !opts.enabled {
        tracing::trace!("skill codegen disabled for project '{}'", project_name);
        return SkillCodegenReport::default();
    }

    let mut resource_roots: Vec<PathBuf> = opts.resource_paths.clone().unwrap_or_else(|| {
        crate::project::get_project_resource_paths(conf, project_name)
            .into_iter()
            .map(PathBuf::from)
            .collect()
    });
    for p in extra_resource_paths {
        resource_roots.push(PathBuf::from(p));
    }

    if resource_roots.is_empty() {
        tracing::trace!(
            "skill codegen: no resource paths configured for project '{}'",
            project_name
        );
        return SkillCodegenReport::default();
    }

    let respect_gitignore = crate::project::get_project_respect_gitignore(conf, project_name);
    let mut excludes = crate::project::get_project_ignore_excludes(conf, project_name);
    excludes.extend(crate::project::get_project_resource_excludes(
        conf,
        project_name,
    ));

    if !project_declares_hot_ai(conf, project_name)
        && let Some(skill_sources) =
            discover_skill_sources(&resource_roots, respect_gitignore, &excludes)
    {
        return missing_hot_ai_report(project_name, skill_sources);
    }

    run_skill_codegen(
        &resource_roots,
        respect_gitignore,
        &excludes,
        std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        &opts,
    )
}

fn project_declares_hot_ai(conf: &Val, project_name: &str) -> bool {
    let Some(deps) = crate::project::get_project_dependencies(conf, project_name) else {
        return false;
    };
    let Val::Map(m) = deps else {
        return false;
    };
    m.keys().any(|k| match k {
        Val::Str(s) => matches!(s.as_ref(), "hot.dev/hot-ai" | "hot-ai"),
        _ => matches!(k.to_string().trim_matches('"'), "hot.dev/hot-ai" | "hot-ai"),
    })
}

fn discover_skill_sources(
    resource_roots: &[PathBuf],
    respect_gitignore: bool,
    excludes: &[String],
) -> Option<Vec<PathBuf>> {
    let discovery_opts = DiscoveryOpts {
        extensions: vec!["md".to_string()],
        respect_gitignore,
        skip_hidden: false,
        extra_excludes: excludes.to_vec(),
        apply_default_excludes: true,
    };
    let mut sources = Vec::new();
    for root in resource_roots {
        if !root.exists() {
            continue;
        }
        let abs_root = root.canonicalize().unwrap_or_else(|_| root.clone());
        for d in discover(&[&abs_root], &discovery_opts) {
            if d.rel_path.ends_with(".skill.md") {
                sources.push(d.abs_path);
            }
        }
    }
    if sources.is_empty() {
        None
    } else {
        sources.sort();
        sources.dedup();
        Some(sources)
    }
}

fn missing_hot_ai_report(project_name: &str, sources: Vec<PathBuf>) -> SkillCodegenReport {
    let mut report = SkillCodegenReport::default();
    let msg = format!(
        "Markdown skill codegen emits ::ai::skill/Skill values; add \
         `\"hot.dev/hot-ai\": {{}}` to hot.project.{}.deps or disable \
         skills.codegen.enabled",
        project_name
    );
    for source in sources {
        report.errors.push((source, msg.clone()));
    }
    report
}

/// Core codegen entry point: scan `resource_roots` for `*.skill.md`,
/// generate stubs under `opts.out_dir` (resolved relative to
/// `project_root`), and prune stale stubs whose source file disappeared.
///
/// Idempotent: re-running with no source changes yields all
/// `unchanged` entries and zero writes.
pub fn run_skill_codegen(
    resource_roots: &[PathBuf],
    respect_gitignore: bool,
    excludes: &[String],
    project_root: PathBuf,
    opts: &SkillCodegenOpts,
) -> SkillCodegenReport {
    let mut report = SkillCodegenReport::default();

    let out_dir = if opts.out_dir.is_absolute() {
        opts.out_dir.clone()
    } else {
        project_root.join(&opts.out_dir)
    };

    let discovery_opts = DiscoveryOpts {
        extensions: vec!["md".to_string()],
        respect_gitignore,
        skip_hidden: false,
        extra_excludes: excludes.to_vec(),
        apply_default_excludes: true,
    };

    // Collect (root, discovered) pairs. We need to know which root each
    // .skill.md came from so the rel_path inside the marker is stable
    // across runs even when multiple roots are configured.
    let mut sources: Vec<SkillSource> = Vec::new();
    for root in resource_roots {
        if !root.exists() {
            continue;
        }
        let abs_root = root.canonicalize().unwrap_or_else(|_| root.clone());
        let found = discover(&[&abs_root], &discovery_opts);
        for d in found {
            if !d.rel_path.ends_with(".skill.md") {
                continue;
            }
            sources.push(SkillSource {
                abs_path: d.abs_path,
                rel_path: d.rel_path,
            });
            // `abs_root` is captured by the discover() call above; we
            // don't persist it on each source because the marker only
            // needs the relative path.
            let _ = &abs_root;
        }
    }

    // De-dup on rel_path (first root wins, matching resource registry).
    let mut seen_rel: ahash::AHashSet<String> = ahash::AHashSet::new();
    sources.retain(|s| seen_rel.insert(s.rel_path.clone()));

    // Track every output path we touched so we can prune stragglers.
    let mut produced_paths: ahash::AHashSet<PathBuf> = ahash::AHashSet::new();

    for src in &sources {
        match generate_one(src, &out_dir, opts) {
            Ok(outcome) => {
                produced_paths.insert(outcome.out_path.clone());
                match outcome.kind {
                    GeneratedKind::Created => report.created.push(outcome.out_path),
                    GeneratedKind::Updated => report.updated.push(outcome.out_path),
                    GeneratedKind::Unchanged => report.unchanged.push(outcome.out_path),
                    GeneratedKind::UserOwned => report.user_owned.push(outcome.out_path),
                }
            }
            Err(e) => {
                tracing::warn!(
                    "skill codegen: failed to generate stub for {}: {}",
                    src.abs_path.display(),
                    e
                );
                report.errors.push((src.abs_path.clone(), e));
            }
        }
    }

    // Prune marker-bearing stubs whose source went away. We only do
    // this when the out_dir actually exists (avoid a spurious mkdir
    // probe on disabled-by-default projects).
    if out_dir.exists() {
        prune_orphan_stubs(&out_dir, &produced_paths, resource_roots, &mut report);
    }

    report
}

#[derive(Debug)]
struct SkillSource {
    abs_path: PathBuf,
    /// Path relative to its source root, e.g. `skills/customer/tone.skill.md`.
    /// Embedded verbatim in the codegen marker so renames are visible
    /// across runs.
    rel_path: String,
}

#[derive(Debug)]
struct GeneratedOutcome {
    out_path: PathBuf,
    kind: GeneratedKind,
}

#[derive(Debug, Clone, Copy)]
enum GeneratedKind {
    Created,
    Updated,
    Unchanged,
    UserOwned,
}

fn generate_one(
    src: &SkillSource,
    out_dir: &Path,
    opts: &SkillCodegenOpts,
) -> Result<GeneratedOutcome, String> {
    let source_text =
        fs::read_to_string(&src.abs_path).map_err(|e| format!("read source: {}", e))?;
    let (frontmatter, body) =
        parse_frontmatter_str(&source_text).map_err(|e| format!("parse frontmatter: {}", e))?;

    // Compute the output path: replace `.skill.md` -> `.skill.hot`,
    // preserve the directory structure under the source root.
    let rel_hot = rel_path_from_md(&src.rel_path)?;
    let out_path = out_dir.join(&rel_hot);

    // Derive namespace + function name from rel path components.
    let (ns, fn_name) = ns_and_fn_from_rel(&rel_hot, &opts.root_namespace);

    let src_hash = blake3_short(source_text.as_bytes());
    let marker_line = format!("{}{} [hash:{}]", MARKER_PREFIX, src.rel_path, src_hash);

    let new_contents = format_stub(&marker_line, &ns, &fn_name, &frontmatter, &body);

    if let Some(existing) = read_existing(&out_path)? {
        let first_line = existing.lines().next().unwrap_or("");
        if !first_line.starts_with(MARKER_PREFIX) {
            // User has taken ownership.
            tracing::trace!(
                "skill codegen: {} is user-owned (no marker), skipping",
                out_path.display()
            );
            return Ok(GeneratedOutcome {
                out_path,
                kind: GeneratedKind::UserOwned,
            });
        }
        if existing == new_contents {
            return Ok(GeneratedOutcome {
                out_path,
                kind: GeneratedKind::Unchanged,
            });
        }
        write_atomic(&out_path, new_contents.as_bytes())?;
        Ok(GeneratedOutcome {
            out_path,
            kind: GeneratedKind::Updated,
        })
    } else {
        write_atomic(&out_path, new_contents.as_bytes())?;
        Ok(GeneratedOutcome {
            out_path,
            kind: GeneratedKind::Created,
        })
    }
}

fn read_existing(path: &Path) -> Result<Option<String>, String> {
    match fs::read_to_string(path) {
        Ok(s) => Ok(Some(s)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("read existing stub: {}", e)),
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("mkdir {}: {}", parent.display(), e))?;
    }
    let tmp = path.with_extension("hot.tmp");
    {
        let mut f =
            fs::File::create(&tmp).map_err(|e| format!("create {}: {}", tmp.display(), e))?;
        f.write_all(bytes)
            .map_err(|e| format!("write {}: {}", tmp.display(), e))?;
        f.sync_all().ok();
    }
    fs::rename(&tmp, path)
        .map_err(|e| format!("rename {} -> {}: {}", tmp.display(), path.display(), e))?;
    Ok(())
}

fn prune_orphan_stubs(
    out_dir: &Path,
    produced: &ahash::AHashSet<PathBuf>,
    resource_roots: &[PathBuf],
    report: &mut SkillCodegenReport,
) {
    // Walk out_dir for *.skill.hot files and delete any that we didn't
    // produce on this run *iff*:
    //   1. The stub carries the codegen marker (hand-authored stubs
    //      are never touched), AND
    //   2. The marker's source `rel_path` is plausibly *ours* — its
    //      parent directory exists under at least one of our resource
    //      roots. Stubs whose marker points at a subtree that doesn't
    //      exist under any of our roots were generated by some other
    //      project sharing this out-dir; we leave them alone.
    //
    // Heuristic in (2) is intentionally lenient: a `.skill.md` deleted
    // alongside its containing directory will not be pruned, but the
    // common case (deleting just the file) still cleans up.
    let opts = DiscoveryOpts {
        extensions: vec!["hot".to_string()],
        respect_gitignore: false, // out_dir may live outside any gitignored area
        skip_hidden: false,
        extra_excludes: Vec::new(),
        apply_default_excludes: false,
    };
    let abs_out = out_dir
        .canonicalize()
        .unwrap_or_else(|_| out_dir.to_path_buf());
    let found = discover(&[&abs_out], &opts);
    for d in found {
        if !d.rel_path.ends_with(".skill.hot") {
            continue;
        }
        let canonical = d
            .abs_path
            .canonicalize()
            .unwrap_or_else(|_| d.abs_path.clone());
        if produced
            .iter()
            .any(|p| p.canonicalize().ok().as_deref() == Some(&canonical))
        {
            continue;
        }
        // Only delete if the file is marker-bearing AND its source
        // path's parent is inside one of our resource roots.
        let Ok(content) = fs::read_to_string(&d.abs_path) else {
            continue;
        };
        let first = content.lines().next().unwrap_or("");
        if !first.starts_with(MARKER_PREFIX) {
            continue;
        }
        let Some(source_rel) = parse_marker_source_rel(first) else {
            continue;
        };
        let source_parent = Path::new(&source_rel)
            .parent()
            .unwrap_or_else(|| Path::new(""));
        let owned_by_us = resource_roots.iter().any(|root| {
            let candidate = root.join(source_parent);
            candidate.is_dir()
        });
        if !owned_by_us {
            tracing::trace!(
                "skill codegen: leaving orphan stub {} alone — source '{}' is outside our resource roots",
                d.abs_path.display(),
                source_rel
            );
            continue;
        }
        match fs::remove_file(&d.abs_path) {
            Ok(_) => report.removed.push(d.abs_path.clone()),
            Err(e) => report
                .errors
                .push((d.abs_path.clone(), format!("remove orphan stub: {}", e))),
        }
    }
}

/// Extract the source `rel_path` from a codegen marker line.
///
/// Marker shape: `// HOT-CODEGEN: skill-stub from <rel_path> [hash:<hash>]`
/// — the rel_path is everything between `MARKER_PREFIX` and the trailing
/// ` [hash:`. Returns `None` if the line doesn't match.
fn parse_marker_source_rel(marker_line: &str) -> Option<String> {
    let after_prefix = marker_line.strip_prefix(MARKER_PREFIX)?;
    // The hash suffix is always ` [hash:...]`. Trim that off — it can
    // be absent for malformed markers, in which case we still try to
    // recover a usable rel_path.
    let rel = match after_prefix.rfind(" [hash:") {
        Some(idx) => &after_prefix[..idx],
        None => after_prefix.trim_end(),
    };
    let rel = rel.trim();
    if rel.is_empty() {
        None
    } else {
        Some(rel.to_string())
    }
}

/// Convert `skills/customer-tone.skill.md` -> `skills/customer-tone.skill.hot`.
fn rel_path_from_md(rel_md: &str) -> Result<PathBuf, String> {
    let stripped = rel_md
        .strip_suffix(".skill.md")
        .ok_or_else(|| format!("expected .skill.md suffix in {}", rel_md))?;
    Ok(PathBuf::from(format!("{}.skill.hot", stripped)))
}

/// Derive `(namespace, fn-name)` from a relative `.skill.hot` path.
///
/// `skills/customer/tone.skill.hot` + root `::skills`
///   -> (`::skills::skills::customer`, `tone`)
///
/// Single-segment paths (no intermediate dirs) keep just the root namespace:
/// `customer-tone.skill.hot` + `::skills` -> (`::skills`, `customer-tone`)
fn ns_and_fn_from_rel(rel_hot: &Path, root_ns: &str) -> (String, String) {
    let mut components: Vec<String> = rel_hot
        .iter()
        .map(|c| c.to_string_lossy().to_string())
        .collect();

    let leaf = components.pop().unwrap_or_else(|| "skill.hot".to_string());
    let fn_name = leaf.strip_suffix(".skill.hot").unwrap_or(&leaf).to_string();

    let root = normalize_namespace(root_ns);
    let ns = if components.is_empty() {
        root
    } else {
        let suffix = components
            .iter()
            .map(|s| sanitize_ns_segment(s))
            .collect::<Vec<_>>()
            .join("::");
        format!("{}::{}", root, suffix)
    };
    (ns, fn_name)
}

/// Strip a leading `::` (we re-prepend it) and reject empty input. Used
/// by both config parsing and the path-derived suffix.
fn normalize_namespace(s: &str) -> String {
    let s = s.trim();
    let trimmed = s.trim_start_matches(':');
    if trimmed.is_empty() {
        return DEFAULT_NAMESPACE.to_string();
    }
    format!("::{}", trimmed)
}

/// Replace characters that aren't legal in a Hot namespace segment.
/// Hot identifiers allow ASCII letters, digits, `-`, `_`, `?`, `!`,
/// etc.; for directory names we conservatively map any non-alnum/`-`/`_`
/// char to `_` to avoid producing unparseable namespaces from
/// awkwardly-named dirs.
fn sanitize_ns_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        return "_".to_string();
    }
    if out
        .chars()
        .next()
        .map(|c| c.is_ascii_digit())
        .unwrap_or(false)
    {
        out.insert(0, '_');
    }
    out
}

/// Format the final stub source. The first line is always the marker so
/// `read_existing` can detect ownership without parsing.
fn format_stub(
    marker_line: &str,
    namespace: &str,
    fn_name: &str,
    frontmatter: &Val,
    body: &str,
) -> String {
    let mut out = String::with_capacity(256 + body.len());
    out.push_str(marker_line);
    out.push('\n');
    out.push_str("// Edit the source .skill.md and re-run `hot dev` / `hot build` to refresh.\n");
    out.push_str("// To take ownership, remove the marker line above; codegen will then leave\n");
    out.push_str("// this file alone (and `*.skill.md` will no longer overwrite it).\n");
    out.push('\n');
    out.push_str(namespace);
    out.push_str(" ns\n\n");
    out.push_str(fn_name);
    out.push_str(" ::ai::skill/Skill(");
    out.push_str(&format_skill_meta(frontmatter, fn_name, body));
    out.push_str(")\n");
    out
}

/// Build the `{description, when, body, ...}` map literal passed to
/// `::ai::skill/Skill`. Frontmatter keys flow through verbatim; we
/// always inject `body` from the markdown body and synthesize a `name`
/// when the frontmatter omits one.
fn format_skill_meta(frontmatter: &Val, fn_name: &str, body: &str) -> String {
    // Build an ordered list of (key, formatted-value) pairs so the
    // output is stable across runs (IndexMap preserves insertion order).
    let mut entries: IndexMap<String, String> = IndexMap::new();

    // Emit `name` first when the frontmatter omits one — keeps stub
    // scannable. If frontmatter provides `name`, we emit it in its
    // original position via the loop below.
    let fm_has_name = match frontmatter {
        Val::Map(m) => m.contains_key(&Val::from("name")),
        _ => false,
    };
    if !fm_has_name {
        entries.insert(
            "name".to_string(),
            format!("\"{}\"", escape_hot_str(fn_name)),
        );
    }

    if let Val::Map(m) = frontmatter {
        for (k, v) in m.iter() {
            let key = match k {
                Val::Str(s) => (**s).to_owned(),
                other => other.to_string(),
            };
            // `body` from the frontmatter is shadowed by the markdown
            // body below — frontmatter `body` is almost certainly a
            // user mistake. Skip silently.
            if key == "body" {
                continue;
            }
            entries.insert(key, format_meta_value(v, 1));
        }
    }

    // Always append `body` last so it's easy to spot at the bottom of
    // the meta block, where the multi-line content reads best.
    entries.insert("body".to_string(), format_body_str(body));

    let mut out = String::from("{\n");
    let total = entries.len();
    for (i, (k, v)) in entries.iter().enumerate() {
        out.push_str("    ");
        out.push_str(&format_meta_key(k));
        out.push_str(": ");
        out.push_str(v);
        if i + 1 < total {
            out.push(',');
        }
        out.push('\n');
    }
    out.push('}');
    out
}

/// Hot map keys can be bare identifiers when they're a valid Hot
/// identifier; otherwise they need quoting. We err on the side of
/// quoting any key with characters outside `[A-Za-z0-9_-?!]` to keep
/// the parser happy regardless of frontmatter creativity.
fn format_meta_key(k: &str) -> String {
    let safe = !k.is_empty()
        && k.chars().enumerate().all(|(i, c)| {
            c.is_ascii_alphanumeric()
                || c == '-'
                || c == '_'
                || c == '?'
                || c == '!'
                || (i > 0 && c == '+')
        })
        && !k
            .chars()
            .next()
            .map(|c| c.is_ascii_digit())
            .unwrap_or(false);
    if safe {
        k.to_string()
    } else {
        format!("\"{}\"", escape_hot_str(k))
    }
}

/// Format a frontmatter value as Hot source. Handles the cases YAML
/// frontmatter actually produces: Str, Int, Dec, Bool, Null, Vec, Map.
/// `indent` is the current nesting depth (0 = top-level meta map).
fn format_meta_value(val: &Val, indent: usize) -> String {
    match val {
        Val::Str(s) => format!("\"{}\"", escape_hot_str(s)),
        Val::Int(n) => n.to_string(),
        Val::Dec(d) => d.to_string(),
        Val::Bool(b) => b.to_string(),
        Val::Null => "null".to_string(),
        Val::Vec(items) => {
            if items.is_empty() {
                return "[]".to_string();
            }
            let all_simple = items.iter().all(is_simple_scalar);
            if all_simple {
                // Inline single-line vec for the common case
                // (e.g. `when: ["a", "b", "c"]`).
                let parts: Vec<String> = items
                    .iter()
                    .map(|v| format_meta_value(v, indent + 1))
                    .collect();
                format!("[{}]", parts.join(", "))
            } else {
                let pad = "    ".repeat(indent + 1);
                let close_pad = "    ".repeat(indent);
                let parts: Vec<String> = items
                    .iter()
                    .map(|v| format!("{}{}", pad, format_meta_value(v, indent + 1)))
                    .collect();
                format!("[\n{}\n{}]", parts.join(",\n"), close_pad)
            }
        }
        Val::Map(m) => {
            if m.is_empty() {
                return "{}".to_string();
            }
            let pad = "    ".repeat(indent + 1);
            let close_pad = "    ".repeat(indent);
            let parts: Vec<String> = m
                .iter()
                .map(|(k, v)| {
                    let key = match k {
                        Val::Str(s) => format_meta_key(s),
                        other => format!("\"{}\"", escape_hot_str(&other.to_string())),
                    };
                    format!("{}{}: {}", pad, key, format_meta_value(v, indent + 1))
                })
                .collect();
            format!("{{\n{}\n{}}}", parts.join(",\n"), close_pad)
        }
        // Fallback: stringify. Frontmatter shouldn't produce other
        // variants, but if it does we'd rather emit *something* than
        // panic in codegen.
        other => format!("\"{}\"", escape_hot_str(&other.to_string())),
    }
}

fn is_simple_scalar(v: &Val) -> bool {
    matches!(
        v,
        Val::Str(_) | Val::Int(_) | Val::Dec(_) | Val::Bool(_) | Val::Null
    )
}

/// Emit the markdown body as a Hot string. We prefer triple-quoted
/// block strings (no escapes, indent-aware, much nicer to read) when
/// the body doesn't contain a literal `"""` sequence; otherwise we
/// fall back to a regular escaped string so the codegen never produces
/// unparseable source.
fn format_body_str(body: &str) -> String {
    if body.is_empty() {
        return "\"\"".to_string();
    }
    if !body.contains("\"\"\"") {
        // Block string indented one level inside `{skill: {body: ... }}`,
        // which is `meta` (1 indent) -> `{` (1) -> `body:` value at
        // indent 2 (8 spaces). The closing `"""` sits at the same
        // indent so the lexer strips that prefix from each body line.
        const INDENT: &str = "        ";
        let mut out = String::with_capacity(body.len() + 32);
        out.push_str("\"\"\"\n");
        for line in body.split('\n') {
            if line.is_empty() {
                out.push('\n');
            } else {
                out.push_str(INDENT);
                out.push_str(line);
                out.push('\n');
            }
        }
        out.push_str(INDENT);
        out.push_str("\"\"\"");
        out
    } else {
        format!("\"{}\"", escape_hot_str(body))
    }
}

fn escape_hot_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => {
                out.push_str(&format!("\\u{{{:x}}}", c as u32));
            }
            c => out.push(c),
        }
    }
    out
}

fn blake3_short(bytes: &[u8]) -> String {
    let h = blake3::hash(bytes);
    let hex = h.to_hex();
    hex[..12].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn write(dir: &Path, rel: &str, content: &str) -> PathBuf {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, content).unwrap();
        p
    }

    fn fixture_md(name: &str) -> String {
        format!(
            "---\nname: {}\ndescription: Demo skill {}\nwhen:\n  - reply\n  - copy\n---\n# Heading\n\nLine one with \"quotes\" and a backslash \\.\n\nLine three.\n",
            name, name
        )
    }

    #[test]
    fn rel_path_md_to_hot() {
        let p = rel_path_from_md("skills/customer-tone.skill.md").unwrap();
        assert_eq!(p, PathBuf::from("skills/customer-tone.skill.hot"));
        let p2 = rel_path_from_md("a/b/c.skill.md").unwrap();
        assert_eq!(p2, PathBuf::from("a/b/c.skill.hot"));
    }

    #[test]
    fn rel_path_md_requires_suffix() {
        assert!(rel_path_from_md("foo.md").is_err());
    }

    #[test]
    fn ns_flat_and_nested() {
        let (ns, fnn) = ns_and_fn_from_rel(Path::new("customer-tone.skill.hot"), "::skills");
        assert_eq!(ns, "::skills");
        assert_eq!(fnn, "customer-tone");

        let (ns, fnn) =
            ns_and_fn_from_rel(Path::new("skills/refunds/escalate.skill.hot"), "::skills");
        assert_eq!(ns, "::skills::skills::refunds");
        assert_eq!(fnn, "escalate");
    }

    #[test]
    fn namespace_normalization() {
        assert_eq!(normalize_namespace("::myapp"), "::myapp");
        assert_eq!(normalize_namespace("myapp"), "::myapp");
        assert_eq!(normalize_namespace("  ::a::b "), "::a::b");
        assert_eq!(normalize_namespace(""), DEFAULT_NAMESPACE);
    }

    #[test]
    fn sanitize_segment_handles_oddities() {
        assert_eq!(sanitize_ns_segment("foo-bar"), "foo-bar");
        assert_eq!(sanitize_ns_segment("foo bar"), "foo_bar");
        assert_eq!(sanitize_ns_segment("123"), "_123");
        assert_eq!(sanitize_ns_segment(""), "_");
    }

    #[test]
    fn body_block_string_when_safe() {
        let body = "# H\n\nplain text\n";
        let s = format_body_str(body);
        assert!(s.starts_with("\"\"\""));
        assert!(s.ends_with("\"\"\""));
        // Indent prefix matches the `INDENT` constant used by formatter.
        assert!(s.contains("        # H"));
    }

    #[test]
    fn body_falls_back_when_triple_quote_present() {
        let body = "contains \"\"\" inside";
        let s = format_body_str(body);
        assert!(s.starts_with('"'));
        assert!(s.ends_with('"'));
        assert!(s.contains("\\\"\\\"\\\""));
    }

    #[test]
    fn meta_value_inline_vec() {
        let v = Val::Vec(vec![Val::from("reply"), Val::from("copy")]);
        assert_eq!(format_meta_value(&v, 1), "[\"reply\", \"copy\"]");
    }

    #[test]
    fn first_run_creates_stub() {
        let tmp = TempDir::new().unwrap();
        let res_root = tmp.path().join("resources");
        let out_dir = tmp.path().join("out");
        fs::create_dir_all(&res_root).unwrap();
        write(&res_root, "skills/foo.skill.md", &fixture_md("foo"));

        let opts = SkillCodegenOpts {
            enabled: true,
            resource_paths: None,
            out_dir: out_dir.clone(),
            root_namespace: "::skills".to_string(),
        };
        let report = run_skill_codegen(
            std::slice::from_ref(&res_root),
            true,
            &[],
            tmp.path().to_path_buf(),
            &opts,
        );
        assert_eq!(report.created.len(), 1, "{:?}", report);
        assert!(report.unchanged.is_empty());
        assert!(report.errors.is_empty());

        let stub_path = out_dir.join("skills/foo.skill.hot");
        assert!(stub_path.exists());
        let stub = fs::read_to_string(&stub_path).unwrap();
        assert!(stub.starts_with(MARKER_PREFIX));
        assert!(stub.contains("::skills::skills ns"));
        assert!(stub.contains("foo ::ai::skill/Skill({"));
        assert!(stub.contains("description: \"Demo skill foo\""));
        assert!(stub.contains("when: [\"reply\", \"copy\"]"));
        assert!(stub.contains("# Heading"));
    }

    #[test]
    fn second_run_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let res_root = tmp.path().join("resources");
        let out_dir = tmp.path().join("out");
        fs::create_dir_all(&res_root).unwrap();
        write(&res_root, "foo.skill.md", &fixture_md("foo"));

        let opts = SkillCodegenOpts {
            enabled: true,
            resource_paths: None,
            out_dir: out_dir.clone(),
            root_namespace: "::skills".to_string(),
        };
        let _ = run_skill_codegen(
            std::slice::from_ref(&res_root),
            true,
            &[],
            tmp.path().to_path_buf(),
            &opts,
        );
        let report = run_skill_codegen(
            std::slice::from_ref(&res_root),
            true,
            &[],
            tmp.path().to_path_buf(),
            &opts,
        );
        assert!(report.created.is_empty(), "{:?}", report);
        assert_eq!(report.unchanged.len(), 1, "{:?}", report);
    }

    #[test]
    fn change_to_md_triggers_update() {
        let tmp = TempDir::new().unwrap();
        let res_root = tmp.path().join("resources");
        let out_dir = tmp.path().join("out");
        fs::create_dir_all(&res_root).unwrap();
        let md_path = write(&res_root, "foo.skill.md", &fixture_md("foo"));

        let opts = SkillCodegenOpts {
            enabled: true,
            resource_paths: None,
            out_dir: out_dir.clone(),
            root_namespace: "::skills".to_string(),
        };
        let _ = run_skill_codegen(
            std::slice::from_ref(&res_root),
            true,
            &[],
            tmp.path().to_path_buf(),
            &opts,
        );

        // Modify the source.
        fs::write(&md_path, format!("{}\n\nMore body.\n", fixture_md("foo"))).unwrap();
        let report = run_skill_codegen(
            std::slice::from_ref(&res_root),
            true,
            &[],
            tmp.path().to_path_buf(),
            &opts,
        );
        assert!(report.created.is_empty());
        assert_eq!(report.updated.len(), 1, "{:?}", report);
    }

    #[test]
    fn user_owned_stub_is_never_overwritten() {
        let tmp = TempDir::new().unwrap();
        let res_root = tmp.path().join("resources");
        let out_dir = tmp.path().join("out");
        fs::create_dir_all(&res_root).unwrap();
        write(&res_root, "foo.skill.md", &fixture_md("foo"));

        let opts = SkillCodegenOpts {
            enabled: true,
            resource_paths: None,
            out_dir: out_dir.clone(),
            root_namespace: "::skills".to_string(),
        };
        let _ = run_skill_codegen(
            std::slice::from_ref(&res_root),
            true,
            &[],
            tmp.path().to_path_buf(),
            &opts,
        );

        // Replace the generated stub with a user-authored version (no marker).
        let stub_path = out_dir.join("foo.skill.hot");
        let custom =
            "::skills ns\n\nfoo\nmeta {skill: {description: \"hand-written\"}}\nfn () { {} }\n";
        fs::write(&stub_path, custom).unwrap();

        let report = run_skill_codegen(
            std::slice::from_ref(&res_root),
            true,
            &[],
            tmp.path().to_path_buf(),
            &opts,
        );
        assert!(report.updated.is_empty(), "{:?}", report);
        assert_eq!(report.user_owned.len(), 1, "{:?}", report);
        assert_eq!(fs::read_to_string(&stub_path).unwrap(), custom);
    }

    #[test]
    fn deleting_md_prunes_stub() {
        let tmp = TempDir::new().unwrap();
        let res_root = tmp.path().join("resources");
        let out_dir = tmp.path().join("out");
        fs::create_dir_all(&res_root).unwrap();
        let md = write(&res_root, "foo.skill.md", &fixture_md("foo"));

        let opts = SkillCodegenOpts {
            enabled: true,
            resource_paths: None,
            out_dir: out_dir.clone(),
            root_namespace: "::skills".to_string(),
        };
        let _ = run_skill_codegen(
            std::slice::from_ref(&res_root),
            true,
            &[],
            tmp.path().to_path_buf(),
            &opts,
        );
        let stub = out_dir.join("foo.skill.hot");
        assert!(stub.exists());

        fs::remove_file(&md).unwrap();
        let report = run_skill_codegen(
            std::slice::from_ref(&res_root),
            true,
            &[],
            tmp.path().to_path_buf(),
            &opts,
        );
        assert!(!stub.exists());
        assert_eq!(report.removed.len(), 1, "{:?}", report);
    }

    #[test]
    fn pruning_leaves_other_projects_stubs_alone() {
        // Two projects share the same out_dir but have disjoint
        // resource roots. Project A generates `a/foo.skill.hot`,
        // project B generates `b/bar.skill.hot`. Re-running project A
        // (which knows nothing about B's source) MUST NOT prune B's
        // stub — the marker source path is outside A's resource set.
        let tmp = TempDir::new().unwrap();
        let res_a = tmp.path().join("res-a");
        let res_b = tmp.path().join("res-b");
        let out_dir = tmp.path().join("out");
        fs::create_dir_all(&res_a).unwrap();
        fs::create_dir_all(&res_b).unwrap();

        write(&res_a, "a/foo.skill.md", &fixture_md("foo"));
        write(&res_b, "b/bar.skill.md", &fixture_md("bar"));

        let opts = SkillCodegenOpts {
            enabled: true,
            resource_paths: None,
            out_dir: out_dir.clone(),
            root_namespace: "::skills".to_string(),
        };

        // Project A run -> creates a/foo.skill.hot
        let report_a = run_skill_codegen(
            std::slice::from_ref(&res_a),
            true,
            &[],
            tmp.path().to_path_buf(),
            &opts,
        );
        assert_eq!(report_a.created.len(), 1, "A first run: {:?}", report_a);

        // Project B run -> creates b/bar.skill.hot AND must leave
        // a/foo.skill.hot alone.
        let report_b = run_skill_codegen(
            std::slice::from_ref(&res_b),
            true,
            &[],
            tmp.path().to_path_buf(),
            &opts,
        );
        assert_eq!(report_b.created.len(), 1, "B first run: {:?}", report_b);
        assert!(
            report_b.removed.is_empty(),
            "B must not prune A's stub: {:?}",
            report_b
        );
        assert!(out_dir.join("a/foo.skill.hot").exists());
        assert!(out_dir.join("b/bar.skill.hot").exists());

        // Re-run A: must prune nothing (B's stub stays put).
        let report_a2 = run_skill_codegen(
            std::slice::from_ref(&res_a),
            true,
            &[],
            tmp.path().to_path_buf(),
            &opts,
        );
        assert!(
            report_a2.removed.is_empty(),
            "A re-run must not prune B's stub: {:?}",
            report_a2
        );
        assert!(out_dir.join("b/bar.skill.hot").exists());
    }

    #[test]
    fn pruning_ignores_user_owned_stubs_in_out_dir() {
        let tmp = TempDir::new().unwrap();
        let res_root = tmp.path().join("resources");
        let out_dir = tmp.path().join("out");
        fs::create_dir_all(&res_root).unwrap();
        fs::create_dir_all(&out_dir).unwrap();

        // Lone hand-authored stub with no corresponding .skill.md.
        let custom = out_dir.join("orphan.skill.hot");
        fs::write(
            &custom,
            "::skills ns\n\norphan\nmeta {skill: {description: \"hand\"}}\nfn () { {} }\n",
        )
        .unwrap();

        let opts = SkillCodegenOpts {
            enabled: true,
            resource_paths: None,
            out_dir: out_dir.clone(),
            root_namespace: "::skills".to_string(),
        };
        let report = run_skill_codegen(
            std::slice::from_ref(&res_root),
            true,
            &[],
            tmp.path().to_path_buf(),
            &opts,
        );
        assert!(report.removed.is_empty(), "{:?}", report);
        assert!(custom.exists());
    }

    /// Smoke test: a generated stub must parse cleanly via the Hot parser.
    /// Runtime construction is intentionally a `Skill` value now, not a
    /// no-op function harvested by the compiler skill-spec registry.
    #[test]
    fn generated_stub_parses_as_skill_value() {
        use crate::lang::parser::Parser;

        let tmp = TempDir::new().unwrap();
        let res_root = tmp.path().join("resources");
        let out_dir = tmp.path().join("out");
        fs::create_dir_all(&res_root).unwrap();
        write(
            &res_root,
            "skills/customer-tone.skill.md",
            "---\nname: customer-tone\ndescription: Be warm.\nwhen:\n  - reply\n---\n# Tone\n\nUse a friendly voice.\n",
        );
        let opts = SkillCodegenOpts {
            enabled: true,
            resource_paths: None,
            out_dir: out_dir.clone(),
            root_namespace: "::skills".to_string(),
        };
        let report = run_skill_codegen(
            std::slice::from_ref(&res_root),
            true,
            &[],
            tmp.path().to_path_buf(),
            &opts,
        );
        assert!(
            report.errors.is_empty() && report.created.len() == 1,
            "{:?}",
            report
        );

        let stub_path = out_dir.join("skills/customer-tone.skill.hot");
        let source = fs::read_to_string(&stub_path).unwrap();
        assert!(source.contains("customer-tone ::ai::skill/Skill({"));

        let mut parser = Parser::new();
        parser
            .parse(&source)
            .unwrap_or_else(|e| panic!("stub failed to parse: {:?}\nSOURCE:\n{}", e, source));
    }

    #[test]
    fn disabled_returns_empty_report() {
        let tmp = TempDir::new().unwrap();
        let res_root = tmp.path().join("resources");
        let out_dir = tmp.path().join("out");
        fs::create_dir_all(&res_root).unwrap();
        write(&res_root, "foo.skill.md", &fixture_md("foo"));

        let opts = SkillCodegenOpts {
            enabled: false,
            resource_paths: None,
            out_dir: out_dir.clone(),
            root_namespace: "::skills".to_string(),
        };
        // Note: run_skill_codegen itself doesn't gate on `enabled` —
        // that's the responsibility of `run_skill_codegen_from_conf`.
        // This test pins down the gate-from-conf behavior.
        let mut conf_map = indexmap::IndexMap::new();
        let mut project_map = indexmap::IndexMap::new();
        let mut my_proj = indexmap::IndexMap::new();
        let mut skills_map = indexmap::IndexMap::new();
        let mut codegen_map = indexmap::IndexMap::new();
        codegen_map.insert(Val::from("enabled"), Val::Bool(false));
        skills_map.insert(Val::from("codegen"), Val::Map(Box::new(codegen_map)));
        my_proj.insert(Val::from("skills"), Val::Map(Box::new(skills_map)));
        project_map.insert(Val::from("default"), Val::Map(Box::new(my_proj.clone())));
        project_map.insert(Val::from("my-proj"), Val::Map(Box::new(my_proj)));
        conf_map.insert(Val::from("project"), Val::Map(Box::new(project_map)));
        let conf = Val::Map(Box::new(conf_map));

        let opts2 = opts_from_conf(&conf, "my-proj");
        assert!(!opts2.enabled);
        let _ = opts; // silence unused
    }

    #[test]
    fn opts_from_conf_reads_codegen_paths_override() {
        let mut conf_map = indexmap::IndexMap::new();
        let mut project_map = indexmap::IndexMap::new();
        let mut my_proj = indexmap::IndexMap::new();
        let mut skills_map = indexmap::IndexMap::new();
        let mut codegen_map = indexmap::IndexMap::new();

        codegen_map.insert(
            Val::from("paths"),
            Val::Vec(vec![Val::from("./skills"), Val::from("./agent/resources")]),
        );
        skills_map.insert(Val::from("codegen"), Val::Map(Box::new(codegen_map)));
        my_proj.insert(Val::from("skills"), Val::Map(Box::new(skills_map)));
        project_map.insert(Val::from("my-proj"), Val::Map(Box::new(my_proj)));
        conf_map.insert(Val::from("project"), Val::Map(Box::new(project_map)));
        let conf = Val::Map(Box::new(conf_map));

        let opts = opts_from_conf(&conf, "my-proj");
        assert_eq!(
            opts.resource_paths,
            Some(vec![
                PathBuf::from("./skills"),
                PathBuf::from("./agent/resources"),
            ])
        );
    }

    #[test]
    fn run_from_conf_errors_when_skill_sources_lack_hot_ai_dep() {
        let tmp = TempDir::new().unwrap();
        let res_root = tmp.path().join("resources");
        write(&res_root, "skills/foo.skill.md", &fixture_md("foo"));

        let mut resources = indexmap::IndexMap::new();
        resources.insert(
            Val::from("paths"),
            Val::Vec(vec![Val::from(res_root.to_string_lossy().to_string())]),
        );

        let mut project = indexmap::IndexMap::new();
        project.insert(Val::from("resources"), Val::Map(Box::new(resources)));
        project.insert(
            Val::from("deps"),
            Val::Map(Box::new(indexmap::IndexMap::new())),
        );

        let mut projects = indexmap::IndexMap::new();
        projects.insert(Val::from("my-proj"), Val::Map(Box::new(project)));

        let mut conf = indexmap::IndexMap::new();
        conf.insert(Val::from("project"), Val::Map(Box::new(projects)));

        let report = run_skill_codegen_from_conf(&Val::Map(Box::new(conf)), "my-proj", &[]);
        assert!(report.created.is_empty(), "{:?}", report);
        assert_eq!(report.errors.len(), 1, "{:?}", report);
        assert!(
            report.errors[0].1.contains("hot.dev/hot-ai"),
            "{:?}",
            report
        );
    }

    #[test]
    fn project_declares_hot_ai_accepts_full_package_name() {
        let mut deps = indexmap::IndexMap::new();
        deps.insert(
            Val::from("hot.dev/hot-ai"),
            Val::Map(Box::new(indexmap::IndexMap::new())),
        );

        let mut project = indexmap::IndexMap::new();
        project.insert(Val::from("deps"), Val::Map(Box::new(deps)));

        let mut projects = indexmap::IndexMap::new();
        projects.insert(Val::from("my-proj"), Val::Map(Box::new(project)));

        let mut conf = indexmap::IndexMap::new();
        conf.insert(Val::from("project"), Val::Map(Box::new(projects)));

        assert!(project_declares_hot_ai(
            &Val::Map(Box::new(conf)),
            "my-proj"
        ));
    }
}
