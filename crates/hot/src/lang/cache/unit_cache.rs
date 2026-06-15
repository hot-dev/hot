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
//! The cache directory is determined by `cache_paths::get_unit_cache_dir()`:
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

/// Compilation unit cache manager
/// Caches parsed AST for packages and source paths
/// Supports cross-process synchronization via file locking
pub struct UnitCache {
    /// Cache directory (typically .hot/cache/unit)
    cache_dir: PathBuf,
}

impl UnitCache {
    /// Create a new unit cache manager
    pub fn new(cache_dir: PathBuf) -> Self {
        Self { cache_dir }
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
        std::fs::create_dir_all(&self.cache_dir)?;

        let lock_path = self.cache_dir.join(format!("{}.lock", unit.fs_safe_id()));
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
            unit.fs_safe_id(),
            &cache_key[..16],
            CacheType::Ast.extension()
        );
        self.cache_dir.join(filename)
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
    /// The cache key includes Hot version and format version, so if the file exists
    /// it's guaranteed to be compatible (no post-load validation needed)
    pub fn load(&self, unit: &CompilationUnit) -> Result<Option<CachedUnit>, String> {
        // Compute cache key (includes version + file hashes)
        let cache_key = self.compute_cache_key(unit)?;

        // Check if cache file exists
        let cache_path = self.cache_path(unit, &cache_key);
        if !cache_path.exists() {
            return Ok(None);
        }

        // Read and decompress
        let compressed = std::fs::read(&cache_path).map_err(|e| e.to_string())?;
        let data = zstd::decode_all(compressed.as_slice()).map_err(|e| e.to_string())?;

        // Deserialize the cache file wrapper
        let cache_file: CacheFile = serde_json::from_slice(&data)
            .map_err(|e| format!("Failed to deserialize cache file: {}", e))?;

        // Deserialize namespaces using ast_cache
        let namespaces = ast_cache::deserialize_namespaces(&cache_file.namespaces_data)?;

        Ok(Some(CachedUnit {
            version: cache_file.metadata.version,
            hot_version: cache_file.metadata.hot_version,
            source_hash: cache_file.metadata.source_hash,
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
        std::fs::create_dir_all(&self.cache_dir).map_err(|e| e.to_string())?;

        // Acquire cross-process file lock (best effort - proceed even if locking fails)
        let mut file_lock = self.acquire_file_lock(unit).ok();
        let _file_lock_guard = file_lock.as_mut().and_then(|lock| lock.try_write().ok());

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

        // Serialize to JSON
        let data = serde_json::to_vec(&cache_file)
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
}
