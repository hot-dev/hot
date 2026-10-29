//! hot-std, the package every Hot program compiles against.
//!
//! Its signatures are Hot source, not Rust data, so a check needs hot-std's
//! `.hot` files, and they must come from the same commit as the compiler: a
//! hot-std that calls a native this compiler doesn't know, or misses one it
//! expects, fails to compile. With the `embed-std` feature (on by default)
//! this crate carries `pkg.hot` and `src/` from its own commit.

use std::path::{Path, PathBuf};

/// Which hot-std a check compiles against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotStd {
    /// The copy embedded in this crate, unpacked once into the machine cache.
    /// `$HOT_HOME/pkg/hot-std` still wins when HOT_HOME is set.
    Embedded,
    /// `hot`'s lookup: `$HOT_HOME/pkg/hot-std`, `./hot/pkg/hot-std`, next to
    /// the executable, then the installed copies under `/usr/local/share` and
    /// `/usr/share`, then the embedded copy.
    Lookup,
    /// A hot-std directory (the one holding `pkg.hot`).
    Dir(PathBuf),
}

impl Default for HotStd {
    /// The embedded copy when this crate has one, else `hot`'s lookup.
    fn default() -> Self {
        if cfg!(feature = "embed-std") {
            HotStd::Embedded
        } else {
            HotStd::Lookup
        }
    }
}

impl HotStd {
    /// The hot-std directory this choice names.
    pub fn dir(&self) -> Result<PathBuf, String> {
        match self {
            HotStd::Embedded => match hot_home_std() {
                Some(dir) => Ok(dir),
                None => embedded_dir(),
            },
            HotStd::Lookup => Ok(crate::lang::project::DependencyResolver::new_default()
                .get_hot_std_dependency()
                .resolved_path),
            HotStd::Dir(dir) => Ok(dir.clone()),
        }
    }
}

fn hot_home_std() -> Option<PathBuf> {
    let home = std::env::var("HOT_HOME").ok()?;
    let dir = PathBuf::from(home).join("pkg").join("hot-std");
    dir.exists().then_some(dir)
}

/// hot-std's files embedded at build time: `(path inside hot-std, content)`.
/// Empty without the `embed-std` feature.
pub const EMBEDDED_FILES: &[(&str, &str)] = include!(concat!(env!("OUT_DIR"), "/hot_std_files.rs"));

/// Fingerprint of [`EMBEDDED_FILES`], which names the unpacked copy.
pub const EMBEDDED_FINGERPRINT: &str = env!("HOT_STD_FINGERPRINT");

/// Unpack the embedded hot-std into the machine cache, once per fingerprint,
/// and return its directory. Compiling reads files (diagnostics point at real
/// paths, and package discovery walks directories), so the embedded copy
/// lives on disk like any other package.
pub fn embedded_dir() -> Result<PathBuf, String> {
    if EMBEDDED_FILES.is_empty() {
        return Err(
            "this build of hot_lang has no embedded hot-std (the `embed-std` feature is off)"
                .to_string(),
        );
    }
    unpack_embedded(
        &crate::lang::cache::paths::get_embedded_std_dir().join(&EMBEDDED_FINGERPRINT[..16]),
    )
}

/// Unpack the embedded hot-std into `root/hot-std` unless it's already there.
fn unpack_embedded(root: &Path) -> Result<PathBuf, String> {
    let dir = root.join("hot-std");
    if dir.join("pkg.hot").is_file() {
        return Ok(dir);
    }

    // Write a private copy, then rename it into place so a concurrent check
    // never sees half a package. Whoever renames first wins; the other
    // copies are dropped.
    std::fs::create_dir_all(root)
        .map_err(|e| format!("Failed to create {}: {}", root.display(), e))?;
    let staging = root.join(format!("hot-std.{}.tmp", std::process::id()));
    let _ = std::fs::remove_dir_all(&staging);
    write_files(&staging)?;
    if std::fs::rename(&staging, &dir).is_err() {
        let _ = std::fs::remove_dir_all(&staging);
        if !dir.join("pkg.hot").is_file() {
            return Err(format!("Failed to unpack hot-std into {}", dir.display()));
        }
    }
    Ok(dir)
}

fn write_files(dir: &Path) -> Result<(), String> {
    for (relative, content) in EMBEDDED_FILES {
        let path = dir.join(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create {}: {}", parent.display(), e))?;
        }
        std::fs::write(&path, content)
            .map_err(|e| format!("Failed to write {}: {}", path.display(), e))?;
    }
    Ok(())
}

#[cfg(all(test, feature = "embed-std"))]
mod tests {
    use super::*;

    #[test]
    fn the_embedded_copy_is_this_commits_hot_std() {
        let std_dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hot/pkg/hot-std");
        assert!(EMBEDDED_FILES.iter().any(|(path, _)| *path == "pkg.hot"));
        for (relative, content) in EMBEDDED_FILES {
            let on_disk = std::fs::read_to_string(std_dir.join(relative)).unwrap();
            assert_eq!(*content, on_disk, "{relative} differs from the checkout");
        }
    }

    #[test]
    fn unpacking_writes_hot_std_once() {
        let root = tempfile::tempdir().unwrap();
        let dir = unpack_embedded(root.path()).unwrap();
        assert_eq!(dir, root.path().join("hot-std"));
        for (relative, content) in EMBEDDED_FILES {
            assert_eq!(
                std::fs::read_to_string(dir.join(relative)).unwrap(),
                *content
            );
        }

        // A second call finds the copy and leaves no staging directory behind.
        assert_eq!(unpack_embedded(root.path()).unwrap(), dir);
        let entries: Vec<_> = std::fs::read_dir(root.path()).unwrap().collect();
        assert_eq!(entries.len(), 1);
    }
}
