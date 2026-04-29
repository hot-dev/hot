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

    // Separate hits from misses
    let mut cached_namespaces: Vec<
        IndexMap<crate::lang::ast::NsPath, crate::lang::ast::Namespace>,
    > = Vec::new();
    let mut units_to_parse: Vec<&DiscoveredUnit> = Vec::new();

    for (discovered, result) in &cache_results {
        match result {
            Some(namespaces) => {
                cached_namespaces.push(namespaces.clone());
            }
            None => {
                units_to_parse.push(discovered);
            }
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

            // Collect namespaces from this unit
            let mut unit_namespaces = IndexMap::new();
            for parsed in &parsed_files {
                for (ns_path, namespace) in &parsed.namespaces {
                    unit_namespaces.insert(ns_path.clone(), namespace.clone());
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

    // Merge all namespaces (cached + parsed)
    let mut all_namespaces = IndexMap::new();

    // Add cached namespaces
    for namespaces in cached_namespaces {
        for (ns_path, namespace) in namespaces {
            all_namespaces.insert(ns_path, namespace);
        }
    }

    // Add parsed namespaces
    let mut all_parsed_files = Vec::new();
    for (_, namespaces, parsed_files) in parsed_units {
        for (ns_path, namespace) in namespaces {
            all_namespaces.insert(ns_path, namespace);
        }
        all_parsed_files.extend(parsed_files);
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
