use crate::bundle::{BundleFile, calculate_bundle_hash, collect_hot_files_with_prefix};
use crate::db::{Build, DatabasePool, Project, get_default_org_and_user_ids};
use crate::hasher::HotHasher;
use crate::lang::compiler::{EventHandlers, ScheduledFunctions};
use ahash::AHashSet;
use async_trait::async_trait;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use uuid::Uuid;

use zip::write::FileOptions;
use zip::{CompressionMethod, ZipWriter};

/// Version compatibility check result
#[derive(Debug)]
pub struct VersionCheckResult {
    pub compatible: bool,
    pub build_engine_version: Option<String>,
    pub build_hot_std_version: Option<String>,
    pub server_version: String,
    pub warnings: Vec<String>,
}

/// Validate version compatibility between a build's manifest and the current server version
pub fn validate_build_version(
    bundle_map: &indexmap::IndexMap<crate::val::Val, crate::val::Val>,
) -> VersionCheckResult {
    let server_version = crate::version::current_runtime_version().to_string();

    // Extract versions from bundle map
    let build_engine_version = bundle_map
        .get(&crate::val::Val::from("engine_version"))
        .and_then(|v| match v {
            crate::val::Val::Str(s) => Some((**s).to_owned()),
            _ => None,
        });

    let build_hot_std_version = bundle_map
        .get(&crate::val::Val::from("hot_std_version"))
        .and_then(|v| match v {
            crate::val::Val::Str(s) => Some((**s).to_owned()),
            _ => None,
        });

    let compatibility = crate::version::check_runtime_version_compatibility(
        build_engine_version.as_deref(),
        build_hot_std_version.as_deref(),
        &server_version,
    );
    let mut warnings = Vec::new();
    if let Some(warning) = compatibility.warning {
        warnings.push(warning.message);
    }
    if let Some(error) = &compatibility.error {
        warnings.push(error.clone());
    }

    VersionCheckResult {
        compatible: compatibility.compatible,
        build_engine_version,
        build_hot_std_version,
        server_version,
        warnings,
    }
}

#[derive(Debug, Clone)]
pub struct BuildResult {
    pub build: Build,
    pub zip_path: PathBuf,
}

/// Parameters for identifying the user, environment, and organization for build creation
#[derive(Debug, Clone)]
pub struct BuildContext {
    pub user_id: Option<Uuid>,
    pub env_id: Option<Uuid>,
    pub org_id: Option<Uuid>,
    pub project_id: Option<Uuid>,
    pub project_name: String,
    pub src_paths: Vec<String>,

    pub test_paths: Vec<String>,

    /// Resource roots to bundle (typically `hot.project.<x>.resources.paths`
    /// plus any `--resource.path` CLI flags). Empty = no resources bundled.
    pub resource_paths: Vec<PathBuf>,

    /// Whether to honor `.gitignore` when walking `resource_paths`. Mirrors
    /// `hot.project.<x>.resources.respect-gitignore` (default `true`).
    pub respect_gitignore: bool,

    /// Extra exclude patterns (gitignore syntax) applied during resource
    /// discovery. Sourced from `hot.project.<x>.ignore.excludes` +
    /// `hot.project.<x>.resources.excludes` so the bundled set matches the
    /// live `::hot::resource/*` view in `hot dev`.
    pub resource_excludes: Vec<String>,

    /// Build-time secret-shape scanner options. Sourced from
    /// `hot.build.allow-secret-shape` (per-file allowlist) and the
    /// `--allow-secret-shape` CLI flag (kill-switch for the entire scan).
    pub secret_scan_opts: crate::secret_scan::SecretScanOpts,
}

impl BuildContext {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        user_id: Option<Uuid>,
        env_id: Option<Uuid>,
        org_id: Option<Uuid>,
        project_id: Option<Uuid>,
        project_name: String,
        src_paths: Vec<String>,
        test_paths: Vec<String>,
    ) -> Self {
        Self {
            user_id,
            env_id,
            org_id,
            project_id,
            project_name,
            src_paths,
            test_paths,
            resource_paths: Vec::new(),
            respect_gitignore: true,
            resource_excludes: Vec::new(),
            secret_scan_opts: crate::secret_scan::SecretScanOpts::default(),
        }
    }

    /// Builder-style setter for `resource_paths`.
    pub fn with_resources(
        mut self,
        paths: Vec<PathBuf>,
        respect_gitignore: bool,
        excludes: Vec<String>,
    ) -> Self {
        self.resource_paths = paths;
        self.respect_gitignore = respect_gitignore;
        self.resource_excludes = excludes;
        self
    }

    /// Builder-style setter for the secret-shape scanner options.
    pub fn with_secret_scan_opts(mut self, opts: crate::secret_scan::SecretScanOpts) -> Self {
        self.secret_scan_opts = opts;
        self
    }
}

/// Threshold (as a fraction of the configured max) at which a build emits
/// a "bundle is large" warning. 0.8 = 80%.
const BUNDLE_SIZE_WARN_FRACTION: f64 = 0.8;

/// Default ceiling (100 MB) the Hot Cloud upload endpoint enforces when
/// its own `hot.build.file.max-bytes` is unset. Kept here so the client
/// can pre-flight the same limit even when the user hasn't set anything
/// in their local `hot.hot`.
///
/// Must stay in sync with the default in
/// `crates/hot_api/src/handlers/builds.rs` (search for `100 * 1024 *
/// 1024`). If we ever expose the live ceiling via an API endpoint, the
/// client should prefer that over this constant.
pub const DEFAULT_REMOTE_BUILD_MAX_BYTES: i64 = 100 * 1024 * 1024;

/// Source of the size limit being enforced — used only for diagnostic
/// messages so the user can tell whether they hit a local override or
/// the well-known remote default.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LimitSource {
    /// User set `hot.build.file.max-bytes` in their project config.
    LocalConf,
    /// Falling back to the documented remote API default.
    RemoteDefault,
}

/// Pre-upload size check for a built bundle.
///
/// The ceiling is resolved as:
///
/// 1. `hot.build.file.max-bytes` from `conf` if it's a positive integer
///    (treated as an explicit override — usually used to set a *lower*
///    limit for local hygiene).
/// 2. Otherwise [`DEFAULT_REMOTE_BUILD_MAX_BYTES`] (100 MB), which is
///    the documented hard ceiling enforced by the Hot Cloud upload
///    endpoint. We pre-flight against this even without local config so
///    the user doesn't waste time uploading a bundle that the API will
///    immediately 413.
///
/// If the on-disk zip is over the limit, returns an error listing the
/// **top-10 largest entries by uncompressed size**. If it's at
/// >= [`BUNDLE_SIZE_WARN_FRACTION`], emits a warning with the same
/// > top-10 list but still succeeds.
///
/// `files` and `resources` are the pre-zip contents; their byte sizes
/// are compared to give a useful diagnostic even when zstd compression
/// makes the zip itself smaller.
pub fn check_bundle_size(
    conf: &crate::val::Val,
    zip_path: &Path,
    files: &[BundleFile],
    resources: &[crate::bundle::BundleResource],
) -> Result<(), String> {
    let (limit, source) = resolve_bundle_size_limit(conf);

    let zip_size = match fs::metadata(zip_path) {
        Ok(m) => m.len() as i64,
        Err(e) => {
            tracing::warn!(
                "bundle size check: failed to stat {}: {}",
                zip_path.display(),
                e
            );
            return Ok(());
        }
    };

    if zip_size > limit {
        let top = top_largest_entries(files, resources, 10);
        return Err(format!(
            "bundle is {} ({} bytes) which exceeds the {} of {}.\n\
             Top {} largest entries (uncompressed):\n{}\n\
             {}",
            human_bytes(zip_size as u64),
            zip_size,
            describe_limit_source(source),
            human_bytes(limit as u64),
            top.len(),
            format_top_entries(&top),
            remediation_hint(source),
        ));
    }

    if (zip_size as f64) >= (limit as f64 * BUNDLE_SIZE_WARN_FRACTION) {
        let top = top_largest_entries(files, resources, 10);
        let pct = (zip_size as f64 / limit as f64) * 100.0;
        let msg = format!(
            "⚠️  bundle is {} ({:.0}% of {} = {}). Top largest entries (uncompressed):\n{}",
            human_bytes(zip_size as u64),
            pct,
            describe_limit_source(source),
            human_bytes(limit as u64),
            format_top_entries(&top),
        );
        eprintln!("{}", msg);
        tracing::warn!("{}", msg);
    }

    Ok(())
}

/// Resolve the active size ceiling and where it came from.
fn resolve_bundle_size_limit(conf: &crate::val::Val) -> (i64, LimitSource) {
    if let Some(local) = build_file_max_bytes_conf(conf) {
        return (local, LimitSource::LocalConf);
    }
    (DEFAULT_REMOTE_BUILD_MAX_BYTES, LimitSource::RemoteDefault)
}

fn describe_limit_source(source: LimitSource) -> &'static str {
    match source {
        LimitSource::LocalConf => "local override `hot.build.file.max-bytes`",
        LimitSource::RemoteDefault => "Hot Cloud upload limit (default `hot.build.file.max-bytes`)",
    }
}

fn remediation_hint(source: LimitSource) -> &'static str {
    match source {
        LimitSource::LocalConf => {
            "Reduce bundle size by removing unused dependencies or large resources, \
             or raise `hot.build.file.max-bytes` for this project (capped by your plan)."
        }
        LimitSource::RemoteDefault => {
            "The Hot Cloud upload endpoint will reject bundles over this size with 413. \
             Trim the bundle by removing unused dependencies or splitting large resources. \
             Self-hosted deployments can raise the server-side `hot.build.file.max-bytes`."
        }
    }
}

/// Read `hot.build.file.max-bytes` from conf. Returns `None` if missing or
/// non-positive (treated as "use the remote default").
fn build_file_max_bytes_conf(conf: &crate::val::Val) -> Option<i64> {
    let v = conf.get("build")?.get("file")?.get("max-bytes")?;
    match v {
        crate::val::Val::Int(i) if i > 0 => Some(i),
        _ => None,
    }
}

#[derive(Debug, Clone)]
struct EntrySize {
    path: String,
    size: u64,
    kind: &'static str,
}

fn top_largest_entries(
    files: &[BundleFile],
    resources: &[crate::bundle::BundleResource],
    n: usize,
) -> Vec<EntrySize> {
    let mut all: Vec<EntrySize> = Vec::with_capacity(files.len() + resources.len());
    for f in files {
        all.push(EntrySize {
            path: f.relative_path.clone(),
            size: f.size,
            kind: "src",
        });
    }
    for r in resources {
        all.push(EntrySize {
            path: r.rel_path.clone(),
            size: r.size,
            kind: "resource",
        });
    }
    all.sort_by_key(|e| std::cmp::Reverse(e.size));
    all.truncate(n);
    all
}

fn format_top_entries(entries: &[EntrySize]) -> String {
    let mut out = String::new();
    for (i, e) in entries.iter().enumerate() {
        out.push_str(&format!(
            "  {:>2}. [{}] {:>9}  {}\n",
            i + 1,
            e.kind,
            human_bytes(e.size),
            e.path
        ));
    }
    out
}

fn human_bytes(n: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut v = n as f64;
    let mut idx = 0;
    while v >= 1024.0 && idx < UNITS.len() - 1 {
        v /= 1024.0;
        idx += 1;
    }
    if idx == 0 {
        format!("{} {}", n, UNITS[idx])
    } else {
        format!("{:.1} {}", v, UNITS[idx])
    }
}

/// Calculate the build Blake3 hash from all file hashes
pub fn calculate_build_hash(files: &[BundleFile]) -> String {
    let mut hasher = HotHasher::new();

    // Hash the concatenation of all file hashes in sorted order
    for file in files {
        hasher.update(file.hash.as_bytes());
    }

    hasher.finalize()
}

/// Collect the files and resources that would be bundled for a project build.
///
/// This is intentionally shared by bundle builds and live-build preflights so
/// safety checks run over the same source/dependency/resource surface.
pub fn collect_build_inputs(
    build_context: &BuildContext,
    conf: Option<&crate::val::Val>,
) -> Result<(Vec<BundleFile>, Vec<crate::bundle::BundleResource>), String> {
    let mut all_files = Vec::new();
    let mut resource_paths = build_context.resource_paths.clone();

    // Collect src files with "hot/src" prefix
    if !build_context.src_paths.is_empty() {
        let src_files = collect_hot_files_with_prefix(&build_context.src_paths, "hot/src")?;
        all_files.extend(src_files);
    }

    // Collect resolved dependency files with "hot/pkg" prefix. Dependencies
    // must be bundled so the worker can find them, except hot-std which is tied
    // to the Hot runtime version and should always come from the system
    // installation.
    if let Some(c) = conf {
        let resolved_deps =
            crate::project::get_resolved_project_dependencies(c, &build_context.project_name)
                .unwrap_or_default();
        for dep in resolved_deps {
            if dep.name == "hot-std" || dep.name == "hot.dev/hot-std" {
                tracing::debug!(
                    "Skipping hot-std from bundle (will use system hot-std at runtime)"
                );
                continue;
            }
            let prefix = format!("hot/pkg/{}", dep.name);
            let dep_files =
                crate::bundle::collect_package_source_files_from_root(&dep.resolved_path, &prefix)?;
            tracing::debug!(
                "Including dependency '{}' with {} source files",
                dep.name,
                dep_files.len()
            );
            all_files.extend(dep_files);

            let dep_resource_roots =
                crate::bundle::collect_package_resource_roots_from_root(&dep.resolved_path)?;
            if !dep_resource_roots.is_empty() {
                tracing::debug!(
                    "Including dependency '{}' with {} resource root(s)",
                    dep.name,
                    dep_resource_roots.len()
                );
                resource_paths.extend(dep_resource_roots);
            }
        }
    }

    // Collect bundled resources (`hot.project.<x>.resources.paths`, package
    // `resource-paths`, plus any CLI-provided extras). Empty paths means "no
    // resources" — the bundle will simply omit the `resources/` directory.
    let resources = if resource_paths.is_empty() {
        Vec::new()
    } else {
        crate::bundle::collect_resource_files(
            &resource_paths,
            build_context.respect_gitignore,
            &build_context.resource_excludes,
        )?
    };

    Ok((all_files, resources))
}

/// Run the build-time secret-shape scan over collected bundle inputs.
pub fn scan_build_inputs(
    build_context: &BuildContext,
    conf: Option<&crate::val::Val>,
) -> Result<(), String> {
    let (files, resources) = collect_build_inputs(build_context, conf)?;
    scan_collected_build_inputs(&files, &resources, &build_context.secret_scan_opts)
}

fn scan_collected_build_inputs(
    files: &[BundleFile],
    resources: &[crate::bundle::BundleResource],
    opts: &crate::secret_scan::SecretScanOpts,
) -> Result<(), String> {
    let findings = crate::secret_scan::scan_bundle(files, resources, opts)
        .map_err(|e| format!("secret-shape scan failed: {}", e))?;
    if findings.is_empty() {
        Ok(())
    } else {
        Err(crate::secret_scan::format_findings(&findings))
    }
}

/// Create a build from the given files with database integration.
///
/// Returns: `(files, resources, build_hash, event_handlers, scheduled_functions,
/// mcp_tools, webhooks, agents, workflows, build_docs, ctx_requirements,
/// box_requirements, send_targets)`. The `build_hash` covers source files **and** resource hashes
/// so a resource-only change still produces a new bundle hash.
pub async fn create_build(
    _db: &DatabasePool,
    build_context: BuildContext,
    conf: Option<&crate::val::Val>,
) -> Result<
    (
        Vec<BundleFile>,
        Vec<crate::bundle::BundleResource>,
        String,
        EventHandlers,
        ScheduledFunctions,
        crate::lang::compiler::McpTools,
        crate::lang::compiler::Webhooks,
        crate::lang::compiler::AgentDefs,
        crate::lang::compiler::WorkflowDefs,
        crate::pkg::docs::BuildDocs,
        crate::lang::compiler::ctx_checker::ProgramCtxRequirements, // ctx_requirements
        crate::lang::compiler::box_checker::ProgramBoxRequirements,
        crate::lang::compiler::SendTargets,
    ),
    String,
> {
    let (all_files, resources) = collect_build_inputs(&build_context, conf)?;

    // Create a temporary empty file for validation (same approach as CLI compile command)
    let temp_file = std::env::temp_dir().join("build_create_validation.hot");
    std::fs::write(&temp_file, "// Build create validation file")
        .map_err(|e| format!("Failed to create temporary validation file: {}", e))?;

    let temp_file_str = temp_file.to_string_lossy().to_string();
    let validation_result = crate::lang::engine::Engine::run_file_pipeline_with_deps(
        &temp_file_str,
        &build_context.src_paths,
        &[],  // No test paths for builds
        conf, // Use the configuration passed from CLI
        Some(&build_context.project_name),
        false,
    );

    // Clean up temporary file
    let _ = std::fs::remove_file(&temp_file);

    match validation_result {
        Ok(_) => {
            tracing::debug!("Compilation validation successful");
        }
        Err(e) => {
            return Err(format!("Compilation validation failed: {}", e));
        }
    }

    // NOTE: We no longer bundle bytecode cache because:
    // 1. hot-std is not bundled (it's tied to the runtime version)
    // 2. The cache key includes hot-std hashes, so it won't match at runtime
    // 3. The worker will generate its own cache on first run with correct hot-std
    // This is cleaner and avoids version mismatch issues.

    // Event handler, scheduled function, MCP tool, webhook, agent, and ctx requirements extraction
    let extracted = crate::lang::engine::Engine::extract_handlers_and_scheduled_functions(
        &build_context.src_paths,
        Some(&build_context.project_name),
        conf,
        false,
    )
    .map_err(|e| format!("Event handler extraction failed: {}", e))?;

    // Build hash covers source files *and* resources so a content-only
    // change to a bundled prompt or asset still produces a new bundle hash
    // (and therefore triggers a redeploy).
    let build_hash = crate::bundle::calculate_bundle_hash_with_resources(&all_files, &resources);

    // Secret-shape scan over the *bundled* contents (sources + resources).
    // Run after collection but before zipping so a hit fails the build
    // without producing a throwaway artifact. Allowlist patterns and the
    // global kill-switch come from `BuildContext::secret_scan_opts`.
    scan_collected_build_inputs(&all_files, &resources, &build_context.secret_scan_opts)?;

    // Generate build documentation (project + dependencies)
    let build_docs = {
        // Get resolved dependencies for doc generation
        let resolved_deps = if let Some(c) = conf {
            crate::project::get_resolved_project_dependencies(c, &build_context.project_name)
                .unwrap_or_default()
        } else {
            vec![]
        };

        crate::pkg::docs::generate_build_docs(
            &build_context.project_name,
            &build_context.src_paths,
            &resolved_deps,
        )
        .unwrap_or_else(|e| {
            tracing::warn!("Failed to generate build docs: {}", e);
            crate::pkg::docs::BuildDocs::new()
        })
    };

    Ok((
        all_files,
        resources,
        build_hash,
        extracted.event_handlers,
        extracted.scheduled_functions,
        extracted.mcp_tools,
        extracted.webhooks,
        extracted.agents,
        extracted.workflows,
        build_docs,
        extracted.ctx_requirements,
        extracted.box_requirements,
        extracted.send_targets,
    ))
}

/// Write the build to a zip file with zstd compression
#[allow(clippy::too_many_arguments)]
pub fn write_build_zip(
    build_id: &Uuid,
    project_name: &str,
    build_hash: &str,
    files: &[BundleFile],
    resources: &[crate::bundle::BundleResource],
    build_dir: &Path,
    event_handlers: &EventHandlers,
    scheduled_functions: &ScheduledFunctions,
    mcp_tools: &crate::lang::compiler::McpTools,
    webhooks: &crate::lang::compiler::Webhooks,
    agents: &crate::lang::compiler::AgentDefs,
    workflows: &crate::lang::compiler::WorkflowDefs,
    send_targets: &crate::lang::compiler::SendTargets,
    storage_path: &str,
    build_docs: &crate::pkg::docs::BuildDocs,
    ctx_requirements: Option<&crate::lang::compiler::ctx_checker::ProgramCtxRequirements>,
    box_requirements: Option<&crate::lang::compiler::box_checker::ProgramBoxRequirements>,
) -> Result<PathBuf, String> {
    // Construct full zip path from storage_path (format: org/env/project/build-id.zip)
    let zip_path = build_dir.join(storage_path);

    // Ensure parent directory exists (create org/env/project/ structure)
    if let Some(parent) = zip_path.parent()
        && let Err(e) = fs::create_dir_all(parent)
    {
        return Err(format!("Failed to create build directory structure: {}", e));
    }

    // Create zip file
    let zip_file = match fs::File::create(&zip_path) {
        Ok(file) => file,
        Err(e) => return Err(format!("Failed to create zip file: {}", e)),
    };

    let mut zip = ZipWriter::new(zip_file);

    // Set up file options with zstd compression
    let options = FileOptions::<()>::default().compression_method(CompressionMethod::Zstd);

    // Create manifest with build information (using the actual build_id)
    let build_id_str = build_id.to_string();
    let manifest = crate::bundle::create_manifest(
        project_name,
        &build_id_str,
        files,
        build_hash,
        event_handlers,
        scheduled_functions,
        mcp_tools,
        webhooks,
        agents,
        workflows,
        send_targets,
        ctx_requirements,
        box_requirements,
        resources,
    );
    let manifest_content = manifest.pretty_print();

    // Add manifest.hot to the zip
    if let Err(e) = zip.start_file("manifest.hot", options) {
        return Err(format!("Failed to start manifest file in zip: {}", e));
    }
    if let Err(e) = zip.write_all(manifest_content.as_bytes()) {
        return Err(format!("Failed to write manifest to zip: {}", e));
    }

    // Add all build files to the zip
    for file in files {
        if let Err(e) = zip.start_file(&file.relative_path, options) {
            return Err(format!(
                "Failed to start file {} in zip: {}",
                file.relative_path, e
            ));
        }
        if let Err(e) = zip.write_all(&file.content) {
            return Err(format!(
                "Failed to write file {} to zip: {}",
                file.relative_path, e
            ));
        }
    }

    // Add resource files under the standard `resources/<rel-path>` prefix.
    // The runtime resource registry uses `<rel-path>` as the lookup key, so
    // `::hot::resource/load("prompts/system.md")` resolves to the extracted
    // bundle path transparently.
    for r in resources {
        let zip_path_str = format!("{}/{}", crate::bundle::RESOURCE_BUNDLE_PREFIX, r.rel_path);
        if let Err(e) = zip.start_file(&zip_path_str, options) {
            return Err(format!(
                "Failed to start resource file {} in zip: {}",
                zip_path_str, e
            ));
        }
        if let Err(e) = zip.write_all(&r.content) {
            return Err(format!(
                "Failed to write resource file {} to zip: {}",
                zip_path_str, e
            ));
        }
    }

    // Add documentation to the zip
    // Structure: docs/project/docs.json, docs/deps/{pkg}/docs.json
    if let Some(project_docs) = &build_docs.project {
        let project_docs_json = serde_json::to_string_pretty(project_docs)
            .map_err(|e| format!("Failed to serialize project docs: {}", e))?;

        if let Err(e) = zip.start_file("docs/project/docs.json", options) {
            return Err(format!("Failed to start project docs file in zip: {}", e));
        }
        if let Err(e) = zip.write_all(project_docs_json.as_bytes()) {
            return Err(format!("Failed to write project docs to zip: {}", e));
        }
        tracing::debug!(
            "Added project docs ({} namespaces)",
            project_docs.namespaces.len()
        );
    }

    for (pkg_name, pkg_docs) in &build_docs.deps {
        let pkg_docs_json = serde_json::to_string_pretty(pkg_docs)
            .map_err(|e| format!("Failed to serialize {} docs: {}", pkg_name, e))?;

        let docs_path = format!("docs/deps/{}/docs.json", pkg_name);
        if let Err(e) = zip.start_file(&docs_path, options) {
            return Err(format!(
                "Failed to start {} docs file in zip: {}",
                pkg_name, e
            ));
        }
        if let Err(e) = zip.write_all(pkg_docs_json.as_bytes()) {
            return Err(format!("Failed to write {} docs to zip: {}", pkg_name, e));
        }
        tracing::debug!(
            "Added {} docs ({} namespaces)",
            pkg_name,
            pkg_docs.namespaces.len()
        );
    }

    // Finish the zip file
    if let Err(e) = zip.finish() {
        return Err(format!("Failed to finish zip file: {}", e));
    }

    Ok(zip_path)
}

/// Main build creation function with database integration
pub async fn build_create(
    db: &DatabasePool,
    build_dir: Option<&str>,
    build_context: BuildContext,
    conf: Option<&crate::val::Val>,
) -> Result<BuildResult, String> {
    let build_dir = build_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".hot/build"));

    tracing::debug!(
        "Creating build for project '{}'...",
        build_context.project_name
    );
    tracing::debug!("  Source paths: {:?}", build_context.src_paths);
    tracing::debug!("  Package paths: [] (using dependency system)");
    tracing::debug!("  Build directory: {}", build_dir.display());

    // Create the build (includes project creation/retrieval and compilation)
    let (
        files,
        resources,
        build_hash,
        event_handlers,
        scheduled_functions,
        mcp_tools,
        webhooks,
        agents,
        workflows,
        build_docs,
        ctx_requirements,
        box_requirements,
        send_targets,
    ) = create_build(db, build_context.clone(), conf).await?;

    tracing::debug!("  Found {} files", files.len());
    tracing::debug!("  Build Hash: {}", build_hash);
    tracing::debug!(
        "  Docs: project={}, deps={}",
        build_docs.project.is_some(),
        build_docs.deps.len()
    );
    tracing::debug!("  Ctx requirements: {:?}", ctx_requirements);

    // Resolve user_id and env_id - use provided values or fall back to defaults
    let resolved_user_id = match build_context.user_id {
        Some(user) => user,
        None => {
            // Fall back to default if not provided
            let (_, default_user_id) = get_default_org_and_user_ids(db)
                .await
                .map_err(|e| format!("Failed to get default user: {}", e))?;
            default_user_id
        }
    };

    let resolved_env_id = build_context
        .env_id
        .ok_or("Environment ID is required for build creation")?;

    // Get environment to obtain org_id
    let env = crate::db::Env::get_env(db, &resolved_env_id)
        .await
        .map_err(|e| format!("Failed to get environment: {}", e))?;

    // Create or get the project record first
    let project_id = build_context.project_id.unwrap_or_else(Uuid::now_v7);
    let project = crate::db::Project::insert_or_get_project(
        db,
        &project_id,
        &resolved_env_id,
        &build_context.project_name,
        &resolved_user_id,
    )
    .await
    .map_err(|e| format!("Failed to insert project: {}", e))?;

    // Use the returned project's ID (handles case where project already existed)
    let project_id = project.project_id;

    // Generate UUID v7 for build ID
    let build_id = Uuid::now_v7();
    tracing::debug!("  Build ID: {}", build_id);

    // Construct hierarchical storage path: org/env/build-id.hot.zip (UUIDs without dashes)
    let storage_path = format!(
        "{}/{}/{}.hot.zip",
        env.org_id.simple(),
        resolved_env_id.simple(),
        build_id.simple()
    );

    // Write the zip file
    let zip_path = write_build_zip(
        &build_id,
        &build_context.project_name,
        &build_hash,
        &files,
        &resources,
        &build_dir,
        &event_handlers,
        &scheduled_functions,
        &mcp_tools,
        &webhooks,
        &agents,
        &workflows,
        &send_targets,
        &storage_path,
        &build_docs,
        Some(&ctx_requirements),
        Some(&box_requirements),
    )?;

    // Pre-upload size check against `hot.build.file.max-bytes`. Fails the
    // build if the zip is over the limit, warns at >= 80%, and either way
    // surfaces the top-10 largest entries (by uncompressed size) so the
    // user knows where the weight is coming from. Running this *after*
    // write_build_zip means we measure exactly what would be uploaded.
    if let Some(c) = conf {
        check_bundle_size(c, &zip_path, &files, &resources)?;
    }

    // Get file size
    let file_size = match fs::metadata(&zip_path) {
        Ok(metadata) => metadata.len() as i32,
        Err(e) => {
            tracing::debug!("Warning: Failed to get file size: {}", e);
            0
        }
    };

    // Insert build record into database with storage path
    Build::insert_build_with_storage(
        db,
        &build_id,
        &project_id,
        &build_hash,
        file_size,
        Build::BUILD_TYPE_BUNDLE,
        &resolved_user_id,
        Some(&storage_path),
        Some("local"), // storage backend
    )
    .await
    .map_err(|e| format!("Failed to insert build record: {}", e))?;

    // Insert event handlers for this build
    if !event_handlers.is_empty() {
        crate::db::event_handler::EventHandler::insert_event_handlers_for_build(
            db,
            &build_id,
            &event_handlers,
            &send_targets,
        )
        .await
        .map_err(|e| format!("Failed to insert event handlers: {}", e))?;

        println!(
            "  Inserted {} event handler(s)",
            event_handlers.values().map(|v| v.len()).sum::<usize>()
        );
    }

    // Insert scheduled functions for this build (upsert will reactivate existing ones)
    if !scheduled_functions.is_empty() {
        crate::db::schedule::Schedule::insert_schedules_for_build(
            db,
            &build_id,
            &scheduled_functions,
            &send_targets,
        )
        .await
        .map_err(|e| format!("Failed to insert scheduled functions: {}", e))?;

        println!(
            "  Inserted {} scheduled function(s)",
            scheduled_functions.values().map(|v| v.len()).sum::<usize>()
        );
    }

    // Insert MCP tools for this build
    if !mcp_tools.is_empty() {
        crate::db::mcp_tool::McpTool::insert_mcp_tools_for_build(db, &build_id, &mcp_tools)
            .await
            .map_err(|e| format!("Failed to insert MCP tools: {}", e))?;

        println!(
            "  Inserted {} MCP tool(s)",
            mcp_tools.values().map(|v| v.len()).sum::<usize>()
        );
    }

    // Insert webhooks for this build
    if !webhooks.is_empty() {
        crate::db::Webhook::insert_webhooks_for_build(db, &build_id, &webhooks, &send_targets)
            .await
            .map_err(|e| format!("Failed to insert webhooks: {}", e))?;

        println!(
            "  Inserted {} webhook(s)",
            webhooks.values().map(|v| v.len()).sum::<usize>()
        );
    }

    // Insert agents for this build
    if !agents.is_empty() {
        crate::db::Agent::insert_agents_for_build(db, &build_id, &resolved_env_id, &agents)
            .await
            .map_err(|e| format!("Failed to insert agents: {}", e))?;

        println!("  Inserted {} agent(s)", agents.len());
    }

    // Insert named workflows for this build
    if !workflows.is_empty() {
        crate::db::Workflow::insert_workflows_for_build(
            db,
            &build_id,
            &resolved_env_id,
            &workflows,
        )
        .await
        .map_err(|e| format!("Failed to insert workflows: {}", e))?;

        println!("  Inserted {} workflow(s)", workflows.len());
    }

    // Get the created build record
    let build = Build::get_build(db, &build_id)
        .await
        .map_err(|e| format!("Failed to get build record: {}", e))?;

    println!("  Created build: {}", zip_path.display());
    println!("  Build ID: {}", build.build_id);

    Ok(BuildResult { build, zip_path })
}

/// Create a live build from source and package files (in-memory compilation only)
pub async fn create_live_build(
    db: &DatabasePool,
    build_context: BuildContext,
    _enable_cache: bool,
    _cache_format: Option<String>,
    _load_ctx_hot: bool,
    color: bool,
) -> Result<
    (
        Build,
        String,
        crate::lang::compiler::EventHandlers,
        crate::lang::compiler::ScheduledFunctions,
        crate::lang::compiler::McpTools,
        crate::lang::compiler::Webhooks,
        crate::lang::compiler::AgentDefs,
        crate::lang::compiler::WorkflowDefs,
        crate::lang::compiler::SendTargets,
    ),
    String,
> {
    // Collect src files
    let src_files = collect_hot_files_with_prefix(&build_context.src_paths, "hot/src")?;

    // Collect pkg files
    let pkg_files: Vec<BundleFile> = vec![]; // Package files handled by dependency system

    // Combine all files
    let mut all_files = Vec::new();
    all_files.extend(src_files);
    all_files.extend(pkg_files);

    // Use Hot for compilation and event handler extraction
    // Note: For live builds, we pass None for conf since ctx requirements validation
    // happens at deploy time, not at live build creation
    let extracted = crate::lang::engine::Engine::extract_handlers_and_scheduled_functions(
        &build_context.src_paths,
        Some(&build_context.project_name),
        None, // No conf for live builds
        color,
    )
    .map_err(|e| format!("Compilation failed: {}", e))?;

    // Calculate hash of all files combined
    let build_hash = calculate_bundle_hash(&all_files);

    // Get environment context
    let env_id = build_context
        .env_id
        .ok_or("Environment ID is required for live builds")?;

    // Resolve user_id - use provided value or fall back to default
    let resolved_user_id = match build_context.user_id {
        Some(user) => user,
        None => {
            // Fall back to default if not provided
            let (_, default_user_id) = crate::db::get_default_org_and_user_ids(db)
                .await
                .map_err(|e| format!("Failed to get default user: {}", e))?;
            default_user_id
        }
    };

    // Create or get the project record first
    let project_id = build_context.project_id.unwrap_or_else(Uuid::now_v7);
    let project = crate::db::Project::insert_or_get_project(
        db,
        &project_id,
        &env_id,
        &build_context.project_name,
        &resolved_user_id,
    )
    .await
    .map_err(|e| format!("Failed to insert project: {}", e))?;

    // Calculate live build size (sum of all file sizes)
    let live_build_size = all_files.iter().map(|f| f.size).sum::<u64>() as i32;

    // Create or update live build
    let build = crate::db::Build::insert_or_update_live_build(
        db,
        &project.project_id,
        &build_hash,
        live_build_size,
        &resolved_user_id,
    )
    .await
    .map_err(|e| format!("Failed to insert or update live build: {}", e))?;

    Ok((
        build,
        build_hash,
        extracted.event_handlers,
        extracted.scheduled_functions,
        extracted.mcp_tools,
        extracted.webhooks,
        extracted.agents,
        extracted.workflows,
        extracted.send_targets,
    ))
}

/// Common compilation setup that creates live project/build/event_handlers and returns a ready compiler
/// This can be used by run, repl, test commands to avoid duplication
pub async fn setup_live_build_and_compiler(
    db: &DatabasePool,
    build_context: BuildContext,
    enable_cache: bool,
    cache_format: Option<String>,
    load_ctx_hot: bool,
    color: bool,
) -> Result<BuildResult, String> {
    tracing::debug!(
        "Setting up live build for project '{}'...",
        build_context.project_name
    );
    tracing::debug!("  Source paths: {:?}", build_context.src_paths);
    tracing::debug!("  Package paths: [] (using dependency system)");

    // Create live build (compilation and DB records) - this returns the compiled data
    let (
        build,
        _build_hash,
        event_handlers,
        scheduled_functions,
        mcp_tools,
        webhooks,
        agents,
        workflows,
        send_targets,
    ) = create_live_build(
        db,
        build_context.clone(),
        enable_cache,
        cache_format,
        load_ctx_hot,
        color,
    )
    .await?;

    // Clear existing event handlers for this build (if any)
    let deleted_count =
        crate::db::EventHandler::delete_event_handlers_by_build(db, &build.build_id)
            .await
            .map_err(|e| format!("Failed to delete existing event handlers: {}", e))?;

    if deleted_count > 0 {
        tracing::debug!("  Cleared {} existing event handlers", deleted_count);
    }

    // Insert new event handlers for this build
    if !event_handlers.is_empty() {
        crate::db::EventHandler::insert_event_handlers_for_build(
            db,
            &build.build_id,
            &event_handlers,
            &send_targets,
        )
        .await
        .map_err(|e| format!("Failed to insert event handlers: {}", e))?;

        let total_handlers: usize = event_handlers.values().map(|v| v.len()).sum();
        tracing::debug!("  Inserted {} event handlers", total_handlers);
    }

    // Insert new scheduled functions for this build (upsert will reactivate existing ones)
    // IMPORTANT: Always call insert_schedules_for_build even if scheduled_functions is empty,
    // because it first deactivates ALL schedules for the build before upserting new ones.
    // This ensures that when a scheduled function is removed, its schedule gets deactivated.
    crate::db::Schedule::insert_schedules_for_build(
        db,
        &build.build_id,
        &scheduled_functions,
        &send_targets,
    )
    .await
    .map_err(|e| format!("Failed to insert scheduled functions: {}", e))?;

    if !scheduled_functions.is_empty() {
        let total_scheduled: usize = scheduled_functions.values().map(|v| v.len()).sum();
        tracing::debug!("  Inserted {} scheduled functions", total_scheduled);
    }

    // Clear existing MCP tools for this build (if any)
    let deleted_mcp_count = crate::db::McpTool::delete_mcp_tools_by_build(db, &build.build_id)
        .await
        .map_err(|e| format!("Failed to delete existing MCP tools: {}", e))?;

    if deleted_mcp_count > 0 {
        tracing::debug!("  Cleared {} existing MCP tools", deleted_mcp_count);
    }

    // Insert new MCP tools for this build
    if !mcp_tools.is_empty() {
        crate::db::McpTool::insert_mcp_tools_for_build(db, &build.build_id, &mcp_tools)
            .await
            .map_err(|e| format!("Failed to insert MCP tools: {}", e))?;

        let total_mcp_tools: usize = mcp_tools.values().map(|v| v.len()).sum();
        tracing::debug!("  Inserted {} MCP tools", total_mcp_tools);
    }

    // NOTE: We do NOT delete existing webhooks before inserting.
    // The insert uses ON CONFLICT (webhook_id) DO UPDATE, which atomically
    // upserts and preserves stable webhook URLs across deploys.

    // Insert new webhooks for this build
    if !webhooks.is_empty() {
        crate::db::Webhook::insert_webhooks_for_build(
            db,
            &build.build_id,
            &webhooks,
            &send_targets,
        )
        .await
        .map_err(|e| format!("Failed to insert webhooks: {}", e))?;

        let total_webhooks: usize = webhooks.values().map(|v| v.len()).sum();
        tracing::debug!("  Inserted {} webhooks", total_webhooks);
    }

    // Clear existing agents for this build (if any)
    let deleted_agents_count = crate::db::Agent::delete_agents_by_build(db, &build.build_id)
        .await
        .map_err(|e| format!("Failed to delete existing agents: {}", e))?;

    if deleted_agents_count > 0 {
        tracing::debug!("  Cleared {} existing agents", deleted_agents_count);
    }

    // Insert new agents for this build
    if !agents.is_empty() {
        let env_id = build_context
            .env_id
            .ok_or("Environment ID is required for agent insertion")?;
        crate::db::Agent::insert_agents_for_build(db, &build.build_id, &env_id, &agents)
            .await
            .map_err(|e| format!("Failed to insert agents: {}", e))?;

        tracing::debug!("  Inserted {} agents", agents.len());
    }

    // Clear existing named workflows for this build (if any)
    let deleted_workflows_count =
        crate::db::Workflow::delete_workflows_by_build(db, &build.build_id)
            .await
            .map_err(|e| format!("Failed to delete existing workflows: {}", e))?;

    if deleted_workflows_count > 0 {
        tracing::debug!("  Cleared {} existing workflows", deleted_workflows_count);
    }

    // Insert new named workflows for this build
    if !workflows.is_empty() {
        let env_id = build_context
            .env_id
            .ok_or("Environment ID is required for workflow insertion")?;
        crate::db::Workflow::insert_workflows_for_build(db, &build.build_id, &env_id, &workflows)
            .await
            .map_err(|e| format!("Failed to insert workflows: {}", e))?;

        tracing::debug!("  Inserted {} workflows", workflows.len());
    }

    // Create compiler with dependency system support
    // Compiler is already created and compiled by create_live_build - no need to compile again

    // For live builds, we don't create a zip file, so use a placeholder path
    let zip_path = PathBuf::from(format!("live-build-{}", build.build_id));

    let build_result = BuildResult { build, zip_path };

    Ok(build_result)
}

/// Main entry point for compile command
pub async fn build_compile(
    db: &DatabasePool,
    build_context: BuildContext,
    enable_cache: bool,
    cache_format: Option<String>,
) -> Result<BuildResult, String> {
    let project_name = build_context.project_name.clone();

    // Use the common setup function - we don't need the compiler for this command
    let build_result = setup_live_build_and_compiler(
        db,
        build_context,
        enable_cache,
        cache_format,
        false, // load_ctx_hot
        false, // color (non-interactive compile)
    )
    .await?;

    println!(
        "Live build compiled successfully for project '{}'",
        project_name
    );
    println!(
        "Project: {} ({})",
        project_name, build_result.build.project_id
    );
    println!(
        "Build: {} (size: {} bytes)",
        build_result.build.build_id, build_result.build.size
    );

    Ok(build_result)
}

// Trait for abstracting build fetching from different sources
#[async_trait]
pub trait BuildFetcher {
    /// Fetch a build by ID and return the path to the extracted directory
    /// If the build is already extracted, return the existing path
    async fn fetch(&self, build: &Build, project: &Project) -> Result<PathBuf, String>;
}

/// Local build fetcher that retrieves builds from .hot/build directory
pub struct LocalBuildFetcher {
    build_dir: PathBuf,
    run_dir: PathBuf,
}

impl LocalBuildFetcher {
    pub fn new(build_dir: Option<PathBuf>, run_dir: Option<PathBuf>) -> Self {
        Self {
            build_dir: build_dir.unwrap_or_else(|| PathBuf::from(".hot/build")),
            run_dir: run_dir.unwrap_or_else(|| PathBuf::from(".hot/run")),
        }
    }

    /// Construct the zip filename from build and project data
    /// Format: {project_name}-{build_id_short}-{hash_short}.hot.zip
    fn construct_zip_filename(&self, build: &Build, project: &Project) -> String {
        let build_id_no_dashes = build.build_id.to_string().replace('-', "");
        let build_id_short = if build_id_no_dashes.len() >= 12 {
            &build_id_no_dashes[..12]
        } else {
            &build_id_no_dashes
        };
        let hash_short = if build.hash.len() >= 8 {
            &build.hash[..8]
        } else {
            &build.hash
        };

        format!("{}-{}-{}.hot.zip", project.name, build_id_short, hash_short)
    }

    /// Get the extraction directory name for this build
    fn get_extract_dir_name(&self, build: &Build, project: &Project) -> String {
        let build_id_no_dashes = build.build_id.to_string().replace('-', "");
        let build_id_short = if build_id_no_dashes.len() >= 12 {
            &build_id_no_dashes[..12]
        } else {
            &build_id_no_dashes
        };
        let hash_short = if build.hash.len() >= 8 {
            &build.hash[..8]
        } else {
            &build.hash
        };

        format!("{}-{}-{}", project.name, build_id_short, hash_short)
    }
}

#[async_trait]
impl BuildFetcher for LocalBuildFetcher {
    async fn fetch(&self, build: &Build, project: &Project) -> Result<PathBuf, String> {
        let extract_dir_name = self.get_extract_dir_name(build, project);
        let extract_path = self.run_dir.join(&extract_dir_name);

        // Check if already extracted
        if extract_path.exists() && extract_path.is_dir() {
            // Verify it has the expected structure (hot/src and hot/pkg directories)
            let hot_dir = extract_path.join("hot");
            if hot_dir.exists() {
                tracing::debug!(
                    "Build {} already extracted at {}",
                    build.build_id,
                    extract_path.display()
                );
                return Ok(extract_path);
            }
        }

        // Get zip path from storage_path if available, otherwise construct from old format
        let zip_path = if let Some(ref storage_path) = build.storage_path {
            self.build_dir.join(storage_path)
        } else {
            // Fall back to old naming convention for backwards compatibility
            let zip_filename = self.construct_zip_filename(build, project);
            self.build_dir.join(&zip_filename)
        };

        if !zip_path.exists() {
            return Err(format!(
                "Build zip file not found: {} (looking for build_id: {})",
                zip_path.display(),
                build.build_id
            ));
        }

        let zip_filename = zip_path
            .file_name()
            .and_then(|n| n.to_str())
            .ok_or_else(|| format!("Invalid zip file path: {}", zip_path.display()))?
            .to_string();

        // Create run directory if it doesn't exist
        if let Err(e) = std::fs::create_dir_all(&self.run_dir) {
            return Err(format!("Failed to create run directory: {}", e));
        }

        // Copy zip file to run directory
        let run_zip_path = self.run_dir.join(&zip_filename);
        if let Err(e) = std::fs::copy(&zip_path, &run_zip_path) {
            return Err(format!("Failed to copy zip file to run directory: {}", e));
        }

        // Extract the zip file
        let file = match std::fs::File::open(&run_zip_path) {
            Ok(file) => file,
            Err(e) => return Err(format!("Failed to open zip file: {}", e)),
        };

        let mut archive = match zip::ZipArchive::new(file) {
            Ok(archive) => archive,
            Err(e) => return Err(format!("Failed to read zip archive: {}", e)),
        };

        // Create extraction directory
        if let Err(e) = std::fs::create_dir_all(&extract_path) {
            return Err(format!("Failed to create extraction directory: {}", e));
        }

        // Extract all files
        for i in 0..archive.len() {
            let mut file = match archive.by_index(i) {
                Ok(file) => file,
                Err(e) => return Err(format!("Failed to access file {} in archive: {}", i, e)),
            };

            let outpath = match file.enclosed_name() {
                Some(path) => extract_path.join(path),
                None => continue,
            };

            if file.name().ends_with('/') {
                // Directory
                if let Err(e) = std::fs::create_dir_all(&outpath) {
                    return Err(format!(
                        "Failed to create directory {}: {}",
                        outpath.display(),
                        e
                    ));
                }
            } else {
                // File
                if let Some(p) = outpath.parent()
                    && !p.exists()
                    && let Err(e) = std::fs::create_dir_all(p)
                {
                    return Err(format!("Failed to create parent directory: {}", e));
                }

                let mut outfile = match std::fs::File::create(&outpath) {
                    Ok(file) => file,
                    Err(e) => {
                        return Err(format!(
                            "Failed to create file {}: {}",
                            outpath.display(),
                            e
                        ));
                    }
                };

                if let Err(e) = std::io::copy(&mut file, &mut outfile) {
                    return Err(format!(
                        "Failed to extract file {}: {}",
                        outpath.display(),
                        e
                    ));
                }
            }
        }

        // Remove the zip file from run directory (keep original in build directory)
        let _ = std::fs::remove_file(&run_zip_path);

        // Read the manifest to get the correct cache key and project name
        let manifest = crate::bundle::read_bundle_manifest(&extract_path);
        let (project_name, cache_key, file_hashes) = match &manifest {
            Ok(m) => (
                m.bundle_name.clone(),
                m.cache_key.clone(),
                Some(m.file_hashes.clone()),
            ),
            Err(e) => {
                tracing::warn!("Failed to read bundle manifest for pre-compile: {}", e);
                (project.name.clone(), None, None)
            }
        };

        // Pre-compile the bundle to generate bytecode cache
        // This ensures routing can find functions immediately after extraction
        let build_src_path = extract_path.join("hot/src");
        let build_pkg_path = extract_path.join("hot/pkg");
        let mut paths = vec![build_src_path.to_string_lossy().to_string()];
        if build_pkg_path.exists() {
            paths.push(build_pkg_path.to_string_lossy().to_string());
        }

        let bundle_cache_dir = extract_path.join(".hot").join("cache");
        let bundle_cache = crate::lang::cache::bytecode_cache::BytecodeCache::new(bundle_cache_dir);
        tracing::debug!(
            "Pre-compiling bundle {} to generate bytecode cache",
            build.build_id
        );
        if let Err(e) = crate::lang::engine::Engine::compile_to_cache(
            &paths,
            &bundle_cache,
            &project_name,
            cache_key.as_deref(),
            file_hashes,
            None, // Bundle builds have deps pre-bundled
        ) {
            tracing::warn!("Failed to pre-compile bundle {}: {}", build.build_id, e);
        } else {
            tracing::info!("Bundle {} pre-compiled successfully", build.build_id);
        }

        // Write extraction complete marker AFTER bytecode is ready
        let marker_path = extract_path.join(".extraction_complete");
        if let Err(e) = std::fs::write(&marker_path, "") {
            tracing::warn!("Failed to write extraction marker: {}", e);
        }

        tracing::debug!(
            "Build {} extracted to {}",
            build.build_id,
            extract_path.display()
        );
        Ok(extract_path)
    }
}

/// Load event handlers and schedules from a build's manifest and insert them into the database
/// This should be called when uploading a build to ensure the scheduler and workers have the handlers
pub async fn load_build_manifest_data(
    db: &DatabasePool,
    build_id: &Uuid,
    env_id: &Uuid,
    build_data: &[u8],
) -> Result<(), String> {
    use std::io::Cursor;
    use zip::ZipArchive;

    tracing::info!(
        "Loading manifest data (handlers, schedules, MCP tools, webhooks, agents, workflows) from build {}",
        build_id
    );

    // Clear any existing event handlers for this build (prevents duplicates if handlers
    // were already inserted locally before upload)
    let deleted_handlers = crate::db::EventHandler::delete_event_handlers_by_build(db, build_id)
        .await
        .map_err(|e| format!("Failed to clear existing event handlers: {}", e))?;
    if deleted_handlers > 0 {
        tracing::debug!(
            "Cleared {} existing event handlers for build {}",
            deleted_handlers,
            build_id
        );
    }

    // Clear any existing MCP tools for this build
    let deleted_mcp = crate::db::McpTool::delete_mcp_tools_by_build(db, build_id)
        .await
        .map_err(|e| format!("Failed to clear existing MCP tools: {}", e))?;
    if deleted_mcp > 0 {
        tracing::debug!(
            "Cleared {} existing MCP tools for build {}",
            deleted_mcp,
            build_id
        );
    }

    // Clear any existing agents for this build
    let deleted_agents = crate::db::Agent::delete_agents_by_build(db, build_id)
        .await
        .map_err(|e| format!("Failed to clear existing agents: {}", e))?;
    if deleted_agents > 0 {
        tracing::debug!(
            "Cleared {} existing agents for build {}",
            deleted_agents,
            build_id
        );
    }

    // Clear any existing named workflows for this build
    let deleted_workflows = crate::db::Workflow::delete_workflows_by_build(db, build_id)
        .await
        .map_err(|e| format!("Failed to clear existing workflows: {}", e))?;
    if deleted_workflows > 0 {
        tracing::debug!(
            "Cleared {} existing workflows for build {}",
            deleted_workflows,
            build_id
        );
    }

    // NOTE: We do NOT delete existing webhooks before re-inserting.
    // The insert uses ON CONFLICT (webhook_id) DO UPDATE, which atomically
    // upserts and preserves stable webhook URLs across deploys.
    // Deleting first would destroy the webhook_id that find_existing looks up.

    // Open the zip file from memory
    let cursor = Cursor::new(build_data);
    let mut archive =
        ZipArchive::new(cursor).map_err(|e| format!("Failed to read build zip: {}", e))?;

    // Read the manifest.hot file
    let manifest_file = archive
        .by_name("manifest.hot")
        .map_err(|_| "Build does not contain manifest.hot".to_string())?;

    let manifest_result = crate::bundle::read_manifest_data(manifest_file)?;

    let mut total_handlers = 0;
    let mut total_schedules = 0;
    let mut total_mcp_tools = 0;
    let mut total_webhooks = 0;
    let mut total_agents = 0;
    let mut total_workflows = 0;

    // Extract event handlers from the manifest
    // Structure: { "hot.bundle.{name}": { "event_handlers": { "event.type": [...] } } }
    if let crate::val::Val::Map(map) = manifest_result {
        for (key, bundle_val) in map.iter() {
            if let crate::val::Val::Str(key_str) = key
                && key_str.starts_with("hot.bundle.")
                && let crate::val::Val::Map(bundle_map) = bundle_val
            {
                // Validate version compatibility
                let version_check = validate_build_version(bundle_map);
                if let Err(e) = Build::update_manifest_versions(
                    db,
                    build_id,
                    version_check.build_engine_version.as_deref(),
                    version_check.build_hot_std_version.as_deref(),
                )
                .await
                {
                    if Build::manifest_version_metadata_unavailable(&e) {
                        tracing::debug!(
                            "Manifest version metadata columns are not available yet; skipping persist for build {}",
                            build_id
                        );
                    } else {
                        tracing::warn!(
                            "Failed to persist manifest versions for build {}: {}",
                            build_id,
                            e
                        );
                    }
                }

                // Log warnings
                for warning in &version_check.warnings {
                    tracing::warn!("Build {}: {}", build_id, warning);
                }

                if !version_check.compatible {
                    tracing::error!(
                        "Build {} has incompatible versions: engine={:?}, hot-std={:?}, server={}",
                        build_id,
                        version_check.build_engine_version,
                        version_check.build_hot_std_version,
                        version_check.server_version
                    );
                    let details = if version_check.warnings.is_empty() {
                        "version compatibility check failed".to_string()
                    } else {
                        version_check.warnings.join("; ")
                    };
                    return Err(format!("Build {} is incompatible: {}", build_id, details));
                } else if version_check.build_engine_version.is_some() {
                    tracing::info!(
                        "Build {} version check passed: engine={}, hot-std={}, server={}",
                        build_id,
                        version_check
                            .build_engine_version
                            .as_deref()
                            .unwrap_or("unknown"),
                        version_check
                            .build_hot_std_version
                            .as_deref()
                            .unwrap_or("unknown"),
                        version_check.server_version
                    );
                }

                // Extract send targets from manifest (static send() detection data)
                let manifest_send_targets = {
                    let mut st = crate::lang::compiler::SendTargets::new();
                    if let Some(crate::val::Val::Map(st_map)) =
                        bundle_map.get(&crate::val::Val::from("send_targets"))
                    {
                        for (fn_key_val, events_val) in st_map.iter() {
                            if let crate::val::Val::Str(fn_key) = fn_key_val
                                && let crate::val::Val::Vec(events) = events_val
                            {
                                let targets: Vec<crate::lang::compiler::SendTarget> = events
                                    .iter()
                                    .filter_map(|v| {
                                        if let crate::val::Val::Str(s) = v {
                                            // Split fn_key on last '/' to get ns and var
                                            let (ns, var) = fn_key
                                                .rsplit_once('/')
                                                .map(|(n, v)| (n.to_string(), v.to_string()))
                                                .unwrap_or_default();
                                            Some(crate::lang::compiler::SendTarget {
                                                event_name: (**s).to_string(),
                                                namespace: ns,
                                                var_name: var,
                                                source:
                                                    crate::lang::compiler::SendTargetSource::Static,
                                            })
                                        } else {
                                            None
                                        }
                                    })
                                    .collect();
                                if !targets.is_empty() {
                                    st.insert((**fn_key).to_string(), targets);
                                }
                            }
                        }
                    }
                    st
                };

                // Extract event handlers
                if let Some(handlers_val) = bundle_map.get(&crate::val::Val::from("event_handlers"))
                    && let crate::val::Val::Map(handlers_map) = handlers_val
                {
                    for (event_type, handlers_list) in handlers_map.iter() {
                        if let crate::val::Val::Str(event_type_str) = event_type
                            && let crate::val::Val::Vec(handlers) = handlers_list
                        {
                            for handler_val in handlers {
                                if let Err(e) =
                                    crate::db::EventHandler::insert_event_handler_from_val(
                                        db,
                                        build_id,
                                        event_type_str,
                                        handler_val,
                                        &manifest_send_targets,
                                    )
                                    .await
                                {
                                    tracing::error!("Failed to insert event handler: {}", e);
                                } else {
                                    total_handlers += 1;
                                }
                            }
                        }
                    }
                }

                // Extract scheduled functions from manifest
                // Structure: { "scheduled_functions": { "cron_expr": [...] } }
                if let Some(schedules_val) =
                    bundle_map.get(&crate::val::Val::from("scheduled_functions"))
                    && let crate::val::Val::Map(schedules_map) = schedules_val
                {
                    // Convert manifest Val structure to ScheduledFunctions format
                    let mut scheduled_functions = crate::lang::compiler::ScheduledFunctions::new();

                    for (cron_expr, functions_list) in schedules_map.iter() {
                        if let crate::val::Val::Str(cron_expr_str) = cron_expr
                            && let crate::val::Val::Vec(functions) = functions_list
                        {
                            let mut function_entries = Vec::new();

                            for function_val in functions {
                                if let Some(entry) =
                                    val_to_scheduled_function_entry(function_val, cron_expr_str)
                                {
                                    function_entries.push(entry);
                                }
                            }

                            if !function_entries.is_empty() {
                                scheduled_functions
                                    .insert((**cron_expr_str).to_owned(), function_entries);
                            }
                        }
                    }

                    // Insert all schedules for this build using the batch function
                    if !scheduled_functions.is_empty() {
                        if let Err(e) = crate::db::Schedule::insert_schedules_for_build(
                            db,
                            build_id,
                            &scheduled_functions,
                            &manifest_send_targets,
                        )
                        .await
                        {
                            tracing::error!("Failed to insert schedules: {}", e);
                        } else {
                            let count: usize = scheduled_functions.values().map(|v| v.len()).sum();
                            total_schedules += count;
                        }
                    }
                }

                // Extract MCP tools from manifest
                // Structure: { "mcp_tools": { "service": [...tool Val maps...] } }
                if let Some(mcp_val) = bundle_map.get(&crate::val::Val::from("mcp_tools"))
                    && let crate::val::Val::Map(mcp_map) = mcp_val
                {
                    for (service_key, tools_list) in mcp_map.iter() {
                        if let crate::val::Val::Str(service) = service_key
                            && let crate::val::Val::Vec(tools) = tools_list
                        {
                            for tool_val in tools {
                                if let Err(e) = crate::db::McpTool::insert_mcp_tool_from_val(
                                    db, build_id, service, tool_val,
                                )
                                .await
                                {
                                    tracing::error!("Failed to insert MCP tool: {}", e);
                                } else {
                                    total_mcp_tools += 1;
                                }
                            }
                        }
                    }
                }

                // Extract webhooks from manifest
                // Structure: { "webhooks": { "service": [...webhook Val maps...] } }
                if let Some(webhook_val) = bundle_map.get(&crate::val::Val::from("webhooks"))
                    && let crate::val::Val::Map(webhook_map) = webhook_val
                {
                    for (service_key, entries_list) in webhook_map.iter() {
                        if let crate::val::Val::Str(service) = service_key
                            && let crate::val::Val::Vec(entries) = entries_list
                        {
                            for entry_val in entries {
                                if let Err(e) = crate::db::Webhook::insert_webhook_from_val(
                                    db,
                                    build_id,
                                    service,
                                    entry_val,
                                    &manifest_send_targets,
                                )
                                .await
                                {
                                    tracing::error!("Failed to insert webhook: {}", e);
                                } else {
                                    total_webhooks += 1;
                                }
                            }
                        }
                    }
                }

                // Extract agents from manifest
                // Structure: { "agents": [agent Val maps...] }
                if let Some(crate::val::Val::Vec(agents_list)) =
                    bundle_map.get(&crate::val::Val::from("agents"))
                {
                    for agent_val in agents_list {
                        if let Err(e) =
                            crate::db::Agent::insert_agent_from_val(db, build_id, env_id, agent_val)
                                .await
                        {
                            tracing::error!("Failed to insert agent: {}", e);
                        } else {
                            total_agents += 1;
                        }
                    }
                }

                // Extract named workflows from manifest
                // Structure: { "workflows": [workflow Val maps...] }
                if let Some(crate::val::Val::Vec(workflows_list)) =
                    bundle_map.get(&crate::val::Val::from("workflows"))
                {
                    for workflow_val in workflows_list {
                        if let Err(e) = crate::db::Workflow::insert_workflow_from_val(
                            db,
                            build_id,
                            env_id,
                            workflow_val,
                        )
                        .await
                        {
                            tracing::error!("Failed to insert workflow: {}", e);
                        } else {
                            total_workflows += 1;
                        }
                    }
                }
            }
        }
    }

    if total_handlers > 0 {
        tracing::info!(
            "Loaded {} event handler(s) for build {}",
            total_handlers,
            build_id
        );
    }

    if total_schedules > 0 {
        tracing::info!(
            "Loaded {} schedule(s) for build {}",
            total_schedules,
            build_id
        );
    }

    if total_mcp_tools > 0 {
        tracing::info!(
            "Loaded {} MCP tool(s) for build {}",
            total_mcp_tools,
            build_id
        );
    }

    if total_webhooks > 0 {
        tracing::info!(
            "Loaded {} webhook(s) for build {}",
            total_webhooks,
            build_id
        );

        // Clean up stale webhooks from previous builds of the same project
        // (e.g., webhooks removed from code). Must happen AFTER upsert so
        // find_existing_webhook_id_for_build can still locate the stable ID.
        let stale_count = crate::db::Webhook::delete_stale_for_project(db, build_id)
            .await
            .map_err(|e| format!("Failed to clean up stale webhooks: {}", e))?;
        if stale_count > 0 {
            tracing::info!(
                "Cleaned up {} stale webhook(s) from previous builds",
                stale_count
            );
        }
    }

    if total_agents > 0 {
        tracing::info!("Loaded {} agent(s) for build {}", total_agents, build_id);
    }

    if total_workflows > 0 {
        tracing::info!(
            "Loaded {} workflow(s) for build {}",
            total_workflows,
            build_id
        );
    }

    if total_handlers == 0
        && total_schedules == 0
        && total_mcp_tools == 0
        && total_webhooks == 0
        && total_agents == 0
        && total_workflows == 0
    {
        tracing::info!(
            "No handlers, schedules, MCP tools, webhooks, agents, or workflows found in build {}",
            build_id
        );
    }

    Ok(())
}

/// Helper function to convert a Val from manifest to a ScheduledFunction entry
fn val_to_scheduled_function_entry(
    function_val: &crate::val::Val,
    cron_expr: &str,
) -> Option<crate::lang::compiler::ScheduledFunction> {
    // The Val should already be in the format we need - it's the scheduled_function field
    Some(crate::lang::compiler::ScheduledFunction {
        cron_expression: cron_expr.to_string(),
        scheduled_function: function_val.clone(),
    })
}

/// Extract ctx_requirements from a build's manifest data
/// Returns a set of required context variable keys, or an empty set if none
pub fn extract_ctx_requirements_from_build(build_data: &[u8]) -> Result<AHashSet<String>, String> {
    let detailed = extract_ctx_requirements_detailed_from_build(build_data)?;
    Ok(detailed.into_iter().map(|e| e.key).collect())
}

/// One required-ctx-key entry as recorded in (or recovered from) a bundle
/// manifest. `declared_by` is the FQN of the package function that asked for
/// the key (e.g. `::slack::api/request`); `source_file` is its source path
/// when known. Both are optional so we stay back-compat with older bundles
/// that wrote a flat `Vec<Str>` of just the key names.
#[derive(Debug, Clone)]
pub struct CtxRequirementEntry {
    pub key: String,
    pub declared_by: Option<String>,
    pub source_file: Option<String>,
}

/// Extract the rich form of ctx requirements from a build, with the
/// declaring package function and source file when available. Accepts both
/// the new `Vec<Map>` shape and the legacy `Vec<Str>` shape.
pub fn extract_ctx_requirements_detailed_from_build(
    build_data: &[u8],
) -> Result<Vec<CtxRequirementEntry>, String> {
    use std::io::Cursor;
    use zip::ZipArchive;

    let cursor = Cursor::new(build_data);
    let mut archive =
        ZipArchive::new(cursor).map_err(|e| format!("Failed to read build zip: {}", e))?;

    let manifest_file = archive
        .by_name("manifest.hot")
        .map_err(|_| "Build does not contain manifest.hot".to_string())?;

    let manifest_result = crate::bundle::read_manifest_data(manifest_file)?;

    let mut entries: Vec<CtxRequirementEntry> = Vec::new();
    let mut seen_keys: AHashSet<String> = AHashSet::new();

    // Structure: { "hot.bundle.{name}": { "ctx_requirements": [...] } }
    if let crate::val::Val::Map(map) = manifest_result {
        for (key, bundle_val) in map.iter() {
            if let crate::val::Val::Str(key_str) = key
                && key_str.starts_with("hot.bundle.")
                && let crate::val::Val::Map(bundle_map) = bundle_val
                && let Some(ctx_reqs_val) =
                    bundle_map.get(&crate::val::Val::from("ctx_requirements"))
                && let crate::val::Val::Vec(ctx_reqs) = ctx_reqs_val
            {
                for req in ctx_reqs {
                    match req {
                        // New shape: {key, declared-by, source-file}
                        crate::val::Val::Map(entry_map) => {
                            let key_val = entry_map.get(&crate::val::Val::from("key"));
                            let key_str = match key_val {
                                Some(crate::val::Val::Str(s)) => s.to_string(),
                                _ => continue,
                            };
                            if !seen_keys.insert(key_str.clone()) {
                                continue;
                            }
                            let declared_by = entry_map
                                .get(&crate::val::Val::from("declared-by"))
                                .and_then(|v| match v {
                                    crate::val::Val::Str(s) => Some(s.to_string()),
                                    _ => None,
                                });
                            let source_file = entry_map
                                .get(&crate::val::Val::from("source-file"))
                                .and_then(|v| match v {
                                    crate::val::Val::Str(s) => Some(s.to_string()),
                                    _ => None,
                                });
                            entries.push(CtxRequirementEntry {
                                key: key_str,
                                declared_by,
                                source_file,
                            });
                        }
                        // Legacy shape: bare key string
                        crate::val::Val::Str(key) => {
                            let key_str = key.to_string();
                            if seen_keys.insert(key_str.clone()) {
                                entries.push(CtxRequirementEntry {
                                    key: key_str,
                                    declared_by: None,
                                    source_file: None,
                                });
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
    }

    Ok(entries)
}

/// Extract box_requirements from a build's manifest data
/// Returns effective min_size and network requirement, or None if not present
pub fn extract_box_requirements_from_build(
    build_data: &[u8],
) -> Result<Option<(Option<String>, bool)>, String> {
    use std::io::Cursor;
    use zip::ZipArchive;

    let cursor = Cursor::new(build_data);
    let mut archive =
        ZipArchive::new(cursor).map_err(|e| format!("Failed to read build zip: {}", e))?;

    let manifest_file = archive
        .by_name("manifest.hot")
        .map_err(|_| "Build does not contain manifest.hot".to_string())?;

    let manifest_result = crate::bundle::read_manifest_data(manifest_file)?;

    // Structure: { "hot.bundle.{name}": { "box_requirements": { min_size: "...", network: true } } }
    if let crate::val::Val::Map(map) = manifest_result {
        for (key, bundle_val) in map.iter() {
            if let crate::val::Val::Str(key_str) = key
                && key_str.starts_with("hot.bundle.")
                && let crate::val::Val::Map(bundle_map) = bundle_val
                && let Some(box_reqs_val) =
                    bundle_map.get(&crate::val::Val::from("box_requirements"))
                && let crate::val::Val::Map(box_map) = box_reqs_val
            {
                if box_map.is_empty() {
                    return Ok(None);
                }

                let min_size = box_map
                    .get(&crate::val::Val::from("min_size"))
                    .and_then(|v| {
                        if let crate::val::Val::Str(s) = v {
                            Some(s.to_string())
                        } else {
                            None
                        }
                    });

                let network = box_map
                    .get(&crate::val::Val::from("network"))
                    .and_then(|v| {
                        if let crate::val::Val::Bool(b) = v {
                            Some(*b)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(false);

                return Ok(Some((min_size, network)));
            }
        }
    }

    Ok(None)
}

fn extract_scheduled_functions_from_build(
    build_data: &[u8],
) -> Result<crate::lang::compiler::ScheduledFunctions, String> {
    use std::io::Cursor;
    use zip::ZipArchive;

    let cursor = Cursor::new(build_data);
    let mut archive =
        ZipArchive::new(cursor).map_err(|e| format!("Failed to read build zip: {}", e))?;
    let manifest_file = archive
        .by_name("manifest.hot")
        .map_err(|_| "Build does not contain manifest.hot".to_string())?;

    let manifest_result = crate::bundle::read_manifest_data(manifest_file)?;

    let mut scheduled_functions = crate::lang::compiler::ScheduledFunctions::new();
    if let crate::val::Val::Map(map) = manifest_result {
        for (key, bundle_val) in map.iter() {
            if let crate::val::Val::Str(key_str) = key
                && key_str.starts_with("hot.bundle.")
                && let crate::val::Val::Map(bundle_map) = bundle_val
                && let Some(crate::val::Val::Map(schedules_map)) =
                    bundle_map.get(&crate::val::Val::from("scheduled_functions"))
            {
                for (cron_expr, functions_list) in schedules_map.iter() {
                    if let crate::val::Val::Str(cron_expr_str) = cron_expr
                        && let crate::val::Val::Vec(functions) = functions_list
                    {
                        let function_entries: Vec<_> = functions
                            .iter()
                            .filter_map(|function_val| {
                                val_to_scheduled_function_entry(function_val, cron_expr_str)
                            })
                            .collect();
                        if !function_entries.is_empty() {
                            scheduled_functions
                                .insert((**cron_expr_str).to_owned(), function_entries);
                        }
                    }
                }
            }
        }
    }

    Ok(scheduled_functions)
}

pub async fn validate_schedule_requirements_for_deploy(
    db: &DatabasePool,
    build_id: &uuid::Uuid,
    org_id: &uuid::Uuid,
    env_id: &uuid::Uuid,
    conf: &crate::val::Val,
    storage: &dyn crate::storage::BuildStorage,
) -> Result<(), String> {
    let build = crate::db::Build::get_build(db, build_id)
        .await
        .map_err(|e| format!("Failed to get build: {}", e))?;

    if !build.is_bundle() {
        return Ok(());
    }

    let build_data = storage
        .retrieve_build(build_id, org_id, env_id)
        .await
        .map_err(|e| format!("Failed to read build file: {}", e))?;
    let scheduled_functions = extract_scheduled_functions_from_build(&build_data)?;
    let features = crate::db::Features::resolve_for_org(db, org_id).await;
    let policy = crate::db::SchedulePolicy::from_conf(conf).with_features(&features);

    for cron_expression in scheduled_functions.keys() {
        crate::db::validate_recurring_schedule_interval(cron_expression, policy.min_interval_secs)
            .map_err(|e| {
                format!(
                    "Deploy blocked: schedule '{}' violates schedule interval policy. {}",
                    cron_expression,
                    e.message()
                )
            })?;
    }

    let new_count: i64 = scheduled_functions.values().map(|v| v.len() as i64).sum();
    crate::db::Schedule::enforce_active_count_for_org_replacing_project(
        db,
        org_id,
        &build.project_id,
        new_count,
        policy,
    )
    .await
    .map_err(|e| format!("Deploy blocked: {}", e))?;

    Ok(())
}

/// Validate that box resource requirements can be satisfied by the org's plan
/// Returns Ok(()) if all requirements are met, or an error describing the gap
pub async fn validate_box_requirements_for_deploy(
    db: &DatabasePool,
    build_id: &uuid::Uuid,
    org_id: &uuid::Uuid,
    env_id: &uuid::Uuid,
    storage: &dyn crate::storage::BuildStorage,
) -> Result<(), String> {
    tracing::debug!(
        "validate_box_requirements_for_deploy: build_id={}",
        build_id
    );

    let build = crate::db::Build::get_build(db, build_id)
        .await
        .map_err(|e| format!("Failed to get build: {}", e))?;

    if !build.is_bundle() {
        tracing::debug!("Skipping box validation: not a bundle build");
        return Ok(());
    }

    let build_data = storage
        .retrieve_build(build_id, org_id, env_id)
        .await
        .map_err(|e| format!("Failed to read build file: {}", e))?;

    let box_reqs = extract_box_requirements_from_build(&build_data)?;

    let Some((min_size, requires_network)) = box_reqs else {
        tracing::debug!("No box_requirements in manifest, skipping validation");
        return Ok(());
    };

    // Resolve org features to get plan limits
    let features = crate::db::features::Features::resolve_for_org(db, org_id).await;
    let plan_memory_mb = features.box_memory_mb();
    let plan_network = features.box_network_allowed();

    // Check min-size against plan
    if let Some(ref required_size) = min_size
        && let Some(required_memory) =
            crate::lang::compiler::box_checker::size_memory_mb(required_size)
    {
        let plan_fits = if plan_memory_mb < 0 {
            true // unlimited
        } else {
            required_memory <= plan_memory_mb as u64
        };

        if !plan_fits {
            // Find the plan's max size name for a helpful error
            let plan_max_size = if plan_memory_mb < 0 {
                "unlimited".to_string()
            } else {
                let sizes = [
                    ("nano", 64),
                    ("micro", 128),
                    ("small", 256),
                    ("medium", 512),
                    ("large", 1024),
                    ("xlarge", 2048),
                    ("2xlarge", 4096),
                    ("4xlarge", 8192),
                ];
                sizes
                    .iter()
                    .rev()
                    .find(|(_, mem)| *mem as i64 <= plan_memory_mb)
                    .map(|(name, _)| name.to_string())
                    .unwrap_or_else(|| format!("{}MB", plan_memory_mb))
            };

            return Err(format!(
                "Deploy blocked: This project requires container size '{}' ({}MB memory) \
                 but your plan only supports up to '{}' ({}MB memory).\n\n\
                 Upgrade your plan or remove the dependency that requires this container size.",
                required_size, required_memory, plan_max_size, plan_memory_mb
            ));
        }
    }

    // Check network requirement against plan
    if requires_network && !plan_network {
        return Err(
            "Deploy blocked: This project requires container network access \
             but your plan does not allow it.\n\n\
             Upgrade your plan or remove the dependency that requires network access."
                .to_string(),
        );
    }

    tracing::info!(
        "Box requirements validated: min_size={:?}, network={}",
        min_size,
        requires_network
    );
    Ok(())
}

/// Validate that all required context variables are set for a project.
///
/// Policy:
/// - **Default (`strict = false`):** missing required ctx vars produce a
///   structured warning (printed to stderr and recorded in tracing) but the
///   deploy is allowed to continue. The static call graph reaches code that
///   may never run at runtime (a webhook handler that's never invoked, a
///   cond branch that's never taken), so refusing to deploy on those is too
///   aggressive for the common "deploy a partial demo" case.
/// - **`strict = true`:** missing required ctx vars block the deploy with an
///   error, preserving the original gate behavior. Use this in CI or for
///   teams that want the hard guarantee.
///
/// Either way, the warning/error message includes the package function that
/// declared each key (e.g. `::slack::api/request`) and the exact `hot ctx
/// set` invocation, so users don't have to grep their dependencies to learn
/// what to do.
///
/// Checks both env-level and project-level context variables (project
/// overrides env).
pub async fn validate_ctx_requirements_for_deploy(
    db: &DatabasePool,
    build_id: &uuid::Uuid,
    project_id: &uuid::Uuid,
    org_id: &uuid::Uuid,
    env_id: &uuid::Uuid,
    storage: &dyn crate::storage::BuildStorage,
    strict: bool,
) -> Result<(), String> {
    tracing::debug!(
        "validate_ctx_requirements_for_deploy: build_id={} strict={}",
        build_id,
        strict
    );

    // Get the build data
    let build = crate::db::Build::get_build(db, build_id)
        .await
        .map_err(|e| format!("Failed to get build: {}", e))?;

    // Only validate bundle builds (live builds don't have ctx_requirements in manifest)
    if !build.is_bundle() {
        tracing::debug!("Skipping ctx validation: not a bundle build");
        return Ok(());
    }

    // Load the build data from storage (supports local and S3)
    tracing::debug!(
        "Retrieving build {} from storage (type: {})",
        build_id,
        storage.storage_type()
    );
    let build_data = storage
        .retrieve_build(build_id, org_id, env_id)
        .await
        .map_err(|e| format!("Failed to read build file: {}", e))?;

    // Extract ctx_requirements (with declaring-namespace info when available)
    let ctx_requirements = extract_ctx_requirements_detailed_from_build(&build_data)?;
    tracing::info!(
        "Build {} has {} ctx_requirements",
        build_id,
        ctx_requirements.len()
    );

    if ctx_requirements.is_empty() {
        tracing::debug!("No ctx_requirements in manifest, skipping validation");
        return Ok(());
    }

    // Try to load encryption for decryption verification
    // This checks HOT_ENCRYPTION_KEY env var first, then falls back to hot/dev.key
    let encryption =
        crate::context_encryption::ContextEncryption::from_env_or_existing_dev_key().ok();
    if encryption.is_none() {
        tracing::debug!(
            "Encryption not configured (no HOT_ENCRYPTION_KEY or hot/dev.key), will validate key existence only"
        );
    }

    // Get the project to find its env_id
    let project = crate::db::Project::get_project(db, project_id)
        .await
        .map_err(|e| format!("Failed to get project: {}", e))?;

    let available_keys =
        load_available_ctx_keys(db, &project.env_id, project_id, org_id, encryption.as_ref())
            .await?;

    // Check for missing requirements
    let missing: Vec<&CtxRequirementEntry> = ctx_requirements
        .iter()
        .filter(|entry| !available_keys.contains(&entry.key))
        .collect();

    if missing.is_empty() {
        tracing::info!(
            "All {} required context variables are configured",
            ctx_requirements.len()
        );
        return Ok(());
    }

    let message = format_missing_ctx_message(&missing, strict);

    if strict {
        Err(message)
    } else {
        tracing::warn!(
            "Deploy proceeding with {} unset required ctx var(s) (warn-only; pass --strict to block)",
            missing.len()
        );
        eprintln!("{}", message);
        Ok(())
    }
}

/// Load the set of available ctx variable keys for a project, drawing from
/// env-level and project-level context (project wins on conflict). When
/// `encryption` is provided we additionally verify that each value can be
/// decrypted before counting it as "set" — this catches the case where a key
/// was written under a different encryption key than the one in use now.
pub async fn load_available_ctx_keys(
    db: &DatabasePool,
    env_id: &uuid::Uuid,
    project_id: &uuid::Uuid,
    org_id: &uuid::Uuid,
    encryption: Option<&crate::context_encryption::ContextEncryption>,
) -> Result<AHashSet<String>, String> {
    let mut available_keys = AHashSet::new();

    let env_context_vars = crate::db::Context::get_by_env(db, env_id)
        .await
        .map_err(|e| format!("Failed to load env-level context variables: {}", e))?;

    for cv in &env_context_vars {
        if !cv.active {
            continue;
        }
        if let Some(enc) = encryption {
            if cv.get_decrypted_value(enc, org_id).is_ok() {
                available_keys.insert(cv.key.clone());
            }
        } else {
            available_keys.insert(cv.key.clone());
        }
    }

    let project_context_vars = crate::db::Context::get_by_project(db, project_id)
        .await
        .map_err(|e| format!("Failed to load project-level context variables: {}", e))?;

    for cv in &project_context_vars {
        if !cv.active {
            continue;
        }
        if let Some(enc) = encryption {
            if cv.get_decrypted_value(enc, org_id).is_ok() {
                available_keys.insert(cv.key.clone());
            }
        } else {
            available_keys.insert(cv.key.clone());
        }
    }

    tracing::debug!(
        "Available context keys ({} env + {} project): {:?}",
        env_context_vars.iter().filter(|cv| cv.active).count(),
        project_context_vars.iter().filter(|cv| cv.active).count(),
        available_keys
    );

    Ok(available_keys)
}

/// Format the structured "missing required ctx vars" message used for both
/// the strict-mode error and the warn-mode stderr line. Same shape either
/// way so users learn one format.
pub fn format_missing_ctx_message(missing: &[&CtxRequirementEntry], strict: bool) -> String {
    let mut out = String::new();
    if strict {
        out.push_str(&format!(
            "Deploy blocked: {} required context variable(s) reachable from your code are not set:\n\n",
            missing.len()
        ));
    } else {
        out.push_str(&format!(
            "warning: {} required context variable(s) reachable from your code are not set.\n         They will fail at runtime if the calling code path executes.\n         Pass --strict to block deploy on this in the future.\n\n",
            missing.len()
        ));
    }
    for entry in missing {
        match &entry.declared_by {
            Some(by) => {
                out.push_str(&format!("  - {}   (required by {})\n", entry.key, by));
            }
            None => {
                out.push_str(&format!("  - {}\n", entry.key));
            }
        }
        out.push_str(&format!("    hot ctx set {} <value>\n", entry.key));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bundle::BundleFile;
    use std::{fs, path::PathBuf};

    fn make_entry(key: &str, declared_by: Option<&str>) -> CtxRequirementEntry {
        CtxRequirementEntry {
            key: key.to_string(),
            declared_by: declared_by.map(String::from),
            source_file: None,
        }
    }

    #[test]
    fn test_scan_build_inputs_flags_source_secret_shapes() {
        let tmp = tempfile::tempdir().unwrap();
        let src_dir = tmp.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(
            src_dir.join("leak.hot"),
            "::demo ns\nleaked \"xoxb-1234567890\"",
        )
        .unwrap();

        let ctx = BuildContext::new(
            None,
            None,
            None,
            None,
            "demo".to_string(),
            vec![src_dir.to_string_lossy().to_string()],
            Vec::new(),
        );

        let err = scan_build_inputs(&ctx, None).unwrap_err();
        assert!(err.contains("slack-token"));
        assert!(err.contains("hot/src/leak.hot"));
    }

    #[test]
    fn test_scan_build_inputs_flags_resource_secret_shapes() {
        let tmp = tempfile::tempdir().unwrap();
        let resource_dir = tmp.path().join("resources");
        fs::create_dir_all(&resource_dir).unwrap();
        fs::write(
            resource_dir.join("prompt.txt"),
            "api key: sk-proj-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
        )
        .unwrap();

        let ctx = BuildContext::new(
            None,
            None,
            None,
            None,
            "demo".to_string(),
            Vec::new(),
            Vec::new(),
        )
        .with_resources(vec![resource_dir], true, Vec::new());

        let err = scan_build_inputs(&ctx, None).unwrap_err();
        assert!(err.contains("openai-key"));
        assert!(err.contains("resources/prompt.txt"));
    }

    #[test]
    fn test_collect_build_inputs_uses_dependency_pkg_src_and_resource_paths() {
        use crate::val::Val;

        let tmp = tempfile::tempdir().unwrap();
        let pkg_dir = tmp.path().join("demo-pkg");
        fs::create_dir_all(pkg_dir.join("src")).unwrap();
        fs::create_dir_all(pkg_dir.join("test")).unwrap();
        fs::create_dir_all(pkg_dir.join("integration-test")).unwrap();
        fs::create_dir_all(pkg_dir.join("resources/prompts")).unwrap();

        fs::write(
            pkg_dir.join("pkg.hot"),
            r#"::hot::pkg ns

hot.pkg.demo {
  name: "hot.dev/demo-pkg",
  version: "0.1.0",
  deps: {},
  src-paths: ["src/"],
  test-paths: ["test/", "integration-test/"],
  resource-paths: ["resources/"]
}
"#,
        )
        .unwrap();
        fs::write(pkg_dir.join("src/main.hot"), "::demo ns\nvalue 1\n").unwrap();
        fs::write(pkg_dir.join("test/unit.hot"), "::demo::test ns\nvalue 2\n").unwrap();
        fs::write(
            pkg_dir.join("integration-test/live.hot"),
            "::demo::integration ns\nvalue 3\n",
        )
        .unwrap();
        fs::write(
            pkg_dir.join("resources/prompts/system.md"),
            "package prompt",
        )
        .unwrap();

        let mut dep_spec = indexmap::IndexMap::new();
        dep_spec.insert(
            Val::from("local"),
            Val::from(pkg_dir.to_string_lossy().as_ref()),
        );
        let mut deps = indexmap::IndexMap::new();
        deps.insert(Val::from("hot.dev/demo-pkg"), Val::Map(Box::new(dep_spec)));
        let mut project = indexmap::IndexMap::new();
        project.insert(Val::from("deps"), Val::Map(Box::new(deps)));
        let mut projects = indexmap::IndexMap::new();
        projects.insert(Val::from("demo"), Val::Map(Box::new(project)));
        let mut conf = indexmap::IndexMap::new();
        conf.insert(Val::from("project"), Val::Map(Box::new(projects)));
        let conf = Val::Map(Box::new(conf));

        let ctx = BuildContext::new(
            None,
            None,
            None,
            None,
            "demo".to_string(),
            Vec::new(),
            Vec::new(),
        );

        let (files, resources) = collect_build_inputs(&ctx, Some(&conf)).unwrap();
        let file_paths: Vec<_> = files.iter().map(|f| f.relative_path.as_str()).collect();

        assert_eq!(file_paths, vec!["hot/pkg/hot.dev/demo-pkg/src/main.hot"]);
        assert!(
            !file_paths
                .iter()
                .any(|path| path.contains("test") || path.contains("integration-test")),
            "dependency test paths should not be bundled: {:?}",
            file_paths
        );

        let resource_paths: Vec<_> = resources.iter().map(|r| r.rel_path.as_str()).collect();
        assert_eq!(resource_paths, vec!["prompts/system.md"]);
    }

    #[test]
    fn test_format_missing_ctx_message_warn_includes_set_command_and_source() {
        let a = make_entry("slack.api.key", Some("::slack::api/request"));
        let b = make_entry("openai.api.key", Some("::openai::chat/completions"));
        let missing = vec![&a, &b];
        let out = format_missing_ctx_message(&missing, false);

        // Warn header + advice
        assert!(out.contains("warning:"));
        assert!(out.contains("--strict"));
        // Per-key info
        assert!(out.contains("slack.api.key"));
        assert!(out.contains("(required by ::slack::api/request)"));
        assert!(out.contains("hot ctx set slack.api.key <value>"));
        assert!(out.contains("openai.api.key"));
        assert!(out.contains("(required by ::openai::chat/completions)"));
        assert!(out.contains("hot ctx set openai.api.key <value>"));
    }

    #[test]
    fn test_format_missing_ctx_message_strict_uses_blocked_header() {
        let a = make_entry("foo.bar", Some("::pkg/fn"));
        let missing = vec![&a];
        let out = format_missing_ctx_message(&missing, true);
        assert!(out.starts_with("Deploy blocked:"));
        assert!(out.contains("foo.bar"));
        assert!(out.contains("(required by ::pkg/fn)"));
        assert!(out.contains("hot ctx set foo.bar <value>"));
        // Strict mode does not advertise the --strict flag (already there).
        assert!(!out.contains("--strict"));
    }

    #[test]
    fn test_format_missing_ctx_message_falls_back_when_namespace_unknown() {
        // Older bundles may not record the declaring namespace.
        let a = make_entry("legacy.key", None);
        let missing = vec![&a];
        let out = format_missing_ctx_message(&missing, false);
        assert!(out.contains("- legacy.key\n"));
        assert!(out.contains("hot ctx set legacy.key <value>"));
        assert!(!out.contains("(required by"));
    }

    /// Build a tiny in-memory zip that contains only a `manifest.hot` with the
    /// given body — mirroring the legacy bundle format that wrote ctx
    /// requirements as a flat `Vec<Str>` of keys. The detailed extractor
    /// must still understand this shape.
    fn build_manifest_zip(manifest_body: &str) -> Vec<u8> {
        use std::io::Write;
        use zip::ZipWriter;
        use zip::write::FileOptions;

        let buf: Vec<u8> = Vec::new();
        let cursor = std::io::Cursor::new(buf);
        let mut zip = ZipWriter::new(cursor);
        zip.start_file("manifest.hot", FileOptions::<()>::default())
            .unwrap();
        zip.write_all(manifest_body.as_bytes()).unwrap();
        zip.finish().unwrap().into_inner()
    }

    struct InMemoryBuildStorage {
        data: Vec<u8>,
    }

    #[async_trait::async_trait]
    impl crate::storage::BuildStorage for InMemoryBuildStorage {
        async fn store_build(
            &self,
            _build_id: &Uuid,
            _org_id: &Uuid,
            _env_id: &Uuid,
            _data: Vec<u8>,
        ) -> Result<String, String> {
            Ok("memory".to_string())
        }

        async fn retrieve_build(
            &self,
            _build_id: &Uuid,
            _org_id: &Uuid,
            _env_id: &Uuid,
        ) -> Result<Vec<u8>, String> {
            Ok(self.data.clone())
        }

        async fn exists(
            &self,
            _build_id: &Uuid,
            _org_id: &Uuid,
            _env_id: &Uuid,
        ) -> Result<bool, String> {
            Ok(true)
        }

        async fn delete_build(
            &self,
            _build_id: &Uuid,
            _org_id: &Uuid,
            _env_id: &Uuid,
        ) -> Result<(), String> {
            Ok(())
        }

        fn build_path(&self, _build_id: &Uuid, _org_id: &Uuid, _env_id: &Uuid) -> String {
            "memory".to_string()
        }

        fn storage_type(&self) -> &str {
            "memory"
        }
    }

    async fn attach_test_plan(
        db: &crate::db::DatabasePool,
        org_id: &Uuid,
        user_id: &Uuid,
        features: serde_json::Value,
    ) -> Uuid {
        let plan_uuid = Uuid::now_v7();
        match db {
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query(
                    "INSERT INTO plan
                     (plan_uuid, plan_id, plan_name, base_price_monthly_cents, base_price_annual_cents, sort_order, active, features)
                     VALUES (?, ?, ?, ?, ?, ?, 1, ?)",
                )
                .bind(plan_uuid)
                .bind(format!("test-plan-{}", plan_uuid.simple()))
                .bind("Test Plan")
                .bind(0)
                .bind(0)
                .bind(1)
                .bind(features.to_string())
                .execute(pool)
                .await
                .unwrap();
            }
            crate::db::DatabasePool::Postgres(_) => unreachable!(),
        }

        crate::db::OrgPlan::create(
            db,
            org_id,
            &plan_uuid,
            crate::db::BillingPeriod::Monthly,
            user_id,
        )
        .await
        .unwrap();

        plan_uuid
    }

    async fn update_test_plan_features(
        db: &crate::db::DatabasePool,
        plan_uuid: &Uuid,
        features: serde_json::Value,
    ) {
        match db {
            crate::db::DatabasePool::Sqlite(pool) => {
                sqlx::query("UPDATE plan SET features = ? WHERE plan_uuid = ?")
                    .bind(features.to_string())
                    .bind(plan_uuid)
                    .execute(pool)
                    .await
                    .unwrap();
            }
            crate::db::DatabasePool::Postgres(_) => unreachable!(),
        }
    }

    #[tokio::test]
    async fn test_validate_schedule_requirements_for_deploy_enforces_plan_interval() {
        let db = crate::db::test_db().await;
        let data = crate::db::insert_test_data(&db).await.unwrap();
        let build_id = Uuid::now_v7();
        crate::db::Build::insert_build(
            &db,
            &build_id,
            &data.project_id,
            "bundle-with-fast-schedule",
            1,
            crate::db::Build::BUILD_TYPE_BUNDLE,
            &data.user_id,
        )
        .await
        .unwrap();

        let plan_uuid = attach_test_plan(
            &db,
            &data.org_id,
            &data.user_id,
            serde_json::json!({
                "schedule_min_interval_secs": 300,
                "schedule_min_delay_secs": 0,
                "active_schedules_per_org": -1
            }),
        )
        .await;

        let manifest = r#"{"hot.bundle.demo": {"scheduled_functions": {
            "every second": [{"fn": "::demo/tick", "meta": {}, "file": null, "line": null, "column": null, "position": null}]
        }}}"#;
        let storage = InMemoryBuildStorage {
            data: build_manifest_zip(manifest),
        };
        let conf = crate::val!({
            "schedule": {
                "min-interval-seconds": 1,
                "min-delay-seconds": 0,
                "max-active-per-org": -1
            }
        });

        let err = validate_schedule_requirements_for_deploy(
            &db,
            &build_id,
            &data.org_id,
            &data.env_id,
            &conf,
            &storage,
        )
        .await
        .unwrap_err();
        assert!(err.contains("violates schedule interval policy"));
        assert!(err.contains("minimum of 300"));

        update_test_plan_features(
            &db,
            &plan_uuid,
            serde_json::json!({
                "schedule_min_interval_secs": 1,
                "schedule_min_delay_secs": 0,
                "active_schedules_per_org": -1
            }),
        )
        .await;

        validate_schedule_requirements_for_deploy(
            &db,
            &build_id,
            &data.org_id,
            &data.env_id,
            &conf,
            &storage,
        )
        .await
        .unwrap();
    }

    #[test]
    fn test_extract_ctx_requirements_detailed_legacy_vec_str_shape() {
        // Legacy shape: `ctx_requirements` is a Vec<Str> of bare keys.
        let manifest = r#"{"hot.bundle.demo": {"ctx_requirements": ["a.key", "b.key"]}}"#;
        let zip = build_manifest_zip(manifest);
        let entries = extract_ctx_requirements_detailed_from_build(&zip).unwrap();
        let keys: Vec<&str> = entries.iter().map(|e| e.key.as_str()).collect();
        assert!(keys.contains(&"a.key"));
        assert!(keys.contains(&"b.key"));
        // Legacy shape carries no namespace/source info.
        assert!(entries.iter().all(|e| e.declared_by.is_none()));
    }

    #[test]
    fn test_extract_ctx_requirements_detailed_new_map_shape() {
        // New shape: each entry is a Map with key + declared-by + source-file.
        let manifest = r#"{"hot.bundle.demo": {"ctx_requirements": [
            {"key": "slack.api.key", "declared-by": "::slack::api/request", "source-file": "pkg/slack/api.hot"},
            {"key": "openai.api.key", "declared-by": "::openai::chat/completions"}
        ]}}"#;
        let zip = build_manifest_zip(manifest);
        let entries = extract_ctx_requirements_detailed_from_build(&zip).unwrap();

        let slack = entries.iter().find(|e| e.key == "slack.api.key").unwrap();
        assert_eq!(slack.declared_by.as_deref(), Some("::slack::api/request"));
        assert_eq!(slack.source_file.as_deref(), Some("pkg/slack/api.hot"));

        let openai = entries.iter().find(|e| e.key == "openai.api.key").unwrap();
        assert_eq!(
            openai.declared_by.as_deref(),
            Some("::openai::chat/completions")
        );
        assert!(openai.source_file.is_none());
    }

    #[test]
    fn test_extract_ctx_requirements_from_build_returns_keys_for_both_shapes() {
        // The legacy `extract_ctx_requirements_from_build` API must keep
        // returning a flat AHashSet<String> regardless of which manifest
        // shape is on disk.
        let legacy = build_manifest_zip(r#"{"hot.bundle.demo": {"ctx_requirements": ["x.k"]}}"#);
        let new = build_manifest_zip(
            r#"{"hot.bundle.demo": {"ctx_requirements": [{"key": "y.k", "declared-by": "::p/f"}]}}"#,
        );
        let legacy_keys = extract_ctx_requirements_from_build(&legacy).unwrap();
        let new_keys = extract_ctx_requirements_from_build(&new).unwrap();
        assert!(legacy_keys.contains("x.k"));
        assert!(new_keys.contains("y.k"));
    }

    #[test]
    fn test_calculate_build_hash() {
        let files = vec![
            BundleFile {
                path: PathBuf::from("test.hot"),
                relative_path: "test.hot".to_string(),
                content: b"test content".to_vec(),
                hash: "abc123".to_string(),
                size: 12,
            },
            BundleFile {
                path: PathBuf::from("test2.hot"),
                relative_path: "test2.hot".to_string(),
                content: b"test content 2".to_vec(),
                hash: "def456".to_string(),
                size: 14,
            },
        ];

        let hash = calculate_build_hash(&files);
        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 64); // Blake3 hash is 64 hex characters (32 bytes)
    }

    fn write_dummy_zip(dir: &Path, name: &str, bytes: usize) -> PathBuf {
        let p = dir.join(name);
        let blob = vec![0u8; bytes];
        std::fs::write(&p, &blob).unwrap();
        p
    }

    fn conf_with_max(max: i64) -> crate::val::Val {
        use crate::val::Val;
        Val::map_empty().set("build.file.max-bytes", Val::Int(max))
    }

    #[test]
    fn test_check_bundle_size_under_limit_ok() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = write_dummy_zip(tmp.path(), "out.zip", 100);
        let conf = conf_with_max(10_000);
        assert!(check_bundle_size(&conf, &zip, &[], &[]).is_ok());
    }

    #[test]
    fn test_check_bundle_size_over_limit_errors_with_top_n() {
        let tmp = tempfile::tempdir().unwrap();
        let zip = write_dummy_zip(tmp.path(), "out.zip", 5_000);
        let conf = conf_with_max(1_000);
        let files = vec![
            BundleFile {
                path: PathBuf::from("a.hot"),
                relative_path: "a.hot".to_string(),
                content: vec![0; 800],
                hash: "h1".into(),
                size: 800,
            },
            BundleFile {
                path: PathBuf::from("b.hot"),
                relative_path: "b.hot".to_string(),
                content: vec![0; 100],
                hash: "h2".into(),
                size: 100,
            },
        ];
        let resources = vec![crate::bundle::BundleResource {
            rel_path: "data/big.bin".to_string(),
            abs_path: PathBuf::from("/tmp/big.bin"),
            content: vec![0; 4_000],
            hash: "rh".into(),
            size: 4_000,
        }];
        let err = check_bundle_size(&conf, &zip, &files, &resources).unwrap_err();
        assert!(
            err.contains("local override `hot.build.file.max-bytes`"),
            "expected message to attribute limit to local override, got: {}",
            err
        );
        // Top entry must be the largest item (the resource).
        assert!(err.contains("data/big.bin"));
        assert!(err.contains("[resource]"));
        assert!(err.contains("[src]"));
    }

    #[test]
    fn test_check_bundle_size_falls_back_to_remote_default_when_unset() {
        let tmp = tempfile::tempdir().unwrap();
        // Negative value in conf means "no local override" — we should
        // still pre-flight against the documented remote default.
        let conf = conf_with_max(-1);

        // Well under the 100 MB default: passes.
        let small = write_dummy_zip(tmp.path(), "small.zip", 10_000);
        assert!(check_bundle_size(&conf, &small, &[], &[]).is_ok());

        // Just over the 100 MB default: fails with a remote-attributed
        // message so the user knows their local conf isn't the cause.
        let big_size = (DEFAULT_REMOTE_BUILD_MAX_BYTES + 1) as usize;
        let big = write_dummy_zip(tmp.path(), "big.zip", big_size);
        let err = check_bundle_size(&conf, &big, &[], &[]).unwrap_err();
        assert!(
            err.contains("Hot Cloud upload limit"),
            "expected message to attribute limit to remote default, got: {}",
            err
        );
        assert!(
            err.contains("413"),
            "expected remediation hint to mention the 413 response, got: {}",
            err
        );
    }

    #[test]
    fn test_check_bundle_size_warn_at_80pct_does_not_fail() {
        let tmp = tempfile::tempdir().unwrap();
        // 850 / 1000 = 85% → warn but ok
        let zip = write_dummy_zip(tmp.path(), "out.zip", 850);
        let conf = conf_with_max(1_000);
        assert!(check_bundle_size(&conf, &zip, &[], &[]).is_ok());
    }

    #[test]
    fn test_top_largest_entries_orders_by_size_desc() {
        let files = vec![BundleFile {
            path: PathBuf::from("small.hot"),
            relative_path: "small.hot".into(),
            content: vec![],
            hash: "h".into(),
            size: 50,
        }];
        let resources = vec![
            crate::bundle::BundleResource {
                rel_path: "big".into(),
                abs_path: PathBuf::new(),
                content: vec![],
                hash: "h".into(),
                size: 1_000,
            },
            crate::bundle::BundleResource {
                rel_path: "mid".into(),
                abs_path: PathBuf::new(),
                content: vec![],
                hash: "h".into(),
                size: 500,
            },
        ];
        let top = top_largest_entries(&files, &resources, 10);
        assert_eq!(top.len(), 3);
        assert_eq!(top[0].path, "big");
        assert_eq!(top[1].path, "mid");
        assert_eq!(top[2].path, "small.hot");
    }

    #[test]
    fn test_human_bytes() {
        assert_eq!(human_bytes(0), "0 B");
        assert_eq!(human_bytes(1023), "1023 B");
        assert_eq!(human_bytes(1024), "1.0 KiB");
        assert_eq!(human_bytes(1024 * 1024), "1.0 MiB");
    }
}
