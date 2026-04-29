use crate::val::Val;
use ahash::{AHashMap, AHashSet};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Project dependency specification from project configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDependency {
    pub name: String,
    pub spec: DependencySpec,
}

/// Dependency specification - supports local, git, registry, or combined local+git fallback
///
/// Resolution priority:
/// 1. If `local` is specified and the path exists → use local
/// 2. If `local` is specified but doesn't exist, and `git` is also specified → use git
/// 3. If `pkg` is specified → use package registry (hot.dev)
/// 4. If only `git` is specified → use git
/// 5. If only `local` is specified but doesn't exist → error
/// 6. If only `version` is specified → use registry (future)
/// 7. If empty `{}` → use default resolution (HOT_HOME, installed paths)
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DependencySpec {
    /// Local file system path (preferred if exists)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub local: Option<String>,

    /// Git repository URL (fallback if local doesn't exist)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git: Option<String>,

    /// Git branch to use
    #[serde(skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,

    /// Git tag or commit SHA to use
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tag: Option<String>,

    /// Path within the git repository (for monorepos)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,

    /// Registry version constraint (future)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,

    /// Package registry coordinate: "<org>/<package>/<version>"
    /// Examples:
    ///   - "hot.dev/anthropic/0.1.0" - anthropic package version 0.1.0
    ///   - "hot.dev/aws-s3/1.2.3" - aws-s3 package version 1.2.3
    ///
    /// Explicit versions are required for reproducible builds.
    /// Currently resolves via git (github.com/hot-dev/hot with prefixed tags).
    /// Will resolve via pkg.hot.dev CDN in future releases.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pkg: Option<String>,
}

impl DependencySpec {
    /// Create a local-only dependency spec
    pub fn local(path: &str) -> Self {
        Self {
            local: Some(path.to_string()),
            ..Default::default()
        }
    }

    /// Create a git-only dependency spec
    pub fn git(url: &str) -> Self {
        Self {
            git: Some(url.to_string()),
            ..Default::default()
        }
    }

    /// Create a local+git fallback dependency spec
    pub fn local_with_git_fallback(
        local_path: &str,
        git_url: &str,
        repo_path: Option<&str>,
    ) -> Self {
        Self {
            local: Some(local_path.to_string()),
            git: Some(git_url.to_string()),
            path: repo_path.map(|s| s.to_string()),
            ..Default::default()
        }
    }

    /// Check if this is an empty spec (resolve from defaults)
    pub fn is_empty(&self) -> bool {
        self.local.is_none() && self.git.is_none() && self.version.is_none() && self.pkg.is_none()
    }

    /// Create a package registry dependency spec
    pub fn pkg(spec: &str) -> Self {
        Self {
            pkg: Some(spec.to_string()),
            ..Default::default()
        }
    }
}

/// Parsed package registry specification
///
/// Format: `<org>/<package>/<version>`
/// Example: `hot.dev/anthropic/0.1.0`
///
/// The org is required and must be `hot.dev` for official packages.
/// Version must be an explicit semver (e.g., `0.1.0`) for reproducible builds.
#[derive(Debug, Clone)]
pub struct ParsedPkgSpec {
    /// Organization/registry (e.g., "hot.dev")
    pub org: String,
    /// Package name (e.g., "anthropic")
    pub package: String,
    /// Version (e.g., "0.1.0") - must be explicit, no "latest"
    pub version: String,
}

impl ParsedPkgSpec {
    /// Parse a pkg spec string like "hot.dev/anthropic/0.1.0"
    ///
    /// Format: `<org>/<package>/<version>`
    /// - org: Required, e.g., "hot.dev"
    /// - package: Required, e.g., "anthropic"
    /// - version: Required, explicit semver e.g., "0.1.0"
    pub fn parse(spec: &str) -> Result<Self, String> {
        let parts: Vec<&str> = spec.split('/').collect();

        if parts.len() != 3 {
            return Err(format!(
                "Invalid pkg spec '{}': expected format '<org>/<package>/<version>' \
                 (e.g., 'hot.dev/anthropic/0.1.0')",
                spec
            ));
        }

        let org = parts[0];
        let package = parts[1];
        let version = parts[2];

        if org.is_empty() {
            return Err(format!("Invalid pkg spec '{}': empty org", spec));
        }
        if package.is_empty() {
            return Err(format!("Invalid pkg spec '{}': empty package name", spec));
        }
        if version.is_empty() {
            return Err(format!("Invalid pkg spec '{}': empty version", spec));
        }

        Ok(Self {
            org: org.to_string(),
            package: package.to_string(),
            version: version.to_string(),
        })
    }

    /// Get the CDN URL for this package
    ///
    /// Returns the URL to fetch the package tarball from pkg.hot.dev
    /// URL format: https://pkg.hot.dev/{org}/{package}/{version}.tar.gz
    pub fn cdn_url(&self) -> String {
        format!(
            "https://pkg.hot.dev/{}/{}/{}.tar.gz",
            self.org, self.package, self.version
        )
    }

    /// Get the versions.json URL for this package
    /// URL format: https://pkg.hot.dev/{org}/{package}/versions.json
    pub fn versions_url(&self) -> String {
        format!(
            "https://pkg.hot.dev/{}/{}/versions.json",
            self.org, self.package
        )
    }

    /// Expand to git-based dependency spec (fallback when CDN unavailable)
    ///
    /// Returns (git_url, tag, path) for cloning from the git repository
    pub fn to_git_spec(&self) -> Result<(String, String, String), String> {
        match self.org.as_str() {
            "hot.dev" => {
                // Official hot.dev packages are in github.com/hot-dev/hot under hot/pkg
                // Git tag format: <package>/v<version> (e.g., anthropic/v0.1.0)
                let git_url = "https://github.com/hot-dev/hot.git".to_string();
                let tag = format!("{}/v{}", self.package, self.version);
                let path = format!("hot/pkg/{}", self.package);
                Ok((git_url, tag, path))
            }
            _ => Err(format!(
                "Unknown package org '{}'. Currently only 'hot.dev' is supported.",
                self.org
            )),
        }
    }
}

/// Resolved dependency with its location and metadata
#[derive(Debug, Clone)]
pub struct ResolvedDependency {
    pub name: String,
    pub spec: DependencySpec,
    pub resolved_path: PathBuf,
}

/// Dependency resolution context
#[derive(Debug, Clone)]
pub struct DependencyResolver {
    /// HOT_STD package path
    hot_std_path: PathBuf,
}

impl DependencyResolver {
    pub fn new(hot_std_path: PathBuf) -> Self {
        Self { hot_std_path }
    }

    /// Get the HOT_STD dependency (always included)
    pub fn get_hot_std_dependency(&self) -> ResolvedDependency {
        ResolvedDependency {
            name: "hot-std".to_string(),
            spec: DependencySpec::local(&self.hot_std_path.to_string_lossy()),
            resolved_path: self.hot_std_path.clone(),
        }
    }

    /// Create a default resolver using a default HOT_STD_PATH
    ///
    /// Resolution order:
    /// 1. HOT_HOME environment variable: $HOT_HOME/pkg/hot-std
    /// 2. Development/local: ./hot/pkg/hot-std (checked early for development workflows)
    /// 3. Executable-relative: <exe_dir>/resources/pkg/hot-std (Windows/bundled installs)
    /// 4. macOS package install: /usr/local/share/hot/pkg/hot-std
    /// 5. Linux package install: /usr/share/hot/pkg/hot-std
    pub fn new_default() -> Self {
        // Check HOT_HOME first
        if let Ok(home) = std::env::var("HOT_HOME") {
            let path = PathBuf::from(home).join("pkg").join("hot-std");
            if path.exists() {
                return Self::new(path);
            }
        }

        // Check development/local path (prioritized for development workflows)
        let dev_path = PathBuf::from("./hot/pkg/hot-std");
        if dev_path.exists() {
            return Self::new(dev_path);
        }

        // Check executable-relative path (Windows install and bundled CLI)
        if let Ok(exe_path) = std::env::current_exe()
            && let Some(exe_dir) = exe_path.parent()
        {
            let exe_relative_path = exe_dir.join("resources").join("pkg").join("hot-std");
            if exe_relative_path.exists() {
                return Self::new(exe_relative_path);
            }
        }

        // Check macOS package installation location
        let macos_path = PathBuf::from("/usr/local/share/hot/pkg/hot-std");
        if macos_path.exists() {
            return Self::new(macos_path);
        }

        // Check Linux package installation location
        let linux_path = PathBuf::from("/usr/share/hot/pkg/hot-std");
        if linux_path.exists() {
            return Self::new(linux_path);
        }

        // Final fallback (will fail gracefully if doesn't exist)
        Self::new(PathBuf::from("./hot/pkg/hot-std"))
    }

    /// Resolve a project dependency to its actual location
    ///
    /// Resolution priority:
    /// 1. If `local` is specified and exists → use local
    /// 2. If `local` is specified but doesn't exist, and `git` is also specified → use git
    /// 3. If `pkg` is specified → use package registry (hot.dev, currently via git)
    /// 4. If only `git` is specified → use git
    /// 5. If only `local` is specified but doesn't exist → error
    /// 6. If only `version` is specified → use registry (future)
    /// 7. If empty spec → use default resolution
    pub fn resolve_dependency(
        &self,
        dep: &ProjectDependency,
    ) -> Result<ResolvedDependency, String> {
        let spec = &dep.spec;

        // Try local path first if specified
        if let Some(local) = &spec.local {
            let resolved_path = PathBuf::from(local);
            if resolved_path.exists() {
                tracing::debug!(
                    "Resolved dependency '{}' from local path: {}",
                    dep.name,
                    local
                );
                return Ok(ResolvedDependency {
                    name: dep.name.clone(),
                    spec: dep.spec.clone(),
                    resolved_path,
                });
            }

            // Local path doesn't exist - check if we have git fallback
            if spec.git.is_none() {
                return Err(format!(
                    "Local dependency path does not exist: {} (for '{}')",
                    local, dep.name
                ));
            }

            tracing::debug!(
                "Local path '{}' not found for '{}', falling back to git",
                local,
                dep.name
            );
        }

        // Try pkg registry if specified (CDN first, no git fallback)
        if let Some(pkg_spec) = &spec.pkg {
            let parsed = ParsedPkgSpec::parse(pkg_spec)?;

            // Try CDN resolution first
            match Self::resolve_from_cdn(&parsed, &dep.name) {
                Ok(resolved_path) => {
                    tracing::debug!(
                        "Resolved dependency '{}' from CDN: {}",
                        dep.name,
                        resolved_path.display()
                    );
                    return Ok(ResolvedDependency {
                        name: dep.name.clone(),
                        spec: dep.spec.clone(),
                        resolved_path,
                    });
                }
                Err(cdn_error) => {
                    // CDN failed - return error (no git fallback for pkg deps)
                    return Err(format!(
                        "Failed to fetch package '{}' from CDN: {}. \
                         Make sure the package version exists at pkg.hot.dev.",
                        dep.name, cdn_error
                    ));
                }
            }
        }

        // Try git if specified
        if let Some(git) = &spec.git {
            let cache_dir = Self::get_git_cache_dir()?;

            // Clone or update the repository
            let repo_path = Self::clone_or_update_repo(
                git,
                &cache_dir,
                spec.branch.as_deref(),
                spec.tag.as_deref(),
            )?;

            // If a path within the repo is specified, resolve to that subdirectory
            let resolved_path = if let Some(subpath) = &spec.path {
                let full_path = repo_path.join(subpath);
                if !full_path.exists() {
                    return Err(format!(
                        "Path '{}' does not exist in git repository '{}' (for '{}')",
                        subpath, git, dep.name
                    ));
                }
                full_path
            } else {
                repo_path
            };

            tracing::debug!(
                "Resolved dependency '{}' from git: {}",
                dep.name,
                resolved_path.display()
            );
            return Ok(ResolvedDependency {
                name: dep.name.clone(),
                spec: dep.spec.clone(),
                resolved_path,
            });
        }

        // Try registry if specified
        if let Some(version) = &spec.version {
            return Err(format!(
                "Registry dependencies not yet supported: {} (version {})",
                dep.name, version
            ));
        }

        // Empty spec - use default resolution
        if spec.is_empty() {
            return self.resolve_from_defaults(&dep.name);
        }

        Err(format!(
            "Invalid dependency specification for '{}': no valid source specified",
            dep.name
        ))
    }

    /// Resolve a dependency from default locations
    fn resolve_from_defaults(&self, name: &str) -> Result<ResolvedDependency, String> {
        // Special case for hot-std
        if name == "hot-std" || name == "hot.dev/hot-std" {
            return Ok(self.get_hot_std_dependency());
        }

        // Extract package name from coordinate (e.g., "hot.dev/aws-core" -> "aws-core")
        let pkg_name = name.rsplit('/').next().unwrap_or(name);

        // Check HOT_HOME
        if let Ok(home) = std::env::var("HOT_HOME") {
            let path = PathBuf::from(&home).join("pkg").join(pkg_name);
            if path.exists() {
                return Ok(ResolvedDependency {
                    name: name.to_string(),
                    spec: DependencySpec::local(&path.to_string_lossy()),
                    resolved_path: path,
                });
            }
        }

        // Check development/local path
        let dev_path = PathBuf::from("./hot/pkg").join(pkg_name);
        if dev_path.exists() {
            return Ok(ResolvedDependency {
                name: name.to_string(),
                spec: DependencySpec::local(&dev_path.to_string_lossy()),
                resolved_path: dev_path,
            });
        }

        // Check executable-relative path
        if let Ok(exe_path) = std::env::current_exe()
            && let Some(exe_dir) = exe_path.parent()
        {
            let exe_relative = exe_dir.join("resources").join("pkg").join(pkg_name);
            if exe_relative.exists() {
                return Ok(ResolvedDependency {
                    name: name.to_string(),
                    spec: DependencySpec::local(&exe_relative.to_string_lossy()),
                    resolved_path: exe_relative,
                });
            }
        }

        // Check macOS installation
        let macos_path = PathBuf::from("/usr/local/share/hot/pkg").join(pkg_name);
        if macos_path.exists() {
            return Ok(ResolvedDependency {
                name: name.to_string(),
                spec: DependencySpec::local(&macos_path.to_string_lossy()),
                resolved_path: macos_path,
            });
        }

        // Check Linux installation
        let linux_path = PathBuf::from("/usr/share/hot/pkg").join(pkg_name);
        if linux_path.exists() {
            return Ok(ResolvedDependency {
                name: name.to_string(),
                spec: DependencySpec::local(&linux_path.to_string_lossy()),
                resolved_path: linux_path,
            });
        }

        Err(format!(
            "Could not resolve dependency '{}' from default locations",
            name
        ))
    }

    /// Resolve all project dependencies including hot-std (which is always first)
    pub fn resolve_all_dependencies(
        &self,
        project_deps: &[ProjectDependency],
    ) -> Result<Vec<ResolvedDependency>, String> {
        let mut resolved = Vec::new();

        // Always include hot-std first
        resolved.push(self.get_hot_std_dependency());

        // Resolve project dependencies
        for dep in project_deps {
            // Skip hot-std if it's explicitly listed (we already added it)
            if dep.name == "hot-std" {
                continue;
            }

            let resolved_dep = self.resolve_dependency(dep)?;
            resolved.push(resolved_dep);
        }

        Ok(resolved)
    }

    /// Parse project dependencies from configuration value
    ///
    /// Accepts:
    /// - Val::Map: A map of package names to dependency specs
    /// - Val::Null: No dependencies (empty {} is parsed as block returning null)
    /// - Val::Vec([]): Empty list also means no dependencies
    ///
    /// Dependency spec formats:
    /// - String shorthand: `"hot.dev/pkg": "0.1.0"` → expands to `pkg: "hot.dev/pkg/0.1.0"`
    /// - Map format: `"hot.dev/pkg": { pkg: "hot.dev/pkg/0.1.0" }`
    /// - Local: `"my-pkg": { local: "./path/to/pkg" }`
    /// - Git: `"my-pkg": { git: "https://...", tag: "v1.0.0" }`
    pub fn parse_project_dependencies(deps_val: &Val) -> Result<Vec<ProjectDependency>, String> {
        let mut dependencies = Vec::new();

        match deps_val {
            // Empty map or null means no dependencies
            Val::Null => {
                // {} in Hot is parsed as a block expression returning Null, not an empty map
                // This is fine - it means no dependencies
                return Ok(dependencies);
            }
            Val::Vec(v) if v.is_empty() => {
                // Empty list also means no dependencies
                return Ok(dependencies);
            }
            Val::Map(deps_map) => {
                // Dependencies format: single map where keys are package names and values are specs
                for (name_val, spec_val) in deps_map.iter() {
                    let name = match name_val {
                        Val::Str(s) => (**s).to_owned(),
                        _ => return Err("Dependency name must be a string".to_string()),
                    };

                    // Handle string shorthand: "hot.dev/pkg": "0.1.0" means pkg format with version
                    let spec = match spec_val {
                        Val::Str(version) => {
                            // String value = version shorthand for pkg format
                            // Combine key (name) + value (version) into full pkg spec: "hot.dev/pkg/0.1.0"
                            let full_pkg_spec = format!("{}/{}", name, version);
                            DependencySpec {
                                pkg: Some(full_pkg_spec),
                                ..Default::default()
                            }
                        }
                        _ => Self::parse_dependency_spec(spec_val)?,
                    };
                    dependencies.push(ProjectDependency { name, spec });
                }
            }
            _ => {
                return Err(
                    "Dependencies must be specified as a map with package names as keys, or {} for no dependencies".to_string(),
                );
            }
        }

        Ok(dependencies)
    }

    /// Parse a dependency specification from a configuration value
    ///
    /// Accepts a map with optional fields: local, git, branch, tag, path, version
    /// An empty map `{}` is valid and means "resolve from defaults"
    pub fn parse_dependency_spec(spec_val: &Val) -> Result<DependencySpec, String> {
        match spec_val {
            Val::Map(spec_map) => {
                let get_str = |key: &str| -> Option<String> {
                    spec_map.get(&Val::from(key.to_string())).and_then(|v| {
                        if let Val::Str(s) = v {
                            Some((**s).to_owned())
                        } else {
                            None
                        }
                    })
                };

                Ok(DependencySpec {
                    local: get_str("local"),
                    git: get_str("git"),
                    branch: get_str("branch"),
                    tag: get_str("tag"),
                    path: get_str("path"),
                    version: get_str("version"),
                    pkg: get_str("pkg"),
                })
            }
            Val::Null => {
                // Empty block {} evaluates to null - treat as empty spec
                Ok(DependencySpec::default())
            }
            _ => Err(format!(
                "Dependency specification must be a map, got: {:?}",
                spec_val
            )),
        }
    }
}

/// Helper functions for CDN operations
impl DependencyResolver {
    /// Get the cache directory for CDN packages.
    ///
    /// Uses smart resolution via `cache_paths::get_cdn_cache_dir()`:
    /// - `$HOT_HOME/cache/cdn` if HOT_HOME is set
    /// - `./.hot/cache/cdn` if `hot.hot` config exists (project-local)
    /// - Platform-specific system cache otherwise (e.g., `~/.cache/hot/cache/cdn`)
    fn get_cdn_cache_dir() -> Result<PathBuf, String> {
        let cache_base = super::cache::paths::get_cdn_cache_dir();

        std::fs::create_dir_all(&cache_base)
            .map_err(|e| format!("Failed to create CDN cache directory: {}", e))?;

        Ok(cache_base)
    }

    /// Get the cache path for a CDN package
    fn get_cdn_package_path(parsed: &ParsedPkgSpec) -> Result<PathBuf, String> {
        let cache_dir = Self::get_cdn_cache_dir()?;
        // Cache structure: {cache_dir}/{org}/{package}/{version}/
        Ok(cache_dir
            .join(&parsed.org)
            .join(&parsed.package)
            .join(&parsed.version))
    }

    /// Acquire a cross-process file lock for a CDN package.
    fn acquire_cdn_file_lock(
        parsed: &ParsedPkgSpec,
    ) -> Result<fd_lock::RwLock<std::fs::File>, std::io::Error> {
        let cache_dir = super::cache::paths::get_cdn_cache_dir();
        std::fs::create_dir_all(&cache_dir)?;

        let lock_name = format!("{}-{}-{}.lock", parsed.org, parsed.package, parsed.version);
        let lock_path = cache_dir.join(lock_name);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&lock_path)?;

        Ok(fd_lock::RwLock::new(file))
    }

    /// Resolve a package from the CDN (pkg.hot.dev)
    /// Uses file locking to prevent cross-process races during download/extraction
    fn resolve_from_cdn(parsed: &ParsedPkgSpec, dep_name: &str) -> Result<PathBuf, String> {
        let pkg_path = Self::get_cdn_package_path(parsed)?;

        // Check if already cached (fast path - no lock needed)
        if pkg_path.exists() && pkg_path.join("pkg.hot").exists() {
            tracing::debug!(
                "Package '{}' already cached at: {}",
                dep_name,
                pkg_path.display()
            );
            return Ok(pkg_path);
        }

        // Acquire cross-process file lock (best effort - proceed even if locking fails)
        let mut file_lock = Self::acquire_cdn_file_lock(parsed).ok();
        let _file_lock_guard = file_lock.as_mut().and_then(|lock| lock.try_write().ok());

        // Re-check if already cached (another process may have just downloaded it)
        if pkg_path.exists() && pkg_path.join("pkg.hot").exists() {
            tracing::debug!(
                "Package '{}' cached by another process at: {}",
                dep_name,
                pkg_path.display()
            );
            return Ok(pkg_path);
        }

        // Get CDN URL
        let cdn_url = parsed.cdn_url();
        tracing::info!("Fetching package '{}' from CDN: {}", dep_name, cdn_url);

        // Download the tarball
        let tarball_bytes = Self::download_cdn_package(&cdn_url)?;

        // Extract to cache directory (uses atomic temp dir + rename)
        Self::extract_tarball(&tarball_bytes, &pkg_path)?;

        // Verify extraction
        if !pkg_path.exists() {
            return Err(format!(
                "Package extraction failed: directory not created at {}",
                pkg_path.display()
            ));
        }

        tracing::debug!(
            "Package '{}' extracted to: {}",
            dep_name,
            pkg_path.display()
        );

        Ok(pkg_path)
    }

    /// Download a package tarball from the CDN
    fn download_cdn_package(url: &str) -> Result<Vec<u8>, String> {
        // Use ureq for blocking HTTP request (no async runtime conflicts)
        let agent = ureq::Agent::config_builder()
            .timeout_global(Some(std::time::Duration::from_secs(60)))
            .build()
            .new_agent();
        let mut response = agent.get(url).call().map_err(|e| match e {
            ureq::Error::StatusCode(404) => {
                format!(
                    "Package not found at {}. Check that the version exists.",
                    url
                )
            }
            ureq::Error::StatusCode(code) => {
                format!("HTTP error {} fetching {}", code, url)
            }
            other => format!("Network error fetching {}: {}", url, other),
        })?;

        let bytes = response
            .body_mut()
            .with_config()
            .limit(100 * 1024 * 1024) // 100MB limit
            .read_to_vec()
            .map_err(|e| format!("Failed to read package data: {}", e))?;

        Ok(bytes)
    }

    /// Extract a gzipped tarball to a directory
    fn extract_tarball(tarball_bytes: &[u8], dest_path: &Path) -> Result<(), String> {
        use flate2::read::GzDecoder;
        use std::io::Cursor;
        use tar::Archive;

        // Create parent directories
        if let Some(parent) = dest_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create directory: {}", e))?;
        }

        // Create a temporary directory for extraction
        let temp_dir = dest_path.with_extension("tmp");
        if temp_dir.exists() {
            std::fs::remove_dir_all(&temp_dir)
                .map_err(|e| format!("Failed to clean temp directory: {}", e))?;
        }
        std::fs::create_dir_all(&temp_dir)
            .map_err(|e| format!("Failed to create temp directory: {}", e))?;

        // Extract tarball
        let cursor = Cursor::new(tarball_bytes);
        let gz = GzDecoder::new(cursor);
        let mut archive = Archive::new(gz);

        archive
            .unpack(&temp_dir)
            .map_err(|e| format!("Failed to extract tarball: {}", e))?;

        // The tarball might have a top-level directory - find and move contents
        let entries: Vec<_> = std::fs::read_dir(&temp_dir)
            .map_err(|e| format!("Failed to read temp directory: {}", e))?
            .filter_map(|e| e.ok())
            .collect();

        // If there's exactly one directory, use its contents
        let source_dir = if entries.len() == 1 && entries[0].path().is_dir() {
            entries[0].path()
        } else {
            temp_dir.clone()
        };

        // Move to final location
        if dest_path.exists() {
            std::fs::remove_dir_all(dest_path)
                .map_err(|e| format!("Failed to clean destination: {}", e))?;
        }

        // Try rename first (fast), fall back to copy (cross-device)
        if std::fs::rename(&source_dir, dest_path).is_err() {
            Self::copy_dir_recursive(&source_dir, dest_path)?;
        }

        // Clean up temp directory
        let _ = std::fs::remove_dir_all(&temp_dir);

        Ok(())
    }

    /// Recursively copy a directory
    fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<(), String> {
        std::fs::create_dir_all(dst)
            .map_err(|e| format!("Failed to create directory {}: {}", dst.display(), e))?;

        for entry in std::fs::read_dir(src)
            .map_err(|e| format!("Failed to read directory {}: {}", src.display(), e))?
        {
            let entry = entry.map_err(|e| format!("Failed to read entry: {}", e))?;
            let src_path = entry.path();
            let dst_path = dst.join(entry.file_name());

            if src_path.is_dir() {
                Self::copy_dir_recursive(&src_path, &dst_path)?;
            } else {
                std::fs::copy(&src_path, &dst_path).map_err(|e| {
                    format!(
                        "Failed to copy {} to {}: {}",
                        src_path.display(),
                        dst_path.display(),
                        e
                    )
                })?;
            }
        }

        Ok(())
    }
}

/// Helper functions for git operations
impl DependencyResolver {
    /// Get the cache directory for git dependencies.
    ///
    /// Uses smart resolution via `cache_paths::get_git_cache_dir()`:
    /// - `$HOT_HOME/cache/git` if HOT_HOME is set
    /// - `./.hot/cache/git` if `hot.hot` config exists (project-local)
    /// - Platform-specific system cache otherwise (e.g., `~/.cache/hot/cache/git`)
    fn get_git_cache_dir() -> Result<PathBuf, String> {
        let cache_base = super::cache::paths::get_git_cache_dir();

        std::fs::create_dir_all(&cache_base)
            .map_err(|e| format!("Failed to create cache directory: {}", e))?;

        Ok(cache_base)
    }

    /// Generate a unique directory name for a git repository
    fn get_repo_cache_name(git_url: &str, ref_name: Option<&str>) -> String {
        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};

        let mut hasher = DefaultHasher::new();
        git_url.hash(&mut hasher);
        if let Some(ref_val) = ref_name {
            ref_val.hash(&mut hasher);
        }
        let hash = hasher.finish();

        // Create a more readable name: sanitized-url_hash
        let sanitized = git_url
            .replace("https://", "")
            .replace("http://", "")
            .replace("git@", "")
            .replace([':', '/', '.'], "_");

        format!("{}_{:x}", sanitized, hash)
    }

    /// Clone or update a git repository
    fn clone_or_update_repo(
        git_url: &str,
        cache_dir: &Path,
        branch: Option<&str>,
        tag: Option<&str>,
    ) -> Result<PathBuf, String> {
        tracing::debug!("Resolving git dependency: {}", git_url);

        // Validate that branch and tag are not both specified
        if branch.is_some() && tag.is_some() {
            return Err("Cannot specify both 'branch' and 'tag' for a git dependency".to_string());
        }

        let ref_name = tag.or(branch);
        let repo_name = Self::get_repo_cache_name(git_url, ref_name);
        let repo_path = cache_dir.join(&repo_name);

        if repo_path.exists() {
            tracing::debug!("Repository already cached at: {}", repo_path.display());

            // For branches, try to update (fetch + checkout)
            // For tags/commits, assume they're immutable and skip update
            if branch.is_some() {
                tracing::debug!("Updating branch...");
                Self::update_git_repo(&repo_path, git_url, branch)?;
            } else {
                tracing::debug!("Using cached tag/commit (immutable)");
            }
        } else {
            tracing::debug!("Cloning repository to: {}", repo_path.display());
            Self::clone_git_repo(git_url, &repo_path, ref_name)?;
        }

        // Verify the repository is valid
        if !repo_path.join(".git").exists() {
            return Err(format!(
                "Git repository at {} is invalid (missing .git directory)",
                repo_path.display()
            ));
        }

        Ok(repo_path)
    }

    /// Rewrite git SSH URLs to HTTPS if in CI environment
    /// This enables git operations in CI with token authentication while preserving SSH for local dev
    fn rewrite_git_url_for_https(git_url: &str) -> String {
        // Check if we're in a CI environment
        let is_ci = std::env::var("CI").is_ok()
            || std::env::var("GITHUB_ACTIONS").is_ok()
            || std::env::var("GITLAB_CI").is_ok()
            || std::env::var("CIRCLECI").is_ok();

        // Only rewrite git@github.com: URLs (SSH format) and only in CI
        if is_ci && git_url.starts_with("git@github.com:") {
            // Convert git@github.com:org/repo.git to https://github.com/org/repo.git
            let repo_path = git_url.strip_prefix("git@github.com:").unwrap();
            let https_url = format!("https://github.com/{}", repo_path);
            tracing::debug!(
                "CI detected: Rewrote SSH URL {} to HTTPS {}",
                git_url,
                https_url
            );
            https_url
        } else {
            // Keep other URLs as-is (already HTTPS, not in CI, or other formats)
            git_url.to_string()
        }
    }

    /// Clone a git repository
    fn clone_git_repo(
        git_url: &str,
        target_path: &Path,
        ref_name: Option<&str>,
    ) -> Result<(), String> {
        // Rewrite SSH URLs to HTTPS for CI compatibility
        let effective_url = Self::rewrite_git_url_for_https(git_url);

        let mut cmd = Command::new("git");
        cmd.arg("clone");

        // Add depth=1 for faster clones when possible
        if ref_name.is_some() {
            cmd.arg("--depth").arg("1");
        }

        cmd.arg(&effective_url);
        cmd.arg(target_path);

        // Ensure git doesn't prompt for credentials (important for CI)
        cmd.env("GIT_TERMINAL_PROMPT", "0");

        // Prevent SSH from prompting for passwords
        cmd.env(
            "GIT_SSH_COMMAND",
            "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new",
        );

        tracing::debug!(
            "Running: git clone {} {}",
            effective_url,
            target_path.display()
        );

        let output = cmd
            .output()
            .map_err(|e| format!("Failed to execute git clone: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "Failed to clone git repository '{}': {}",
                git_url, stderr
            ));
        }

        // Checkout the specific branch, tag, or commit if specified
        if let Some(ref_val) = ref_name {
            Self::checkout_ref(target_path, ref_val)?;
        }

        Ok(())
    }

    /// Update an existing git repository (fetch + checkout)
    fn update_git_repo(
        repo_path: &Path,
        git_url: &str,
        branch: Option<&str>,
    ) -> Result<(), String> {
        // Fetch updates
        let fetch_output = Command::new("git")
            .args(["fetch", "origin"])
            .current_dir(repo_path)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env(
                "GIT_SSH_COMMAND",
                "ssh -o BatchMode=yes -o StrictHostKeyChecking=accept-new",
            )
            .output()
            .map_err(|e| format!("Failed to execute git fetch: {}", e))?;

        if !fetch_output.status.success() {
            let stderr = String::from_utf8_lossy(&fetch_output.stderr);
            tracing::warn!("Git fetch warning for {}: {}", git_url, stderr);
        }

        // Checkout the branch if specified
        if let Some(branch_name) = branch {
            let checkout_output = Command::new("git")
                .args(["checkout", branch_name])
                .current_dir(repo_path)
                .output()
                .map_err(|e| format!("Failed to execute git checkout: {}", e))?;

            if !checkout_output.status.success() {
                let stderr = String::from_utf8_lossy(&checkout_output.stderr);
                return Err(format!(
                    "Failed to checkout branch '{}': {}",
                    branch_name, stderr
                ));
            }

            // Pull latest changes
            let pull_output = Command::new("git")
                .args(["pull", "origin", branch_name])
                .current_dir(repo_path)
                .output()
                .map_err(|e| format!("Failed to execute git pull: {}", e))?;

            if !pull_output.status.success() {
                let stderr = String::from_utf8_lossy(&pull_output.stderr);
                tracing::warn!("Git pull warning: {}", stderr);
            }
        }

        Ok(())
    }

    /// Checkout a specific ref (branch, tag, or commit SHA)
    fn checkout_ref(repo_path: &Path, ref_name: &str) -> Result<(), String> {
        tracing::debug!("Checking out ref: {}", ref_name);

        let output = Command::new("git")
            .args(["checkout", ref_name])
            .current_dir(repo_path)
            .output()
            .map_err(|e| format!("Failed to execute git checkout: {}", e))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(format!(
                "Failed to checkout ref '{}': {}. Note: 'tag' field accepts git tags or commit SHAs.",
                ref_name, stderr
            ));
        }

        Ok(())
    }
}

impl Default for DependencyResolver {
    fn default() -> Self {
        Self::new_default()
    }
}

/// Package metadata from pkg.hot
#[derive(Debug, Clone, Default)]
pub struct PackageMetadata {
    pub name: String,
    pub version: String,
    pub description: String,
    pub author: String,
    pub email: String,
    pub license: String,
    pub url: String,
    pub deps: Vec<ProjectDependency>,
    pub src_paths: Vec<String>,
    pub test_paths: Vec<String>,
    pub resource_paths: Vec<String>,
    /// Minimum Hot version required by this package (optional)
    pub hot_min_version: Option<String>,
}

impl PackageMetadata {
    /// Parse a pkg.hot file and extract package metadata including dependencies
    pub fn parse_from_file(pkg_hot_path: &Path) -> Result<Self, String> {
        let content = std::fs::read_to_string(pkg_hot_path)
            .map_err(|e| format!("Failed to read pkg.hot: {}", e))?;

        Self::parse_from_content(&content)
    }

    /// Parse pkg.hot content using Val-based parsing
    ///
    /// This executes the Hot code to get a Val and then extracts metadata from it.
    fn parse_from_content(content: &str) -> Result<Self, String> {
        let mut meta = PackageMetadata::default();

        // Parse and execute the Hot code to get a Val
        let result = crate::lang::engine::Engine::eval_simple(content)
            .map_err(|e| format!("Failed to execute pkg.hot: {}", e))?;

        // The result is a nested map from the dot-path assignment, e.g.:
        //   hot.pkg.my-pkg { name: "...", version: "..." }
        // evaluates to: {pkg: {my-pkg: {name: "...", version: "..."}}}
        //
        // Walk into nested single-value maps until we find the metadata map
        // (one that contains known keys like "name" or "version").
        if let Val::Map(ref map) = result {
            let inner = Self::find_metadata_map(map);
            Self::extract_metadata_from_val(&mut meta, inner)?;
        }

        Ok(meta)
    }

    /// Walk into nested maps to find the innermost one containing package metadata.
    /// The pkg.hot dot-path assignment (e.g. `hot.pkg.foo {name: ...}`) produces
    /// nested wrapper maps. We drill down until we find a map with known metadata
    /// keys like "name" or "version".
    fn find_metadata_map(map: &indexmap::IndexMap<Val, Val>) -> &indexmap::IndexMap<Val, Val> {
        // Check if this map has known metadata keys
        let has_metadata_key = map.keys().any(|k| {
            matches!(
                Self::val_to_string(k).as_str(),
                "name" | "version" | "description" | "deps"
            )
        });

        if has_metadata_key || map.len() != 1 {
            return map;
        }

        // Single-key map — check if the value is also a map and recurse
        if let Some((_, val)) = map.iter().next()
            && let Val::Map(inner) = val
        {
            return Self::find_metadata_map(inner);
        }

        map
    }

    /// Extract metadata fields from a Val::Map
    fn extract_metadata_from_val(
        meta: &mut PackageMetadata,
        map: &indexmap::IndexMap<Val, Val>,
    ) -> Result<(), String> {
        for (key, val) in map.iter() {
            let key_str = Self::val_to_string(key);
            match key_str.as_str() {
                "name" => meta.name = Self::val_to_string(val),
                "version" => meta.version = Self::val_to_string(val),
                "description" => meta.description = Self::val_to_string(val),
                "author" => meta.author = Self::val_to_string(val),
                "email" => meta.email = Self::val_to_string(val),
                "license" => meta.license = Self::val_to_string(val),
                "url" => meta.url = Self::val_to_string(val),
                "src-paths" | "src_paths" => meta.src_paths = Self::val_to_string_vec(val),
                "test-paths" | "test_paths" => meta.test_paths = Self::val_to_string_vec(val),
                "resource-paths" | "resource_paths" | "resources-paths" | "resources_paths" => {
                    meta.resource_paths = Self::val_to_string_vec(val)
                }
                "resources" => {
                    if let Val::Map(resources) = val
                        && let Some(paths) = resources
                            .get(&Val::from("paths"))
                            .or_else(|| resources.get(&Val::from("path")))
                    {
                        meta.resource_paths = Self::val_to_string_vec(paths);
                    }
                }
                "deps" => meta.deps = Self::parse_deps_from_val(val)?,
                "hot-min-version" => {
                    let v = Self::val_to_string(val);
                    if !v.is_empty() {
                        meta.hot_min_version = Some(v);
                    }
                }
                _ => {} // Ignore unknown fields
            }
        }
        Ok(())
    }

    /// Convert a Val to a string
    fn val_to_string(val: &Val) -> String {
        match val {
            Val::Str(s) => (**s).to_owned(),
            _ => String::new(),
        }
    }

    /// Convert a Val to a vector of strings
    fn val_to_string_vec(val: &Val) -> Vec<String> {
        if let Val::Vec(items) = val {
            items.iter().map(Self::val_to_string).collect()
        } else {
            vec![]
        }
    }

    /// Parse deps map from Val
    fn parse_deps_from_val(val: &Val) -> Result<Vec<ProjectDependency>, String> {
        // Use the existing parser which handles Val::Map
        DependencyResolver::parse_project_dependencies(val)
    }
}

/// Transitive dependency resolution
impl DependencyResolver {
    /// Resolve all dependencies including transitive dependencies from pkg.hot files
    ///
    /// Resolution process:
    /// 1. Start with direct project dependencies
    /// 2. For each resolved dependency, parse its pkg.hot for transitive deps
    /// 3. Apply project overrides to transitive deps before resolution
    /// 4. Continue recursively until all deps are resolved
    /// 5. Return in topologically sorted order (deps before dependents)
    pub fn resolve_all_dependencies_recursive(
        &self,
        project_deps: &[ProjectDependency],
        project_overrides: &AHashMap<String, DependencySpec>,
    ) -> Result<Vec<ResolvedDependency>, String> {
        use std::collections::VecDeque;

        let mut resolved: AHashMap<String, ResolvedDependency> = AHashMap::new();
        let mut dep_graph: AHashMap<String, Vec<String>> = AHashMap::new();
        let mut queue: VecDeque<ProjectDependency> = VecDeque::new();
        let mut seen: AHashSet<String> = AHashSet::new();

        // Always include hot-std first
        let hot_std = self.get_hot_std_dependency();
        resolved.insert("hot-std".to_string(), hot_std);
        dep_graph.insert("hot-std".to_string(), vec![]);
        seen.insert("hot-std".to_string());
        seen.insert("hot.dev/hot-std".to_string());

        // Add direct project dependencies to queue
        for dep in project_deps {
            if dep.name == "hot-std" || dep.name == "hot.dev/hot-std" {
                continue; // Already added
            }
            queue.push_back(dep.clone());
        }

        // Process dependencies breadth-first
        while let Some(dep) = queue.pop_front() {
            let dep_key = Self::normalize_dep_name(&dep.name);

            if seen.contains(&dep_key) {
                continue;
            }
            seen.insert(dep_key.clone());

            // Apply project override if available
            let effective_dep = if let Some(override_spec) = project_overrides.get(&dep.name) {
                tracing::debug!("Applying project override for dependency '{}'", dep.name);
                ProjectDependency {
                    name: dep.name.clone(),
                    spec: override_spec.clone(),
                }
            } else {
                dep.clone()
            };

            // Resolve this dependency
            let resolved_dep = self.resolve_dependency(&effective_dep)?;
            let resolved_path = resolved_dep.resolved_path.clone();

            // Parse pkg.hot for transitive dependencies
            let pkg_hot_path = resolved_path.join("pkg.hot");
            let transitive_deps = if pkg_hot_path.exists() {
                match PackageMetadata::parse_from_file(&pkg_hot_path) {
                    Ok(pkg_meta) => {
                        tracing::debug!(
                            "Found {} transitive deps in {}/pkg.hot",
                            pkg_meta.deps.len(),
                            resolved_path.display()
                        );

                        // Check minimum Hot version requirement
                        if let Some(ref min_version) = pkg_meta.hot_min_version {
                            if let Err(e) = crate::build_info::check_min_version(min_version) {
                                return Err(format!(
                                    "Package '{}' requires Hot {}: {}",
                                    dep.name, min_version, e
                                ));
                            }
                            tracing::debug!(
                                "Package '{}' hot-min-version {} satisfied",
                                dep.name,
                                min_version
                            );
                        }

                        pkg_meta.deps
                    }
                    Err(e) => {
                        tracing::warn!("Failed to parse pkg.hot for '{}': {}", dep.name, e);
                        vec![]
                    }
                }
            } else {
                vec![]
            };

            // Record transitive deps in graph
            let transitive_keys: Vec<String> = transitive_deps
                .iter()
                .map(|d| Self::normalize_dep_name(&d.name))
                .collect();
            dep_graph.insert(dep_key.clone(), transitive_keys);

            // Add transitive deps to queue
            for trans_dep in transitive_deps {
                let trans_key = Self::normalize_dep_name(&trans_dep.name);
                if !seen.contains(&trans_key) {
                    queue.push_back(trans_dep);
                }
            }

            resolved.insert(dep_key, resolved_dep);
        }

        // Topological sort: dependencies before dependents
        let sorted = Self::topological_sort(&dep_graph)?;

        // Build result in sorted order
        let mut result = Vec::new();
        for key in sorted {
            if let Some(dep) = resolved.remove(&key) {
                result.push(dep);
            }
        }

        Ok(result)
    }

    /// Normalize dependency name for comparison
    /// "hot.dev/aws-core" and "aws-core" should be treated as the same
    fn normalize_dep_name(name: &str) -> String {
        name.rsplit('/').next().unwrap_or(name).to_string()
    }

    /// Topological sort of dependency graph
    fn topological_sort(graph: &AHashMap<String, Vec<String>>) -> Result<Vec<String>, String> {
        let mut in_degree: AHashMap<String, usize> = AHashMap::new();
        let mut result = Vec::new();

        // Initialize in-degrees
        for node in graph.keys() {
            in_degree.entry(node.clone()).or_insert(0);
        }
        for deps in graph.values() {
            for dep in deps {
                *in_degree.entry(dep.clone()).or_insert(0) += 1;
            }
        }

        // Find nodes with no incoming edges
        let mut queue: Vec<String> = in_degree
            .iter()
            .filter(|(_, deg)| **deg == 0)
            .map(|(node, _)| node.clone())
            .collect();
        queue.sort(); // Deterministic order

        let mut visited = AHashSet::new();

        while let Some(node) = queue.pop() {
            if visited.contains(&node) {
                continue;
            }
            visited.insert(node.clone());
            result.push(node.clone());

            if let Some(deps) = graph.get(&node) {
                for dep in deps {
                    if let Some(deg) = in_degree.get_mut(dep) {
                        *deg = deg.saturating_sub(1);
                        if *deg == 0 && !visited.contains(dep) {
                            queue.push(dep.clone());
                            queue.sort();
                        }
                    }
                }
            }
        }

        // Check for cycles
        if result.len() < graph.len() {
            let missing: Vec<_> = graph.keys().filter(|k| !visited.contains(*k)).collect();
            return Err(format!(
                "Circular dependency detected involving: {:?}",
                missing
            ));
        }

        Ok(result)
    }

    /// Build project overrides map from project dependencies
    pub fn build_project_overrides(
        project_deps: &[ProjectDependency],
    ) -> AHashMap<String, DependencySpec> {
        let mut overrides = AHashMap::new();
        for dep in project_deps {
            overrides.insert(dep.name.clone(), dep.spec.clone());
            // Also add normalized name for matching
            let normalized = Self::normalize_dep_name(&dep.name);
            if normalized != dep.name {
                overrides.insert(normalized, dep.spec.clone());
            }
        }
        overrides
    }
}
