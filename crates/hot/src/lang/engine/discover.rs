//! File and source discovery + parse caching for the engine.
//!
//! Splits cleanly off `mod.rs` because it has a self-contained surface:
//!   * Module-level structs (`ParsedFile`, `CachedParseResult`,
//!     `DiscoveredUnit`, `DiskCacheEntry`) and the in-memory `PARSE_CACHE`
//!     used by the parse pipeline.
//!   * Free fns: `discover_compilation_units`, `parse_units_with_cache`,
//!     `parse_files_parallel`, plus the disk-cache helpers and content
//!     hashing.
//!   * `Engine` impl methods that walk the filesystem to discover `.hot`
//!     files for projects and dependency packages.

use super::Engine;
use ahash::{AHashMap, AHashSet};
use indexmap::IndexMap;
use rayon::prelude::*;

/// Policy for resolving a same-name binding when merging a namespace that
/// already exists in the target map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum NsMergePolicy {
    /// Static source files: a name may be declared only once per namespace
    /// across files. A second declaration from a different file is an error —
    /// this keeps the merged program independent of file, unit, and
    /// cache-hit/miss ordering (a conflict errors no matter which side was
    /// merged first).
    Strict,
    /// Eval / REPL input: a later declaration shadows earlier ones. The
    /// binding is appended, so name resolution and execution order pick the
    /// newest definition — the same rule that lets accumulated REPL source
    /// redefine a function within one file.
    Shadow,
}

/// Source file a binding came from, for conflict diagnostics and same-file
/// detection. Falls back to the namespace's file when the var has no span.
fn binding_file(
    var: &crate::lang::ast::Var,
    ns_source_file: &Option<std::path::PathBuf>,
) -> Option<String> {
    var.src
        .as_ref()
        .and_then(|s| s.file.clone())
        .or_else(|| ns_source_file.as_ref().map(|p| p.display().to_string()))
}

fn file_label(file: &Option<String>) -> &str {
    file.as_deref().unwrap_or("<unknown file>")
}

/// Merge `incoming` into `target[ns_path]`, combining members instead of
/// replacing the whole namespace. All namespace-merge sites (unit files,
/// cross-unit, target file, eval/REPL code) go through here so duplicate
/// handling is uniform and deterministic.
///
/// Bindings whose name already exists in the target namespace:
///   * same source file — skipped (idempotent re-merge; e.g. `hot run x.hot`
///     parses `x.hot` both as part of its source unit and as the target file)
///   * different file + `Strict` — error naming both files
///   * different file + `Shadow` — appended, newest wins
pub(crate) fn merge_namespace(
    target: &mut IndexMap<crate::lang::ast::NsPath, crate::lang::ast::Namespace>,
    ns_path: crate::lang::ast::NsPath,
    incoming: crate::lang::ast::Namespace,
    policy: NsMergePolicy,
) -> Result<(), String> {
    use indexmap::map::Entry;

    let slot = match target.entry(ns_path) {
        Entry::Vacant(slot) => {
            slot.insert(incoming);
            return Ok(());
        }
        Entry::Occupied(slot) => slot,
    };
    let ns_path = slot.key().clone();
    let existing = slot.into_mut();
    let incoming_source_file = incoming.source_file.clone();

    // Namespace metadata: adopt when the existing declaration has none;
    // conflicting redeclaration is an error for static code, newest-wins
    // for eval input.
    if let Some(incoming_meta) = incoming.meta {
        match &existing.meta {
            None => existing.meta = Some(incoming_meta),
            Some(existing_meta) if *existing_meta != incoming_meta => {
                if policy == NsMergePolicy::Strict {
                    return Err(format!(
                        "Namespace '{}' declares conflicting metadata in {} and {}",
                        ns_path,
                        existing
                            .source_file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .as_deref()
                            .unwrap_or("<unknown file>"),
                        incoming_source_file
                            .as_ref()
                            .map(|p| p.display().to_string())
                            .as_deref()
                            .unwrap_or("<unknown file>"),
                    ));
                }
                existing.meta = Some(incoming_meta);
            }
            Some(_) => {}
        }
    }

    // Aliases: union; the same alias name pointing at two different targets
    // is a conflict under Strict, newest-wins under Shadow.
    for (alias, alias_target) in incoming.aliases {
        match existing.aliases.get(&alias) {
            Some(current) if *current != alias_target => {
                if policy == NsMergePolicy::Strict {
                    return Err(format!(
                        "Namespace '{}' declares alias '{}' with conflicting targets '{}' and '{}'",
                        ns_path, alias, current, alias_target
                    ));
                }
                existing.aliases.insert(alias, alias_target);
            }
            Some(_) => {}
            None => {
                existing.aliases.insert(alias, alias_target);
            }
        }
    }

    // Bindings: compare by symbol name (Var identity includes source spans,
    // so map-key equality can't detect same-name redeclarations).
    for (var, value) in incoming.scope.vars {
        let name = var.sym.name();
        let existing_same_name = existing
            .scope
            .vars
            .keys()
            .rev()
            .find(|v| v.sym.name() == name)
            .cloned();

        // Every file's `::path ns` declaration parses to a `ns` binding, so a
        // namespace assembled from several files legitimately sees one per
        // file — keep the first and skip the rest.
        if name == "ns" && existing_same_name.is_some() {
            continue;
        }

        match existing_same_name {
            None => {
                existing.scope.vars.insert(var, value);
            }
            Some(prior) => {
                let prior_file = binding_file(&prior, &existing.source_file);
                let incoming_file = binding_file(&var, &incoming_source_file);
                if prior_file.is_some() && prior_file == incoming_file {
                    // Same file merged twice — keep the existing binding.
                    continue;
                }
                match policy {
                    NsMergePolicy::Strict => {
                        return Err(format!(
                            "Duplicate definition of '{}' in namespace '{}': defined in {} and {}",
                            name,
                            ns_path,
                            file_label(&prior_file),
                            file_label(&incoming_file),
                        ));
                    }
                    NsMergePolicy::Shadow => {
                        existing.scope.vars.insert(var, value);
                    }
                }
            }
        }
    }

    Ok(())
}

/// Result of parsing a single file
pub(super) struct ParsedFile {
    /// File path
    pub(super) path: String,
    /// File content
    pub(super) content: String,
    /// Parsed namespaces
    pub(super) namespaces: IndexMap<crate::lang::ast::NsPath, crate::lang::ast::Namespace>,
}

/// Cached parse result with content hash (for in-memory cache)
struct CachedParseResult {
    /// Hash of the file content
    content_hash: String,
    /// Parsed namespaces (cloneable)
    namespaces: IndexMap<crate::lang::ast::NsPath, crate::lang::ast::Namespace>,
}

/// Global in-memory cache for parsed files (within same process)
/// Key: file path, Value: cached parse result
///
/// Uses parking_lot::Mutex (no poisoning) so a panic during parsing of one file
/// doesn't permanently disable the cache for all subsequent parses.
static PARSE_CACHE: std::sync::LazyLock<parking_lot::Mutex<AHashMap<String, CachedParseResult>>> =
    std::sync::LazyLock::new(|| parking_lot::Mutex::new(AHashMap::new()));

/// Compute content hash using Blake3
fn compute_content_hash(content: &str) -> String {
    use crate::hasher::HotHasher;
    let mut hasher = HotHasher::new();
    hasher.update(content.as_bytes());
    hasher.finalize()
}

/// Get the unit cache instance
fn get_unit_cache() -> crate::lang::cache::unit_cache::UnitCache {
    crate::lang::cache::unit_cache::UnitCache::new(
        crate::lang::cache::unit_cache::UnitCache::default_cache_dir(),
    )
}

/// A discovered compilation unit with its files
pub(super) struct DiscoveredUnit {
    pub(super) unit: crate::lang::cache::unit_cache::CompilationUnit,
    pub(super) files: Vec<String>,
}

/// Discover all compilation units from configuration
pub(super) fn discover_compilation_units(
    conf: Option<&crate::val::Val>,
    project_name: Option<&str>,
    src_paths: &[String],
    test_paths: &[String],
) -> Result<Vec<DiscoveredUnit>, String> {
    let mut units = Vec::new();
    let mut loaded_packages = AHashSet::new();

    // Only load hot-std and dependencies when we have actual source/test paths to compile.
    // For eval_simple (pkg.hot parsing), we skip dependency loading since those files
    // only define simple data structures and don't need the standard library.
    let has_sources = !src_paths.is_empty() || !test_paths.is_empty() || conf.is_some();

    if has_sources {
        // Inject hot-std first using the dependency resolver (no pkg.hot parsing needed)
        // This avoids the recursive pipeline issue when parsing pkg.hot files
        let resolver = crate::lang::project::DependencyResolver::default();
        let hot_std = resolver.get_hot_std_dependency();
        let hot_std_path = hot_std.resolved_path.to_string_lossy().to_string();

        // Check hot-std's hot-min-version requirement
        let hot_std_pkg_hot = hot_std.resolved_path.join("pkg.hot");
        if hot_std_pkg_hot.exists() {
            match crate::lang::project::PackageMetadata::parse_from_file(&hot_std_pkg_hot) {
                Ok(pkg_meta) => {
                    if let Some(ref min_version) = pkg_meta.hot_min_version {
                        crate::build_info::check_min_version(min_version).map_err(|e| {
                            format!("Package 'hot-std' requires Hot {}: {}", min_version, e)
                        })?;
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "Failed to parse hot-std pkg.hot at {}: {}",
                        hot_std_pkg_hot.display(),
                        e
                    );
                }
            }
        }

        match Engine::discover_dependency_source_files(&hot_std.resolved_path) {
            Ok(files) if !files.is_empty() => {
                tracing::debug!(
                    "Injecting hot-std from: {} ({} files)",
                    hot_std_path,
                    files.len()
                );
                units.push(DiscoveredUnit {
                    unit: crate::lang::cache::unit_cache::CompilationUnit::Package {
                        name: "hot-std".to_string(),
                        path: hot_std.resolved_path.clone(),
                    },
                    files,
                });
                loaded_packages.insert("hot-std".to_string());
            }
            Ok(_) => {
                tracing::error!(
                    "hot-std found at {} but contains no .hot files!",
                    hot_std_path
                );
            }
            Err(e) => {
                tracing::error!(
                    "Failed to discover hot-std files at {}: {}",
                    hot_std_path,
                    e
                );
            }
        }

        // Discover additional dependency packages (from project config)
        if let (Some(conf), Some(project_name)) = (conf, project_name) {
            tracing::debug!("Resolving dependencies for project '{}'...", project_name);
            match crate::project::get_resolved_project_dependencies(conf, project_name) {
                Ok(resolved_deps) => {
                    tracing::debug!("Found {} resolved dependencies", resolved_deps.len());
                    for dep in resolved_deps {
                        // Skip hot-std since we already loaded it
                        if loaded_packages.contains(&dep.name) {
                            continue;
                        }
                        // Use discover_dependency_source_files to only load src_paths, not test_paths
                        let files = Engine::discover_dependency_source_files(&dep.resolved_path)?;
                        if !files.is_empty() {
                            units.push(DiscoveredUnit {
                                unit: crate::lang::cache::unit_cache::CompilationUnit::Package {
                                    name: dep.name.clone(),
                                    path: dep.resolved_path.clone(),
                                },
                                files,
                            });
                            loaded_packages.insert(dep.name.clone());
                        }
                    }
                }
                Err(e) => {
                    return Err(format!(
                        "Failed to resolve dependencies for '{}': {}",
                        project_name, e
                    ));
                }
            }
        }
    }

    // Discover source paths as separate units
    for (idx, src_path) in src_paths.iter().enumerate() {
        let files = Engine::discover_hot_files(src_path)?;
        if !files.is_empty() {
            // Derive a name from the path
            let name = std::path::Path::new(src_path)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| format!("src-{}", idx));

            units.push(DiscoveredUnit {
                unit: crate::lang::cache::unit_cache::CompilationUnit::SourcePath {
                    name,
                    path: std::path::PathBuf::from(src_path),
                },
                files,
            });
        }
    }

    // Discover test paths as separate units
    for (idx, test_path) in test_paths.iter().enumerate() {
        let files = Engine::discover_hot_files(test_path)?;
        if !files.is_empty() {
            let name = std::path::Path::new(test_path)
                .file_name()
                .map(|n| format!("test-{}", n.to_string_lossy()))
                .unwrap_or_else(|| format!("test-{}", idx));

            units.push(DiscoveredUnit {
                unit: crate::lang::cache::unit_cache::CompilationUnit::SourcePath {
                    name,
                    path: std::path::PathBuf::from(test_path),
                },
                files,
            });
        }
    }

    Ok(units)
}

/// Parse compilation units with caching
/// Returns merged namespaces from all units
///
/// Caching uses custom AST serialization (ast_cache module) which properly handles:
/// - Val::Map with non-string keys (serialized as [[key, value], ...] arrays)
/// - Val::Box containing AstNode (serialized using TaggedVal::AstNode)
/// - All nested Value/Val types in FnCall, Flow, Lambda, etc.
///
/// Cache files are stored with zstd compression. Location is determined by
/// `cache_paths::get_unit_cache_dir()`: project-local `.hot/cache/unit/` when
/// `hot.hot` exists, otherwise platform-specific system cache directory.
///
/// This function uses full parallelization for both cache loading and parsing:
/// 1. Load all cached units in parallel
/// 2. Parse all cache-miss units in parallel
/// 3. Save all new cache entries in parallel (background)
pub(super) fn parse_units_with_cache(
    units: &[DiscoveredUnit],
    color: bool,
) -> Result<
    (
        IndexMap<crate::lang::ast::NsPath, crate::lang::ast::Namespace>,
        Vec<ParsedFile>,
    ),
    String,
> {
    use rayon::prelude::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    let unit_cache = get_unit_cache();
    let cache_hits = AtomicUsize::new(0);
    let cache_misses = AtomicUsize::new(0);

    // Phase 1: Load all cached units in parallel, identify cache misses
    type CacheResult = Option<IndexMap<crate::lang::ast::NsPath, crate::lang::ast::Namespace>>;

    let cache_results: Vec<(&DiscoveredUnit, CacheResult)> = units
        .par_iter()
        .map(|discovered| match unit_cache.load(&discovered.unit) {
            Ok(Some(cached)) => {
                cache_hits.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    "Cache hit for {} ({} namespaces)",
                    discovered.unit.id(),
                    cached.namespaces.len()
                );
                (discovered, Some(cached.namespaces))
            }
            Ok(None) => {
                cache_misses.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    "Cache miss for {} ({} files to parse)",
                    discovered.unit.id(),
                    discovered.files.len()
                );
                (discovered, None)
            }
            Err(e) => {
                cache_misses.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(
                    "Cache error for {}: {}, will parse",
                    discovered.unit.id(),
                    e
                );
                (discovered, None)
            }
        })
        .collect();

    // Separate hits from misses (unit order is preserved in cache_results)
    let mut units_to_parse: Vec<&DiscoveredUnit> = Vec::new();
    for (discovered, result) in &cache_results {
        if result.is_none() {
            units_to_parse.push(discovered);
        }
    }

    // Phase 2: Parse all cache-miss units in parallel
    let parsed_results: Vec<_> = units_to_parse
        .par_iter()
        .map(|discovered| {
            let (parsed_files, parse_errors) = parse_files_parallel(&discovered.files, color);

            if !parse_errors.is_empty() {
                return Err(format!(
                    "Parse errors in {}:\n{}",
                    discovered.unit.id(),
                    parse_errors.join("\n")
                ));
            }

            // Collect namespaces from this unit, merging members when several
            // files contribute to the same namespace. Duplicate definitions
            // across files are compile errors (previously the last file
            // silently replaced the whole namespace).
            let mut unit_namespaces = IndexMap::new();
            for parsed in &parsed_files {
                for (ns_path, namespace) in &parsed.namespaces {
                    merge_namespace(
                        &mut unit_namespaces,
                        ns_path.clone(),
                        namespace.clone(),
                        NsMergePolicy::Strict,
                    )?;
                }
            }

            Ok((discovered.unit.clone(), unit_namespaces, parsed_files))
        })
        .collect();

    // Check for parse errors
    let mut parsed_units: Vec<(
        crate::lang::cache::unit_cache::CompilationUnit,
        IndexMap<crate::lang::ast::NsPath, crate::lang::ast::Namespace>,
        Vec<ParsedFile>,
    )> = Vec::new();

    for result in parsed_results {
        match result {
            Ok(data) => parsed_units.push(data),
            Err(e) => return Err(e),
        }
    }

    // Phase 3: Save all new cache entries in parallel (non-blocking)
    // We collect the units to save and spawn parallel saves
    let units_to_save: Vec<_> = parsed_units
        .iter()
        .map(|(unit, namespaces, _)| (unit.clone(), namespaces.clone()))
        .collect();

    // Save caches in parallel
    units_to_save.par_iter().for_each(|(unit, namespaces)| {
        if let Err(e) = unit_cache.save(unit, namespaces) {
            tracing::warn!("Failed to save cache for {}: {}", unit.id(), e);
        }
    });

    // Merge all namespaces in discovery order — the same order whether a unit
    // came from cache or was freshly parsed, so the merged program is
    // independent of cache state. (Previously cached units merged before
    // parsed ones, so the winner of a namespace collision depended on which
    // unit happened to be cached.) Cross-unit duplicate definitions are
    // strict errors, which also makes the outcome order-independent.
    let mut all_namespaces = IndexMap::new();
    let mut all_parsed_files = Vec::new();
    let mut parsed_iter = parsed_units.into_iter();
    for (discovered, cached) in cache_results {
        let namespaces = match cached {
            Some(namespaces) => namespaces,
            None => {
                let (unit, namespaces, parsed_files) = parsed_iter.next().ok_or_else(|| {
                    "internal error: fewer parsed units than cache misses".to_string()
                })?;
                debug_assert_eq!(unit.id(), discovered.unit.id());
                all_parsed_files.extend(parsed_files);
                namespaces
            }
        };
        for (ns_path, namespace) in namespaces {
            merge_namespace(
                &mut all_namespaces,
                ns_path,
                namespace,
                NsMergePolicy::Strict,
            )?;
        }
    }

    // Report cache stats
    if std::env::var("DEBUG_TIMING").is_ok() {
        let hits = cache_hits.load(Ordering::Relaxed);
        let misses = cache_misses.load(Ordering::Relaxed);
        let total = hits + misses;
        if total > 0 {
            eprintln!(
                "Package cache: {} hits, {} misses ({:.1}% hit rate)",
                hits,
                misses,
                (hits as f64 / total as f64) * 100.0
            );
        }
    }

    Ok((all_namespaces, all_parsed_files))
}

/// Get the disk cache path for a source file (per-file caching)
/// Returns None - per-file disk caching is disabled in favor of per-unit caching
/// (see unit_cache module) which provides better granularity and cache invalidation.
fn get_disk_cache_path(_file_path: &str) -> Option<std::path::PathBuf> {
    // Per-file disk caching disabled. Per-unit caching (unit_cache.rs) is now
    // the primary disk caching mechanism, providing namespace-level caching with
    // proper cache invalidation based on source file hashes.
    None
}

/// Disk cache entry with version and content hash
#[derive(serde::Serialize, serde::Deserialize)]
struct DiskCacheEntry {
    /// Cache format version
    version: u32,
    /// Hash of the source file content
    content_hash: String,
    /// Serialized namespaces using ast_cache format
    data: Vec<u8>,
}

/// Current disk cache version (increment when format changes)
const DISK_CACHE_VERSION: u32 = 1;

/// Load from disk cache if valid
fn load_from_disk_cache(
    cache_path: &std::path::Path,
    content_hash: &str,
) -> Option<IndexMap<crate::lang::ast::NsPath, crate::lang::ast::Namespace>> {
    let data = std::fs::read(cache_path).ok()?;
    let entry: DiskCacheEntry = serde_json::from_slice(&data).ok()?;

    // Validate version and content hash
    if entry.version != DISK_CACHE_VERSION || entry.content_hash != content_hash {
        return None;
    }

    // Deserialize namespaces using ast_cache
    crate::lang::cache::ast_cache::deserialize_namespaces(&entry.data).ok()
}

/// Save to disk cache
fn save_to_disk_cache(
    cache_path: &std::path::Path,
    content_hash: &str,
    namespaces: &IndexMap<crate::lang::ast::NsPath, crate::lang::ast::Namespace>,
) {
    // Serialize namespaces using ast_cache (handles Val::Map with non-string keys)
    let data = match crate::lang::cache::ast_cache::serialize_namespaces(namespaces) {
        Ok(d) => d,
        Err(e) => {
            tracing::debug!("Failed to serialize AST for disk cache: {}", e);
            return;
        }
    };

    let entry = DiskCacheEntry {
        version: DISK_CACHE_VERSION,
        content_hash: content_hash.to_string(),
        data,
    };

    // Ensure cache directory exists
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    // Write atomically (write to temp then rename)
    let temp_path = cache_path.with_extension("json.tmp");
    if let Ok(json) = serde_json::to_vec(&entry)
        && std::fs::write(&temp_path, json).is_ok()
    {
        let _ = std::fs::rename(temp_path, cache_path);
    }
}

/// Parse files in parallel using rayon with memory and disk caching
/// Returns parsed files and any errors that occurred
pub(super) fn parse_files_parallel(
    file_paths: &[String],
    color: bool,
) -> (Vec<ParsedFile>, Vec<String>) {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Cache stats
    let memory_hits = AtomicUsize::new(0);
    let disk_hits = AtomicUsize::new(0);
    let cache_misses = AtomicUsize::new(0);

    let results: Vec<_> = file_paths
        .par_iter()
        .map(|file_path| match std::fs::read_to_string(file_path) {
            Ok(content) => {
                let content_hash = compute_content_hash(&content);

                // Check memory cache first (fastest)
                {
                    let cache = PARSE_CACHE.lock();
                    if let Some(cached) = cache.get(file_path)
                        && cached.content_hash == content_hash
                    {
                        memory_hits.fetch_add(1, Ordering::Relaxed);
                        return Ok(ParsedFile {
                            path: file_path.clone(),
                            content,
                            namespaces: cached.namespaces.clone(),
                        });
                    }
                }

                // Check disk cache next
                if let Some(cache_path) = get_disk_cache_path(file_path)
                    && let Some(namespaces) = load_from_disk_cache(&cache_path, &content_hash)
                {
                    disk_hits.fetch_add(1, Ordering::Relaxed);

                    // Populate memory cache for future accesses
                    PARSE_CACHE.lock().insert(
                        file_path.clone(),
                        CachedParseResult {
                            content_hash: content_hash.clone(),
                            namespaces: namespaces.clone(),
                        },
                    );

                    return Ok(ParsedFile {
                        path: file_path.clone(),
                        content,
                        namespaces,
                    });
                }

                // Cache miss - need to parse
                cache_misses.fetch_add(1, Ordering::Relaxed);
                match crate::lang::parser::parse_hot_file(&content, file_path) {
                    Ok(program) => {
                        // Save to memory cache
                        PARSE_CACHE.lock().insert(
                            file_path.clone(),
                            CachedParseResult {
                                content_hash: content_hash.clone(),
                                namespaces: program.namespaces.clone(),
                            },
                        );

                        // Save to disk cache (async-safe since we use atomic rename)
                        if let Some(cache_path) = get_disk_cache_path(file_path) {
                            save_to_disk_cache(&cache_path, &content_hash, &program.namespaces);
                        }

                        Ok(ParsedFile {
                            path: file_path.clone(),
                            content,
                            namespaces: program.namespaces,
                        })
                    }
                    Err(e) => {
                        if let Some(formatted) = e.format_error(&content, color) {
                            Err(format!("Parse errors in {}:\n{}", file_path, formatted))
                        } else {
                            Err(format!("Parse error in {}: {}", file_path, e))
                        }
                    }
                }
            }
            Err(e) => Err(format!("Failed to read {}: {}", file_path, e)),
        })
        .collect();

    // Report cache stats (only when DEBUG_TIMING is set)
    if std::env::var("DEBUG_TIMING").is_ok() {
        let mem_hits = memory_hits.load(Ordering::Relaxed);
        let disk = disk_hits.load(Ordering::Relaxed);
        let misses = cache_misses.load(Ordering::Relaxed);
        let total = mem_hits + disk + misses;
        if total > 0 {
            eprintln!(
                "Parse cache: {} memory hits, {} disk hits, {} misses ({:.1}% hit rate)",
                mem_hits,
                disk,
                misses,
                ((mem_hits + disk) as f64 / total as f64) * 100.0
            );
        }
    }

    let mut parsed_files = Vec::new();
    let mut errors = Vec::new();

    for result in results {
        match result {
            Ok(parsed) => parsed_files.push(parsed),
            Err(e) => errors.push(e),
        }
    }

    (parsed_files, errors)
}

// ============================================================================
// Engine impl: filesystem discovery
// ============================================================================

impl Engine {
    /// Discover `.hot` files under a path recursively.
    ///
    /// Routes through [`crate::discovery::discover`] so `.gitignore`,
    /// `.git/info/exclude`, global git ignores, `.ignore`, and `.hotignore`
    /// are all honored, and the [`crate::discovery::DEFAULT_HARD_EXCLUDES`]
    /// list (`target/`, `node_modules/`, `.hot/`, …) is always applied.
    pub fn discover_hot_files(path: &str) -> Result<Vec<String>, String> {
        let path_buf = std::path::PathBuf::from(path);
        if !path_buf.exists() {
            tracing::debug!("Path does not exist: {}", path);
            return Ok(Vec::new());
        }

        let opts = crate::discovery::DiscoveryOpts::for_extension("hot");
        Ok(crate::discovery::discover_paths(&[path_buf], &opts))
    }

    /// Discover .hot files from a dependency's src_paths only (not test_paths)
    ///
    /// This reads the package's pkg.hot file to get src_paths and only discovers
    /// files from those directories. This prevents test files from being included
    /// in production builds.
    pub fn discover_dependency_source_files(
        pkg_root: &std::path::Path,
    ) -> Result<Vec<String>, String> {
        let pkg_hot_path = pkg_root.join("pkg.hot");

        // If no pkg.hot exists, fall back to discovering all files (for simple packages)
        if !pkg_hot_path.exists() {
            tracing::debug!(
                "No pkg.hot found in {}, falling back to full discovery",
                pkg_root.display()
            );
            return Self::discover_hot_files(&pkg_root.to_string_lossy());
        }

        // Parse the pkg.hot file to get src_paths
        let pkg_content = std::fs::read_to_string(&pkg_hot_path)
            .map_err(|e| format!("Failed to read {}: {}", pkg_hot_path.display(), e))?;

        // Use eval_simple to parse the pkg.hot file
        let pkg_val = Self::eval_simple(&pkg_content)?;

        // Find the package config (the value of the first key that starts with "hot.pkg.")
        let src_paths = match &pkg_val {
            crate::val::Val::Map(map) => {
                let mut found_src_paths: Vec<String> = Vec::new();
                for (key, value) in map.iter() {
                    if let crate::val::Val::Str(key_str) = key
                        && key_str.starts_with("hot.pkg.")
                    {
                        // This is the package config
                        if let crate::val::Val::Map(config) = value {
                            // Look for src-paths (preferred) or src_paths (legacy)
                            let paths_val = config
                                .get(&crate::val::Val::from("src-paths"))
                                .or_else(|| config.get(&crate::val::Val::from("src_paths")));
                            if let Some(crate::val::Val::Vec(paths)) = paths_val {
                                for path in paths {
                                    if let crate::val::Val::Str(path_str) = path {
                                        found_src_paths.push((**path_str).to_owned());
                                    }
                                }
                            }
                        }
                    }
                }
                if found_src_paths.is_empty() {
                    // No src_paths found, default to "src/"
                    vec!["src/".to_string()]
                } else {
                    found_src_paths
                }
            }
            _ => vec!["src/".to_string()],
        };

        tracing::debug!(
            "Dependency {}: src_paths = {:?}",
            pkg_root.display(),
            src_paths
        );

        // Discover files from each src_path
        let mut all_files = Vec::new();
        for src_path in src_paths {
            let full_path = pkg_root.join(&src_path);
            if full_path.exists() {
                let files = Self::discover_hot_files(&full_path.to_string_lossy())?;
                all_files.extend(files);
            } else {
                tracing::debug!(
                    "src_path {} does not exist in {}",
                    src_path,
                    pkg_root.display()
                );
            }
        }

        Ok(all_files)
    }

    /// Return the resolved src-path directories for a package (from its pkg.hot).
    /// Unlike `discover_dependency_source_files`, this returns directory paths
    /// rather than individual files, for use by the bundler.
    pub fn discover_dependency_source_dirs(
        pkg_root: &std::path::Path,
    ) -> Result<Vec<String>, String> {
        let pkg_hot_path = pkg_root.join("pkg.hot");

        if !pkg_hot_path.exists() {
            return Ok(vec![pkg_root.to_string_lossy().to_string()]);
        }

        let pkg_content = std::fs::read_to_string(&pkg_hot_path)
            .map_err(|e| format!("Failed to read {}: {}", pkg_hot_path.display(), e))?;

        let pkg_val = Self::eval_simple(&pkg_content)?;

        let src_paths = match &pkg_val {
            crate::val::Val::Map(map) => {
                let mut found: Vec<String> = Vec::new();
                for (key, value) in map.iter() {
                    if let crate::val::Val::Str(key_str) = key
                        && key_str.starts_with("hot.pkg.")
                        && let crate::val::Val::Map(config) = value
                    {
                        let paths_val = config
                            .get(&crate::val::Val::from("src-paths"))
                            .or_else(|| config.get(&crate::val::Val::from("src_paths")));
                        if let Some(crate::val::Val::Vec(paths)) = paths_val {
                            for path in paths {
                                if let crate::val::Val::Str(path_str) = path {
                                    found.push((**path_str).to_owned());
                                }
                            }
                        }
                    }
                }
                if found.is_empty() {
                    vec!["src/".to_string()]
                } else {
                    found
                }
            }
            _ => vec!["src/".to_string()],
        };

        Ok(src_paths
            .into_iter()
            .map(|p| pkg_root.join(&p).to_string_lossy().to_string())
            .filter(|p| std::path::Path::new(p).exists())
            .collect())
    }
}

#[cfg(test)]
mod ns_merge_tests {
    use super::*;
    use crate::lang::ast::{Namespace, NsPath};

    /// Parse `source` as if it came from `file` and return its namespaces.
    fn parse(source: &str, file: &str) -> IndexMap<NsPath, Namespace> {
        crate::lang::parser::parse_hot_file(source, file)
            .expect("test source should parse")
            .namespaces
    }

    fn merge_all(
        target: &mut IndexMap<NsPath, Namespace>,
        namespaces: IndexMap<NsPath, Namespace>,
        policy: NsMergePolicy,
    ) -> Result<(), String> {
        for (ns_path, namespace) in namespaces {
            merge_namespace(target, ns_path, namespace, policy)?;
        }
        Ok(())
    }

    fn ns<'a>(target: &'a IndexMap<NsPath, Namespace>, path: &str) -> &'a Namespace {
        target
            .iter()
            .find(|(p, _)| p.to_string() == path)
            .map(|(_, ns)| ns)
            .unwrap_or_else(|| panic!("namespace {} missing", path))
    }

    fn var_names(namespace: &Namespace) -> Vec<String> {
        namespace
            .scope
            .vars
            .keys()
            .map(|v| v.sym.name().to_string())
            .collect()
    }

    #[test]
    fn two_files_same_namespace_members_combine() {
        let mut target = IndexMap::new();
        merge_all(
            &mut target,
            parse("::myapp ns\n\nalpha fn (): Str { \"A\" }\n", "a.hot"),
            NsMergePolicy::Strict,
        )
        .unwrap();
        merge_all(
            &mut target,
            parse("::myapp ns\n\nbeta fn (): Str { \"B\" }\n", "b.hot"),
            NsMergePolicy::Strict,
        )
        .unwrap();

        let merged = ns(&target, "::myapp");
        let names = var_names(merged);
        assert!(
            names.contains(&"alpha".to_string()),
            "alpha kept: {names:?}"
        );
        assert!(names.contains(&"beta".to_string()), "beta added: {names:?}");
    }

    #[test]
    fn duplicate_definition_across_files_is_error_in_both_orders() {
        let a = parse("::myapp ns\n\nthing fn (): Str { \"A\" }\n", "a.hot");
        let b = parse("::myapp ns\n\nthing fn (): Str { \"B\" }\n", "b.hot");

        // Strict merging must fail regardless of merge order — this is what
        // makes the pipeline result independent of cache-hit/miss ordering.
        for (first, second) in [(a.clone(), b.clone()), (b, a)] {
            let mut target = IndexMap::new();
            merge_all(&mut target, first, NsMergePolicy::Strict).unwrap();
            let err = merge_all(&mut target, second, NsMergePolicy::Strict)
                .expect_err("duplicate definition must error");
            assert!(err.contains("Duplicate definition of 'thing'"), "{err}");
            assert!(err.contains("a.hot") && err.contains("b.hot"), "{err}");
        }
    }

    #[test]
    fn same_file_merged_twice_is_idempotent() {
        // `hot run x.hot` parses the target file both as part of its source
        // unit and as the target file — the second merge must be a no-op.
        let mut target = IndexMap::new();
        let first = parse("::myapp ns\n\nalpha fn (): Str { \"A\" }\n", "x.hot");
        merge_all(&mut target, first.clone(), NsMergePolicy::Strict).unwrap();
        merge_all(&mut target, first, NsMergePolicy::Strict).unwrap();

        let names = var_names(ns(&target, "::myapp"));
        let alpha_count = names.iter().filter(|n| n.as_str() == "alpha").count();
        assert_eq!(alpha_count, 1, "no duplicated binding: {names:?}");
    }

    #[test]
    fn shadow_policy_appends_newest_binding_and_keeps_others() {
        // Eval/REPL: declaring into an existing namespace must preserve its
        // other members (previously the whole namespace was replaced) and a
        // re-declared name must shadow the old binding (appended last).
        let mut target = IndexMap::new();
        merge_all(
            &mut target,
            parse(
                "::myapp ns\n\nalpha fn (): Str { \"A\" }\nbeta fn (): Str { \"B\" }\n",
                "src.hot",
            ),
            NsMergePolicy::Strict,
        )
        .unwrap();
        merge_all(
            &mut target,
            parse("::myapp ns\n\nbeta fn (): Str { \"B2\" }\n", "<eval>"),
            NsMergePolicy::Shadow,
        )
        .unwrap();

        let names = var_names(ns(&target, "::myapp"));
        assert!(
            names.contains(&"alpha".to_string()),
            "alpha kept: {names:?}"
        );
        assert_eq!(
            names.last().map(String::as_str),
            Some("beta"),
            "newest beta appended last so it wins resolution: {names:?}"
        );
        assert_eq!(
            names.iter().filter(|n| n.as_str() == "beta").count(),
            2,
            "shadowed binding coexists like REPL redefinition: {names:?}"
        );
    }

    #[test]
    fn project_declaring_hot_std_namespace_collides_deterministically() {
        // Simulates a project src file declaring a hot-std namespace path.
        // Adding a NEW name merges member-wise; redefining an EXISTING name
        // errors — in both merge orders.
        let std_ns = parse(
            "::hot::str ns\n\ntrim fn (s: Str): Str { s }\n",
            "/usr/local/share/hot/pkg/hot-std/src/hot/str.hot",
        );
        let addition = parse(
            "::hot::str ns\n\nshout fn (s: Str): Str { s }\n",
            "src/ext.hot",
        );
        let redefinition = parse(
            "::hot::str ns\n\ntrim fn (s: Str): Str { s }\n",
            "src/ext.hot",
        );

        let mut target = IndexMap::new();
        merge_all(&mut target, std_ns.clone(), NsMergePolicy::Strict).unwrap();
        merge_all(&mut target, addition, NsMergePolicy::Strict).unwrap();
        let names = var_names(ns(&target, "::hot::str"));
        assert!(
            names.contains(&"trim".to_string()),
            "std member kept: {names:?}"
        );
        assert!(
            names.contains(&"shout".to_string()),
            "addition merged: {names:?}"
        );

        for (first, second) in [
            (std_ns.clone(), redefinition.clone()),
            (redefinition, std_ns),
        ] {
            let mut target = IndexMap::new();
            merge_all(&mut target, first, NsMergePolicy::Strict).unwrap();
            let err = merge_all(&mut target, second, NsMergePolicy::Strict)
                .expect_err("redefining a std binding must error");
            assert!(err.contains("Duplicate definition of 'trim'"), "{err}");
        }
    }

    #[test]
    fn alias_conflicts_error_under_strict_and_win_under_shadow() {
        let a = parse("::myapp ns\n::h ::hot::http\n", "a.hot");
        let b = parse("::myapp ns\n::h ::hot::html\n", "b.hot");

        let mut target = IndexMap::new();
        merge_all(&mut target, a.clone(), NsMergePolicy::Strict).unwrap();
        let err = merge_all(&mut target, b.clone(), NsMergePolicy::Strict)
            .expect_err("conflicting alias must error");
        assert!(err.contains("alias"), "{err}");

        let mut target = IndexMap::new();
        merge_all(&mut target, a, NsMergePolicy::Strict).unwrap();
        merge_all(&mut target, b, NsMergePolicy::Shadow).unwrap();
        let merged = ns(&target, "::myapp");
        let aliased = merged
            .aliases
            .iter()
            .find(|(alias, _)| alias.to_string() == "::h")
            .map(|(_, t)| t.to_string());
        assert_eq!(aliased.as_deref(), Some("::hot::html"), "shadow alias wins");
    }
}
