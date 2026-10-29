//! Compilation units: the packages and source paths a program is parsed from,
//! and the store their parsed namespaces can be cached in.

use crate::lang::ast::{Namespace, NsPath};
use indexmap::IndexMap;
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
        let digest = blake3::hash(path.to_string_lossy().as_bytes()).to_hex();
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

/// Where parsed units can be cached between runs. `hot` keeps them on disk
/// (`hot::lang::cache::unit_cache::UnitCache`); without a store every unit is
/// parsed each time, which gives the same program.
pub trait UnitStore: Sync {
    /// The unit's namespaces, if a valid entry exists for its current sources.
    fn load(&self, unit: &CompilationUnit) -> Result<Option<IndexMap<NsPath, Namespace>>, String>;

    /// Store the unit's freshly parsed namespaces.
    fn save(
        &self,
        unit: &CompilationUnit,
        namespaces: &IndexMap<NsPath, Namespace>,
    ) -> Result<(), String>;
}
