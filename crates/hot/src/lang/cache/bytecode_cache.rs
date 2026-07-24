// Bytecode cache system for Hot.
//
// This module provides bytecode caching for production worker loads.
// Cache is generated on first worker run and reused for subsequent executions.

use crate::hasher::{CacheKeyBuilder, CacheType, HotHasher};
use crate::lang::bytecode::BytecodeProgram;
use crate::lang::compiler::core_registry::CoreVariableRegistry;
use ahash::{AHashMap, AHashSet};
use parking_lot::Mutex;
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

/// Metadata stored with cached bytecode for validation
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheMetadata {
    /// Project name
    pub project_name: String,
    /// Hot engine version
    pub hot_version: String,
    /// Git SHA of the Hot runtime build
    #[serde(default)]
    pub git_sha: String,
    /// Cache format version
    pub cache_format_version: u32,
    /// Unix timestamp when cache was created
    pub created_at: i64,
    /// File hashes for validation
    pub file_hashes: Vec<FileHash>,
    /// Cache key used to identify this cache
    pub cache_key: String,
}

/// Hash information for a source file
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileHash {
    /// Relative path to file
    pub path: String,
    /// Blake3 hash of file contents
    pub hash: String,
}

/// Complete cached bytecode with metadata and compilation artifacts
#[derive(Debug, Clone)]
pub struct CachedBytecode {
    /// Serialized bytecode program
    pub program: BytecodeProgram,
    /// Metadata for validation
    pub metadata: CacheMetadata,
    /// Function name to ID mapping (for observability)
    pub function_mapping: indexmap::IndexMap<String, u32>,
    /// Core functions registry (for observability)
    pub core_functions: indexmap::IndexMap<String, u32>,
    /// Type implementations mapping (for type dispatch)
    pub type_implementations: indexmap::IndexMap<(String, String), String>,
    /// Parsed AST program (for metadata enrichment) - now serializable!
    pub ast_program: crate::lang::ast::Program,
    /// Pre-built HotAst with variable index (skips expensive indexing on load!)
    pub hot_ast: crate::lang::ast::HotAst,
    /// Tool/MCP schemas captured at compile time, keyed by FQ function
    /// name. Installed into the global tool-schema registry whenever
    /// this cached bytecode is loaded for execution. Persisted with
    /// the cache so packaged builds and cross-process live-build cache hits
    /// both restore the registry without recompilation.
    pub tool_specs: crate::lang::hot::internal_mcp::ToolSchemaRegistry,
    /// Skill metadata captured at compile time from `meta {skill: ...}`
    /// annotations. Installed into the global skill-spec registry
    /// whenever this cached bytecode is loaded for execution.
    pub skill_specs: crate::lang::hot::internal_skill::SkillSpecRegistry,
    /// Shared runtime program for VM creation without cloning bytecode.
    pub runtime_program: Arc<BytecodeProgram>,
    /// Shared function name to ID mapping for VM creation.
    pub runtime_function_mapping: Arc<indexmap::IndexMap<String, u32>>,
    /// Shared core functions registry for VM creation.
    pub runtime_core_functions: Arc<indexmap::IndexMap<String, u32>>,
    /// Shared type implementations mapping for VM creation.
    pub runtime_type_implementations: Arc<indexmap::IndexMap<(String, String), String>>,
    /// Shared pre-built HotAst for VM creation.
    pub runtime_hot_ast: Arc<crate::lang::ast::HotAst>,
    /// Shared core variable registry extracted once from the cached AST.
    pub runtime_core_variables: Arc<CoreVariableRegistry>,
    /// Context default values extracted once from the cached AST call graph.
    pub runtime_ctx_defaults: Arc<AHashMap<String, crate::val::Val>>,
    /// Secret context keys extracted once from the cached AST call graph.
    pub runtime_secret_keys: Arc<AHashSet<String>>,
}

pub struct CachedBytecodeArtifacts {
    pub program: BytecodeProgram,
    pub function_mapping: indexmap::IndexMap<String, u32>,
    pub core_functions: indexmap::IndexMap<String, u32>,
    pub type_implementations: indexmap::IndexMap<(String, String), String>,
    pub ast_program: crate::lang::ast::Program,
    pub hot_ast: crate::lang::ast::HotAst,
    pub tool_specs: crate::lang::hot::internal_mcp::ToolSchemaRegistry,
    pub skill_specs: crate::lang::hot::internal_skill::SkillSpecRegistry,
}

impl CachedBytecode {
    /// Build cached bytecode plus runtime-ready shared artifacts.
    pub fn new(metadata: CacheMetadata, artifacts: CachedBytecodeArtifacts) -> Self {
        let CachedBytecodeArtifacts {
            program,
            function_mapping,
            core_functions,
            type_implementations,
            ast_program,
            hot_ast,
            tool_specs,
            skill_specs,
        } = artifacts;

        let runtime_core_variables = Arc::new(
            crate::lang::compiler::core_registry::extract_core_variables_from_ast(&ast_program),
        );
        let ctx_requirements =
            crate::lang::compiler::ctx_checker::extract_ctx_requirements_via_call_graph(
                &ast_program,
            );
        let runtime_ctx_defaults = Arc::new(ctx_requirements.all_defaults());
        let runtime_secret_keys = Arc::new(ctx_requirements.all_secret_keys());

        Self {
            runtime_program: Arc::new(program.clone()),
            runtime_function_mapping: Arc::new(function_mapping.clone()),
            runtime_core_functions: Arc::new(core_functions.clone()),
            runtime_type_implementations: Arc::new(type_implementations.clone()),
            runtime_hot_ast: Arc::new(hot_ast.clone()),
            runtime_core_variables,
            runtime_ctx_defaults,
            runtime_secret_keys,
            program,
            metadata,
            function_mapping,
            core_functions,
            type_implementations,
            ast_program,
            hot_ast,
            tool_specs,
            skill_specs,
        }
    }
}

/// On-disk cache payload (postcard inside zstd).
///
/// Sections that carry their own encoding stay as byte blobs
/// (`ast_cache`-encoded namespaces), everything else is typed and encoded
/// directly by the payload's own postcard pass. Same wire-format rules as
/// the AST cache apply (see the module docs in ast_cache.rs): no variant
/// reordering, no field changes without bumping
/// `CacheType::Bytecode.format_version()`.
#[derive(Serialize, Deserialize)]
struct CacheFilePayload {
    metadata: CacheMetadata,
    /// BytecodeProgram via `BytecodeProgram::serialize` (postcard)
    program_data: Vec<u8>,
    /// AST program namespaces via `ast_cache::serialize_namespaces`
    ast_program_namespaces: Vec<u8>,
    /// HotAst namespaces via `ast_cache::serialize_namespaces`
    hot_ast_namespaces: Vec<u8>,
    /// AstVarIndex via `ast_cache::serialize_var_index`
    var_index: Vec<u8>,
    function_mapping: Vec<(String, u32)>,
    core_functions: Vec<(String, u32)>,
    type_implementations: Vec<((String, String), String)>,
    tool_specs: crate::lang::hot::internal_mcp::ToolSchemaRegistry,
    skill_specs: crate::lang::hot::internal_skill::SkillSpecRegistry,
}

/// In-memory cache entry with access tracking for LRU
/// Uses Arc to avoid cloning the entire bytecode structure on each access
struct MemoryCacheEntry {
    bytecode: Arc<CachedBytecode>,
    last_accessed: std::time::Instant,
}

/// Type alias for the global memory cache
type MemoryCache = Arc<Mutex<AHashMap<String, MemoryCacheEntry>>>;

/// Type alias for compilation locks
type CompilationLocks = Arc<Mutex<AHashMap<String, Arc<Mutex<()>>>>>;

/// Global in-memory bytecode cache shared across all BytecodeCache instances.
/// This ensures that loaded bytecode is reused even when new BytecodeCache
/// instances are created (e.g., for bundle builds with different cache directories).
static GLOBAL_MEMORY_CACHE: std::sync::LazyLock<MemoryCache> =
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(AHashMap::new())));

/// Global compilation locks shared across all BytecodeCache instances.
static GLOBAL_COMPILATION_LOCKS: std::sync::LazyLock<CompilationLocks> =
    std::sync::LazyLock::new(|| Arc::new(Mutex::new(AHashMap::new())));

/// Maximum number of builds to keep in the global memory cache
const MAX_GLOBAL_MEMORY_ENTRIES: usize = 20;

/// Bytecode cache manager with in-memory LRU cache
/// Supports cross-process synchronization via file locking
pub struct BytecodeCache {
    /// Cache directory (project-local or system cache, see cache_paths module)
    cache_dir: PathBuf,
}

impl BytecodeCache {
    /// Create a new cache manager with the specified directory.
    /// The in-memory cache is shared globally across all instances.
    pub fn new(cache_dir: PathBuf) -> Self {
        Self { cache_dir }
    }

    /// Get or create an in-process compilation lock for a cache key.
    /// This prevents multiple threads from compiling the same project simultaneously.
    pub fn get_compilation_lock(&self, cache_key: &str) -> Arc<Mutex<()>> {
        let mut locks = GLOBAL_COMPILATION_LOCKS.lock();
        locks
            .entry(cache_key.to_string())
            .or_insert_with(|| Arc::new(Mutex::new(())))
            .clone()
    }

    /// Acquire a cross-process file lock for cache operations.
    /// Returns the lock file handle (releases lock when dropped).
    pub fn acquire_file_lock(
        &self,
        cache_key: &str,
    ) -> Result<fd_lock::RwLock<std::fs::File>, std::io::Error> {
        // Ensure cache directory exists
        std::fs::create_dir_all(&self.cache_dir)?;

        let lock_path = self.cache_dir.join(format!("{}.lock", &cache_key[..12]));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;

        Ok(fd_lock::RwLock::new(file))
    }

    /// Create cache manager with default directory.
    ///
    /// Uses smart resolution via `cache_paths::get_bytecode_cache_dir()`:
    /// - `$HOT_HOME/cache` if HOT_HOME is set
    /// - `./.hot/cache` if `hot.hot` config exists (project-local)
    /// - Platform-specific system cache otherwise (e.g., `~/.cache/hot/cache`)
    pub fn default_location() -> Self {
        Self::new(super::paths::get_bytecode_cache_dir())
    }

    /// Create cache manager with custom memory limit.
    /// Note: Memory limit is now global (MAX_GLOBAL_MEMORY_ENTRIES) and this
    /// parameter is ignored. Use this constructor for API compatibility.
    #[allow(unused_variables)]
    pub fn with_memory_limit(cache_dir: PathBuf, max_entries: usize) -> Self {
        Self { cache_dir }
    }

    /// Get the cache directory path
    pub fn cache_dir(&self) -> &std::path::Path {
        &self.cache_dir
    }

    /// Ensure cache directory exists
    fn ensure_cache_dir(&self) -> Result<(), String> {
        if !self.cache_dir.exists() {
            std::fs::create_dir_all(&self.cache_dir)
                .map_err(|e| format!("Failed to create cache directory: {}", e))?;
        }
        Ok(())
    }

    /// Calculate cache key from project information
    /// Uses unified CacheKeyBuilder which includes Hot version and format version
    pub fn calculate_cache_key(
        project_name: &str,
        file_hashes: &[FileHash],
    ) -> Result<String, String> {
        // Convert FileHash to (String, String) for the builder
        let hashes: Vec<(String, String)> = file_hashes
            .iter()
            .map(|fh| (fh.path.clone(), fh.hash.clone()))
            .collect();

        Ok(CacheKeyBuilder::new(CacheType::Bytecode)
            .with_prefix(project_name)
            .with_file_hashes(&hashes)
            .finalize())
    }

    /// Get cache file path for a given cache key
    fn cache_file_path(&self, cache_key: &str) -> PathBuf {
        self.cache_dir
            .join(format!("{}.{}", cache_key, CacheType::Bytecode.extension()))
    }

    /// Check if cache exists and is valid
    pub fn exists(&self, cache_key: &str) -> bool {
        let cache_path = self.cache_file_path(cache_key);
        cache_path.exists()
    }

    /// Load cached bytecode (checks global memory first, then disk)
    /// Returns Arc<CachedBytecode> to avoid expensive clones on each access
    pub fn load(&self, cache_key: &str) -> Result<Arc<CachedBytecode>, String> {
        // Check global in-memory cache first
        {
            let mut cache = GLOBAL_MEMORY_CACHE.lock();
            if let Some(entry) = cache.get_mut(cache_key) {
                // Update last accessed time
                entry.last_accessed = std::time::Instant::now();
                tracing::debug!(
                    "✓ Cache loaded from MEMORY (project: {}, {} functions, {} core, {} namespaces)",
                    entry.bytecode.metadata.project_name,
                    entry.bytecode.function_mapping.len(),
                    entry.bytecode.core_functions.len(),
                    entry.bytecode.ast_program.namespaces.len()
                );
                return Ok(Arc::clone(&entry.bytecode));
            }
        }

        // Not in memory, load from disk
        tracing::debug!(
            "Cache miss in MEMORY, loading from disk for key: {}",
            &cache_key[..12]
        );
        let cache_path = self.cache_file_path(cache_key);

        if !cache_path.exists() {
            return Err(format!("Cache file not found: {}", cache_path.display()));
        }

        tracing::debug!("Loading bytecode cache from: {}", cache_path.display());

        // Read compressed cache file
        let compressed_data =
            std::fs::read(&cache_path).map_err(|e| format!("Failed to read cache file: {}", e))?;

        // Decompress with zstd
        let decompressed = zstd::decode_all(compressed_data.as_slice())
            .map_err(|e| format!("Failed to decompress cache: {}", e))?;

        // Decode the postcard payload
        let payload: CacheFilePayload = postcard::from_bytes(&decompressed)
            .map_err(|e| format!("Failed to decode cache payload: {}", e))?;

        // Validate cache metadata
        self.validate_metadata(&payload.metadata)?;

        let CacheFilePayload {
            metadata,
            program_data,
            ast_program_namespaces,
            hot_ast_namespaces,
            var_index,
            function_mapping,
            core_functions,
            type_implementations,
            tool_specs,
            skill_specs,
        } = payload;

        let program = BytecodeProgram::deserialize(&program_data)?;

        let ast_program_namespaces =
            crate::lang::cache::ast_cache::deserialize_namespaces(&ast_program_namespaces)
                .map_err(|e| format!("Failed to deserialize AST program namespaces: {}", e))?;

        let ast_program = crate::lang::ast::Program {
            namespaces: ast_program_namespaces,
            current_namespace: crate::lang::ast::NsPath::new(),
        };

        let hot_ast_namespaces =
            crate::lang::cache::ast_cache::deserialize_namespaces(&hot_ast_namespaces)
                .map_err(|e| format!("Failed to deserialize HotAst namespaces: {}", e))?;

        // Reconstruct HotAst
        let var_index = crate::lang::cache::ast_cache::deserialize_var_index(&var_index)?;
        let hot_ast = crate::lang::ast::HotAst {
            program: ast_program.clone(),
            namespaces: crate::lang::ast::Namespaces {
                namespaces: hot_ast_namespaces,
            },
            var_index,
        };

        let function_mapping: indexmap::IndexMap<String, u32> =
            function_mapping.into_iter().collect();
        let core_functions: indexmap::IndexMap<String, u32> = core_functions.into_iter().collect();
        let type_implementations: indexmap::IndexMap<(String, String), String> =
            type_implementations.into_iter().collect();

        tracing::debug!(
            "✓ Cache loaded from disk (project: {}, created: {}, {} functions, {} core, {} namespaces)",
            metadata.project_name,
            metadata.created_at,
            function_mapping.len(),
            core_functions.len(),
            ast_program.namespaces.len()
        );

        let cached_bytecode = Arc::new(CachedBytecode::new(
            metadata,
            CachedBytecodeArtifacts {
                program,
                function_mapping,
                core_functions,
                type_implementations,
                ast_program,
                hot_ast,
                tool_specs,
                skill_specs,
            },
        ));

        // Store in memory cache for next time
        self.store_in_memory(cache_key, Arc::clone(&cached_bytecode));

        Ok(cached_bytecode)
    }

    /// Save compiled bytecode to cache with compilation artifacts, AST, and HotAst
    #[allow(clippy::too_many_arguments)]
    pub fn save(
        &self,
        cache_key: &str,
        program: &BytecodeProgram,
        metadata: CacheMetadata,
        function_mapping: &indexmap::IndexMap<String, u32>,
        core_functions: &indexmap::IndexMap<String, u32>,
        type_implementations: &indexmap::IndexMap<(String, String), String>,
        ast_program: &crate::lang::ast::Program,
        hot_ast: &crate::lang::ast::HotAst,
        tool_specs: &crate::lang::hot::internal_mcp::ToolSchemaRegistry,
        skill_specs: &crate::lang::hot::internal_skill::SkillSpecRegistry,
    ) -> Result<(), String> {
        self.ensure_cache_dir()?;

        let cache_path = self.cache_file_path(cache_key);

        tracing::debug!("Saving bytecode cache to: {}", cache_path.display());

        // Assemble the typed payload. The tool/skill spec registries are
        // persisted so they survive cross-process cache reuse and zip-build
        // deployment: a worker that only loads bytecode (no recompile) would
        // otherwise have an empty global tool-schema registry and
        // `::ai::tool/from-fn` would fail at module-init time.
        let payload = CacheFilePayload {
            metadata: metadata.clone(),
            program_data: program.serialize()?,
            ast_program_namespaces: crate::lang::cache::ast_cache::serialize_namespaces(
                &ast_program.namespaces,
            )?,
            hot_ast_namespaces: crate::lang::cache::ast_cache::serialize_namespaces(
                &hot_ast.namespaces.namespaces,
            )?,
            var_index: crate::lang::cache::ast_cache::serialize_var_index(&hot_ast.var_index)?,
            function_mapping: function_mapping
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            core_functions: core_functions
                .iter()
                .map(|(k, v)| (k.clone(), *v))
                .collect(),
            type_implementations: type_implementations
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            tool_specs: tool_specs.clone(),
            skill_specs: skill_specs.clone(),
        };

        let encoded = postcard::to_allocvec(&payload)
            .map_err(|e| format!("Failed to encode cache payload: {}", e))?;

        // Compress with zstd (level 1: the payload is already compact binary,
        // favor speed)
        let compressed = zstd::encode_all(encoded.as_slice(), 1)
            .map_err(|e| format!("Failed to compress cache: {}", e))?;

        let compressed_size = compressed.len();
        let encoded_size = encoded.len();

        // Write to disk using atomic write pattern (write to .tmp, then rename).
        // Use a per-writer-unique temp path (PID + thread id + nanos) to
        // avoid concurrent writers racing on the same `.bytecode.tmp`
        // path — that race manifests as
        // `Failed to rename cache file: No such file or directory` when
        // one writer's rename consumes the temp file before the other
        // writer's rename runs.
        let unique_suffix = format!(
            "{}.{}.{}",
            std::process::id(),
            // Thread id is opaque so use a hash; format!("{:?}", ...) is
            // stable enough for uniqueness within a single process.
            {
                use std::collections::hash_map::DefaultHasher;
                use std::hash::{Hash, Hasher};
                let mut h = DefaultHasher::new();
                format!("{:?}", std::thread::current().id()).hash(&mut h);
                h.finish()
            },
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        );
        let temp_path = cache_path.with_extension(format!("bytecode.tmp.{}", unique_suffix));
        std::fs::write(&temp_path, &compressed)
            .map_err(|e| format!("Failed to write temp cache file: {}", e))?;

        // Atomic rename (on Unix, rename is atomic; on Windows, it's best-effort).
        // Rename overwrites the destination; concurrent writers will each
        // overwrite the final cache file, which is fine since they're
        // writing the same content (deterministic compilation).
        if let Err(e) = std::fs::rename(&temp_path, &cache_path) {
            // Best-effort cleanup of our temp file on failure
            let _ = std::fs::remove_file(&temp_path);
            return Err(format!("Failed to rename cache file: {}", e));
        }

        tracing::debug!(
            "Cache saved successfully ({} bytes compressed from {} bytes encoded)",
            compressed_size,
            encoded_size
        );

        // Also store in memory cache
        let cached_bytecode = Arc::new(CachedBytecode::new(
            metadata,
            CachedBytecodeArtifacts {
                program: program.clone(),
                function_mapping: function_mapping.clone(),
                core_functions: core_functions.clone(),
                type_implementations: type_implementations.clone(),
                ast_program: ast_program.clone(),
                hot_ast: hot_ast.clone(),
                tool_specs: tool_specs.clone(),
                skill_specs: skill_specs.clone(),
            },
        ));
        self.store_in_memory(cache_key, cached_bytecode);

        Ok(())
    }

    /// Store bytecode in memory cache with LRU eviction
    fn store_in_memory(&self, cache_key: &str, bytecode: Arc<CachedBytecode>) {
        let mut cache = GLOBAL_MEMORY_CACHE.lock();
        // If at capacity, evict the least recently used entry
        if cache.len() >= MAX_GLOBAL_MEMORY_ENTRIES
            && !cache.contains_key(cache_key)
            && let Some((lru_key, _)) = cache
                .iter()
                .min_by_key(|(_, entry)| entry.last_accessed)
                .map(|(k, v)| (k.clone(), v.last_accessed))
        {
            cache.remove(&lru_key);
            tracing::debug!(
                "Evicted LRU entry from global memory cache: {}",
                &lru_key[..12]
            );
        }

        cache.insert(
            cache_key.to_string(),
            MemoryCacheEntry {
                bytecode,
                last_accessed: std::time::Instant::now(),
            },
        );
        tracing::debug!(
            "✓ Stored in GLOBAL memory cache ({}/{} entries): {}",
            cache.len(),
            MAX_GLOBAL_MEMORY_ENTRIES,
            &cache_key[..12]
        );
    }

    /// Clear the global in-memory cache (useful for testing or memory management)
    pub fn clear_memory(&self) {
        let mut cache = GLOBAL_MEMORY_CACHE.lock();
        let count = cache.len();
        cache.clear();
        tracing::debug!("Cleared {} entries from global memory cache", count);
    }

    /// Get memory cache statistics
    pub fn memory_stats(&self) -> (usize, usize) {
        let cache = GLOBAL_MEMORY_CACHE.lock();
        (cache.len(), MAX_GLOBAL_MEMORY_ENTRIES)
    }

    /// Validate cache metadata
    fn validate_metadata(&self, metadata: &CacheMetadata) -> Result<(), String> {
        // Check hot version (use build_info::VERSION which comes from resources/version.txt)
        if metadata.hot_version != crate::build_info::VERSION {
            return Err(format!(
                "Cache version mismatch: cache={}, current={}",
                metadata.hot_version,
                crate::build_info::VERSION
            ));
        }

        // Check git SHA (catches deploys where version wasn't bumped but code changed)
        // Only check if the cached metadata has a git_sha (backward compatibility)
        if !metadata.git_sha.is_empty() && metadata.git_sha != crate::build_info::GIT_SHA {
            return Err(format!(
                "Cache git SHA mismatch: cache={}, current={}",
                &metadata.git_sha[..7.min(metadata.git_sha.len())],
                &crate::build_info::GIT_SHA[..7.min(crate::build_info::GIT_SHA.len())]
            ));
        }

        // Check cache format version
        if metadata.cache_format_version != CacheType::Bytecode.format_version() {
            return Err(format!(
                "Cache format version mismatch: cache={}, current={}",
                metadata.cache_format_version,
                CacheType::Bytecode.format_version()
            ));
        }

        Ok(())
    }

    /// Create file hashes for a list of file paths
    pub fn hash_files(file_paths: &[String]) -> Result<Vec<FileHash>, String> {
        let mut file_hashes = Vec::new();

        for file_path in file_paths {
            let content = std::fs::read(file_path)
                .map_err(|e| format!("Failed to read file {}: {}", file_path, e))?;

            let mut hasher = HotHasher::new();
            hasher.update(&content);
            let hash = hasher.finalize();

            file_hashes.push(FileHash {
                path: file_path.clone(),
                hash,
            });
        }

        Ok(file_hashes)
    }

    /// Validate that current files match cached file hashes
    /// Uses parallel hashing for better performance with many files
    pub fn validate_file_hashes(
        current_files: &[String],
        cached_hashes: &[FileHash],
    ) -> Result<bool, String> {
        // Early exit: Check file count first (very cheap)
        if cached_hashes.len() != current_files.len() {
            tracing::debug!(
                "Cache invalid: file count mismatch (cached: {}, current: {})",
                cached_hashes.len(),
                current_files.len()
            );
            return Ok(false);
        }

        // Create a map of cached hashes for quick lookup
        let cached_map: AHashMap<&str, &str> = cached_hashes
            .iter()
            .map(|fh| (fh.path.as_str(), fh.hash.as_str()))
            .collect();

        // Check for new files not in cache (cheap string comparison before hashing)
        for file_path in current_files {
            if !cached_map.contains_key(file_path.as_str()) {
                tracing::debug!("Cache invalid: new file not in cache: {}", file_path);
                return Ok(false);
            }
        }

        // Parallel hash validation - hash all files concurrently
        // Use par_iter for parallel processing with early termination on first mismatch
        use std::sync::atomic::{AtomicBool, Ordering};
        let cache_valid = AtomicBool::new(true);

        current_files.par_iter().for_each(|file_path| {
            // Skip if we already found a mismatch
            if !cache_valid.load(Ordering::Relaxed) {
                return;
            }

            if let Ok(content) = std::fs::read(file_path) {
                let mut hasher = HotHasher::new();
                hasher.update(&content);
                let current_hash = hasher.finalize();

                if let Some(&cached_hash) = cached_map.get(file_path.as_str())
                    && cached_hash != current_hash
                {
                    tracing::debug!("Cache invalid: file changed: {}", file_path);
                    cache_valid.store(false, Ordering::Relaxed);
                }
            } else {
                tracing::debug!("Cache invalid: failed to read file: {}", file_path);
                cache_valid.store(false, Ordering::Relaxed);
            }
        });

        Ok(cache_valid.load(Ordering::Relaxed))
    }

    /// Delete a specific cache file
    pub fn delete(&self, cache_key: &str) -> Result<(), String> {
        let cache_path = self.cache_file_path(cache_key);

        if cache_path.exists() {
            std::fs::remove_file(&cache_path)
                .map_err(|e| format!("Failed to delete cache file: {}", e))?;
            tracing::debug!("Deleted cache file: {}", cache_path.display());
        }

        Ok(())
    }

    /// Clean all cache files in the cache directory
    pub fn clean_all(&self) -> Result<usize, String> {
        // Also clear in-memory cache
        self.clear_memory_cache();

        if !self.cache_dir.exists() {
            return Ok(0);
        }

        let mut count = 0;
        let entries = std::fs::read_dir(&self.cache_dir)
            .map_err(|e| format!("Failed to read cache directory: {}", e))?;

        for entry in entries {
            let entry = entry.map_err(|e| format!("Failed to read directory entry: {}", e))?;
            let path = entry.path();

            if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("cache") {
                std::fs::remove_file(&path)
                    .map_err(|e| format!("Failed to delete {}: {}", path.display(), e))?;
                count += 1;
            }
        }

        tracing::debug!("Cleaned {} cache file(s)", count);
        Ok(count)
    }

    /// Clear the global in-memory cache only (useful when builds change)
    pub fn clear_memory_cache(&self) {
        let mut cache = GLOBAL_MEMORY_CACHE.lock();
        let count = cache.len();
        cache.clear();
        tracing::debug!(
            "Cleared {} entries from global bytecode memory cache",
            count
        );
    }

    /// Invalidate cache entries for a specific project (both memory and disk)
    /// This should be called when a project's source files change
    pub fn invalidate_project(&self, project_name: &str) {
        // Clear matching entries from global memory cache
        {
            let mut cache = GLOBAL_MEMORY_CACHE.lock();
            let keys_to_remove: Vec<String> = cache
                .keys()
                .filter(|k| k.starts_with(&format!("bytecode-{}-", project_name)))
                .cloned()
                .collect();

            for key in &keys_to_remove {
                cache.remove(key);
            }

            if !keys_to_remove.is_empty() {
                tracing::debug!(
                    "Invalidated {} memory cache entries for project '{}'",
                    keys_to_remove.len(),
                    project_name
                );
            }
        }

        // Note: Disk cache entries will naturally be invalidated when file hashes change
        // because the cache key includes file hashes
    }
}

/// Helper function to create metadata for a new cache entry
pub fn create_cache_metadata(
    project_name: &str,
    file_hashes: Vec<FileHash>,
    cache_key: String,
) -> CacheMetadata {
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;

    CacheMetadata {
        project_name: project_name.to_string(),
        hot_version: crate::build_info::VERSION.to_string(),
        git_sha: crate::build_info::GIT_SHA.to_string(),
        cache_format_version: CacheType::Bytecode.format_version(),
        created_at,
        file_hashes,
        cache_key,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::bytecode::{
        BytecodeProgram, Constant, FlowResultModifier, FlowType, FunctionInfo, Instruction,
        SourceLocation, VariableMetadata,
    };
    use crate::val::Val;

    #[test]
    fn test_cache_key_calculation() {
        let file_hashes = vec![
            FileHash {
                path: "test.hot".to_string(),
                hash: "abc123".to_string(),
            },
            FileHash {
                path: "test2.hot".to_string(),
                hash: "def456".to_string(),
            },
        ];

        let key1 = BytecodeCache::calculate_cache_key("test_project", &file_hashes).unwrap();
        let key2 = BytecodeCache::calculate_cache_key("test_project", &file_hashes).unwrap();

        // Same inputs should produce same key
        assert_eq!(key1, key2);

        // Different project should produce different key
        let key3 = BytecodeCache::calculate_cache_key("other_project", &file_hashes).unwrap();
        assert_ne!(key1, key3);
    }

    #[test]
    fn test_cache_metadata_creation() {
        let file_hashes = vec![FileHash {
            path: "test.hot".to_string(),
            hash: "abc123".to_string(),
        }];

        let metadata = create_cache_metadata(
            "test_project",
            file_hashes.clone(),
            "test_cache_key".to_string(),
        );

        assert_eq!(metadata.project_name, "test_project");
        assert_eq!(metadata.hot_version, crate::build_info::VERSION);
        assert_eq!(
            metadata.cache_format_version,
            CacheType::Bytecode.format_version()
        );
        assert_eq!(metadata.cache_key, "test_cache_key");
        assert!(metadata.created_at > 0);
    }

    /// Create a test bytecode program with various instruction types
    fn create_test_bytecode_program() -> BytecodeProgram {
        let mut program = BytecodeProgram::new();

        // Add constants including the new StringRef type
        program.add_constant(Constant::Val(Val::Int(42)));
        program.add_constant(Constant::Val(Val::from("hello")));
        program.add_constant(Constant::FunctionRef("::test::module/my-func".into()));
        program.add_constant(Constant::TypeRef("::test::types/MyType".into()));
        program.add_constant(Constant::StringRef("branch_name_1".into()));
        program.add_constant(Constant::StringRef("::hot::core/do".into()));

        // Add a function with instructions covering all refactored variants
        let function = FunctionInfo {
            name: "test-function".to_string(),
            namespace: "::test::module".to_string(),
            arity: 2,
            is_variadic: false,
            param_names: vec!["a".to_string(), "b".to_string()],
            param_types: vec![0, 1],
            return_type: 2,
            lazy_params: vec![false, true],
            flow_type: None,
            instructions: vec![
                // Test LoadConst
                Instruction::LoadConst {
                    dest: 0,
                    constant: 0,
                },
                // Test LoadFunctionRef with ConstantId (refactored)
                Instruction::LoadFunctionRef {
                    dest: 1,
                    function_name: 2, // ConstantId pointing to FunctionRef
                },
                // Test CondBranchStart with ConstantId (refactored)
                Instruction::CondBranchStart {
                    branch_name: 4, // ConstantId pointing to StringRef
                    condition: 0,
                    skip_target: 5,
                },
                // Test CondBranchEnd with ConstantId (refactored)
                Instruction::CondBranchEnd {
                    branch_name: 4,
                    result: 1,
                },
                // Test RegisterLocalType with ConstantId (refactored)
                Instruction::RegisterLocalType {
                    type_name: 3,
                    constructor_function_name: 5,
                },
                // Test RegisterLocalImplementation with ConstantId (refactored)
                Instruction::RegisterLocalImplementation {
                    source_type: 3,
                    target_type: 3,
                    implementation_function_name: 5,
                },
                // Test StoreVar with boxed metadata
                Instruction::StoreVar {
                    var_name: 1,
                    value: 0,
                    metadata: Some(Box::new(VariableMetadata {
                        name: "test_var".to_string(),
                        namespace: "::test::module".to_string(),
                        static_scope: Some("::test::module".to_string()),
                        meta: Some(Val::map_empty()),
                        source: Some(SourceLocation {
                            file: Some("test.hot".to_string()),
                            line: 10,
                            column: 5,
                            position: 100,
                            length: 8,
                        }),
                    })),
                },
                // Test BeginFlow with SourceLocation
                Instruction::BeginFlow {
                    flow_type: FlowType::Serial,
                    result_modifier: FlowResultModifier::One,
                    source: Some(Box::new(SourceLocation {
                        file: Some("test.hot".to_string()),
                        line: 1,
                        column: 1,
                        position: 0,
                        length: 50,
                    })),
                    origin_name: None,
                },
                Instruction::EndFlow { dest: 2 },
                Instruction::Return { value: 2 },
            ],
            register_count: 10,
            source: Some(SourceLocation {
                file: Some("test.hot".to_string()),
                line: 5,
                column: 1,
                position: 50,
                length: 100,
            }),
        };

        program.add_function(function);
        program.entry_register_count = 10;

        program
    }

    #[test]
    fn test_bytecode_program_serialization_roundtrip() {
        let program = create_test_bytecode_program();

        // Serialize to JSON
        let json = program
            .serialize()
            .expect("Failed to serialize BytecodeProgram");

        // Deserialize from JSON
        let restored =
            BytecodeProgram::deserialize(&json).expect("Failed to deserialize BytecodeProgram");

        // Verify constants
        assert_eq!(program.constants.len(), restored.constants.len());
        for (i, (original, restored)) in program
            .constants
            .iter()
            .zip(restored.constants.iter())
            .enumerate()
        {
            assert_eq!(
                original, restored,
                "Constant mismatch at index {}: {:?} != {:?}",
                i, original, restored
            );
        }

        // Verify functions
        assert_eq!(program.functions.len(), restored.functions.len());
        for (i, (orig_fn, restored_fn)) in program
            .functions
            .iter()
            .zip(restored.functions.iter())
            .enumerate()
        {
            assert_eq!(
                orig_fn.name, restored_fn.name,
                "Function name mismatch at {}",
                i
            );
            assert_eq!(
                orig_fn.instructions.len(),
                restored_fn.instructions.len(),
                "Instruction count mismatch for function {}",
                orig_fn.name
            );
        }

        // Verify entry_register_count
        assert_eq!(program.entry_register_count, restored.entry_register_count);
    }

    #[test]
    fn test_instruction_serialization_all_variants() {
        // Test that all instruction variants with ConstantId serialize correctly
        let instructions = vec![
            Instruction::LoadFunctionRef {
                dest: 0,
                function_name: 42,
            },
            Instruction::CondBranchStart {
                branch_name: 100,
                condition: 1,
                skip_target: 10,
            },
            Instruction::CondBranchEnd {
                branch_name: 100,
                result: 2,
            },
            Instruction::RegisterLocalType {
                type_name: 50,
                constructor_function_name: 51,
            },
            Instruction::RegisterLocalImplementation {
                source_type: 60,
                target_type: 61,
                implementation_function_name: 62,
            },
            Instruction::StoreVar {
                var_name: 10,
                value: 5,
                metadata: None,
            },
            Instruction::StoreVar {
                var_name: 10,
                value: 5,
                metadata: Some(Box::new(VariableMetadata {
                    name: "x".to_string(),
                    namespace: "::ns".to_string(),
                    static_scope: None,
                    meta: None,
                    source: None,
                })),
            },
        ];

        for instruction in &instructions {
            let json = serde_json::to_string(instruction)
                .unwrap_or_else(|e| panic!("Failed to serialize {:?}: {}", instruction, e));

            let restored: Instruction = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("Failed to deserialize {:?}: {}", json, e));

            // Re-serialize to compare (handles enum variant differences)
            let json2 = serde_json::to_string(&restored).unwrap();
            assert_eq!(json, json2, "Round-trip mismatch for instruction");
        }
    }

    #[test]
    fn test_constant_serialization_all_variants() {
        let constants = vec![
            Constant::Val(Val::Int(42)),
            Constant::Val(Val::from("hello")),
            Constant::Val(Val::Bool(true)),
            Constant::Val(Val::Null),
            // Note: Dec values may serialize/deserialize differently, skip in this test
            Constant::FunctionRef("::test/func".into()),
            Constant::TypeRef("::test/Type".into()),
            Constant::StringRef("branch_name".into()),
        ];

        for constant in &constants {
            let json = serde_json::to_string(constant)
                .unwrap_or_else(|e| panic!("Failed to serialize {:?}: {}", constant, e));

            let restored: Constant = serde_json::from_str(&json)
                .unwrap_or_else(|e| panic!("Failed to deserialize {:?}: {}", json, e));

            assert_eq!(
                constant, &restored,
                "Round-trip mismatch for constant: {:?}",
                constant
            );
        }
    }

    /// Schema-drift canary for the postcard bytecode encoding (see the
    /// equivalent test in ast_cache.rs for rationale). If this fails, the
    /// wire layout of BytecodeProgram/Constant/Instruction changed: bump
    /// `CacheType::Bytecode.format_version()` in hasher.rs and update the
    /// expected hash.
    #[test]
    fn postcard_bytecode_schema_drift_canary() {
        let mut program = create_test_bytecode_program();
        let mut map = indexmap::IndexMap::new();
        map.insert(crate::val::Val::Int(1), crate::val::Val::from("one"));
        program.add_constant(Constant::Val(crate::val::Val::Map(Box::new(map))));
        program.add_constant(Constant::Val(crate::val::Val::Bytes(vec![1, 2, 3])));
        program.add_constant(Constant::FunctionRef("::canary/f".into()));
        program.add_constant(Constant::StringRef("branch".into()));

        let bytes = program.serialize().expect("program must serialize");
        let hash = crate::hasher::HotHasher::hash_content(&bytes);

        let expected = "327a19cfc4a0ced16098b069a09e7d0f932d1e7357ab681d094d3eaabc57ff5e";
        assert_eq!(
            hash, expected,
            "postcard bytecode wire layout changed (got hash {hash}). If \
             intentional, bump CacheType::Bytecode format_version in \
             hasher.rs and update this test's expected hash."
        );
    }

    /// Bench (run explicitly): compile the installed hot-std sources to a
    /// bytecode cache and time cold disk loads of the resulting entry.
    ///
    ///   cargo test --release -p hot --lib bench_bytecode_cache_load -- --ignored --nocapture
    #[test]
    #[ignore]
    fn bench_bytecode_cache_load() {
        let std_src = "/usr/local/share/hot/pkg/hot-std/src";
        if !std::path::Path::new(std_src).exists() {
            eprintln!("hot-std not installed, skipping bench");
            return;
        }

        let temp_dir = std::env::temp_dir().join(format!("hot_cache_bench_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir);
        std::fs::create_dir_all(&temp_dir).expect("create temp cache dir");
        let cache = BytecodeCache::new(temp_dir.clone());

        let (_, artifacts) = crate::lang::engine::Engine::compile_project_for_cache(
            &[std_src.to_string()],
            None,
            Some("bench"),
            None,
            None,
            None,
            None,
            None,
            None,
        )
        .expect("hot-std must compile");

        let cache_key = "bench_key_0000000000000000";
        let metadata = create_cache_metadata("bench", vec![], cache_key.to_string());

        let t0 = std::time::Instant::now();
        cache
            .save(
                cache_key,
                &artifacts.program,
                metadata,
                &artifacts.function_mapping,
                &artifacts.core_functions,
                &artifacts.type_implementations,
                &artifacts.ast_program,
                &artifacts.hot_ast,
                &Default::default(),
                &Default::default(),
            )
            .expect("save must succeed");
        let save_ms = t0.elapsed().as_secs_f64() * 1000.0;

        let disk_size = std::fs::metadata(temp_dir.join(format!("{}.bc.zst", cache_key)))
            .map(|m| m.len())
            .unwrap_or(0);

        let mut load_times = Vec::new();
        for _ in 0..5 {
            cache.clear_memory();
            let t = std::time::Instant::now();
            let loaded = cache.load(cache_key).expect("load must succeed");
            load_times.push(t.elapsed().as_secs_f64() * 1000.0);
            assert!(!loaded.function_mapping.is_empty());
        }
        load_times.sort_by(|a, b| a.partial_cmp(b).unwrap());

        eprintln!(
            "bench_bytecode_cache_load: save {:.1}ms, disk {}KB, cold load median {:.1}ms (min {:.1} max {:.1}, n=5)",
            save_ms,
            disk_size / 1024,
            load_times[2],
            load_times[0],
            load_times[4]
        );

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_cache_save_load_roundtrip() {
        // Create a temporary cache directory
        let temp_dir = std::env::temp_dir().join(format!("hot_cache_test_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&temp_dir); // Clean up if exists
        std::fs::create_dir_all(&temp_dir).expect("Failed to create temp cache dir");

        let cache = BytecodeCache::new(temp_dir.clone());
        let cache_key = "test_roundtrip_key_12345";

        // Create test data
        let program = create_test_bytecode_program();
        let metadata = create_cache_metadata(
            "test_project",
            vec![FileHash {
                path: "test.hot".to_string(),
                hash: "abc123".to_string(),
            }],
            cache_key.to_string(),
        );

        let mut function_mapping = indexmap::IndexMap::new();
        function_mapping.insert("::test/func".to_string(), 0u32);
        function_mapping.insert("::test/func2".to_string(), 1u32);

        let mut core_functions = indexmap::IndexMap::new();
        core_functions.insert("add".to_string(), 100u32);

        let mut type_implementations = indexmap::IndexMap::new();
        type_implementations.insert(
            ("String".to_string(), "Int".to_string()),
            "::conv/string-to-int".to_string(),
        );

        let ast_program = crate::lang::ast::Program {
            namespaces: indexmap::IndexMap::new(),
            current_namespace: crate::lang::ast::NsPath::from_vec(vec![
                crate::lang::ast::NsPathPart::Sym(crate::lang::ast::Sym::String(
                    "test".to_string(),
                )),
            ]),
        };

        let hot_ast = crate::lang::ast::HotAst::new();

        // Save to cache
        let tool_specs = crate::lang::hot::internal_mcp::ToolSchemaRegistry::default();
        let skill_specs = crate::lang::hot::internal_skill::SkillSpecRegistry::default();
        cache
            .save(
                cache_key,
                &program,
                metadata.clone(),
                &function_mapping,
                &core_functions,
                &type_implementations,
                &ast_program,
                &hot_ast,
                &tool_specs,
                &skill_specs,
            )
            .expect("Failed to save to cache");

        // Clear global memory cache to force disk load
        cache.clear_memory();

        // Load from cache
        let loaded = cache.load(cache_key).expect("Failed to load from cache");

        // Verify metadata
        assert_eq!(loaded.metadata.project_name, metadata.project_name);
        assert_eq!(loaded.metadata.cache_key, metadata.cache_key);

        // Verify program
        assert_eq!(loaded.program.constants.len(), program.constants.len());
        assert_eq!(loaded.program.functions.len(), program.functions.len());

        // Verify registries
        assert_eq!(loaded.function_mapping.len(), function_mapping.len());
        assert_eq!(loaded.core_functions.len(), core_functions.len());
        assert_eq!(
            loaded.type_implementations.len(),
            type_implementations.len()
        );

        // Cleanup
        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn test_variable_metadata_serialization() {
        let metadata = VariableMetadata {
            name: "my_variable".to_string(),
            namespace: "::my::namespace".to_string(),
            static_scope: Some("::my::namespace/my-func".to_string()),
            meta: Some(Val::Map(Box::new({
                let mut m = indexmap::IndexMap::new();
                m.insert(Val::from("key"), Val::from("value"));
                m
            }))),
            source: Some(SourceLocation {
                file: Some("path/to/file.hot".to_string()),
                line: 42,
                column: 10,
                position: 500,
                length: 15,
            }),
        };

        let json = serde_json::to_string(&metadata).expect("Failed to serialize VariableMetadata");
        let restored: VariableMetadata =
            serde_json::from_str(&json).expect("Failed to deserialize VariableMetadata");

        assert_eq!(metadata.name, restored.name);
        assert_eq!(metadata.namespace, restored.namespace);
        assert_eq!(metadata.static_scope, restored.static_scope);
        assert_eq!(metadata.source, restored.source);
    }
}
