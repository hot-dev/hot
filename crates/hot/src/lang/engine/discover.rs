//! File and source discovery + parse caching for the engine.
//!
//! Discovery and parsing live in `hot_lang::lang::discover`, so a checker
//! without the runtime finds and parses a project the same way. This module
//! binds them to `hot`'s on-disk unit cache and keeps the `Engine` entry
//! points that walk the filesystem for `.hot` files.

use super::Engine;
use indexmap::IndexMap;

pub use hot_lang::lang::discover::{
    DiscoveredUnit, ParsedFile, discover_compilation_units, discover_compilation_units_with,
    parse_files_parallel,
};
pub use hot_lang::lang::merge::merge_program_namespaces_strict;
pub(crate) use hot_lang::lang::merge::{NsMergePolicy, merge_namespace};

/// `hot`'s on-disk unit cache.
pub fn unit_cache() -> crate::lang::cache::unit_cache::UnitCache {
    // Project sources stay project-local; package ASTs are machine-scoped so
    // one parsed copy of a dependency serves every project on the machine.
    crate::lang::cache::unit_cache::UnitCache::with_package_dir(
        crate::lang::cache::unit_cache::UnitCache::default_cache_dir(),
        crate::lang::cache::paths::get_package_unit_cache_dir(),
    )
}

/// Parse compilation units through `hot`'s on-disk unit cache.
/// Returns merged namespaces from all units.
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
    hot_lang::lang::discover::parse_units_with_cache(units, color, Some(&unit_cache()))
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
        hot_lang::lang::discover::discover_hot_files(path)
    }

    /// Discover .hot files from a dependency's src_paths only (not test_paths)
    pub fn discover_dependency_source_files(
        pkg_root: &std::path::Path,
    ) -> Result<Vec<String>, String> {
        hot_lang::lang::discover::discover_dependency_source_files(pkg_root)
    }

    /// Return the resolved src-path directories for a package (from its pkg.hot).
    /// Unlike `discover_dependency_source_files`, this returns directory paths
    /// rather than individual files, for use by the bundler.
    pub fn discover_dependency_source_dirs(
        pkg_root: &std::path::Path,
    ) -> Result<Vec<String>, String> {
        hot_lang::lang::discover::discover_dependency_source_dirs(pkg_root)
    }
}

#[cfg(test)]
mod tests {
    use super::Engine;

    /// `pkg.hot` files are read as literal data (`hot_lang::lang::data`), not
    /// run on the VM. Every package in the repo must read the same both ways.
    #[test]
    fn pkg_hot_reads_the_same_as_on_the_vm() {
        let pkg_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../hot/pkg");
        let mut checked = 0;
        for entry in std::fs::read_dir(&pkg_root).unwrap() {
            let pkg_hot = entry.unwrap().path().join("pkg.hot");
            let Ok(content) = std::fs::read_to_string(&pkg_hot) else {
                continue;
            };
            assert_eq!(
                hot_lang::lang::data::eval_data(&content).unwrap(),
                Engine::eval_simple(&content).unwrap(),
                "{}",
                pkg_hot.display()
            );
            checked += 1;
        }
        assert!(checked > 10, "only {checked} packages found");
    }
}
