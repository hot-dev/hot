//! Compilation Unit AST Cache
//!
//! This module provides fine-grained AST (parsed namespace) caching at the
//! compilation unit level. A compilation unit is either a package (dependency)
//! or a source path (project src/test). Unlike the whole-program bytecode cache,
//! this enables:
//! - Incremental compilation: only reparse changed units
//! - Faster cold starts: load cached AST for unchanged dependencies
//! - Better cache invalidation: only invalidate affected units
//!
//! ## Cache Location
//!
//! Package units and source-path units are stored in *different* directories.
//! A package's parsed AST depends only on its own immutable sources plus the
//! runtime identity already in the cache key, so it is machine-scoped
//! (`cache_paths::get_package_unit_cache_dir()`) and one parsed copy serves
//! every project. Project sources are project-scoped
//! (`cache_paths::get_unit_cache_dir()`) so their ASTs never leak between
//! checkouts.
//!
//! Source-path resolution:
//! - `$HOT_HOME/cache/unit` if HOT_HOME is set
//! - `./.hot/cache/unit` if `hot.hot` config exists (project-local cache)
//! - System cache directory otherwise (platform-specific):
//!   - Linux: `~/.cache/hot/cache/unit`
//!   - macOS: `~/Library/Caches/hot/cache/unit`
//!   - Windows: `%LOCALAPPDATA%\hot\cache\unit`
//!
//! ## Cache Structure
//! ```text
//! .hot/cache/unit/
//!   pkg-hot-std-{hash}.ast.zst       # hot-std package
//!   pkg-openai-{hash}.ast.zst        # openai package
//!   src-main-{hash}.ast.zst          # project src/main
//!   src-test-{hash}.ast.zst          # project src/test
//! ```
//!
//! ## Cache Key
//! Each cache entry is keyed by:
//! - Hash of all source files in the unit
//! - Hot engine version
//! - Cache format version
//!
//! ## Serialization Strategy
//! Uses the ast_cache module which provides tagged JSON serialization
//! that correctly handles Val::Map with non-string keys. The JSON is then
//! compressed with zstd.

use crate::hasher::{CacheKeyBuilder, CacheType, compute_hot_file_hashes};
use crate::lang::ast::{Namespace, NsPath};
use crate::lang::cache::ast_cache;
use indexmap::IndexMap;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// A compilation unit (package or source path)
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CompilationUnit {
    /// A package from dependencies (e.g., "hot-std", "openai")
    Package { name: String, path: PathBuf },
    /// A source path from the project (e.g., "src/main", "src/test")
    SourcePath { name: String, path: PathBuf },
}

impl CompilationUnit {
    /// Get a unique identifier for this compilation unit
    pub fn id(&self) -> String {
        match self {
            CompilationUnit::Package { name, .. } => format!("pkg-{}", name),
            CompilationUnit::SourcePath { name, .. } => format!("src-{}", name),
        }
    }

    /// Get a filesystem-safe identifier (no special chars)
    pub fn fs_safe_id(&self) -> String {
        self.id().replace(['/', '\\', ':'], "-").replace('.', "_")
    }

    /// Filesystem-safe identity for this unit *at this location*.
    ///
    /// The plain `fs_safe_id` is only the unit's name, which is ambiguous in
    /// the machine-shared package cache: two monorepos can each have a local
    /// dependency called `mylib`. Appending a hash of the resolved path scopes
    /// every entry (and its lock) to one specific directory, which is what
    /// makes the generation sweep in `save` safe.
    pub fn scoped_fs_id(&self) -> String {
        let path = self
            .path()
            .canonicalize()
            .unwrap_or_else(|_| self.path().to_path_buf());
        let digest = crate::hasher::HotHasher::hash_content(path.to_string_lossy().as_bytes());
        format!("{}-{}", self.fs_safe_id(), &digest[..8])
    }

    /// Get the path to source files
    pub fn path(&self) -> &Path {
        match self {
            CompilationUnit::Package { path, .. } => path,
            CompilationUnit::SourcePath { path, .. } => path,
        }
    }
}

/// Cached parsed namespaces for a compilation unit
/// Note: This struct uses custom serialization via ast_cache module
#[derive(Debug, Clone)]
pub struct CachedUnit {
    /// Cache format version
    pub version: u32,
    /// Hot engine version
    pub hot_version: String,
    /// Hash of all source files in this unit
    pub source_hash: String,
    /// Parsed namespaces
    pub namespaces: IndexMap<NsPath, Namespace>,
}

/// Serializable wrapper for cache metadata (without namespaces)
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheMetadata {
    version: u32,
    hot_version: String,
    source_hash: String,
}

/// Complete cache file structure
#[derive(Debug, Clone, Serialize, Deserialize)]
struct CacheFile {
    metadata: CacheMetadata,
    /// Namespaces serialized using ast_cache format (as JSON bytes)
    namespaces_data: Vec<u8>,
}

fn validate_cache_metadata(
    metadata: &CacheMetadata,
    expected_source_hash: &str,
) -> Result<(), String> {
    if metadata.source_hash != expected_source_hash {
        return Err(format!(
            "Unit cache source hash mismatch: cache={}, requested={}",
            metadata.source_hash, expected_source_hash
        ));
    }
    if metadata.version != CacheType::Ast.format_version() {
        return Err(format!(
            "Unit cache format version mismatch: cache={}, current={}",
            metadata.version,
            CacheType::Ast.format_version()
        ));
    }
    if metadata.hot_version != crate::build_info::VERSION {
        return Err(format!(
            "Unit cache Hot version mismatch: cache={}, current={}",
            metadata.hot_version,
            crate::build_info::VERSION
        ));
    }
    Ok(())
}

/// Compilation unit cache manager
/// Caches parsed AST for packages and source paths
/// Supports cross-process synchronization via file locking
pub struct UnitCache {
    /// Cache directory for source-path units (project-local when in a project)
    cache_dir: PathBuf,
    /// Cache directory for package units. Machine-scoped so one parsed copy of
    /// a dependency serves every project; `None` means "same as `cache_dir`"
    /// (used by tests that pin a single directory).
    package_cache_dir: Option<PathBuf>,
}

impl UnitCache {
    /// Create a new unit cache manager
    pub fn new(cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            package_cache_dir: None,
        }
    }

    /// Cache manager that stores package units in a separate (machine-scoped)
    /// directory from source-path units.
    pub fn with_package_dir(cache_dir: PathBuf, package_cache_dir: PathBuf) -> Self {
        Self {
            cache_dir,
            package_cache_dir: Some(package_cache_dir),
        }
    }

    /// Directory holding this unit's cache entry: packages are machine-scoped,
    /// project sources stay project-local.
    fn dir_for(&self, unit: &CompilationUnit) -> &Path {
        match (unit, &self.package_cache_dir) {
            (CompilationUnit::Package { .. }, Some(dir)) => dir,
            _ => &self.cache_dir,
        }
    }

    /// Get the default cache directory.
    ///
    /// Uses smart resolution:
    /// - `$HOT_HOME/cache/unit` if HOT_HOME is set
    /// - `./.hot/cache/unit` if `hot.hot` config exists (project-local)
    /// - Platform-specific system cache otherwise (e.g., `~/.cache/hot/cache/unit`)
    pub fn default_cache_dir() -> PathBuf {
        super::paths::get_unit_cache_dir()
    }

    /// Acquire a cross-process file lock for a compilation unit.
    /// Returns the lock file handle (releases lock when dropped).
    pub fn acquire_file_lock(
        &self,
        unit: &CompilationUnit,
    ) -> Result<fd_lock::RwLock<std::fs::File>, std::io::Error> {
        let dir = self.dir_for(unit);
        std::fs::create_dir_all(dir)?;

        let lock_path = dir.join(format!("{}.lock", unit.scoped_fs_id()));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;

        Ok(fd_lock::RwLock::new(file))
    }

    /// Get the cache file path for a compilation unit
    fn cache_path(&self, unit: &CompilationUnit, cache_key: &str) -> PathBuf {
        let filename = format!(
            "{}-{}.{}",
            unit.scoped_fs_id(),
            &cache_key[..16],
            CacheType::Ast.extension()
        );
        self.dir_for(unit).join(filename)
    }

    /// Compute cache key for a compilation unit
    /// Uses unified CacheKeyBuilder which includes Hot version and format version
    pub fn compute_cache_key(&self, unit: &CompilationUnit) -> Result<String, String> {
        let file_hashes = compute_hot_file_hashes(unit.path())?;

        Ok(CacheKeyBuilder::new(CacheType::Ast)
            .with_prefix(&unit.id())
            .with_file_hashes(&file_hashes)
            .finalize())
    }

    /// Try to load a cached unit
    /// The full computed key and runtime metadata are revalidated from the
    /// payload before the entry is accepted.
    pub fn load(&self, unit: &CompilationUnit) -> Result<Option<CachedUnit>, String> {
        // Compute cache key (includes version + file hashes)
        let cache_key = self.compute_cache_key(unit)?;

        // Check if cache file exists
        let cache_path = self.cache_path(unit, &cache_key);
        if !cache_path.exists() {
            return Ok(None);
        }

        // Read failures can be transient. Only bytes that were successfully
        // read but then fail decode/validation establish corruption or staleness.
        let compressed = std::fs::read(&cache_path).map_err(|e| e.to_string())?;
        let decoded = (|| {
            let data = zstd::decode_all(compressed.as_slice()).map_err(|e| e.to_string())?;
            let cache_file: CacheFile = postcard::from_bytes(&data)
                .map_err(|e| format!("Failed to deserialize cache file: {}", e))?;
            validate_cache_metadata(&cache_file.metadata, &cache_key)?;
            let namespaces = ast_cache::deserialize_namespaces(&cache_file.namespaces_data)?;
            Ok((cache_file.metadata, namespaces))
        })();
        let (metadata, namespaces) = match decoded {
            Ok(decoded) => decoded,
            Err(error) => {
                super::remove_invalid_cache_entry(&cache_path);
                return Err(error);
            }
        };

        super::touch_cache_entry(&cache_path);
        Ok(Some(CachedUnit {
            version: metadata.version,
            hot_version: metadata.hot_version,
            source_hash: metadata.source_hash,
            namespaces,
        }))
    }

    /// Save a compilation unit to cache
    /// Uses parallel zstd compression for faster saves on larger payloads
    /// Uses file locking to prevent cross-process races
    pub fn save(
        &self,
        unit: &CompilationUnit,
        namespaces: &IndexMap<NsPath, Namespace>,
    ) -> Result<(), String> {
        // Compute cache key (includes version + file hashes)
        let cache_key = self.compute_cache_key(unit)?;

        // Ensure cache directory exists
        std::fs::create_dir_all(self.dir_for(unit)).map_err(|e| e.to_string())?;

        // Acquire cross-process file lock (best effort - proceed even if locking fails)
        // Block rather than try_write: a writer that lost the lock still went
        // on to write its entry, and the lock holder's sweep then deleted that
        // fresh generation. Waiting keeps write-then-prune atomic per unit.
        // Locks are per-unit, so unrelated units still save in parallel.
        let mut file_lock = self.acquire_file_lock(unit).ok();
        let file_lock_guard = file_lock.as_mut().and_then(|lock| lock.write().ok());

        // Check if cache already exists (another process may have just saved it)
        let cache_path = self.cache_path(unit, &cache_key);
        if cache_path.exists() {
            tracing::debug!("Cache already exists for {} (skipping save)", unit.id());
            return Ok(());
        }

        // Serialize namespaces using ast_cache (handles Val::Map correctly)
        let namespaces_data = ast_cache::serialize_namespaces(namespaces)?;

        // Create cache file with metadata for debugging
        let cache_file = CacheFile {
            metadata: CacheMetadata {
                version: CacheType::Ast.format_version(),
                hot_version: crate::build_info::VERSION.to_string(),
                source_hash: cache_key.clone(),
            },
            namespaces_data,
        };

        // Serialize with postcard (compact binary, much faster to decode
        // than the previous serde_json encoding)
        let data = postcard::to_allocvec(&cache_file)
            .map_err(|e| format!("Failed to serialize cache: {}", e))?;

        // Compress with zstd level 1 for speed (level 1 is ~3x faster than level 3
        // with only slightly worse ratio). Parallel compression across multiple cache
        // files is already handled at the caller level via rayon.
        let mut encoder = zstd::Encoder::new(Vec::new(), 1).map_err(|e| e.to_string())?;
        encoder.write_all(&data).map_err(|e| e.to_string())?;
        let compressed = encoder.finish().map_err(|e| e.to_string())?;

        // Write atomically (temp file + rename)
        let temp_path = cache_path.with_extension("ast.zst.tmp");
        std::fs::write(&temp_path, &compressed).map_err(|e| e.to_string())?;
        std::fs::rename(&temp_path, &cache_path).map_err(|e| e.to_string())?;

        // Opportunistic housekeeping: old-generation entries (superseded
        // versions/formats) are unreachable via their keys and would
        // otherwise accumulate forever.
        // Collapse this unit to a single live generation, then age-sweep the
        // rest of the directory.
        // Only prune while holding this unit's lock. Unsynchronized, two
        // processes writing different generations would each delete the
        // other's freshly written entry and leave nothing cached.
        if file_lock_guard.is_some() {
            let suffix = format!(".{}", CacheType::Ast.extension());
            super::prune_superseded_entries(
                self.dir_for(unit),
                &unit.scoped_fs_id(),
                &suffix,
                &cache_path,
            );
            super::prune_legacy_unscoped_entries(self.dir_for(unit), &unit.fs_safe_id(), &suffix);
        }
        drop(file_lock_guard);
        super::prune_stale_cache_files(self.dir_for(unit), &cache_path);

        tracing::debug!(
            "Saved cache for {} ({} namespaces, {} bytes -> {} bytes compressed, {:.1}x)",
            unit.id(),
            namespaces.len(),
            data.len(),
            compressed.len(),
            data.len() as f64 / compressed.len() as f64
        );

        Ok(())
    }

    /// Clear all cached data
    pub fn clear(&self) -> Result<(), String> {
        if self.cache_dir.exists() {
            std::fs::remove_dir_all(&self.cache_dir).map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    /// Get cache statistics
    pub fn stats(&self) -> CacheStats {
        let mut stats = CacheStats::default();

        if !self.cache_dir.exists() {
            return stats;
        }

        if let Ok(entries) = std::fs::read_dir(&self.cache_dir) {
            for entry in entries.flatten() {
                if let Ok(metadata) = entry.metadata()
                    && metadata.is_file()
                {
                    stats.total_files += 1;
                    stats.total_bytes += metadata.len();

                    let name = entry.file_name().to_string_lossy().to_string();
                    if name.starts_with("pkg-") {
                        stats.package_entries += 1;
                    } else if name.starts_with("src-") {
                        stats.source_entries += 1;
                    }
                }
            }
        }

        stats
    }
}

/// Cache statistics
#[derive(Debug, Clone, Default)]
pub struct CacheStats {
    /// Total number of cache files
    pub total_files: usize,
    /// Total size in bytes
    pub total_bytes: u64,
    /// Number of package entries
    pub package_entries: usize,
    /// Number of source path entries
    pub source_entries: usize,
}

impl std::fmt::Display for CacheStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} files ({} packages, {} sources), {:.2} MB",
            self.total_files,
            self.package_entries,
            self.source_entries,
            self.total_bytes as f64 / 1024.0 / 1024.0
        )
    }
}

/// Result of loading cached units
pub struct CacheLoadResult {
    /// Successfully loaded cached units
    pub cached: Vec<(CompilationUnit, IndexMap<NsPath, Namespace>)>,
    /// Units that need parsing (cache miss)
    pub needs_parsing: Vec<CompilationUnit>,
}

impl UnitCache {
    /// Load multiple units in parallel, returning which need parsing
    pub fn load_units(&self, units: &[CompilationUnit]) -> CacheLoadResult {
        let results: Vec<(CompilationUnit, Option<IndexMap<NsPath, Namespace>>)> = units
            .par_iter()
            .map(|unit| {
                let cached = self.load(unit).ok().flatten().map(|c| c.namespaces);
                (unit.clone(), cached)
            })
            .collect();

        let mut cached = Vec::new();
        let mut needs_parsing = Vec::new();

        for (unit, namespaces) in results {
            if let Some(ns) = namespaces {
                cached.push((unit, ns));
            } else {
                needs_parsing.push(unit);
            }
        }

        CacheLoadResult {
            cached,
            needs_parsing,
        }
    }

    /// Save multiple units in parallel
    pub fn save_units(
        &self,
        units: &[(CompilationUnit, IndexMap<NsPath, Namespace>)],
    ) -> Vec<Result<(), String>> {
        units
            .par_iter()
            .map(|(unit, namespaces)| self.save(unit, namespaces))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::ast::{Namespace, NamespaceAliases, NsPath, Scope, Sym, Var};
    use crate::val::Val;

    fn create_test_namespace(name: &str) -> (NsPath, Namespace) {
        let path = NsPath::from_string(name);
        let mut vars = IndexMap::new();

        // Add a variable with a Val::Map with integer keys (the problematic case)
        let mut map = IndexMap::new();
        map.insert(Val::Int(1), Val::from("one"));
        map.insert(Val::Int(2), Val::from("two"));

        vars.insert(
            Var {
                sym: Sym::String("lookup".to_string()),
                deep_set: None,
                deep_path: None,
                meta: None,
                type_annotation: None,
                src: None,
            },
            crate::lang::ast::Value::Val(Val::Map(Box::new(map)), None),
        );

        let ns = Namespace {
            path: path.clone(),
            scope: Scope { vars },
            meta: None,
            source_file: None,
            aliases: NamespaceAliases::new(),
        };

        (path, ns)
    }

    fn create_test_unit(root: &Path) -> CompilationUnit {
        let source = root.join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("test.hot"), b"::test ns\nvalue 1\n").unwrap();
        CompilationUnit::SourcePath {
            name: "test".to_string(),
            path: source,
        }
    }

    fn write_unit_cache_file(
        cache: &UnitCache,
        unit: &CompilationUnit,
        requested_key: &str,
        embedded_key: &str,
        namespaces: &IndexMap<NsPath, Namespace>,
    ) -> PathBuf {
        std::fs::create_dir_all(&cache.cache_dir).unwrap();
        let cache_file = CacheFile {
            metadata: CacheMetadata {
                version: CacheType::Ast.format_version(),
                hot_version: crate::build_info::VERSION.to_string(),
                source_hash: embedded_key.to_string(),
            },
            namespaces_data: ast_cache::serialize_namespaces(namespaces).unwrap(),
        };
        let data = postcard::to_allocvec(&cache_file).unwrap();
        let compressed = zstd::encode_all(data.as_slice(), 1).unwrap();
        let path = cache.cache_path(unit, requested_key);
        std::fs::write(&path, compressed).unwrap();
        path
    }

    #[test]
    fn load_rejects_source_hash_mismatch_then_save_repairs() {
        let root = tempfile::tempdir().expect("tempdir");
        let unit = create_test_unit(root.path());
        let cache = UnitCache::new(root.path().join("cache"));
        let requested_key = cache.compute_cache_key(&unit).unwrap();
        let (path, namespace) = create_test_namespace("test");
        let namespaces = [(path, namespace)].into_iter().collect();
        let cache_path = write_unit_cache_file(
            &cache,
            &unit,
            &requested_key,
            "different-full-source-hash",
            &namespaces,
        );

        let error = cache
            .load(&unit)
            .expect_err("source hash mismatch must fail");
        assert!(error.contains("source hash mismatch"));
        assert!(!cache_path.exists(), "stale unit cache must be deleted");

        cache.save(&unit, &namespaces).expect("save must repair");
        assert!(cache.load(&unit).unwrap().is_some());
    }

    #[test]
    fn load_deletes_corrupt_unit_entry() {
        let root = tempfile::tempdir().expect("tempdir");
        let unit = create_test_unit(root.path());
        let cache = UnitCache::new(root.path().join("cache"));
        let key = cache.compute_cache_key(&unit).unwrap();
        std::fs::create_dir_all(&cache.cache_dir).unwrap();
        let path = cache.cache_path(&unit, &key);
        std::fs::write(&path, b"not-zstd").unwrap();

        assert!(cache.load(&unit).is_err());
        assert!(!path.exists(), "corrupt unit cache must be deleted");
    }

    #[test]
    fn load_does_not_delete_unit_entry_on_read_failure() {
        let root = tempfile::tempdir().expect("tempdir");
        let unit = create_test_unit(root.path());
        let cache = UnitCache::new(root.path().join("cache"));
        let key = cache.compute_cache_key(&unit).unwrap();
        std::fs::create_dir_all(&cache.cache_dir).unwrap();
        let path = cache.cache_path(&unit, &key);
        std::fs::create_dir(&path).unwrap();

        assert!(cache.load(&unit).is_err());
        assert!(path.is_dir(), "read failures must leave the entry in place");
    }

    #[test]
    fn load_touches_only_after_valid_unit_decode() {
        let root = tempfile::tempdir().expect("tempdir");
        let unit = create_test_unit(root.path());
        let cache = UnitCache::new(root.path().join("cache"));
        let (path, namespace) = create_test_namespace("test");
        let namespaces = [(path, namespace)].into_iter().collect();
        cache.save(&unit, &namespaces).unwrap();

        let key = cache.compute_cache_key(&unit).unwrap();
        let cache_path = cache.cache_path(&unit, &key);
        let old_system = std::time::SystemTime::now() - std::time::Duration::from_secs(48 * 3600);
        let old = filetime::FileTime::from_system_time(old_system);
        filetime::set_file_mtime(&cache_path, old).unwrap();

        assert!(cache.load(&unit).unwrap().is_some());
        let touched = std::fs::metadata(&cache_path).unwrap().modified().unwrap();
        assert!(
            touched > old_system,
            "valid cache load must refresh modification time"
        );
    }

    #[test]
    fn test_cached_unit_roundtrip() {
        let (path, ns) = create_test_namespace("test::module");
        let mut namespaces = IndexMap::new();
        namespaces.insert(path.clone(), ns);

        // Serialize using ast_cache
        let namespaces_data =
            ast_cache::serialize_namespaces(&namespaces).expect("Failed to serialize namespaces");

        // Create cache file
        let cache_file = CacheFile {
            metadata: CacheMetadata {
                version: CacheType::Ast.format_version(),
                hot_version: crate::build_info::VERSION.to_string(),
                source_hash: "abc123".to_string(),
            },
            namespaces_data,
        };

        // Serialize to JSON
        let json = serde_json::to_vec(&cache_file).expect("Failed to serialize cache file");

        // Deserialize
        let restored_file: CacheFile =
            serde_json::from_slice(&json).expect("Failed to deserialize cache file");
        let restored_namespaces = ast_cache::deserialize_namespaces(&restored_file.namespaces_data)
            .expect("Failed to deserialize namespaces");

        assert_eq!(cache_file.metadata.version, restored_file.metadata.version);
        assert_eq!(
            cache_file.metadata.source_hash,
            restored_file.metadata.source_hash
        );
        assert_eq!(namespaces.len(), restored_namespaces.len());

        // Verify the Val::Map with int keys preserved correctly
        let original_ns = namespaces.get(&path).unwrap();
        let restored_ns = restored_namespaces.get(&path).unwrap();

        let lookup_var = Var {
            sym: Sym::String("lookup".to_string()),
            deep_set: None,
            deep_path: None,
            meta: None,
            type_annotation: None,
            src: None,
        };

        assert_eq!(
            original_ns.scope.vars.get(&lookup_var),
            restored_ns.scope.vars.get(&lookup_var),
            "Val::Map with int keys should round-trip correctly"
        );
    }

    #[test]
    fn test_real_cache_file_roundtrip() {
        // This test verifies that we can serialize and deserialize a namespace
        // containing all the common AST node types
        let mut vars = IndexMap::new();

        // 1. Simple Val
        vars.insert(
            Var {
                sym: Sym::String("simple".to_string()),
                deep_set: None,
                deep_path: None,
                meta: None,
                type_annotation: None,
                src: None,
            },
            crate::lang::ast::Value::Val(Val::from("hello"), None),
        );

        // 2. Val::Map with int keys
        let mut map = IndexMap::new();
        map.insert(Val::Int(1), Val::from("one"));
        map.insert(Val::Int(2), Val::from("two"));
        vars.insert(
            Var {
                sym: Sym::String("int-map".to_string()),
                deep_set: None,
                deep_path: None,
                meta: None,
                type_annotation: None,
                src: None,
            },
            crate::lang::ast::Value::Val(Val::Map(Box::new(map)), None),
        );

        // 3. TemplateLiteral with Expression
        let template_lit = crate::lang::ast::TemplateLiteral {
            parts: vec![
                crate::lang::ast::TemplatePart::Text("hello ".to_string()),
                crate::lang::ast::TemplatePart::Expression(Box::new(crate::lang::ast::Value::Val(
                    Val::from("world"),
                    None,
                ))),
            ],
        };
        vars.insert(
            Var {
                sym: Sym::String("template".to_string()),
                deep_set: None,
                deep_path: None,
                meta: None,
                type_annotation: None,
                src: None,
            },
            crate::lang::ast::Value::TemplateLiteral(template_lit),
        );

        // 4. FnDef with body
        let fn_def = crate::lang::ast::FnDef {
            args: crate::lang::ast::FnArgs {
                args: vec![crate::lang::ast::FnArg {
                    var: Var {
                        sym: Sym::String("x".to_string()),
                        deep_set: None,
                        deep_path: None,
                        meta: None,
                        type_annotation: None,
                        src: None,
                    },
                    lazy: false,
                    type_annotation: Some("Int".to_string()),
                }],
                variadic: false,
            },
            body: crate::lang::ast::Value::Val(Val::Int(42), None),
            return_type: Some("Int".to_string()),
        };
        vars.insert(
            Var {
                sym: Sym::String("my-fn".to_string()),
                deep_set: None,
                deep_path: None,
                meta: None,
                type_annotation: None,
                src: None,
            },
            crate::lang::ast::Value::Fn(vec![fn_def]),
        );

        let ns = Namespace {
            path: NsPath::from_string("test::comprehensive"),
            scope: Scope { vars },
            meta: None,
            source_file: None,
            aliases: NamespaceAliases::new(),
        };

        let mut namespaces = IndexMap::new();
        namespaces.insert(ns.path.clone(), ns);

        // Serialize
        let serialized = ast_cache::serialize_namespaces(&namespaces).expect("serialize failed");

        // Deserialize
        let deserialized =
            ast_cache::deserialize_namespaces(&serialized).expect("deserialize failed");

        assert_eq!(namespaces.len(), deserialized.len());

        // Verify all variable types came back correctly
        let original_ns = namespaces
            .get(&NsPath::from_string("test::comprehensive"))
            .unwrap();
        let restored_ns = deserialized
            .get(&NsPath::from_string("test::comprehensive"))
            .unwrap();

        assert_eq!(
            original_ns.scope.vars.len(),
            restored_ns.scope.vars.len(),
            "Should have same number of variables"
        );

        // Check the int-map specifically
        let int_map_var = Var {
            sym: Sym::String("int-map".to_string()),
            deep_set: None,
            deep_path: None,
            meta: None,
            type_annotation: None,
            src: None,
        };
        let original_val = original_ns.scope.vars.get(&int_map_var).unwrap();
        let restored_val = restored_ns.scope.vars.get(&int_map_var).unwrap();
        assert_eq!(original_val, restored_val, "int-map should match exactly");
    }

    #[test]
    fn test_zstd_compression_roundtrip() {
        let (path, ns) = create_test_namespace("test::compression");
        let mut namespaces = IndexMap::new();
        namespaces.insert(path, ns);

        // Serialize using ast_cache
        let namespaces_data =
            ast_cache::serialize_namespaces(&namespaces).expect("Failed to serialize namespaces");

        let cache_file = CacheFile {
            metadata: CacheMetadata {
                version: CacheType::Ast.format_version(),
                hot_version: crate::build_info::VERSION.to_string(),
                source_hash: "def456".to_string(),
            },
            namespaces_data,
        };

        // Serialize to JSON
        let data = serde_json::to_vec(&cache_file).expect("Failed to serialize");

        // Compress
        let mut encoder = zstd::Encoder::new(Vec::new(), 3).expect("Failed to create encoder");
        encoder.write_all(&data).expect("Failed to write");
        let compressed = encoder.finish().expect("Failed to finish");

        // Decompress
        let decompressed = zstd::decode_all(compressed.as_slice()).expect("Failed to decompress");

        // Deserialize
        let restored: CacheFile =
            serde_json::from_slice(&decompressed).expect("Failed to deserialize");

        assert_eq!(cache_file.metadata.version, restored.metadata.version);

        // Verify compression actually reduced size (JSON is quite verbose)
        println!(
            "Compression: {} bytes -> {} bytes ({:.1}x)",
            data.len(),
            compressed.len(),
            data.len() as f64 / compressed.len() as f64
        );
        assert!(
            compressed.len() < data.len(),
            "Compression should reduce size: {} vs {}",
            compressed.len(),
            data.len()
        );
    }

    /// Test with REAL parsed Hot code - this is the critical test
    /// that should catch issues the hand-crafted tests miss
    #[test]
    fn test_real_hot_code_roundtrip() {
        use crate::lang::parser::Parser;

        // Parse actual Hot code from the test file
        let hot_code = r#"
::test::cache ns

// Simple variable
message "hello world"

// Map with string keys
config {
    "host": "localhost",
    "port": 8080
}

// Function with template literal
greet fn (name: Str): Str {
    `Hello, ${name}!`
}

// Nested function calls and cond
process fn (x: Int): Int {
    result cond {
        ::hot::cmp/gt(x, 10) => { ::hot::math/mul(x, 2) }
        => { x }
    }
    result
}

// Type definition
MyType type {
    name: Str,
    value: Int
}

// Type implementation
MyType -> Str fn (t: MyType): Str {
    `${t.name}: ${Str(t.value)}`
}
"#;

        // Parse the code
        let mut parser = Parser::new();
        let program = parser.parse(hot_code).expect("Failed to parse Hot code");

        println!("Parsed {} namespaces", program.namespaces.len());
        for (path, ns) in &program.namespaces {
            println!("  {} - {} vars", path, ns.scope.vars.len());
        }

        // Serialize using ast_cache
        let serialized =
            ast_cache::serialize_namespaces(&program.namespaces).expect("Failed to serialize");

        println!("Serialized to {} bytes", serialized.len());

        // Deserialize
        let deserialized =
            ast_cache::deserialize_namespaces(&serialized).expect("Failed to deserialize");

        // Compare
        assert_eq!(
            program.namespaces.len(),
            deserialized.len(),
            "Namespace count should match"
        );

        for (path, original_ns) in &program.namespaces {
            let restored_ns = deserialized
                .get(path)
                .unwrap_or_else(|| panic!("Namespace {} should exist in deserialized", path));

            assert_eq!(
                original_ns.scope.vars.len(),
                restored_ns.scope.vars.len(),
                "Var count should match for namespace {}",
                path
            );

            // Deep compare each variable
            for (var, original_value) in &original_ns.scope.vars {
                let restored_value = restored_ns.scope.vars.get(var).unwrap_or_else(|| {
                    panic!("Var {} should exist in namespace {}", var.sym.name(), path)
                });

                // Compare using Debug representation for detailed diff
                let original_debug = format!("{:?}", original_value);
                let restored_debug = format!("{:?}", restored_value);

                if original_debug != restored_debug {
                    println!("\n=== MISMATCH for var '{}' ===", var.sym.name());
                    println!(
                        "Original: {}",
                        &original_debug[..original_debug.len().min(500)]
                    );
                    println!(
                        "Restored: {}",
                        &restored_debug[..restored_debug.len().min(500)]
                    );
                    panic!(
                        "Value mismatch for var '{}' in namespace '{}'",
                        var.sym.name(),
                        path
                    );
                }
            }
        }

        println!("All variables match after round-trip!");
    }

    /// Test with the actual ::hot::test module
    #[test]
    fn test_hot_test_module_roundtrip() {
        use crate::lang::parser::Parser;
        use std::path::Path;

        // Find the workspace root and build the path
        let manifest_dir = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string());
        let workspace_root = Path::new(&manifest_dir).parent().and_then(|p| p.parent());

        let test_file = if let Some(root) = workspace_root {
            root.join("hot/pkg/hot-std/src/hot/test.hot")
        } else {
            // Fallback to relative path
            Path::new("hot/pkg/hot-std/src/hot/test.hot").to_path_buf()
        };

        if !test_file.exists() {
            println!("Test file not found at {:?}, skipping", test_file);
            return;
        }

        let hot_code = std::fs::read_to_string(&test_file).expect("Failed to read test.hot");

        println!(
            "Parsing {} bytes of Hot code from {:?}",
            hot_code.len(),
            test_file
        );

        // Parse the code
        let mut parser = Parser::new();
        let program = parser.parse(&hot_code).expect("Failed to parse Hot code");

        println!("Parsed {} namespaces", program.namespaces.len());
        let mut total_vars = 0;
        for (path, ns) in &program.namespaces {
            println!("  {} - {} vars", path, ns.scope.vars.len());
            total_vars += ns.scope.vars.len();
        }
        println!("Total: {} variables", total_vars);

        // Serialize using ast_cache
        let serialized =
            ast_cache::serialize_namespaces(&program.namespaces).expect("Failed to serialize");

        println!("Serialized to {} bytes", serialized.len());

        // Deserialize
        let deserialized =
            ast_cache::deserialize_namespaces(&serialized).expect("Failed to deserialize");

        // Compare
        assert_eq!(
            program.namespaces.len(),
            deserialized.len(),
            "Namespace count should match"
        );

        let mut mismatches = Vec::new();

        for (path, original_ns) in &program.namespaces {
            let restored_ns = deserialized
                .get(path)
                .unwrap_or_else(|| panic!("Namespace {} should exist in deserialized", path));

            assert_eq!(
                original_ns.scope.vars.len(),
                restored_ns.scope.vars.len(),
                "Var count should match for namespace {}",
                path
            );

            // Deep compare each variable
            for (var, original_value) in &original_ns.scope.vars {
                let restored_value = restored_ns.scope.vars.get(var).unwrap_or_else(|| {
                    panic!("Var {} should exist in namespace {}", var.sym.name(), path)
                });

                // Compare using Debug representation
                let original_debug = format!("{:?}", original_value);
                let restored_debug = format!("{:?}", restored_value);

                if original_debug != restored_debug {
                    mismatches.push((var.sym.name().to_string(), path.to_string()));
                    println!(
                        "\n=== MISMATCH for var '{}' in '{}' ===",
                        var.sym.name(),
                        path
                    );
                    // Show first difference
                    let orig_chars: Vec<char> = original_debug.chars().collect();
                    let rest_chars: Vec<char> = restored_debug.chars().collect();
                    for (i, (o, r)) in orig_chars.iter().zip(rest_chars.iter()).enumerate() {
                        if o != r {
                            let start = i.saturating_sub(50);
                            let end = (i + 100).min(orig_chars.len()).min(rest_chars.len());
                            println!("First diff at position {}:", i);
                            println!(
                                "  Original: ...{}...",
                                orig_chars[start..end].iter().collect::<String>()
                            );
                            println!(
                                "  Restored: ...{}...",
                                rest_chars[start..end.min(rest_chars.len())]
                                    .iter()
                                    .collect::<String>()
                            );
                            break;
                        }
                    }
                    // If lengths differ
                    if orig_chars.len() != rest_chars.len() {
                        println!(
                            "Length diff: original={}, restored={}",
                            orig_chars.len(),
                            rest_chars.len()
                        );
                    }
                }
            }
        }

        if !mismatches.is_empty() {
            panic!(
                "Found {} mismatched variables: {:?}",
                mismatches.len(),
                mismatches.iter().take(5).collect::<Vec<_>>()
            );
        }

        println!("All {} variables match after round-trip!", total_vars);
    }

    #[test]
    fn package_units_use_the_shared_dir_while_sources_stay_local() {
        let dir = tempfile::tempdir().expect("tempdir");
        let project = dir.path().join("project-local");
        let shared = dir.path().join("machine-shared");
        let cache = UnitCache::with_package_dir(project.clone(), shared.clone());

        let root = dir.path().join("unit-src");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("a.hot"), "::a ns\n\nvalue 1\n").unwrap();

        let package = CompilationUnit::Package {
            name: "hot-std".to_string(),
            path: root.clone(),
        };
        let source = CompilationUnit::SourcePath {
            name: "src".to_string(),
            path: root.clone(),
        };

        let namespaces = IndexMap::new();
        cache.save(&package, &namespaces).expect("save package");
        cache.save(&source, &namespaces).expect("save source");

        let entries = |dir: &Path| -> Vec<String> {
            std::fs::read_dir(dir)
                .map(|read| {
                    read.flatten()
                        .map(|entry| entry.file_name().to_string_lossy().to_string())
                        .filter(|name| name.ends_with(".ast.zst"))
                        .collect()
                })
                .unwrap_or_default()
        };

        assert!(
            entries(&shared).iter().all(|name| name.starts_with("pkg-")),
            "shared dir holds only package units: {:?}",
            entries(&shared)
        );
        assert!(
            !entries(&shared).is_empty(),
            "package unit must land in the shared dir"
        );
        assert!(
            entries(&project)
                .iter()
                .all(|name| name.starts_with("src-")),
            "project dir holds only source units: {:?}",
            entries(&project)
        );
        assert!(
            !entries(&project).is_empty(),
            "source unit must land in the project dir"
        );
    }

    /// The shared package cache must never serve one version's AST for
    /// another. The key hashes every `.hot` file under the package root —
    /// including `pkg.hot`, which carries the declared version — so a version
    /// bump, a source edit, or a different install location all produce
    /// distinct entries.
    #[test]
    fn package_cache_key_distinguishes_version_sources_and_location() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache = UnitCache::new(dir.path().join("cache"));

        let write_pkg = |root: &Path, version: &str, body: &str| {
            std::fs::create_dir_all(root.join("src")).unwrap();
            std::fs::write(
                root.join("pkg.hot"),
                format!(
                    "::hot::pkg ns\n\nhot.pkg.demo {{\n  name: \"demo\",\n  version: \"{version}\",\n  src: [\"src/\"],\n}}\n"
                ),
            )
            .unwrap();
            std::fs::write(root.join("src").join("lib.hot"), body).unwrap();
        };
        let unit_at = |root: &Path| CompilationUnit::Package {
            name: "demo".to_string(),
            path: root.to_path_buf(),
        };

        let root = dir.path().join("demo");
        write_pkg(&root, "1.0.0", "::demo ns\n\nvalue 1\n");
        let v1 = cache.compute_cache_key(&unit_at(&root)).unwrap();

        // Same sources, bumped version in pkg.hot.
        write_pkg(&root, "2.0.0", "::demo ns\n\nvalue 1\n");
        let v2 = cache.compute_cache_key(&unit_at(&root)).unwrap();
        assert_ne!(v1, v2, "a version bump must change the cache key");

        // Same version, edited sources.
        write_pkg(&root, "2.0.0", "::demo ns\n\nvalue 2\n");
        let v2_edited = cache.compute_cache_key(&unit_at(&root)).unwrap();
        assert_ne!(v2, v2_edited, "a source edit must change the cache key");

        // Same name and content at a different install location.
        let elsewhere = dir.path().join("elsewhere").join("demo");
        write_pkg(&elsewhere, "2.0.0", "::demo ns\n\nvalue 2\n");
        let other_location = cache.compute_cache_key(&unit_at(&elsewhere)).unwrap();
        assert_ne!(
            v2_edited, other_location,
            "a different install location must not reuse another package's entry"
        );

        // Recomputing an unchanged package is stable (the cache can actually hit).
        write_pkg(&root, "2.0.0", "::demo ns\n\nvalue 2\n");
        assert_eq!(
            v2_edited,
            cache.compute_cache_key(&unit_at(&root)).unwrap(),
            "unchanged package must produce a stable key"
        );
    }

    /// A mutable unit must not accumulate generations, and the sweep must be
    /// scoped to one unit at one location — two projects with a same-named
    /// local dependency share the package cache directory.
    #[test]
    fn saving_collapses_generations_without_evicting_other_locations() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache_dir = dir.path().join("unit");
        let cache = UnitCache::new(cache_dir.clone());

        let make_lib = |root: &Path, body: &str| {
            std::fs::create_dir_all(root).unwrap();
            std::fs::write(root.join("lib.hot"), body).unwrap();
        };
        let unit_at = |root: &Path| CompilationUnit::Package {
            name: "mylib".to_string(),
            path: root.to_path_buf(),
        };
        let entries = |prefix: &str| -> usize {
            std::fs::read_dir(&cache_dir)
                .map(|read| {
                    read.flatten()
                        .filter(|entry| {
                            let name = entry.file_name().to_string_lossy().to_string();
                            name.starts_with(prefix) && name.ends_with(".ast.zst")
                        })
                        .count()
                })
                .unwrap_or(0)
        };

        // Project A edits its local dependency three times.
        let a = dir.path().join("a").join("mylib");
        let namespaces = IndexMap::new();
        for body in [
            "::mylib ns\n\nv 1\n",
            "::mylib ns\n\nv 2\n",
            "::mylib ns\n\nv 3\n",
        ] {
            make_lib(&a, body);
            cache.save(&unit_at(&a), &namespaces).expect("save");
        }
        let a_prefix = unit_at(&a).scoped_fs_id();
        assert_eq!(
            entries(&a_prefix),
            1,
            "an edited unit keeps exactly one live generation"
        );

        // Project B has a different local dependency, also called `mylib`.
        let b = dir.path().join("b").join("mylib");
        make_lib(&b, "::mylib ns\n\nother 1\n");
        cache.save(&unit_at(&b), &namespaces).expect("save");
        let b_prefix = unit_at(&b).scoped_fs_id();
        assert_ne!(
            a_prefix, b_prefix,
            "same name at different paths must differ"
        );
        assert_eq!(entries(&a_prefix), 1, "project A's entry survives");
        assert_eq!(entries(&b_prefix), 1, "project B's entry is stored");

        // Project A edits again: still must not touch project B.
        make_lib(&a, "::mylib ns\n\nv 4\n");
        cache.save(&unit_at(&a), &namespaces).expect("save");
        assert_eq!(entries(&a_prefix), 1);
        assert_eq!(entries(&b_prefix), 1, "sweep is scoped to one location");
    }

    /// Legacy (unscoped) entries are unreachable after the rename, so the
    /// first save for a unit reclaims them — without touching a scoped entry
    /// that belongs to a different location.
    #[test]
    fn saving_reclaims_legacy_unscoped_entries_only() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache_dir = dir.path().join("unit");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let cache = UnitCache::new(cache_dir.clone());

        let root = dir.path().join("mylib");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("lib.hot"), "::mylib ns\n\nv 1\n").unwrap();
        let unit = CompilationUnit::Package {
            name: "mylib".to_string(),
            path: root.clone(),
        };

        // A pre-rename entry for this unit, and a scoped entry belonging to a
        // different location that must survive.
        let legacy = cache_dir.join("pkg-mylib-0123456789abcdef.ast.zst");
        let other_location = cache_dir.join("pkg-mylib-feedface-0123456789abcdef.ast.zst");
        std::fs::write(&legacy, b"stale").unwrap();
        std::fs::write(&other_location, b"other project").unwrap();

        cache.save(&unit, &IndexMap::new()).expect("save");

        assert!(!legacy.exists(), "unreachable legacy entry is reclaimed");
        assert!(
            other_location.exists(),
            "another location's scoped entry must survive"
        );
    }

    /// Concurrent writers converge on one valid, loadable generation.
    ///
    /// Scope note: this is a smoke test, not a proof. The lost-generation race
    /// it guards against (a writer that lost `try_write` writing an entry the
    /// lock holder then sweeps) only manifests under real contention, and this
    /// harness reproduces it only intermittently — it caught the pre-fix code
    /// about one run in three, and not at all once rounds were added. The
    /// structural fix is the blocking lock in `save`; what this test reliably
    /// catches is gross breakage: an empty cache, a surviving half-written
    /// file, or unbounded generation growth under parallel saves.
    #[test]
    fn concurrent_writers_leave_exactly_one_live_generation() {
        let dir = tempfile::tempdir().expect("tempdir");
        let cache_dir = dir.path().join("unit");
        std::fs::create_dir_all(&cache_dir).unwrap();

        let root = dir.path().join("mylib");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("lib.hot"), "::mylib ns\n\nv 0\n").unwrap();
        let unit = CompilationUnit::Package {
            name: "mylib".to_string(),
            path: root.clone(),
        };

        // Each writer gets its own cache instance (its own lock fd), mirroring
        // separate processes racing on the same unit. Repeated because losing
        // the race is timing-dependent: a single round detects the unlocked
        // write-then-prune only intermittently.
        for round in 0..6 {
            std::thread::scope(|scope| {
                for generation in (round * 8)..(round * 8 + 8) {
                    let cache_dir = cache_dir.clone();
                    let unit = unit.clone();
                    scope.spawn(move || {
                        let cache = UnitCache::new(cache_dir);
                        let mut namespaces = IndexMap::new();
                        namespaces.insert(
                            NsPath::from_string(&format!("::gen{}", generation)),
                            Namespace {
                                path: NsPath::from_string(&format!("::gen{}", generation)),
                                scope: crate::lang::ast::Scope {
                                    vars: IndexMap::new(),
                                },
                                meta: None,
                                source_file: None,
                                aliases: Default::default(),
                            },
                        );
                        let _ = cache.save(&unit, &namespaces);
                    });
                }
            });

            let live: Vec<_> = std::fs::read_dir(&cache_dir)
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".ast.zst"))
                .collect();
            assert_eq!(
                live.len(),
                1,
                "round {round}: concurrent writers must converge on one generation, found {:?}",
                live.iter().map(|e| e.file_name()).collect::<Vec<_>>()
            );
        }

        let entries: Vec<_> = std::fs::read_dir(&cache_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".ast.zst"))
            .collect();
        assert_eq!(
            entries.len(),
            1,
            "concurrent writers must converge on one generation, found {:?}",
            entries.iter().map(|e| e.file_name()).collect::<Vec<_>>()
        );
        // And the survivor must be loadable, not a half-written or deleted file.
        let reader = UnitCache::new(cache_dir.clone());
        assert!(
            reader.load(&unit).ok().flatten().is_some(),
            "the surviving entry must be readable"
        );
    }
}
