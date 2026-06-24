use crate::discovery::{DiscoveryOpts, discover};
use crate::hasher::HotHasher;
use crate::val::{Val, val};
use ahash::{AHashMap, AHashSet};
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use uuid::Uuid;
use walkdir::WalkDir;
use zip::write::FileOptions;
use zip::{CompressionMethod, ZipArchive, ZipWriter};

/// A file in a bundle, either source code or cache
#[derive(Debug, Clone)]
pub struct BundleFile {
    pub path: PathBuf,
    pub relative_path: String,
    pub content: Vec<u8>,
    pub hash: String, // Blake3 hash
    pub size: u64,
}

/// A resource file shipped alongside source files. Resources are addressed by
/// their `rel_path` from the resource registry root (e.g. `"prompts/system.md"`)
/// and stored in the bundle under `resources/<rel_path>`. The `hash` is
/// blake3 of the file contents.
#[derive(Debug, Clone)]
pub struct BundleResource {
    /// User-facing path passed to `::hot::resource/load(rel)`. Always uses
    /// forward slashes for portability.
    pub rel_path: String,
    /// Absolute path on the build machine (only meaningful at build time).
    pub abs_path: PathBuf,
    pub content: Vec<u8>,
    pub hash: String,
    pub size: u64,
}

/// Standard prefix used for resource files inside the bundle zip.
pub const RESOURCE_BUNDLE_PREFIX: &str = "resources";

/// A Hot bundle containing source files, cache files, resources, and metadata
#[derive(Debug, Clone)]
pub struct Bundle {
    pub name: String,
    pub bundle_id: String,
    pub files: Vec<BundleFile>,
    pub cache_files: Vec<BundleFile>,
    pub resources: Vec<BundleResource>,
    pub manifest: Val,
    pub bundle_hash: String,
}

/// Collect all `.hot` files from the given paths with the specified bundle
/// path prefix.
///
/// Discovery routes through [`crate::discovery::discover`], so `.gitignore`,
/// `.hotignore`, and the default hard-excludes (`target/`, `node_modules/`,
/// `.hot/`, …) are honored. Each root is walked independently and the
/// resulting bundle paths are `"{bundle_prefix}/{rel_path_in_root}"`.
/// Files that resolve to the same canonical filesystem path across roots are
/// deduplicated.
pub fn collect_hot_files_with_prefix(
    paths: &[String],
    bundle_prefix: &str,
) -> Result<Vec<BundleFile>, String> {
    let mut files = Vec::new();
    let mut seen_canonical: AHashSet<PathBuf> = AHashSet::new();
    let opts = DiscoveryOpts::for_extension("hot");

    for path_str in paths {
        let root = Path::new(path_str);
        if !root.exists() {
            continue;
        }

        for found in discover(&[root], &opts) {
            let canonical_path = match found.abs_path.canonicalize() {
                Ok(p) => p,
                Err(_) => continue,
            };
            if !seen_canonical.insert(canonical_path) {
                continue;
            }

            let content = fs::read(&found.abs_path)
                .map_err(|e| format!("Failed to read file {}: {}", found.abs_path.display(), e))?;
            let size = content.len() as u64;
            let hash = HotHasher::hash_content(&content);
            let bundle_path = format!("{}/{}", bundle_prefix, found.rel_path);

            files.push(BundleFile {
                path: found.abs_path,
                relative_path: bundle_path,
                content,
                hash,
                size,
            });
        }
    }

    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(files)
}

/// Collect all .hot files from the given paths (legacy function for backward compatibility)
pub fn collect_hot_files(paths: &[String]) -> Result<Vec<BundleFile>, String> {
    collect_hot_files_with_prefix(paths, "")
}

/// Collect resource files from the given roots for inclusion in a bundle.
///
/// Walks each root through the shared resource registry builder so the
/// `.gitignore` / `.hotignore` / first-root-wins / `extra_excludes` semantics
/// match what the live `::hot::resource/*` namespace sees in `hot dev`,
/// then reads + hashes each file. The returned list is sorted by `rel_path`
/// for deterministic bundle output.
pub fn collect_resource_files(
    roots: &[PathBuf],
    respect_gitignore: bool,
    extra_excludes: &[String],
) -> Result<Vec<BundleResource>, String> {
    let registry =
        crate::lang::hot::resource::build_registry(roots, respect_gitignore, extra_excludes);
    let mut out = Vec::with_capacity(registry.entries.len());

    for entry in registry.entries.values() {
        let content = fs::read(&entry.abs_path).map_err(|e| {
            format!(
                "Failed to read resource file {}: {}",
                entry.abs_path.display(),
                e
            )
        })?;
        let size = content.len() as u64;
        let hash = HotHasher::hash_content(&content);
        out.push(BundleResource {
            rel_path: entry.rel_path.clone(),
            abs_path: entry.abs_path.clone(),
            content,
            hash,
            size,
        });
    }

    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    Ok(out)
}

/// Collect package source files from a single package root, honoring that
/// package's declared `src-paths` from `pkg.hot`.
pub fn collect_package_source_files_from_root(
    package_root: &Path,
    bundle_prefix: &str,
) -> Result<Vec<BundleFile>, String> {
    let mut files = Vec::new();
    let src_dirs = crate::lang::engine::Engine::discover_dependency_source_dirs(package_root)?;

    for src_dir in &src_dirs {
        let src_path = Path::new(src_dir);
        let src_relative = src_path.strip_prefix(package_root).unwrap_or(src_path);
        let prefix = format!("{}/{}", bundle_prefix, src_relative.to_string_lossy());
        let collected = collect_hot_files_with_prefix(std::slice::from_ref(src_dir), &prefix)?;
        files.extend(collected);
    }

    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(files)
}

/// Return resource roots declared by a package's `pkg.hot`.
///
/// Package resources are opt-in: packages without `pkg.hot` or without
/// resource paths contribute no resources to the bundle.
pub fn collect_package_resource_roots_from_root(
    package_root: &Path,
) -> Result<Vec<PathBuf>, String> {
    let pkg_hot_path = package_root.join("pkg.hot");
    if !pkg_hot_path.exists() {
        return Ok(Vec::new());
    }

    let meta = crate::lang::project::PackageMetadata::parse_from_file(&pkg_hot_path)?;
    Ok(meta
        .resource_paths
        .into_iter()
        .map(|path| package_root.join(path))
        .filter(|path| path.exists())
        .collect())
}

/// Collect package source files by reading each package's pkg.hot to determine src-paths.
/// This ensures only declared source files are bundled, excluding test files.
pub fn collect_package_source_files(pkg_paths: &[String]) -> Result<Vec<BundleFile>, String> {
    let mut files = Vec::new();

    for pkg_path_str in pkg_paths {
        let pkg_path = Path::new(pkg_path_str);
        if !pkg_path.exists() {
            continue;
        }

        // Each pkg_path is a directory containing package subdirectories.
        // Walk one level to find package roots (directories containing pkg.hot).
        let entries: Vec<_> = std::fs::read_dir(pkg_path)
            .map_err(|e| format!("Failed to read pkg directory {}: {}", pkg_path.display(), e))?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();

        for entry in entries {
            let package_root = entry.path();
            let pkg_dir_name = package_root.strip_prefix(pkg_path).unwrap_or(&package_root);
            let bundle_prefix = format!("hot/pkg/{}", pkg_dir_name.to_string_lossy());
            let collected = collect_package_source_files_from_root(&package_root, &bundle_prefix)?;
            files.extend(collected);
        }
    }

    files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(files)
}

/// Collect package resource roots from package directories under the supplied
/// package parent paths.
pub fn collect_package_resource_roots(pkg_paths: &[String]) -> Result<Vec<PathBuf>, String> {
    let mut roots = Vec::new();

    for pkg_path_str in pkg_paths {
        let pkg_path = Path::new(pkg_path_str);
        if !pkg_path.exists() {
            continue;
        }

        let entries: Vec<_> = std::fs::read_dir(pkg_path)
            .map_err(|e| format!("Failed to read pkg directory {}: {}", pkg_path.display(), e))?
            .filter_map(|e| e.ok())
            .filter(|e| e.path().is_dir())
            .collect();

        for entry in entries {
            roots.extend(collect_package_resource_roots_from_root(&entry.path())?);
        }
    }

    Ok(roots)
}

/// Calculate the bundle Blake3 hash from all file hashes
pub fn calculate_bundle_hash(files: &[BundleFile]) -> String {
    let mut hasher = HotHasher::new();
    for file in files {
        hasher.update(file.hash.as_bytes());
    }
    hasher.finalize()
}

/// Calculate the bundle Blake3 hash from both source files *and* resources.
///
/// Resources are folded into the hash so that changes to a bundled prompt,
/// asset, or template produce a new bundle hash and trigger a redeploy. The
/// resource portion is fed in after the source-file portion in `rel_path`
/// order to keep the hash stable.
pub fn calculate_bundle_hash_with_resources(
    files: &[BundleFile],
    resources: &[BundleResource],
) -> String {
    let mut hasher = HotHasher::new();
    for file in files {
        hasher.update(file.hash.as_bytes());
    }
    for r in resources {
        hasher.update(r.rel_path.as_bytes());
        hasher.update(r.hash.as_bytes());
    }
    hasher.finalize()
}

struct ManifestSourcePathNormalizer {
    by_path: AHashMap<String, String>,
}

impl ManifestSourcePathNormalizer {
    fn new(files: &[BundleFile], resources: &[BundleResource]) -> Self {
        let mut by_path = AHashMap::new();

        for file in files {
            let bundle_path = normalize_path_separators(&file.relative_path);
            insert_manifest_source_path(&mut by_path, &file.path, bundle_path);
        }

        for resource in resources {
            let bundle_path = normalize_path_separators(&format!(
                "{RESOURCE_BUNDLE_PREFIX}/{}",
                resource.rel_path
            ));
            insert_manifest_source_path(&mut by_path, &resource.abs_path, bundle_path);
        }

        Self { by_path }
    }

    fn normalize(&self, path: &str) -> String {
        let normalized = normalize_path_separators(path);
        if let Some(bundle_path) = self.by_path.get(&normalized) {
            return bundle_path.clone();
        }

        if let Ok(canonical) = Path::new(path).canonicalize() {
            let canonical = normalize_path_separators(&canonical.to_string_lossy());
            if let Some(bundle_path) = self.by_path.get(&canonical) {
                return bundle_path.clone();
            }
        }

        path.to_string()
    }
}

fn insert_manifest_source_path(
    by_path: &mut AHashMap<String, String>,
    source_path: &Path,
    bundle_path: String,
) {
    by_path.insert(bundle_path.clone(), bundle_path.clone());
    by_path.insert(
        normalize_path_separators(&source_path.to_string_lossy()),
        bundle_path.clone(),
    );

    if let Ok(canonical) = source_path.canonicalize() {
        by_path.insert(
            normalize_path_separators(&canonical.to_string_lossy()),
            bundle_path,
        );
    }
}

fn normalize_path_separators(path: &str) -> String {
    path.replace('\\', "/")
}

fn normalize_manifest_artifact_source_path(
    val: &Val,
    normalizer: &ManifestSourcePathNormalizer,
) -> Val {
    let mut val = val.clone();
    if let Val::Map(map) = &mut val {
        normalize_manifest_source_field(map, "file", normalizer);
    }
    val
}

fn normalize_manifest_source_field(
    map: &mut indexmap::IndexMap<Val, Val>,
    field: &str,
    normalizer: &ManifestSourcePathNormalizer,
) {
    if let Some(value) = map.get_mut(&Val::from(field))
        && let Val::Str(path) = value
    {
        *value = Val::from(normalizer.normalize(path));
    }
}

/// Create the manifest.hot content
#[allow(clippy::too_many_arguments)]
pub fn create_manifest(
    bundle_name: &str,
    bundle_id: &str,
    files: &[BundleFile],
    bundle_hash: &str,
    event_handlers: &crate::lang::compiler::EventHandlers,
    scheduled_functions: &crate::lang::compiler::ScheduledFunctions,
    mcp_tools: &crate::lang::compiler::McpTools,
    webhooks: &crate::lang::compiler::Webhooks,
    agents: &crate::lang::compiler::AgentDefs,
    workflows: &crate::lang::compiler::WorkflowDefs,
    send_targets: &crate::lang::compiler::SendTargets,
    ctx_requirements: Option<&crate::lang::compiler::ctx_checker::ProgramCtxRequirements>,
    box_requirements: Option<&crate::lang::compiler::box_checker::ProgramBoxRequirements>,
    resources: &[BundleResource],
) -> Val {
    let source_path_normalizer = ManifestSourcePathNormalizer::new(files, resources);
    let mut files_entries = Vec::new();

    for file in files {
        let file_info = val!({
            "path": file.relative_path.clone(),
            "hash": file.hash.clone(),
            "size": file.content.len() as i64,
        });
        files_entries.push(file_info);
    }

    // Convert event handlers to manifest format - organize by event type
    let mut handlers_map = indexmap::IndexMap::new();
    for (event_type, handlers) in event_handlers {
        let mut handler_list = Vec::new();
        for handler in handlers {
            // The event_handler field is already a Val, so we can use it directly
            handler_list.push(normalize_manifest_artifact_source_path(
                &handler.event_handler,
                &source_path_normalizer,
            ));
        }
        handlers_map.insert(Val::from(event_type.clone()), Val::Vec(handler_list));
    }
    let handlers_entries = Val::Map(Box::new(handlers_map));

    // Convert scheduled functions to manifest format - organize by cron expression
    let mut schedules_map = indexmap::IndexMap::new();
    for (cron_expr, functions) in scheduled_functions {
        let mut function_list = Vec::new();
        for function in functions {
            // The scheduled_function field is already a Val, so we can use it directly
            function_list.push(normalize_manifest_artifact_source_path(
                &function.scheduled_function,
                &source_path_normalizer,
            ));
        }
        schedules_map.insert(Val::from(cron_expr.clone()), Val::Vec(function_list));
    }
    let schedules_entries = Val::Map(Box::new(schedules_map));

    // Convert MCP tools to manifest format - organize by service
    let mut mcp_tools_map = indexmap::IndexMap::new();
    for (service, tools) in mcp_tools {
        let mut tool_list = Vec::new();
        for tool in tools {
            tool_list.push(normalize_manifest_artifact_source_path(
                &tool.mcp_tool,
                &source_path_normalizer,
            ));
        }
        mcp_tools_map.insert(Val::from(service.clone()), Val::Vec(tool_list));
    }
    let mcp_tools_entries = Val::Map(Box::new(mcp_tools_map));

    // Convert webhooks to manifest format - organize by service
    let mut webhooks_map = indexmap::IndexMap::new();
    for (service, entries) in webhooks {
        let mut entry_list = Vec::new();
        for entry in entries {
            entry_list.push(normalize_manifest_artifact_source_path(
                &entry.webhook,
                &source_path_normalizer,
            ));
        }
        webhooks_map.insert(Val::from(service.clone()), Val::Vec(entry_list));
    }
    let webhooks_entries = Val::Map(Box::new(webhooks_map));

    // Convert ctx_requirements to manifest format.
    //
    // Each entry is a Map: `{key, declared-by, source-file}` so deploy-time
    // tooling can show actionable warnings ("required by ::slack::api/request
    // — set with: hot ctx set slack.api.key <value>"). We keep just the
    // `required: true` keys here; defaults/optional keys don't gate deploy.
    //
    // NOTE: older bundles wrote a flat `Vec<Str>` of keys. Readers must
    // accept both shapes; see `extract_ctx_requirements_from_build`.
    let ctx_requirements_entries: Vec<Val> = ctx_requirements
        .map(|reqs| {
            let mut entries: Vec<Val> = Vec::new();
            for ns in &reqs.namespaces {
                for k in ns.required_keys() {
                    let mut entry = indexmap::IndexMap::new();
                    entry.insert(Val::from("key"), Val::from(k.key.as_str()));
                    entry.insert(Val::from("declared-by"), Val::from(ns.namespace.as_str()));
                    if let Some(src) = &ns.source_file {
                        entry.insert(
                            Val::from("source-file"),
                            Val::from(source_path_normalizer.normalize(src)),
                        );
                    }
                    entries.push(Val::Map(Box::new(entry)));
                }
            }
            entries.sort_by(|a, b| {
                let ak = a.get("key").map(|v| v.to_string()).unwrap_or_default();
                let bk = b.get("key").map(|v| v.to_string()).unwrap_or_default();
                ak.cmp(&bk)
            });
            entries
        })
        .unwrap_or_default();

    // Convert box_requirements to manifest format
    let box_requirements_val = if let Some(box_reqs) = box_requirements {
        let mut box_map = indexmap::IndexMap::new();
        if let Some(min_size) = box_reqs.effective_min_size() {
            box_map.insert(Val::from("min_size"), Val::from(min_size.as_str()));
        }
        if box_reqs.requires_network() {
            box_map.insert(Val::from("network"), Val::Bool(true));
        }
        // Include per-function details for debugging/reporting
        let fn_reqs: Vec<Val> = box_reqs
            .requirements
            .iter()
            .map(|r| {
                let mut entry = indexmap::IndexMap::new();
                entry.insert(Val::from("fn"), Val::from(r.fqn.as_str()));
                if let Some(ref size) = r.requirement.min_size {
                    entry.insert(Val::from("min_size"), Val::from(size.as_str()));
                }
                if let Some(net) = r.requirement.network {
                    entry.insert(Val::from("network"), Val::Bool(net));
                }
                if let Some(ref file) = r.source_file {
                    entry.insert(
                        Val::from("file"),
                        Val::from(source_path_normalizer.normalize(file)),
                    );
                }
                Val::Map(Box::new(entry))
            })
            .collect();
        if !fn_reqs.is_empty() {
            box_map.insert(Val::from("functions"), Val::Vec(fn_reqs));
        }
        Val::Map(Box::new(box_map))
    } else {
        Val::Map(Box::new(indexmap::IndexMap::new()))
    };

    // Convert agents to manifest format — flat Vec of agent_val maps
    let agents_entries: Vec<Val> = agents
        .iter()
        .map(|a| {
            normalize_manifest_artifact_source_path(
                &a.agent_val.resolve_boxes(),
                &source_path_normalizer,
            )
        })
        .collect();

    // Convert named workflows to manifest format — flat Vec of workflow_val maps
    let workflows_entries: Vec<Val> = workflows
        .iter()
        .map(|w| {
            normalize_manifest_artifact_source_path(
                &w.workflow_val.resolve_boxes(),
                &source_path_normalizer,
            )
        })
        .collect();

    // Convert send targets to manifest format: { "ns/var": ["event-a", "event-b"] }
    let mut send_targets_map = indexmap::IndexMap::new();
    for (fn_key, targets) in send_targets {
        let event_names: Vec<Val> = targets
            .iter()
            .map(|t| Val::from(t.event_name.as_str()))
            .collect();
        if !event_names.is_empty() {
            send_targets_map.insert(Val::from(fn_key.as_str()), Val::Vec(event_names));
        }
    }
    let send_targets_entries = Val::Map(Box::new(send_targets_map));

    // Convert resources to manifest format. Each entry carries the
    // user-facing rel-path (the same key that `::hot::resource/load(rel)`
    // takes), the blake3 content hash, and the byte size. The runtime uses
    // this list to populate the resource registry from the extracted bundle.
    let resources_entries: Vec<Val> = resources
        .iter()
        .map(|r| {
            val!({
                "path": r.rel_path.clone(),
                "hash": r.hash.clone(),
                "size": r.size as i64,
            })
        })
        .collect();

    let bundle_info = val!({
        "bundle_id": bundle_id,
        "bundle_hash": bundle_hash,
        "engine_version": crate::build_info::VERSION,
        "hot_std_version": crate::build_info::VERSION,
        "files": files_entries,
        "resources": resources_entries,
        "event_handlers": handlers_entries,
        "scheduled_functions": schedules_entries,
        "mcp_tools": mcp_tools_entries,
        "webhooks": webhooks_entries,
        "agents": agents_entries,
        "workflows": workflows_entries,
        "send_targets": send_targets_entries,
        "ctx_requirements": ctx_requirements_entries,
        "box_requirements": box_requirements_val,
        "created_at": chrono::Utc::now().to_rfc3339(),
    });

    // Create the full manifest with dynamic key
    let bundle_key = format!("hot.bundle.{}", bundle_name);
    let manifest_entries = vec![(Val::from(bundle_key.as_str()), bundle_info)];
    Val::from(manifest_entries)
}

/// Create a bundle from the given files with standardized directory structure.
///
/// `resource_roots` are walked through the shared resource registry builder
/// (honoring `respect_gitignore` + `extra_excludes`) and bundled under the
/// [`RESOURCE_BUNDLE_PREFIX`] directory in the zip. Pass an empty slice to
/// skip resource bundling.
pub fn create_bundle(
    bundle_name: &str,
    src_paths: &[String],
    pkg_paths: &[String],
    resource_roots: &[PathBuf],
    respect_gitignore: bool,
    extra_excludes: &[String],
) -> Result<Bundle, String> {
    let mut all_files = Vec::new();

    let bundle_id = Uuid::now_v7().to_string();

    if !src_paths.is_empty() {
        let src_files = collect_hot_files_with_prefix(src_paths, "hot/src")?;
        all_files.extend(src_files);
    }

    if !pkg_paths.is_empty() {
        let pkg_files = collect_package_source_files(pkg_paths)?;
        all_files.extend(pkg_files);
    }

    if all_files.is_empty() {
        return Err("No .hot files found in the specified paths".to_string());
    }

    all_files.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));

    let mut all_resource_roots = resource_roots.to_vec();
    if !pkg_paths.is_empty() {
        all_resource_roots.extend(collect_package_resource_roots(pkg_paths)?);
    }

    let resources = if all_resource_roots.is_empty() {
        Vec::new()
    } else {
        collect_resource_files(&all_resource_roots, respect_gitignore, extra_excludes)?
    };

    let extracted = crate::lang::engine::Engine::extract_handlers_and_scheduled_functions(
        src_paths, None, None, false,
    )
    .map_err(|e| format!("Compilation error: {}", e))?;

    let cache_files = Vec::new();

    let bundle_hash = calculate_bundle_hash_with_resources(&all_files, &resources);
    let manifest = create_manifest(
        bundle_name,
        &bundle_id,
        &all_files,
        &bundle_hash,
        &extracted.event_handlers,
        &extracted.scheduled_functions,
        &extracted.mcp_tools,
        &extracted.webhooks,
        &extracted.agents,
        &extracted.workflows,
        &extracted.send_targets,
        Some(&extracted.ctx_requirements),
        Some(&extracted.box_requirements),
        &resources,
    );

    if resources.is_empty() {
        println!("📦 Bundle created with {} source files", all_files.len());
    } else {
        println!(
            "📦 Bundle created with {} source files and {} resources",
            all_files.len(),
            resources.len()
        );
    }

    Ok(Bundle {
        name: bundle_name.to_string(),
        bundle_id,
        files: all_files,
        cache_files,
        resources,
        manifest,
        bundle_hash,
    })
}

/// Write the bundle to a zip file with zstd compression
pub fn write_bundle_zip(bundle: &Bundle, bundle_dir: &Path) -> Result<PathBuf, String> {
    // Ensure bundle directory exists
    if let Err(e) = fs::create_dir_all(bundle_dir) {
        return Err(format!("Failed to create bundle directory: {}", e));
    }

    // Create zip file path with format: bundle-name-<bundle_id(12,no-dashes)>-<hash(8)>.hot.zip
    let bundle_id_no_dashes = bundle.bundle_id.replace('-', "");
    let bundle_id_short = if bundle_id_no_dashes.len() >= 12 {
        &bundle_id_no_dashes[..12]
    } else {
        &bundle_id_no_dashes
    };
    let hash_short = if bundle.bundle_hash.len() >= 8 {
        &bundle.bundle_hash[..8]
    } else {
        &bundle.bundle_hash
    };

    let zip_filename = format!("{}-{}-{}.hot.zip", bundle.name, bundle_id_short, hash_short);
    let zip_path = bundle_dir.join(&zip_filename);

    // Create zip file
    let zip_file = match fs::File::create(&zip_path) {
        Ok(file) => file,
        Err(e) => return Err(format!("Failed to create zip file: {}", e)),
    };

    let mut zip = ZipWriter::new(zip_file);

    // Set up file options with zstd compression
    let options = FileOptions::<()>::default().compression_method(CompressionMethod::Zstd);

    // Add manifest.hot to the zip
    let manifest_content = bundle.manifest.pretty_print();
    if let Err(e) = zip.start_file("manifest.hot", options) {
        return Err(format!("Failed to start manifest file in zip: {}", e));
    }
    if let Err(e) = zip.write_all(manifest_content.as_bytes()) {
        return Err(format!("Failed to write manifest to zip: {}", e));
    }

    // Add all bundle files to the zip
    for file in &bundle.files {
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

    for cache_file in &bundle.cache_files {
        if let Err(e) = zip.start_file(&cache_file.relative_path, options) {
            return Err(format!(
                "Failed to start cache file {} in zip: {}",
                cache_file.relative_path, e
            ));
        }
        if let Err(e) = zip.write_all(&cache_file.content) {
            return Err(format!(
                "Failed to write cache file {} to zip: {}",
                cache_file.relative_path, e
            ));
        }
    }

    // Add all resource files to the zip under `resources/<rel-path>`. The
    // runtime resource registry uses the same `<rel-path>` key so loaders
    // like `::hot::resource/load("prompts/system.md")` resolve to the
    // extracted bundle path transparently.
    for r in &bundle.resources {
        let zip_path_str = format!("{}/{}", RESOURCE_BUNDLE_PREFIX, r.rel_path);
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

    if let Err(e) = zip.finish() {
        return Err(format!("Failed to finish zip file: {}", e));
    }

    println!("✅ Bundle zip created: {}", zip_path.display());
    if !bundle.cache_files.is_empty() {
        println!(
            "   Includes {} cache files for faster deployment",
            bundle.cache_files.len()
        );
    }
    if !bundle.resources.is_empty() {
        println!("   Includes {} resource(s)", bundle.resources.len());
    }

    Ok(zip_path)
}

/// Main bundle creation function.
///
/// `resource_roots`, `respect_gitignore`, and `extra_excludes` mirror the
/// shape of the project-conf inputs the CLI passes from
/// `hot.project.<x>.resources.paths` / `.respect-gitignore` /
/// `.ignore.excludes`. Pass an empty `resource_roots` to skip resource
/// bundling entirely.
pub fn bundle_create(
    bundle_name: &str,
    src_paths: &[String],
    pkg_paths: &[String],
    bundle_dir: Option<&str>,
    resource_roots: &[PathBuf],
    respect_gitignore: bool,
    extra_excludes: &[String],
) -> Result<PathBuf, String> {
    let bundle_dir = bundle_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(".hot/build"));

    println!("Creating bundle '{}'...", bundle_name);
    println!("  Source paths: {:?}", src_paths);
    println!("  Package paths: {:?}", pkg_paths);
    if !resource_roots.is_empty() {
        println!("  Resource paths: {:?}", resource_roots);
    }
    println!("  Bundle directory: {}", bundle_dir.display());

    let bundle = create_bundle(
        bundle_name,
        src_paths,
        pkg_paths,
        resource_roots,
        respect_gitignore,
        extra_excludes,
    )?;

    println!("  Found {} files", bundle.files.len());
    if !bundle.resources.is_empty() {
        println!("  Found {} resource(s)", bundle.resources.len());
    }
    println!("  Bundle Hash: {}", bundle.bundle_hash);

    let zip_path = write_bundle_zip(&bundle, &bundle_dir)?;

    println!("  Created bundle: {}", zip_path.display());

    Ok(zip_path)
}

/// Check if a directory is safe to extract to (non-existent or empty)
pub fn is_safe_extract_dir(dir: &Path) -> Result<bool, String> {
    if !dir.exists() {
        return Ok(true); // Non-existent directory is safe
    }

    if !dir.is_dir() {
        return Err(format!("{} exists but is not a directory", dir.display()));
    }

    // Check if directory is empty
    match fs::read_dir(dir) {
        Ok(mut entries) => {
            if entries.next().is_none() {
                Ok(true) // Empty directory is safe
            } else {
                Ok(false) // Directory has contents
            }
        }
        Err(e) => Err(format!("Failed to read directory {}: {}", dir.display(), e)),
    }
}

/// Extract bundle metadata from a zip file
pub fn get_bundle_metadata(zip_path: &Path) -> Result<(String, String), String> {
    let file = match fs::File::open(zip_path) {
        Ok(file) => file,
        Err(e) => return Err(format!("Failed to open zip file: {}", e)),
    };

    let mut archive = match ZipArchive::new(file) {
        Ok(archive) => archive,
        Err(e) => return Err(format!("Failed to read zip archive: {}", e)),
    };

    // Read the manifest.hot file
    let mut manifest_file = match archive.by_name("manifest.hot") {
        Ok(file) => file,
        Err(_) => return Err("Bundle does not contain manifest.hot".to_string()),
    };

    let mut manifest_content = String::new();
    if let Err(e) = manifest_file.read_to_string(&mut manifest_content) {
        return Err(format!("Failed to read manifest.hot: {}", e));
    }

    // Try to extract bundle name and hash from the zip filename as fallback
    let zip_filename = zip_path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown");

    // Parse the filename pattern: bundle-name-bundle_id(12,no-dashes)-hash(8).hot
    if let Some(dot_pos) = zip_filename.rfind(".hot") {
        let name_and_parts = &zip_filename[..dot_pos];
        let parts: Vec<&str> = name_and_parts.split('-').collect();
        if parts.len() >= 3 {
            // Last part is hash, second to last is bundle_id, everything else is bundle name
            let bundle_hash = parts[parts.len() - 1].to_string();
            let bundle_name = parts[..parts.len() - 2].join("-");
            return Ok((bundle_name, bundle_hash));
        } else if parts.len() == 2 {
            // Legacy format: bundle-name-hash
            let bundle_name = parts[0].to_string();
            let bundle_hash = parts[1].to_string();
            return Ok((bundle_name, bundle_hash));
        }
    }

    Err("Could not extract bundle metadata from filename".to_string())
}

/// Extract a bundle from bytes to the specified directory
pub fn extract_bundle_from_bytes(zip_data: &[u8], extract_dir: &Path) -> Result<(), String> {
    // Ensure the extract directory exists
    if let Err(e) = fs::create_dir_all(extract_dir) {
        return Err(format!("Failed to create extract directory: {}", e));
    }

    let cursor = std::io::Cursor::new(zip_data);
    let mut archive = match ZipArchive::new(cursor) {
        Ok(archive) => archive,
        Err(e) => return Err(format!("Failed to read zip archive: {}", e)),
    };

    // Extract all files
    for i in 0..archive.len() {
        let mut file = match archive.by_index(i) {
            Ok(file) => file,
            Err(e) => return Err(format!("Failed to read file at index {}: {}", i, e)),
        };

        let file_path = extract_dir.join(file.name());

        // Skip directories (they're created when we create parent directories for files)
        if file.name().ends_with('/') {
            continue;
        }

        // Create parent directories if needed
        if let Some(parent) = file_path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            return Err(format!(
                "Failed to create directory {}: {}",
                parent.display(),
                e
            ));
        }

        // Extract the file
        let mut output_file = match fs::File::create(&file_path) {
            Ok(file) => file,
            Err(e) => {
                return Err(format!(
                    "Failed to create file {}: {}",
                    file_path.display(),
                    e
                ));
            }
        };

        if let Err(e) = std::io::copy(&mut file, &mut output_file) {
            return Err(format!(
                "Failed to extract file {}: {}",
                file_path.display(),
                e
            ));
        }
    }

    tracing::debug!("Bundle extracted to: {}", extract_dir.display());
    Ok(())
}

/// Parsed bundle manifest information
#[derive(Debug, Clone)]
pub struct BundleManifest {
    /// Bundle name (project name)
    pub bundle_name: String,
    /// Bundle ID (UUID)
    pub bundle_id: String,
    /// Bundle hash (content hash)
    pub bundle_hash: String,
    /// Engine version used to create the bundle
    pub engine_version: String,
    /// hot-std version used
    pub hot_std_version: String,
    /// File hashes from the manifest (path -> hash)
    pub file_hashes: Vec<crate::lang::cache::bytecode_cache::FileHash>,
    /// Cache key for bytecode lookup
    pub cache_key: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct SemverCore {
    major: u64,
    minor: u64,
    patch: u64,
}

fn parse_semver_core(version: &str) -> Option<SemverCore> {
    let core = version
        .split_once(['-', '+'])
        .map(|(core, _)| core)
        .unwrap_or(version);
    let mut parts = core.split('.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;

    if parts.next().is_some() {
        return None;
    }

    Some(SemverCore {
        major,
        minor,
        patch,
    })
}

fn validate_bundle_runtime_compatibility(bundle_engine_version: &str) -> Result<(), String> {
    if bundle_engine_version.is_empty() {
        tracing::warn!(
            "Bundle manifest is missing engine_version; allowing deployment for backward compatibility"
        );
        return Ok(());
    }

    let runtime_version = crate::build_info::VERSION;
    let Some(bundle_version) = parse_semver_core(bundle_engine_version) else {
        tracing::warn!(
            "Bundle manifest engine_version '{}' is not semantic; allowing deployment",
            bundle_engine_version
        );
        return Ok(());
    };
    let Some(runtime_version_core) = parse_semver_core(runtime_version) else {
        tracing::warn!(
            "Runtime version '{}' is not semantic; allowing bundle version '{}'",
            runtime_version,
            bundle_engine_version
        );
        return Ok(());
    };

    if bundle_version.major != runtime_version_core.major
        || bundle_version.minor != runtime_version_core.minor
    {
        return Err(format!(
            "Bundle engine version {} is incompatible with runtime {}; major/minor versions must match",
            bundle_engine_version, runtime_version
        ));
    }

    Ok(())
}

/// Read and parse manifest.hot from an extracted bundle directory
pub fn read_bundle_manifest(extract_dir: &Path) -> Result<BundleManifest, String> {
    let manifest_path = extract_dir.join("manifest.hot");

    if !manifest_path.exists() {
        return Err(format!(
            "Manifest not found at: {}",
            manifest_path.display()
        ));
    }

    // Read the manifest file
    let manifest_content = fs::read_to_string(&manifest_path)
        .map_err(|e| format!("Failed to read manifest: {}", e))?;

    // Eval the manifest as Hot code to get a Val
    // The manifest is a simple data literal (a map), so we can eval it directly
    let manifest_val = crate::lang::engine::Engine::eval_code_pipeline_with_deps(
        &manifest_content,
        &[],  // No source paths needed for data literal
        &[],  // No test paths
        None, // No config
        None, // No project name
    )
    .map_err(|e| format!("Failed to eval manifest: {}", e))?;

    // The manifest has structure: { "hot.bundle.{name}": { bundle_id, bundle_hash, ... } }
    // We need to extract the bundle info from the first key
    let Val::Map(manifest_map) = manifest_val else {
        return Err("Manifest is not a map".to_string());
    };

    // Find the bundle key (starts with "hot.bundle.")
    let mut bundle_name = String::new();
    let mut bundle_info: Option<&Val> = None;

    for (key, value) in manifest_map.iter() {
        if let Val::Str(key_str) = key
            && key_str.starts_with("hot.bundle.")
        {
            bundle_name = key_str
                .strip_prefix("hot.bundle.")
                .unwrap_or(key_str)
                .to_string();
            bundle_info = Some(value);
            break;
        }
    }

    let Some(Val::Map(info)) = bundle_info else {
        return Err("Could not find bundle info in manifest".to_string());
    };

    // Extract fields from bundle info
    let get_str = |key: &str| -> String {
        info.get(&Val::from(key))
            .and_then(|v| {
                if let Val::Str(s) = v {
                    Some((**s).to_string())
                } else {
                    None
                }
            })
            .unwrap_or_default()
    };

    let bundle_id = get_str("bundle_id");
    let bundle_hash = get_str("bundle_hash");
    let engine_version = get_str("engine_version");
    let hot_std_version = get_str("hot_std_version");
    validate_bundle_runtime_compatibility(&engine_version)?;

    // Extract file hashes from the files array
    let mut file_hashes = Vec::new();
    if let Some(Val::Vec(files)) = info.get(&Val::from("files")) {
        for file_val in files {
            if let Val::Map(file_map) = file_val {
                let path = file_map
                    .get(&Val::from("path"))
                    .and_then(|v| {
                        if let Val::Str(s) = v {
                            Some((**s).to_string())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();
                let hash = file_map
                    .get(&Val::from("hash"))
                    .and_then(|v| {
                        if let Val::Str(s) = v {
                            Some((**s).to_string())
                        } else {
                            None
                        }
                    })
                    .unwrap_or_default();

                if !path.is_empty() && !hash.is_empty() {
                    file_hashes.push(crate::lang::cache::bytecode_cache::FileHash { path, hash });
                }
            }
        }
    }

    // Calculate cache key from file hashes
    let cache_key = if !file_hashes.is_empty() {
        crate::lang::cache::bytecode_cache::BytecodeCache::calculate_cache_key(
            &bundle_name,
            &file_hashes,
        )
        .ok()
    } else {
        None
    };

    tracing::debug!(
        "Read bundle manifest: name={}, id={}, hash={}, files={}, cache_key={:?}",
        bundle_name,
        bundle_id,
        bundle_hash,
        file_hashes.len(),
        cache_key
    );

    Ok(BundleManifest {
        bundle_name,
        bundle_id,
        bundle_hash,
        engine_version,
        hot_std_version,
        file_hashes,
        cache_key,
    })
}

/// Read the raw `resources` Vec from an extracted bundle's `manifest.hot`.
/// Returns `Val::Null` if the manifest is missing or has no resources entry —
/// callers should treat that as "no resources" (an empty registry). Returns
/// the structured `Val::Vec` on success so it can be passed directly to
/// [`crate::lang::hot::resource::build_registry_from_manifest`].
pub fn read_bundle_resources(extract_dir: &Path) -> Result<Val, String> {
    let manifest_path = extract_dir.join("manifest.hot");
    if !manifest_path.exists() {
        return Ok(Val::Null);
    }

    let manifest_content = fs::read_to_string(&manifest_path)
        .map_err(|e| format!("Failed to read manifest: {}", e))?;

    let manifest_val = crate::lang::engine::Engine::eval_code_pipeline_with_deps(
        &manifest_content,
        &[],
        &[],
        None,
        None,
    )
    .map_err(|e| format!("Failed to eval manifest: {}", e))?;

    let Val::Map(manifest_map) = manifest_val else {
        return Ok(Val::Null);
    };

    let mut bundle_info: Option<&Val> = None;
    for (key, value) in manifest_map.iter() {
        if let Val::Str(key_str) = key
            && key_str.starts_with("hot.bundle.")
        {
            bundle_info = Some(value);
            break;
        }
    }

    let Some(Val::Map(info)) = bundle_info else {
        return Ok(Val::Null);
    };

    Ok(info
        .get(&Val::from("resources"))
        .cloned()
        .unwrap_or(Val::Null))
}

/// Extract a bundle to the specified directory
pub fn extract_bundle_to_dir(zip_path: &Path, extract_dir: &Path) -> Result<(), String> {
    // Ensure the extract directory exists
    if let Err(e) = fs::create_dir_all(extract_dir) {
        return Err(format!("Failed to create extract directory: {}", e));
    }

    let file = match fs::File::open(zip_path) {
        Ok(file) => file,
        Err(e) => return Err(format!("Failed to open zip file: {}", e)),
    };

    let mut archive = match ZipArchive::new(file) {
        Ok(archive) => archive,
        Err(e) => return Err(format!("Failed to read zip archive: {}", e)),
    };

    // Count different types of files for feedback
    let mut source_files = 0;
    let mut cache_files = 0;
    let mut other_files = 0;

    // Extract all files
    for i in 0..archive.len() {
        let mut file = match archive.by_index(i) {
            Ok(file) => file,
            Err(e) => return Err(format!("Failed to read file at index {}: {}", i, e)),
        };

        let file_path = extract_dir.join(file.name());

        // Count file types
        if file.name().starts_with(".hot/cache/") {
            cache_files += 1;
        } else if file.name().ends_with(".hot") && file.name() != "manifest.hot" {
            source_files += 1;
        } else {
            other_files += 1;
        }

        // Create parent directories if needed
        if let Some(parent) = file_path.parent()
            && let Err(e) = fs::create_dir_all(parent)
        {
            return Err(format!(
                "Failed to create directory {}: {}",
                parent.display(),
                e
            ));
        }

        // Extract the file
        let mut output_file = match fs::File::create(&file_path) {
            Ok(file) => file,
            Err(e) => {
                return Err(format!(
                    "Failed to create file {}: {}",
                    file_path.display(),
                    e
                ));
            }
        };

        if let Err(e) = std::io::copy(&mut file, &mut output_file) {
            return Err(format!(
                "Failed to extract file {}: {}",
                file_path.display(),
                e
            ));
        }
    }

    println!("📂 Bundle extracted to: {}", extract_dir.display());
    println!("   {} source files", source_files);
    if cache_files > 0 {
        println!(
            "   {} cache files (pre-compiled for faster execution)",
            cache_files
        );
    }
    if other_files > 0 {
        println!("   {} other files", other_files);
    }

    Ok(())
}

/// Main unbundle function
pub fn bundle_extract(zip_path: &str, extract_dir: Option<&str>) -> Result<PathBuf, String> {
    let zip_path = Path::new(zip_path);

    if !zip_path.exists() {
        return Err(format!("Bundle file not found: {}", zip_path.display()));
    }

    // Determine the extract directory
    let target_dir = if let Some(dir_str) = extract_dir {
        PathBuf::from(dir_str)
    } else {
        // Use bundle name + first 8 characters of hash as default
        let (bundle_name, bundle_hash) = get_bundle_metadata(zip_path)?;
        let short_hash = if bundle_hash.len() >= 8 {
            &bundle_hash[..8]
        } else {
            &bundle_hash
        };
        PathBuf::from(format!("{}-{}", bundle_name, short_hash))
    };

    println!("Extracting bundle...");
    println!("  Bundle: {}", zip_path.display());
    println!("  Target directory: {}", target_dir.display());

    // Check if the target directory is safe
    match is_safe_extract_dir(&target_dir) {
        Ok(true) => {
            // Safe to extract
        }
        Ok(false) => {
            return Err(format!(
                "Target directory {} is not empty. Please specify a different directory or remove existing contents.",
                target_dir.display()
            ));
        }
        Err(e) => return Err(e),
    }

    // Extract the bundle
    extract_bundle_to_dir(zip_path, &target_dir)?;

    println!("  Extracted {} files", count_files_in_dir(&target_dir)?);
    println!(
        "  Bundle extracted successfully to: {}",
        target_dir.display()
    );

    Ok(target_dir)
}

/// Count files in a directory (for reporting)
fn count_files_in_dir(dir: &Path) -> Result<usize, String> {
    let mut count = 0;
    let walker = WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file());

    for _ in walker {
        count += 1;
    }

    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_calculate_bundle_hash() {
        let files = vec![
            BundleFile {
                path: PathBuf::from("file1.hot"),
                relative_path: "file1.hot".to_string(),
                content: b"content1".to_vec(),
                hash: "hash1".to_string(),
                size: 8,
            },
            BundleFile {
                path: PathBuf::from("file2.hot"),
                relative_path: "file2.hot".to_string(),
                content: b"content2".to_vec(),
                hash: "hash2".to_string(),
                size: 8,
            },
        ];

        let hash = calculate_bundle_hash(&files);
        assert!(!hash.is_empty());
        assert_eq!(hash.len(), 64); // Blake3 hash should be 64 chars
    }

    #[test]
    fn test_create_manifest() {
        let files = vec![BundleFile {
            path: PathBuf::from("test.hot"),
            relative_path: "hot/src/test.hot".to_string(),
            content: b"test content".to_vec(),
            hash: "test-hash".to_string(),
            size: 12,
        }];

        let manifest = create_manifest(
            "test-bundle",
            "test-bundle-id",
            &files,
            "bundle-hash",
            &crate::lang::compiler::EventHandlers::new(),
            &crate::lang::compiler::ScheduledFunctions::new(),
            &crate::lang::compiler::McpTools::new(),
            &crate::lang::compiler::Webhooks::new(),
            &crate::lang::compiler::AgentDefs::new(),
            &crate::lang::compiler::WorkflowDefs::new(),
            &crate::lang::compiler::SendTargets::new(),
            None,
            None,
            &[],
        );

        // Check that the manifest has the expected structure
        if let Val::Map(manifest_map) = manifest {
            assert!(manifest_map.contains_key(&Val::from("hot.bundle.test-bundle")));

            if let Some(Val::Map(bundle_info)) =
                manifest_map.get(&Val::from("hot.bundle.test-bundle"))
            {
                assert!(bundle_info.contains_key(&Val::from("bundle_id")));
                assert!(bundle_info.contains_key(&Val::from("bundle_hash")));
                assert!(bundle_info.contains_key(&Val::from("files")));
                assert!(bundle_info.contains_key(&Val::from("event_handlers")));
                assert!(bundle_info.contains_key(&Val::from("scheduled_functions")));
                assert!(bundle_info.contains_key(&Val::from("workflows")));
            } else {
                panic!("bundle info should be a map");
            }
        } else {
            panic!("manifest should be a map");
        }
    }

    #[test]
    fn test_bundle_runtime_compatibility_allows_same_and_patch_versions() {
        let runtime = parse_semver_core(crate::build_info::VERSION)
            .expect("runtime version should be semantic");
        let same_minor_next_patch =
            format!("{}.{}.{}", runtime.major, runtime.minor, runtime.patch + 1);

        assert!(validate_bundle_runtime_compatibility(crate::build_info::VERSION).is_ok());
        assert!(validate_bundle_runtime_compatibility(&same_minor_next_patch).is_ok());
    }

    #[test]
    fn test_bundle_runtime_compatibility_rejects_major_minor_mismatch() {
        let runtime = parse_semver_core(crate::build_info::VERSION)
            .expect("runtime version should be semantic");
        let next_minor = format!("{}.{}.0", runtime.major, runtime.minor + 1);
        let next_major = format!("{}.0.0", runtime.major + 1);

        assert!(validate_bundle_runtime_compatibility(&next_minor).is_err());
        assert!(validate_bundle_runtime_compatibility(&next_major).is_err());
    }

    #[test]
    fn test_create_manifest_normalizes_artifact_source_paths_to_bundle_paths() {
        let source_abs = std::env::temp_dir()
            .join("hot-manifest-source-normalization")
            .join("src")
            .join("agent.hot");
        let source_abs_str = source_abs.to_string_lossy().to_string();
        let resource_abs = std::env::temp_dir()
            .join("hot-manifest-source-normalization")
            .join("resources")
            .join("prompts")
            .join("system.md");
        let resource_abs_str = resource_abs.to_string_lossy().to_string();
        let bundle_path = "hot/src/agent.hot";
        let resource_bundle_path = "resources/prompts/system.md";
        let files = vec![BundleFile {
            path: source_abs,
            relative_path: bundle_path.to_string(),
            content: b"::demo ns".to_vec(),
            hash: "test-hash".to_string(),
            size: 9,
        }];
        let resources = vec![BundleResource {
            rel_path: "prompts/system.md".to_string(),
            abs_path: resource_abs,
            content: b"system prompt".to_vec(),
            hash: "resource-hash".to_string(),
            size: 13,
        }];
        let source_val = || {
            crate::val!({
                "fn": "::demo/handler",
                "file": source_abs_str.clone(),
                "line": 1,
            })
        };

        let mut event_handlers = crate::lang::compiler::EventHandlers::new();
        event_handlers.insert(
            "demo:event".to_string(),
            vec![crate::lang::compiler::EventHandler {
                event_type: "demo:event".to_string(),
                event_handler: source_val(),
            }],
        );

        let mut scheduled_functions = crate::lang::compiler::ScheduledFunctions::new();
        scheduled_functions.insert(
            "* * * * *".to_string(),
            vec![crate::lang::compiler::ScheduledFunction {
                cron_expression: "* * * * *".to_string(),
                scheduled_function: source_val(),
            }],
        );

        let mut mcp_tools = crate::lang::compiler::McpTools::new();
        mcp_tools.insert(
            "demo".to_string(),
            vec![crate::lang::compiler::McpTool {
                service: "demo".to_string(),
                name: "tool".to_string(),
                auth_mode: "none".to_string(),
                mcp_tool: source_val(),
            }],
        );
        mcp_tools.insert(
            "resource".to_string(),
            vec![crate::lang::compiler::McpTool {
                service: "resource".to_string(),
                name: "tool".to_string(),
                auth_mode: "none".to_string(),
                mcp_tool: crate::val!({
                    "fn": "::demo/resource-tool",
                    "file": resource_abs_str.clone(),
                    "line": 1,
                }),
            }],
        );

        let mut webhooks = crate::lang::compiler::Webhooks::new();
        webhooks.insert(
            "demo".to_string(),
            vec![crate::lang::compiler::Webhook {
                service: "demo".to_string(),
                path: "/hook".to_string(),
                method: "POST".to_string(),
                name: "hook".to_string(),
                webhook: source_val(),
            }],
        );

        let agents = vec![crate::lang::compiler::AgentDef {
            type_name: "DemoAgent".to_string(),
            namespace: "::demo".to_string(),
            agent_val: source_val(),
        }];
        let workflows = vec![crate::lang::compiler::WorkflowDef {
            type_name: "DemoWorkflow".to_string(),
            namespace: "::demo".to_string(),
            workflow_val: source_val(),
        }];

        let manifest = create_manifest(
            "demo",
            "demo-id",
            &files,
            "bundle-hash",
            &event_handlers,
            &scheduled_functions,
            &mcp_tools,
            &webhooks,
            &agents,
            &workflows,
            &crate::lang::compiler::SendTargets::new(),
            None,
            None,
            &resources,
        );

        let Val::Map(manifest_map) = manifest else {
            panic!("manifest should be a map");
        };
        let Some(Val::Map(bundle_info)) = manifest_map.get(&Val::from("hot.bundle.demo")) else {
            panic!("bundle info should be a map");
        };

        let event_handler = first_nested_vec_item(bundle_info, "event_handlers", "demo:event");
        assert_manifest_file(event_handler, bundle_path);

        let scheduled = first_nested_vec_item(bundle_info, "scheduled_functions", "* * * * *");
        assert_manifest_file(scheduled, bundle_path);

        let tool = first_nested_vec_item(bundle_info, "mcp_tools", "demo");
        assert_manifest_file(tool, bundle_path);

        let resource_tool = first_nested_vec_item(bundle_info, "mcp_tools", "resource");
        assert_manifest_file(resource_tool, resource_bundle_path);

        let webhook = first_nested_vec_item(bundle_info, "webhooks", "demo");
        assert_manifest_file(webhook, bundle_path);

        let agent = first_vec_item(bundle_info, "agents");
        assert_manifest_file(agent, bundle_path);

        let workflow = first_vec_item(bundle_info, "workflows");
        assert_manifest_file(workflow, bundle_path);
    }

    fn first_nested_vec_item<'a>(
        bundle_info: &'a indexmap::IndexMap<Val, Val>,
        section: &str,
        key: &str,
    ) -> &'a Val {
        let Some(Val::Map(section_map)) = bundle_info.get(&Val::from(section)) else {
            panic!("{section} should be a map");
        };
        let Some(Val::Vec(items)) = section_map.get(&Val::from(key)) else {
            panic!("{section}.{key} should be a vec");
        };
        items.first().expect("section should have an item")
    }

    fn first_vec_item<'a>(bundle_info: &'a indexmap::IndexMap<Val, Val>, section: &str) -> &'a Val {
        let Some(Val::Vec(items)) = bundle_info.get(&Val::from(section)) else {
            panic!("{section} should be a vec");
        };
        items.first().expect("section should have an item")
    }

    fn assert_manifest_file(val: &Val, expected: &str) {
        let Val::Map(map) = val else {
            panic!("artifact should be a map");
        };
        assert_eq!(map.get(&Val::from("file")), Some(&Val::from(expected)));
    }

    #[test]
    fn test_bundle_creation_with_event_handlers() {
        use std::fs;
        use tempfile::TempDir;

        // Create a temporary directory for testing
        let temp_dir = TempDir::new().unwrap();
        let src_dir = temp_dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();

        // Create a test file with event handlers
        let test_file = src_dir.join("test_events.hot");
        let test_content = r#"::demo::test ns

user-created-handler meta {on-event: "user:created"} fn (event) {
  "handling user created event"
}

order-handler meta {on-event: "order:placed"} fn (event) {
  "handling order placed event"
}
"#;
        fs::write(&test_file, test_content).unwrap();

        // Test bundle creation
        let src_paths = vec![src_dir.to_string_lossy().to_string()];
        let pkg_paths = vec![];

        let bundle_result = create_bundle("test-bundle", &src_paths, &pkg_paths, &[], true, &[]);
        assert!(bundle_result.is_ok(), "Bundle creation should succeed");

        let bundle = bundle_result.unwrap();
        assert_eq!(bundle.name, "test-bundle");
        assert_eq!(bundle.files.len(), 1);

        // Cache files may or may not be present depending on whether compilation generated cache
        println!("Bundle has {} cache files", bundle.cache_files.len());

        // Verify the manifest contains event handlers
        if let Val::Map(manifest_map) = &bundle.manifest {
            if let Some(Val::Map(bundle_info)) =
                manifest_map.get(&Val::from("hot.bundle.test-bundle"))
            {
                if let Some(Val::Map(event_handlers)) =
                    bundle_info.get(&Val::from("event_handlers"))
                {
                    println!("Event handlers in manifest: {:#?}", event_handlers);

                    // Check that we have the expected event types
                    assert!(
                        event_handlers.contains_key(&Val::from("user:created")),
                        "Should have user:created event handlers"
                    );
                    assert!(
                        event_handlers.contains_key(&Val::from("order:placed")),
                        "Should have order:placed event handlers"
                    );

                    // Check that user:created has 1 handler
                    if let Some(Val::Vec(user_handlers)) =
                        event_handlers.get(&Val::from("user:created"))
                    {
                        assert_eq!(user_handlers.len(), 1, "Should have 1 user:created handler");

                        // Verify handler structure (uses new `fn` format instead of ns/var/value)
                        if let Val::Map(handler_info) = &user_handlers[0] {
                            assert!(
                                handler_info.contains_key(&Val::from("fn")),
                                "Handler should have 'fn' key"
                            );
                            assert!(
                                handler_info.contains_key(&Val::from("meta")),
                                "Handler should have 'meta' key"
                            );
                            assert!(
                                handler_info.contains_key(&Val::from("file")),
                                "Handler should have 'file' key"
                            );
                        } else {
                            panic!("Handler should be a map");
                        }
                    } else {
                        panic!("user:created handlers should be a vector");
                    }

                    // Check that order:placed has 1 handler
                    if let Some(Val::Vec(order_handlers)) =
                        event_handlers.get(&Val::from("order:placed"))
                    {
                        assert_eq!(
                            order_handlers.len(),
                            1,
                            "Should have 1 order:placed handler"
                        );
                    } else {
                        panic!("order:placed handlers should be a vector");
                    }
                } else {
                    panic!("Bundle info should contain event_handlers");
                }
            } else {
                panic!("Manifest should contain bundle info");
            }
        } else {
            panic!("Manifest should be a map");
        }

        println!("✅ Bundle creation with event handlers test passed!");
        println!("Bundle contains {} files", bundle.files.len());
        println!("Manifest contains event handlers for the compiled source files");
    }

    #[test]
    fn test_bundle_creation_with_cache_files() {
        use std::fs;
        use tempfile::TempDir;

        // Create a temporary directory for testing
        let temp_dir = TempDir::new().unwrap();
        let src_dir = temp_dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();

        // Create a test file
        let test_file = src_dir.join("test_cache.hot");
        let test_content = r#"::demo::cache::test ns

test-var "hello world"

test-fn fn (name) {
  "Hello from the cache test!"
}
"#;
        fs::write(&test_file, test_content).unwrap();

        // Create cache directory
        let cache_dir = temp_dir.path().join(".hot/cache");
        fs::create_dir_all(&cache_dir).unwrap();

        // Simulate some cache files
        let metadata_content = r#"{"version":1,"format":"ZstdJson","namespaces":{}}"#;
        fs::write(cache_dir.join("metadata.json"), metadata_content).unwrap();

        let paths_content = r#"{"contexts":{},"next_context_id":1}"#;
        fs::write(cache_dir.join("paths.json"), paths_content).unwrap();

        // Change to temp directory so cache collection works
        let original_dir = std::env::current_dir().unwrap();
        std::env::set_current_dir(temp_dir.path()).unwrap();

        // Test bundle creation
        let src_paths = vec![src_dir.to_string_lossy().to_string()];
        let pkg_paths = vec![];

        let bundle_result =
            create_bundle("test-cache-bundle", &src_paths, &pkg_paths, &[], true, &[]);

        // Restore original directory
        std::env::set_current_dir(original_dir).unwrap();

        if let Err(e) = &bundle_result {
            println!("Bundle creation error: {}", e);
        }
        assert!(bundle_result.is_ok(), "Bundle creation should succeed");

        let bundle = bundle_result.unwrap();
        assert_eq!(bundle.name, "test-cache-bundle");
        assert_eq!(bundle.files.len(), 1);

        // Should have collected some cache files (compilation may generate different files than our manual ones)
        println!("Found {} cache files", bundle.cache_files.len());

        // Verify cache files have correct relative paths
        let cache_file_paths: Vec<_> = bundle
            .cache_files
            .iter()
            .map(|f| f.relative_path.as_str())
            .collect();

        // Check that all cache files start with .hot/cache/
        for cache_file_path in &cache_file_paths {
            assert!(
                cache_file_path.starts_with(".hot/cache/"),
                "Cache file path should start with .hot/cache/, got: {}",
                cache_file_path
            );
        }

        println!("✅ Bundle with cache files test passed!");
        println!("Cache files included: {:?}", cache_file_paths);
    }

    #[test]
    fn test_bundle_includes_resources_with_hashes() {
        use std::fs;
        use tempfile::TempDir;

        let temp_dir = TempDir::new().unwrap();
        let src_dir = temp_dir.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("main.hot"), b"::demo ns\n").unwrap();

        // Two resource roots; second adds a file *and* shadows one in the
        // first (first-root-wins semantics, mirroring the live registry).
        let res_a = temp_dir.path().join("res-a");
        let res_b = temp_dir.path().join("res-b");
        fs::create_dir_all(res_a.join("prompts")).unwrap();
        fs::create_dir_all(res_b.join("prompts")).unwrap();
        fs::write(res_a.join("prompts/system.md"), b"a-system").unwrap();
        fs::write(res_b.join("prompts/system.md"), b"b-system").unwrap(); // shadowed
        fs::write(res_b.join("prompts/extra.md"), b"b-extra").unwrap();

        let src_paths = vec![src_dir.to_string_lossy().to_string()];
        let resource_roots = vec![res_a.clone(), res_b.clone()];

        let bundle = create_bundle("res-test", &src_paths, &[], &resource_roots, true, &[])
            .expect("bundle should build");

        // First-root-wins: system.md from res-a survives, extra.md from res-b
        // is added.
        let mut paths: Vec<_> = bundle
            .resources
            .iter()
            .map(|r| r.rel_path.clone())
            .collect();
        paths.sort();
        assert_eq!(paths, vec!["prompts/extra.md", "prompts/system.md"]);

        let system = bundle
            .resources
            .iter()
            .find(|r| r.rel_path == "prompts/system.md")
            .unwrap();
        assert_eq!(system.content, b"a-system");
        assert!(!system.hash.is_empty(), "hash should be populated");
        assert_eq!(system.size, b"a-system".len() as u64);

        // Manifest exposes resources alongside files.
        if let Val::Map(top) = &bundle.manifest
            && let Some(Val::Map(info)) = top.get(&Val::from("hot.bundle.res-test"))
            && let Some(Val::Vec(items)) = info.get(&Val::from("resources"))
        {
            assert_eq!(items.len(), 2);
            let first = items.iter().next().unwrap();
            if let Val::Map(m) = first {
                assert!(m.contains_key(&Val::from("path")));
                assert!(m.contains_key(&Val::from("hash")));
                assert!(m.contains_key(&Val::from("size")));
            } else {
                panic!("resource manifest entry should be a Map");
            }
        } else {
            panic!("manifest should expose `resources` Vec under bundle key");
        }

        // Resource changes feed the build hash so a content-only change
        // produces a different hash.
        let bundle_no_resources =
            create_bundle("res-test", &src_paths, &[], &[], true, &[]).unwrap();
        assert_ne!(bundle.bundle_hash, bundle_no_resources.bundle_hash);
    }

    #[test]
    fn test_build_registry_from_manifest_round_trip() {
        use std::fs;
        use tempfile::TempDir;

        let tmp = TempDir::new().unwrap();
        let src_dir = tmp.path().join("src");
        fs::create_dir_all(&src_dir).unwrap();
        fs::write(src_dir.join("main.hot"), b"::demo ns\n").unwrap();

        let res_root = tmp.path().join("res");
        fs::create_dir_all(res_root.join("prompts")).unwrap();
        fs::write(res_root.join("prompts/system.md"), b"hello").unwrap();

        let bundle = create_bundle(
            "round-trip",
            &[src_dir.to_string_lossy().to_string()],
            &[],
            std::slice::from_ref(&res_root),
            true,
            &[],
        )
        .unwrap();

        // Pull the resources Vec from the manifest and rebuild a registry as
        // the runtime would after extracting the bundle. Files are not yet
        // extracted on disk; we just validate the registry shape and that
        // each entry's `abs_path` points under `<extract>/resources/<rel>`.
        let extract_dir = tmp.path().join("extract");
        let resources_val = if let Val::Map(top) = &bundle.manifest
            && let Some(Val::Map(info)) = top.get(&Val::from("hot.bundle.round-trip"))
        {
            info.get(&Val::from("resources"))
                .cloned()
                .unwrap_or(Val::Null)
        } else {
            panic!("manifest missing bundle key");
        };

        let registry =
            crate::lang::hot::resource::build_registry_from_manifest(&resources_val, &extract_dir);
        let entry = registry
            .entries
            .get("prompts/system.md")
            .expect("entry should exist");
        assert!(entry.from_manifest);
        assert_eq!(
            entry.abs_path,
            extract_dir.join("resources").join("prompts/system.md")
        );
        assert!(!entry.hash.is_empty());
        assert_eq!(entry.size, b"hello".len() as u64);
    }
}
