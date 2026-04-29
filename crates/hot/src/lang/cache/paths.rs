//! Cache Path Resolution
//!
//! This module provides platform-aware cache directory resolution for Hot.
//! It determines the appropriate cache location based on:
//!
//! 1. `HOT_HOME` environment variable (highest priority)
//! 2. Project-local `.hot/` directory (when `hot.hot` config exists)
//! 3. System/user cache directory (for one-off script execution)
//!
//! ## System Cache Locations
//!
//! - **Linux**: `$XDG_CACHE_HOME/hot` or `~/.cache/hot`
//! - **macOS**: `~/Library/Caches/hot`
//! - **Windows**: `%LOCALAPPDATA%\hot\cache`
//!
//! ## Cache Structure
//!
//! All caches are organized under `.hot/cache/`:
//! ```text
//! .hot/cache/
//!   bytecode/   - Compiled bytecode for projects
//!   unit/       - Per-unit parsed AST cache
//!   cdn/        - Packages downloaded from pkg.hot.dev
//!   git/        - Cloned git dependencies
//!   docs/       - Extracted documentation (hot_app only)
//! ```

use std::path::PathBuf;

/// Check if a project configuration file or directory exists in the current directory.
/// This returns true if either:
/// - `hot.hot` config file exists, OR
/// - `.hot/` directory exists (indicating a project that was initialized)
///
/// This ensures that after `hot init` creates `.hot/`, subsequent operations
/// use project-local paths rather than system cache.
pub fn has_project_config() -> bool {
    std::path::Path::new("hot.hot").exists() || std::path::Path::new(".hot").exists()
}

/// Get the base cache directory for Hot.
///
/// Resolution order:
/// 1. `HOT_HOME` environment variable → `$HOT_HOME`
/// 2. Project config exists (`hot.hot`) → `./.hot` (project-local)
/// 3. Otherwise → system cache directory (platform-specific)
///
/// This prevents creating `.hot/` directories in random locations when
/// users run one-off scripts without a project configuration.
pub fn get_cache_base_dir() -> PathBuf {
    // 1. HOT_HOME takes highest priority
    if let Ok(hot_home) = std::env::var("HOT_HOME") {
        return PathBuf::from(hot_home);
    }

    // 2. If project config exists, use project-local cache
    if has_project_config() {
        return PathBuf::from(".hot");
    }

    // 3. Use system cache directory
    get_system_cache_dir()
}

/// Get the system-level cache directory (user-specific, not project-specific).
///
/// Platform-specific locations:
/// - **Linux**: `$XDG_CACHE_HOME/hot` or `~/.cache/hot`
/// - **macOS**: `~/Library/Caches/hot`
/// - **Windows**: `%LOCALAPPDATA%\hot\cache`
///
/// Falls back to `~/.hot` if platform detection fails.
pub fn get_system_cache_dir() -> PathBuf {
    // Try platform-specific cache directories
    if let Some(cache_dir) = get_platform_cache_dir() {
        return cache_dir;
    }

    // Fallback: ~/.hot
    if let Some(home) = get_home_dir() {
        return home.join(".hot");
    }

    // Last resort: use current directory (shouldn't happen in practice)
    tracing::warn!("Could not determine system cache directory, using .hot");
    PathBuf::from(".hot")
}

/// Get the platform-specific cache directory
#[cfg(target_os = "linux")]
fn get_platform_cache_dir() -> Option<PathBuf> {
    // Follow XDG Base Directory Specification
    // https://specifications.freedesktop.org/basedir-spec/basedir-spec-latest.html
    if let Ok(xdg_cache) = std::env::var("XDG_CACHE_HOME") {
        return Some(PathBuf::from(xdg_cache).join("hot"));
    }

    // Default: ~/.cache/hot
    get_home_dir().map(|home| home.join(".cache").join("hot"))
}

#[cfg(target_os = "macos")]
fn get_platform_cache_dir() -> Option<PathBuf> {
    // macOS standard cache location: ~/Library/Caches
    get_home_dir().map(|home| home.join("Library").join("Caches").join("hot"))
}

#[cfg(target_os = "windows")]
fn get_platform_cache_dir() -> Option<PathBuf> {
    // Windows: %LOCALAPPDATA%\hot\cache
    if let Ok(local_app_data) = std::env::var("LOCALAPPDATA") {
        return Some(PathBuf::from(local_app_data).join("hot").join("cache"));
    }

    // Fallback for Windows: %USERPROFILE%\.hot
    get_home_dir().map(|home| home.join(".hot"))
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
fn get_platform_cache_dir() -> Option<PathBuf> {
    // Other Unix-like systems: use XDG or ~/.cache/hot
    if let Ok(xdg_cache) = std::env::var("XDG_CACHE_HOME") {
        return Some(PathBuf::from(xdg_cache).join("hot"));
    }

    get_home_dir().map(|home| home.join(".cache").join("hot"))
}

/// Get the user's home directory
fn get_home_dir() -> Option<PathBuf> {
    // Try HOME first (Unix/macOS, also works on Windows with some setups)
    if let Ok(home) = std::env::var("HOME") {
        return Some(PathBuf::from(home));
    }

    // Windows-specific: USERPROFILE
    #[cfg(target_os = "windows")]
    if let Ok(userprofile) = std::env::var("USERPROFILE") {
        return Some(PathBuf::from(userprofile));
    }

    None
}

// ============================================================================
// Specific cache directory getters
// All caches are now organized under .hot/cache/
// ============================================================================

/// Get the root cache directory (.hot/cache/).
/// All specific caches are subdirectories of this.
pub fn get_cache_root_dir() -> PathBuf {
    get_cache_base_dir().join("cache")
}

/// Get the bytecode cache directory (.hot/cache/bytecode/).
/// Used by `BytecodeCache` for compiled bytecode storage.
pub fn get_bytecode_cache_dir() -> PathBuf {
    get_cache_root_dir().join("bytecode")
}

/// Get the unit cache directory (.hot/cache/unit/).
/// Used by `UnitCache` for per-unit (package/source) parsed AST storage.
pub fn get_unit_cache_dir() -> PathBuf {
    get_cache_root_dir().join("unit")
}

/// Get the git dependency cache directory (.hot/cache/git/).
/// Used by `DependencyResolver` for cloned git repositories.
pub fn get_git_cache_dir() -> PathBuf {
    get_cache_root_dir().join("git")
}

/// Get the CDN package cache directory (.hot/cache/cdn/).
/// Used by `DependencyResolver` for packages downloaded from pkg.hot.dev.
pub fn get_cdn_cache_dir() -> PathBuf {
    get_cache_root_dir().join("cdn")
}

/// Get the docs cache directory (.hot/cache/docs/).
/// Used by `hot_app` for extracted build documentation.
pub fn get_docs_cache_dir() -> PathBuf {
    get_cache_root_dir().join("docs")
}

/// Get the database directory.
/// Used by SQLite database when no explicit db.uri is configured.
///
/// Unlike cache directories, the database is ALWAYS project-local (`.hot/db/`).
/// Database contains project-specific state (orgs, users, builds, events) that
/// should never be shared across projects or stored at system level.
///
/// If `HOT_HOME` is set, uses `$HOT_HOME/db/` instead.
pub fn get_db_dir() -> PathBuf {
    // HOT_HOME takes priority for all hot data including db
    if let Ok(hot_home) = std::env::var("HOT_HOME") {
        return PathBuf::from(hot_home).join("db");
    }

    // Always use project-local .hot/db/ - never system cache
    PathBuf::from(".hot/db")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hot_home_takes_priority() {
        // Save current env
        let original_hot_home = std::env::var("HOT_HOME").ok();

        // SAFETY: This test runs in isolation and we restore the value afterward.
        // Environment variable modification is unsafe due to potential data races,
        // but in a single-threaded test context this is acceptable.
        unsafe {
            std::env::set_var("HOT_HOME", "/custom/hot/home");
        }

        let base = get_cache_base_dir();
        assert_eq!(base, PathBuf::from("/custom/hot/home"));

        // Restore original env
        // SAFETY: Same as above - restoring env var in test context
        unsafe {
            if let Some(val) = original_hot_home {
                std::env::set_var("HOT_HOME", val);
            } else {
                std::env::remove_var("HOT_HOME");
            }
        }
    }

    #[test]
    fn test_system_cache_returns_valid_path() {
        let cache_dir = get_system_cache_dir();
        // Should return some path, not be empty
        assert!(!cache_dir.as_os_str().is_empty());

        // Should contain "hot" somewhere in the path
        assert!(
            cache_dir.to_string_lossy().contains("hot"),
            "Cache dir should contain 'hot': {:?}",
            cache_dir
        );
    }

    #[test]
    fn test_cache_subdirs() {
        // Note: These tests depend on current environment state
        // If HOT_HOME is set, they'll use that; otherwise project-local or system cache
        let bytecode_dir = get_bytecode_cache_dir();
        let unit_cache_dir = get_unit_cache_dir();
        let git_cache_dir = get_git_cache_dir();
        let cdn_cache_dir = get_cdn_cache_dir();
        let docs_cache_dir = get_docs_cache_dir();

        // All caches should be under cache/
        assert!(bytecode_dir.ends_with("cache/bytecode"));
        assert!(unit_cache_dir.ends_with("cache/unit"));
        assert!(git_cache_dir.ends_with("cache/git"));
        assert!(cdn_cache_dir.ends_with("cache/cdn"));
        assert!(docs_cache_dir.ends_with("cache/docs"));
    }
}
