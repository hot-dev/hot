//! Build docs cache for hot_app
//!
//! Provides LRU caching of build documentation extracted from build zip files.
//! This avoids repeated downloads from S3 and zip extraction.
//!
//! For live builds:
//! - Docs are cached in memory and refreshed in background when files change
//! - Initial generation happens in a background task to avoid blocking page loads
//! - Returns cached docs immediately if available, or empty state while generating

use ahash::AHashMap;
use hot::db::{Build, DatabasePool, Project};
use hot::pkg::docs::{PkgDocs, ProjectDocs};
use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::RwLock;
use uuid::Uuid;

/// Maximum number of build docs entries to cache in memory
const MAX_MEMORY_CACHE_ENTRIES: usize = 100;

/// Maximum disk cache size in bytes (100 MB)
const MAX_DISK_CACHE_BYTES: u64 = 100 * 1024 * 1024;

/// Build docs cache entry
#[derive(Debug, Clone)]
pub struct CachedBuildDocs {
    pub build_id: Uuid,
    pub project_docs: ProjectDocs,
    pub dependency_docs: AHashMap<String, PkgDocs>,
    pub cached_at: std::time::Instant,
}

/// Status of live docs generation
#[derive(Debug, Clone, PartialEq)]
pub enum LiveDocsStatus {
    /// Docs are being generated in background
    Generating,
    /// Docs are ready
    Ready,
}

/// Build docs cache manager
pub struct BuildDocsCache {
    /// In-memory cache of recently accessed build docs
    memory_cache: RwLock<AHashMap<Uuid, CachedBuildDocs>>,
    /// Cache for live builds (keyed by project name for easy invalidation)
    live_docs_cache: RwLock<AHashMap<String, CachedBuildDocs>>,
    /// Track which live docs are currently being generated
    live_docs_generating: RwLock<AHashMap<String, bool>>,
    /// Track the build hash that live docs were generated from (for staleness detection)
    live_docs_build_hash: RwLock<AHashMap<String, String>>,
    /// Disk cache directory
    cache_dir: PathBuf,
    /// Build storage for retrieving build zip files
    conf: hot::val::Val,
}

impl BuildDocsCache {
    /// Create a new build docs cache
    pub fn new(conf: hot::val::Val) -> Self {
        let cache_dir = std::env::var("HOT_APP_CACHE_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(|_| hot::lang::cache::paths::get_docs_cache_dir());

        // Ensure cache directory exists
        if let Err(e) = std::fs::create_dir_all(&cache_dir) {
            tracing::warn!("Failed to create docs cache directory: {}", e);
        }

        Self {
            memory_cache: RwLock::new(AHashMap::new()),
            live_docs_cache: RwLock::new(AHashMap::new()),
            live_docs_generating: RwLock::new(AHashMap::new()),
            live_docs_build_hash: RwLock::new(AHashMap::new()),
            cache_dir,
            conf,
        }
    }

    /// Get docs for a build, using cache if available
    pub async fn get_build_docs(
        &self,
        db: &DatabasePool,
        build: &Build,
    ) -> Result<CachedBuildDocs, String> {
        // For live builds, use the live docs cache
        if build.is_live() {
            return self.get_live_build_docs(db, build).await;
        }

        // For regular builds, use caching
        // Check memory cache first
        {
            let cache = self.memory_cache.read().await;
            if let Some(cached) = cache.get(&build.build_id) {
                tracing::debug!("Build docs cache hit (memory) for {}", build.build_id);
                return Ok(cached.clone());
            }
        }

        // Check disk cache
        let disk_cache_path = self.get_disk_cache_path(&build.build_id);
        if disk_cache_path.exists()
            && let Ok(cached) = self.load_from_disk(&build.build_id).await
        {
            // Add to memory cache
            self.add_to_memory_cache(cached.clone()).await;
            tracing::debug!("Build docs cache hit (disk) for {}", build.build_id);
            return Ok(cached);
        }

        // Not in cache - need to extract from build
        tracing::debug!(
            "Build docs cache miss for {}, extracting from build",
            build.build_id
        );
        let cached = self.extract_docs_from_build(db, build).await?;

        // Save to disk cache
        if let Err(e) = self.save_to_disk(&cached).await {
            tracing::warn!("Failed to save docs to disk cache: {}", e);
        }

        // Add to memory cache
        self.add_to_memory_cache(cached.clone()).await;

        Ok(cached)
    }

    /// Get docs for a live build with caching and background generation
    async fn get_live_build_docs(
        &self,
        db: &DatabasePool,
        build: &Build,
    ) -> Result<CachedBuildDocs, String> {
        // Get project name for cache key
        let project = Project::get_project(db, &build.project_id)
            .await
            .map_err(|e| format!("Failed to get project: {}", e))?;
        let project_name = project.name.clone();

        // Check if cached docs are stale (build hash changed since last generation)
        let is_stale = {
            let hashes = self.live_docs_build_hash.read().await;
            match hashes.get(&project_name) {
                Some(cached_hash) => cached_hash != &build.hash,
                None => false, // No cached hash means first load, not stale
            }
        };

        // Check in-memory cache first
        if !is_stale {
            let cache = self.live_docs_cache.read().await;
            if let Some(cached) = cache.get(&project_name) {
                tracing::debug!(
                    "Live docs cache hit (memory) for '{}' (cached {} seconds ago)",
                    project_name,
                    cached.cached_at.elapsed().as_secs()
                );
                return Ok(cached.clone());
            }
        } else {
            tracing::debug!(
                "Live docs for '{}' are stale (build hash changed), regenerating",
                project_name
            );
            // Clear stale cache
            {
                let mut cache = self.live_docs_cache.write().await;
                cache.remove(&project_name);
            }
            self.invalidate_live_disk_cache(&project_name).await;
        }

        // Check disk cache before generating (skip if already known stale from memory)
        if !is_stale && let Ok((cached, disk_hash)) = self.load_live_from_disk(&project_name).await
        {
            // Verify disk cache freshness: compare stored hash against current build hash.
            // If the disk cache has no hash (written before staleness tracking), treat as stale
            // since we can't verify it matches the current build.
            let disk_is_stale = match &disk_hash {
                Some(h) => h != &build.hash,
                None => true, // No hash stored -- can't verify freshness, regenerate
            };

            if disk_is_stale {
                tracing::debug!(
                    "Live docs disk cache for '{}' is stale (disk hash {:?} != build hash {}), regenerating",
                    project_name,
                    disk_hash,
                    build.hash
                );
                self.invalidate_live_disk_cache(&project_name).await;
            } else {
                tracing::debug!(
                    "Live docs cache hit (disk) for '{}' - {} project ns, {} deps",
                    project_name,
                    cached.project_docs.namespaces.len(),
                    cached.dependency_docs.len()
                );
                // Add to memory cache and record build hash
                {
                    let mut cache = self.live_docs_cache.write().await;
                    cache.insert(project_name.clone(), cached.clone());
                }
                {
                    let mut hashes = self.live_docs_build_hash.write().await;
                    hashes.insert(project_name.clone(), build.hash.clone());
                }
                return Ok(cached);
            }
        }

        // Check if already generating
        {
            let generating = self.live_docs_generating.read().await;
            if generating.get(&project_name) == Some(&true) {
                if is_stale {
                    // Build changed while generating -- update the hash so when
                    // the current generation finishes, the next page visit will
                    // detect staleness and re-trigger generation with the new build.
                    tracing::debug!(
                        "Live docs for '{}' are being generated but build has changed, \
                         will regenerate after current generation completes",
                        project_name
                    );
                } else {
                    tracing::debug!(
                        "Live docs for '{}' are being generated, returning empty state",
                        project_name
                    );
                }
                // Return empty docs while generating
                return Ok(CachedBuildDocs {
                    build_id: build.build_id,
                    project_docs: ProjectDocs {
                        name: project_name.clone(),
                        namespaces: vec![],
                        type_index: AHashMap::new(),
                    },
                    dependency_docs: AHashMap::new(),
                    cached_at: std::time::Instant::now(),
                });
            }
        }

        // Mark as generating and record the build hash we're generating for
        {
            let mut generating = self.live_docs_generating.write().await;
            generating.insert(project_name.clone(), true);
        }
        {
            let mut hashes = self.live_docs_build_hash.write().await;
            hashes.insert(project_name.clone(), build.hash.clone());
        }

        // Spawn background task to generate docs
        tracing::debug!(
            "Live docs for '{}' not cached, generating in background (refresh page in a few seconds)",
            project_name
        );

        let build_id = build.build_id;
        let project_name_clone = project_name.clone();

        // Clone the global cache reference for the background task
        if let Some(cache) = crate::build_cache::get_build_docs_cache() {
            tokio::spawn(async move {
                let start = std::time::Instant::now();

                // Generate docs incrementally - project first, then dependencies
                cache
                    .generate_docs_incrementally(&build_id, &project_name_clone)
                    .await;

                // Clear generating flag
                {
                    let mut generating = cache.live_docs_generating.write().await;
                    generating.remove(&project_name_clone);
                }

                tracing::debug!(
                    "Live docs for '{}' fully generated in {:.2}s",
                    project_name_clone,
                    start.elapsed().as_secs_f64()
                );
            });
        }

        // Return empty docs immediately while generation happens in background
        Ok(CachedBuildDocs {
            build_id: build.build_id,
            project_docs: ProjectDocs {
                name: project_name.clone(),
                namespaces: vec![],
                type_index: AHashMap::new(),
            },
            dependency_docs: AHashMap::new(),
            cached_at: std::time::Instant::now(),
        })
    }

    /// Check if docs are currently being generated for a project
    pub async fn is_generating(&self, project_name: &str) -> bool {
        let generating = self.live_docs_generating.read().await;
        generating.get(project_name) == Some(&true)
    }

    /// Invalidate live docs cache for a project (call when files change)
    /// This clears both memory and disk cache
    pub async fn invalidate_live_docs(&self, project_name: &str) {
        // Clear memory cache
        let mut cache = self.live_docs_cache.write().await;
        if cache.remove(project_name).is_some() {
            tracing::debug!("Invalidated live docs memory cache for '{}'", project_name);
        }
        drop(cache); // Release lock before async disk operation

        // Clear build hash so next access triggers regeneration
        {
            let mut hashes = self.live_docs_build_hash.write().await;
            hashes.remove(project_name);
        }

        // Clear disk cache
        self.invalidate_live_disk_cache(project_name).await;
    }

    /// Invalidate all live docs caches
    pub async fn invalidate_all_live_docs(&self) {
        let mut cache = self.live_docs_cache.write().await;
        let count = cache.len();
        cache.clear();
        if count > 0 {
            tracing::debug!("Invalidated {} live docs cache entries", count);
        }
        drop(cache);

        let mut hashes = self.live_docs_build_hash.write().await;
        hashes.clear();
    }

    /// Extract docs from a build zip file or generate on-the-fly for live builds
    async fn extract_docs_from_build(
        &self,
        db: &DatabasePool,
        build: &Build,
    ) -> Result<CachedBuildDocs, String> {
        // Get project to get env_id and name
        let project = Project::get_project(db, &build.project_id)
            .await
            .map_err(|e| format!("Failed to get project: {}", e))?;

        // For live builds or when build file doesn't exist, generate docs on-the-fly
        if build.is_live() {
            tracing::debug!(
                "Generating docs on-the-fly for live build {} (project: {})",
                build.build_id,
                project.name
            );
            return self
                .generate_docs_from_source(&build.build_id, &project.name)
                .await;
        }

        // Get env to get org_id
        let env = hot::db::Env::get_env(db, &project.env_id)
            .await
            .map_err(|e| format!("Failed to get env: {}", e))?;

        // Get build storage
        let storage = hot::storage::build_storage_from_config(&self.conf)
            .await
            .map_err(|e| format!("Failed to get build storage: {}", e))?;

        // Try to retrieve build zip data
        match storage
            .retrieve_build(&build.build_id, &env.org_id, &project.env_id)
            .await
        {
            Ok(build_data) => {
                // Extract docs from zip
                self.extract_docs_from_zip(&build.build_id, &build_data)
            }
            Err(e) => {
                // Build file not found - try generating on-the-fly as fallback
                tracing::warn!(
                    "Build file not found for {}, generating docs on-the-fly: {}",
                    build.build_id,
                    e
                );
                self.generate_docs_from_source(&build.build_id, &project.name)
                    .await
            }
        }
    }

    /// Generate docs incrementally - project first, then dependencies one by one
    /// Updates the cache after each step so partial docs are available immediately
    /// Uses spawn_blocking for CPU-bound work to allow Ctrl-C interruption
    async fn generate_docs_incrementally(&self, build_id: &Uuid, project_name: &str) {
        // Get project src paths from configuration
        let src_paths = hot::project::get_project_src_paths(&self.conf, project_name);
        if src_paths.is_empty() {
            tracing::debug!(
                "Project '{}' not in local config - cannot generate live docs (use deployed bundle instead)",
                project_name
            );
            return;
        }

        // Step 1: Generate and cache project docs first (faster)
        // Use spawn_blocking for CPU-bound doc generation
        tracing::debug!(
            "Generating project docs for '{}' from {:?}",
            project_name,
            src_paths
        );
        let project_start = std::time::Instant::now();

        let project_name_clone = project_name.to_string();
        let src_paths_clone = src_paths.clone();
        let project_docs_result = tokio::task::spawn_blocking(move || {
            hot::pkg::docs::generate_project_docs(&project_name_clone, &src_paths_clone)
        })
        .await;

        let project_docs = match project_docs_result {
            Ok(Ok(docs)) => {
                tracing::debug!(
                    "Project docs for '{}' generated in {:.2}s ({} namespaces)",
                    project_name,
                    project_start.elapsed().as_secs_f64(),
                    docs.namespaces.len()
                );
                docs
            }
            Ok(Err(e)) => {
                tracing::warn!(
                    "Failed to generate project docs for '{}': {}",
                    project_name,
                    e
                );
                ProjectDocs {
                    name: project_name.to_string(),
                    namespaces: vec![],
                    type_index: AHashMap::new(),
                }
            }
            Err(e) => {
                tracing::warn!(
                    "Doc generation task cancelled for '{}': {}",
                    project_name,
                    e
                );
                return; // Task was cancelled (e.g., by Ctrl-C)
            }
        };

        // Cache project docs immediately (even before dependencies)
        {
            let mut cache = self.live_docs_cache.write().await;
            cache.insert(
                project_name.to_string(),
                CachedBuildDocs {
                    build_id: *build_id,
                    project_docs: project_docs.clone(),
                    dependency_docs: AHashMap::new(),
                    cached_at: std::time::Instant::now(),
                },
            );
        }
        tracing::debug!(
            "Cached project docs for '{}', now loading dependencies...",
            project_name
        );

        // Step 2: Generate hot-std docs first (standard library, always included)
        // Use spawn_blocking for CPU-bound doc generation
        let mut dependency_docs: AHashMap<String, hot::pkg::docs::PkgDocs> = AHashMap::new();

        let resolver = hot::lang::project::DependencyResolver::new_default();
        let hot_std_dep = resolver.get_hot_std_dependency();
        let hot_std_start = std::time::Instant::now();
        tracing::debug!(
            "Generating docs for hot-std from {:?}",
            hot_std_dep.resolved_path
        );

        let hot_std_path = hot_std_dep.resolved_path.clone();
        let hot_std_result =
            tokio::task::spawn_blocking(move || hot::pkg::docs::load_pkg_docs(&hot_std_path)).await;

        match hot_std_result {
            Ok(Ok(docs)) => {
                // Use the package's canonical name from its metadata (e.g., "hot.dev/hot-std")
                let canonical_name = docs.meta.name.clone();
                tracing::debug!(
                    "hot-std docs generated in {:.2}s ({} namespaces)",
                    hot_std_start.elapsed().as_secs_f64(),
                    docs.namespaces.len()
                );
                dependency_docs.insert(canonical_name, docs);

                // Update cache with hot-std
                {
                    let mut cache = self.live_docs_cache.write().await;
                    cache.insert(
                        project_name.to_string(),
                        CachedBuildDocs {
                            build_id: *build_id,
                            project_docs: project_docs.clone(),
                            dependency_docs: dependency_docs.clone(),
                            cached_at: std::time::Instant::now(),
                        },
                    );
                }
            }
            Ok(Err(e)) => {
                tracing::warn!("Failed to generate hot-std docs: {}", e);
            }
            Err(e) => {
                tracing::warn!("hot-std doc generation cancelled: {}", e);
                return; // Task was cancelled
            }
        }

        // Step 3: Get other resolved dependencies
        let resolved_deps =
            hot::project::get_resolved_project_dependencies(&self.conf, project_name)
                .unwrap_or_else(|e| {
                    tracing::warn!("Failed to get project dependencies: {}", e);
                    vec![]
                });

        // Step 4: Generate other dependency docs one by one, updating cache after each
        // Use spawn_blocking for CPU-bound doc generation
        for dep in &resolved_deps {
            // Skip hot-std (already handled above)
            if dep.name == "hot-std" {
                continue;
            }

            let dep_start = std::time::Instant::now();
            tracing::debug!("Generating docs for dependency: {}", dep.name);

            let dep_path = dep.resolved_path.clone();
            let dep_name = dep.name.clone();
            let dep_result =
                tokio::task::spawn_blocking(move || hot::pkg::docs::load_pkg_docs(&dep_path)).await;

            match dep_result {
                Ok(Ok(docs)) => {
                    // Use the package's canonical name from its metadata (e.g., "hot.dev/anthropic")
                    let canonical_name = docs.meta.name.clone();
                    tracing::debug!(
                        "Dependency docs for '{}' generated in {:.2}s ({} namespaces)",
                        canonical_name,
                        dep_start.elapsed().as_secs_f64(),
                        docs.namespaces.len()
                    );
                    dependency_docs.insert(canonical_name, docs);

                    // Update cache with this dependency added
                    {
                        let mut cache = self.live_docs_cache.write().await;
                        cache.insert(
                            project_name.to_string(),
                            CachedBuildDocs {
                                build_id: *build_id,
                                project_docs: project_docs.clone(),
                                dependency_docs: dependency_docs.clone(),
                                cached_at: std::time::Instant::now(),
                            },
                        );
                    }
                }
                Ok(Err(e)) => {
                    tracing::warn!(
                        "Failed to generate docs for dependency '{}': {}",
                        dep_name,
                        e
                    );
                }
                Err(e) => {
                    tracing::warn!(
                        "Doc generation cancelled for dependency '{}': {}",
                        dep_name,
                        e
                    );
                    return; // Task was cancelled
                }
            }
        }

        tracing::debug!(
            "All docs for '{}' complete: {} project namespaces, {} dependencies",
            project_name,
            project_docs.namespaces.len(),
            dependency_docs.len()
        );

        // Save to disk cache for persistence across restarts
        let final_docs = CachedBuildDocs {
            build_id: *build_id,
            project_docs,
            dependency_docs,
            cached_at: std::time::Instant::now(),
        };
        if let Err(e) = self.save_live_to_disk(project_name, &final_docs).await {
            tracing::warn!(
                "Failed to save live docs to disk for '{}': {}",
                project_name,
                e
            );
        }
    }

    /// Generate docs on-the-fly from source files (all at once, for non-incremental use)
    pub async fn generate_docs_from_source(
        &self,
        build_id: &Uuid,
        project_name: &str,
    ) -> Result<CachedBuildDocs, String> {
        // Get project src paths from configuration
        let src_paths = hot::project::get_project_src_paths(&self.conf, project_name);
        if src_paths.is_empty() {
            return Err(format!(
                "Project '{}' not found or has no source paths",
                project_name
            ));
        }

        tracing::debug!(
            "Generating docs on-the-fly for project '{}' from paths: {:?}",
            project_name,
            src_paths
        );

        // Get resolved dependencies
        let resolved_deps =
            hot::project::get_resolved_project_dependencies(&self.conf, project_name)
                .unwrap_or_else(|e| {
                    tracing::warn!("Failed to get project dependencies: {}", e);
                    vec![]
                });

        // Generate docs (synchronous)
        let build_docs =
            hot::pkg::docs::generate_build_docs(project_name, &src_paths, &resolved_deps)
                .map_err(|e| format!("Failed to generate docs: {}", e))?;

        // Convert to cached format
        Ok(CachedBuildDocs {
            build_id: *build_id,
            project_docs: build_docs.project.unwrap_or_else(|| ProjectDocs {
                name: project_name.to_string(),
                namespaces: vec![],
                type_index: AHashMap::new(),
            }),
            dependency_docs: build_docs.deps,
            cached_at: std::time::Instant::now(),
        })
    }

    /// Extract docs from a build zip file in memory
    fn extract_docs_from_zip(
        &self,
        build_id: &Uuid,
        zip_data: &[u8],
    ) -> Result<CachedBuildDocs, String> {
        use std::io::Cursor;

        let cursor = Cursor::new(zip_data);
        let mut archive = ::zip::ZipArchive::new(cursor)
            .map_err(|e| format!("Failed to read build zip: {}", e))?;

        let mut project_docs: Option<ProjectDocs> = None;
        let mut dependency_docs = AHashMap::new();

        // Look for docs files in the archive
        for i in 0..archive.len() {
            let mut file = archive
                .by_index(i)
                .map_err(|e| format!("Failed to access file in archive: {}", e))?;

            let name = file.name().to_string();

            // Extract project docs
            if name == "docs/project/docs.json" {
                let mut content = String::new();
                file.read_to_string(&mut content)
                    .map_err(|e| format!("Failed to read project docs: {}", e))?;
                project_docs = Some(
                    serde_json::from_str(&content)
                        .map_err(|e| format!("Failed to parse project docs: {}", e))?,
                );
            }
            // Extract dependency docs (docs/deps/{pkg_name}/docs.json)
            // Note: pkg_name can contain slashes (e.g., "hot.dev/anthropic" -> "docs/deps/hot.dev/anthropic/docs.json")
            else if name.starts_with("docs/deps/") && name.ends_with("/docs.json") {
                // Extract the package identifier from path: strip "docs/deps/" prefix and "/docs.json" suffix
                // This gives us the full org/name (e.g., "hot.dev/anthropic") from the path
                let pkg_id = name["docs/deps/".len()..name.len() - "/docs.json".len()].to_string();
                let mut content = String::new();
                file.read_to_string(&mut content)
                    .map_err(|e| format!("Failed to read {} docs: {}", pkg_id, e))?;
                let pkg_docs: PkgDocs = serde_json::from_str(&content)
                    .map_err(|e| format!("Failed to parse {} docs: {}", pkg_id, e))?;
                // Use the path-derived identifier (e.g., "hot.dev/anthropic") as the key
                // This matches how deps are specified in project config and URL routing
                dependency_docs.insert(pkg_id, pkg_docs);
            }
        }

        // Return empty project docs if none found
        let project_docs = project_docs.unwrap_or_else(|| ProjectDocs {
            name: String::new(),
            namespaces: vec![],
            type_index: AHashMap::new(),
        });

        Ok(CachedBuildDocs {
            build_id: *build_id,
            project_docs,
            dependency_docs,
            cached_at: std::time::Instant::now(),
        })
    }

    /// Get disk cache path for a build
    fn get_disk_cache_path(&self, build_id: &Uuid) -> PathBuf {
        self.cache_dir.join(format!("{}.json", build_id.simple()))
    }

    /// Load cached docs from disk
    async fn load_from_disk(&self, build_id: &Uuid) -> Result<CachedBuildDocs, String> {
        let path = self.get_disk_cache_path(build_id);
        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| format!("Failed to read disk cache: {}", e))?;

        let disk_entry: DiskCacheEntry = serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse disk cache: {}", e))?;

        Ok(CachedBuildDocs {
            build_id: *build_id,
            project_docs: disk_entry.project_docs,
            dependency_docs: disk_entry.dependency_docs,
            cached_at: std::time::Instant::now(),
        })
    }

    /// Save cached docs to disk
    async fn save_to_disk(&self, cached: &CachedBuildDocs) -> Result<(), String> {
        // Ensure cache directory exists
        if let Err(e) = tokio::fs::create_dir_all(&self.cache_dir).await {
            return Err(format!("Failed to create cache directory: {}", e));
        }

        // Check disk cache size and evict if needed
        self.maybe_evict_disk_cache().await;

        let disk_entry = DiskCacheEntry {
            build_hash: None, // Non-live builds don't need hash tracking
            project_docs: cached.project_docs.clone(),
            dependency_docs: cached.dependency_docs.clone(),
        };

        let content = serde_json::to_string_pretty(&disk_entry)
            .map_err(|e| format!("Failed to serialize docs: {}", e))?;

        let path = self.get_disk_cache_path(&cached.build_id);
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| format!("Failed to write disk cache: {}", e))?;

        Ok(())
    }

    // ========== Live Build Disk Cache ==========

    /// Get disk cache path for live build docs (keyed by project name)
    fn get_live_disk_cache_path(&self, project_name: &str) -> PathBuf {
        self.cache_dir.join(format!("live-{}.json", project_name))
    }

    /// Load live build docs from disk cache
    /// Returns (cached_docs, disk_build_hash) so the caller can verify freshness
    async fn load_live_from_disk(
        &self,
        project_name: &str,
    ) -> Result<(CachedBuildDocs, Option<String>), String> {
        let path = self.get_live_disk_cache_path(project_name);
        if !path.exists() {
            return Err("Live disk cache not found".to_string());
        }

        let content = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| format!("Failed to read live disk cache: {}", e))?;

        let disk_entry: DiskCacheEntry = serde_json::from_str(&content)
            .map_err(|e| format!("Failed to parse live disk cache: {}", e))?;

        tracing::debug!(
            "Loaded live docs from disk for '{}' ({} project ns, {} deps, hash: {:?})",
            project_name,
            disk_entry.project_docs.namespaces.len(),
            disk_entry.dependency_docs.len(),
            disk_entry.build_hash,
        );

        let disk_hash = disk_entry.build_hash;
        Ok((
            CachedBuildDocs {
                build_id: Uuid::nil(), // Live builds don't have a fixed build_id
                project_docs: disk_entry.project_docs,
                dependency_docs: disk_entry.dependency_docs,
                cached_at: std::time::Instant::now(),
            },
            disk_hash,
        ))
    }

    /// Save live build docs to disk cache
    async fn save_live_to_disk(
        &self,
        project_name: &str,
        cached: &CachedBuildDocs,
    ) -> Result<(), String> {
        // Ensure cache directory exists
        if let Err(e) = tokio::fs::create_dir_all(&self.cache_dir).await {
            return Err(format!("Failed to create cache directory: {}", e));
        }

        // Get the build hash for this project to persist with the disk cache
        let build_hash = {
            let hashes = self.live_docs_build_hash.read().await;
            hashes.get(project_name).cloned()
        };

        let disk_entry = DiskCacheEntry {
            build_hash,
            project_docs: cached.project_docs.clone(),
            dependency_docs: cached.dependency_docs.clone(),
        };

        let content = serde_json::to_string_pretty(&disk_entry)
            .map_err(|e| format!("Failed to serialize live docs: {}", e))?;

        let path = self.get_live_disk_cache_path(project_name);
        tokio::fs::write(&path, content)
            .await
            .map_err(|e| format!("Failed to write live disk cache: {}", e))?;

        tracing::debug!(
            "Saved live docs to disk for '{}' ({} project ns, {} deps)",
            project_name,
            cached.project_docs.namespaces.len(),
            cached.dependency_docs.len()
        );

        Ok(())
    }

    /// Invalidate live disk cache for a project
    pub async fn invalidate_live_disk_cache(&self, project_name: &str) {
        let path = self.get_live_disk_cache_path(project_name);
        if path.exists() {
            if let Err(e) = tokio::fs::remove_file(&path).await {
                tracing::warn!(
                    "Failed to remove live disk cache for '{}': {}",
                    project_name,
                    e
                );
            } else {
                tracing::debug!("Invalidated live disk cache for '{}'", project_name);
            }
        }
    }

    /// Add to memory cache with LRU eviction
    async fn add_to_memory_cache(&self, cached: CachedBuildDocs) {
        let mut cache = self.memory_cache.write().await;

        // Evict oldest entry if at capacity
        if cache.len() >= MAX_MEMORY_CACHE_ENTRIES
            && let Some(oldest_key) = cache
                .iter()
                .min_by_key(|(_, v)| v.cached_at)
                .map(|(k, _)| *k)
        {
            cache.remove(&oldest_key);
        }

        cache.insert(cached.build_id, cached);
    }

    /// Evict old entries from disk cache if over size limit
    async fn maybe_evict_disk_cache(&self) {
        let entries = match std::fs::read_dir(&self.cache_dir) {
            Ok(entries) => entries,
            Err(_) => return,
        };

        let mut files: Vec<(PathBuf, u64, std::time::SystemTime)> = entries
            .filter_map(|e| e.ok())
            .filter_map(|e| {
                let path = e.path();
                let metadata = e.metadata().ok()?;
                let modified = metadata.modified().ok()?;
                Some((path, metadata.len(), modified))
            })
            .collect();

        // Calculate total size
        let total_size: u64 = files.iter().map(|(_, size, _)| size).sum();

        if total_size <= MAX_DISK_CACHE_BYTES {
            return;
        }

        // Sort by modified time (oldest first)
        files.sort_by_key(|(_, _, modified)| *modified);

        // Remove oldest files until under limit
        let mut current_size = total_size;
        for (path, size, _) in files {
            if current_size <= MAX_DISK_CACHE_BYTES {
                break;
            }
            if std::fs::remove_file(&path).is_ok() {
                current_size -= size;
                tracing::debug!("Evicted old cache file: {}", path.display());
            }
        }
    }

    /// Clear the entire cache
    pub async fn clear(&self) {
        // Clear memory cache
        {
            let mut cache = self.memory_cache.write().await;
            cache.clear();
        }

        // Clear disk cache
        if let Ok(entries) = std::fs::read_dir(&self.cache_dir) {
            for entry in entries.flatten() {
                let _ = std::fs::remove_file(entry.path());
            }
        }

        tracing::debug!("Build docs cache cleared");
    }

    /// Invalidate cache for a specific build
    pub async fn invalidate(&self, build_id: &Uuid) {
        // Remove from memory cache
        {
            let mut cache = self.memory_cache.write().await;
            cache.remove(build_id);
        }

        // Remove from disk cache
        let path = self.get_disk_cache_path(build_id);
        let _ = std::fs::remove_file(path);

        tracing::debug!("Invalidated cache for build {}", build_id);
    }
}

/// Serializable disk cache entry
#[derive(serde::Serialize, serde::Deserialize)]
struct DiskCacheEntry {
    /// Build hash at time of generation (for staleness detection across restarts)
    #[serde(default)]
    build_hash: Option<String>,
    project_docs: ProjectDocs,
    dependency_docs: AHashMap<String, PkgDocs>,
}

/// Global build docs cache instance
static BUILD_DOCS_CACHE: once_cell::sync::OnceCell<Arc<BuildDocsCache>> =
    once_cell::sync::OnceCell::new();

/// Initialize the global build docs cache
pub fn init_build_docs_cache(conf: hot::val::Val) {
    let cache = Arc::new(BuildDocsCache::new(conf));
    if BUILD_DOCS_CACHE.set(cache).is_err() {
        tracing::warn!("Build docs cache already initialized");
    }
}

/// Get the global build docs cache
pub fn get_build_docs_cache() -> Option<Arc<BuildDocsCache>> {
    BUILD_DOCS_CACHE.get().cloned()
}
