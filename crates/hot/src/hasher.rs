use blake3::Hasher as Blake3Hasher;
use rayon::prelude::*;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

/// A wrapper around blake3::Hasher that provides both std::hash::Hasher trait
/// and direct content hashing functionality
pub struct HotHasher {
    hasher: Blake3Hasher,
}

/// Cache type for distinguishing bytecode vs AST cache format versions
#[derive(Debug, Clone, Copy)]
pub enum CacheType {
    /// Bytecode cache (.bc.zst) - compiled VM instructions
    Bytecode,
    /// AST cache (.ast.zst) - parsed namespaces
    Ast,
}

impl CacheType {
    /// Get the file extension for this cache type
    pub fn extension(&self) -> &'static str {
        match self {
            CacheType::Bytecode => "bc.zst",
            CacheType::Ast => "ast.zst",
        }
    }

    /// Get the format version for this cache type
    /// These are separate because the formats evolve independently
    pub fn format_version(&self) -> u32 {
        match self {
            CacheType::Bytecode => 6, // Bumped to invalidate caches missing tool_specs/skill_specs
            CacheType::Ast => 1,      // AST cache format version
        }
    }
}

/// Builder for creating consistent cache keys across all cache types
pub struct CacheKeyBuilder {
    hasher: HotHasher,
}

impl CacheKeyBuilder {
    /// Create a new cache key builder for the specified cache type
    /// Automatically includes Hot version, git SHA, and cache format version
    pub fn new(cache_type: CacheType) -> Self {
        let mut hasher = HotHasher::new();
        // Include Hot release version (from resources/version.txt via build.rs)
        // This ensures cache invalidation when the binary version changes
        hasher.update(crate::build_info::VERSION.as_bytes());
        // Include git SHA to invalidate cache when runtime code changes
        // This catches deploys where version wasn't bumped but code changed
        hasher.update(crate::build_info::GIT_SHA.as_bytes());
        // Include cache-type-specific format version
        hasher.update(&cache_type.format_version().to_le_bytes());
        Self { hasher }
    }

    /// Add a prefix (e.g., project name or unit ID) to the cache key
    pub fn with_prefix(mut self, prefix: &str) -> Self {
        self.hasher.update(prefix.as_bytes());
        self
    }

    /// Add pre-computed file hashes to the cache key
    /// Files are sorted by path for deterministic ordering
    pub fn with_file_hashes(mut self, file_hashes: &[(String, String)]) -> Self {
        let mut sorted = file_hashes.to_vec();
        sorted.sort_by(|a, b| a.0.cmp(&b.0));
        for (path, hash) in sorted {
            self.hasher.update(path.as_bytes());
            self.hasher.update(hash.as_bytes());
        }
        self
    }

    /// Finalize and return the cache key
    pub fn finalize(self) -> String {
        self.hasher.finalize()
    }
}

/// Compute file hashes for a directory of `.hot` files in parallel.
/// Returns `Vec<(path_string, content_hash)>`.
///
/// Discovery routes through [`crate::discovery::discover`] so `.gitignore` /
/// `.hotignore` and the default hard-excludes are honored consistently with
/// the rest of the codebase.
pub fn compute_hot_file_hashes(source_dir: &Path) -> Result<Vec<(String, String)>, String> {
    let opts = crate::discovery::DiscoveryOpts::for_extension("hot");
    let files: Vec<PathBuf> = if source_dir.is_dir() {
        crate::discovery::discover(&[source_dir], &opts)
            .into_iter()
            .map(|d| d.abs_path)
            .collect()
    } else if source_dir.extension().is_some_and(|e| e == "hot") {
        vec![source_dir.to_path_buf()]
    } else {
        Vec::new()
    };

    let hashes: Vec<(String, String)> = files
        .par_iter()
        .filter_map(|path| {
            std::fs::read_to_string(path).ok().map(|content| {
                let hash = HotHasher::hash_content(content.as_bytes());
                (path.to_string_lossy().to_string(), hash)
            })
        })
        .collect();

    Ok(hashes)
}

impl HotHasher {
    /// Create a new hasher instance
    pub fn new() -> Self {
        Self {
            hasher: Blake3Hasher::new(),
        }
    }

    /// Hash content directly and return hex string
    pub fn hash_content(content: &[u8]) -> String {
        let hash = blake3::hash(content);
        hash.to_hex().to_string()
    }

    /// Hash multiple content pieces and return hex string
    pub fn hash_contents(contents: &[&[u8]]) -> String {
        let mut hasher = Blake3Hasher::new();
        for content in contents {
            hasher.update(content);
        }
        hasher.finalize().to_hex().to_string()
    }

    /// Update the hasher with new content
    pub fn update(&mut self, content: &[u8]) {
        self.hasher.update(content);
    }

    /// Finalize the hash and return as hex string
    pub fn finalize(self) -> String {
        self.hasher.finalize().to_hex().to_string()
    }

    /// Finalize the hash and return as hex string (keeping hasher alive)
    pub fn finalize_hex(&self) -> String {
        self.hasher.finalize().to_hex().to_string()
    }
}

impl Default for HotHasher {
    fn default() -> Self {
        Self::new()
    }
}

/// Implement std::hash::Hasher trait for compatibility with existing hash-based collections
impl Hasher for HotHasher {
    fn write(&mut self, bytes: &[u8]) {
        self.hasher.update(bytes);
    }

    fn finish(&self) -> u64 {
        // Convert first 8 bytes of blake3 hash to u64 for std::hash::Hasher compatibility
        let hash_bytes = self.hasher.finalize();
        let bytes = hash_bytes.as_bytes();
        u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])
    }
}

/// Convenience function to hash a single value that implements Hash trait
pub fn hash_value<T: Hash>(value: &T) -> String {
    let mut hasher = HotHasher::new();
    value.hash(&mut hasher);
    hasher.finalize()
}

/// Convenience function to hash multiple values that implement Hash trait
pub fn hash_values<T: Hash>(values: &[T]) -> String {
    let mut hasher = HotHasher::new();
    for value in values {
        value.hash(&mut hasher);
    }
    hasher.finalize()
}

/// Convenience function to compute content hash for source files
pub fn compute_source_files_hash(source_files: &[(std::path::PathBuf, String)]) -> String {
    let mut hasher = HotHasher::new();

    // Sort files by path for consistent hashing
    let mut sorted_files: Vec<_> = source_files.iter().collect();
    sorted_files.sort_by(|a, b| a.0.cmp(&b.0));

    for (path, content) in sorted_files {
        path.hash(&mut hasher);
        content.hash(&mut hasher);
    }

    hasher.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_hash_content() {
        let content = b"hello world";
        let hash = HotHasher::hash_content(content);

        // Blake3 hash should be 64 hex characters (32 bytes)
        assert_eq!(hash.len(), 64);

        // Should be consistent
        let hash2 = HotHasher::hash_content(content);
        assert_eq!(hash, hash2);
    }

    #[test]
    fn test_hash_contents() {
        let contents = vec![b"hello".as_slice(), b" ".as_slice(), b"world".as_slice()];
        let hash1 = HotHasher::hash_contents(&contents);

        // Should be the same as hashing concatenated content
        let hash2 = HotHasher::hash_content(b"hello world");
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_incremental_hashing() {
        let mut hasher = HotHasher::new();
        hasher.update(b"hello");
        hasher.update(b" ");
        hasher.update(b"world");
        let hash1 = hasher.finalize();

        let hash2 = HotHasher::hash_content(b"hello world");
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_hasher_trait() {
        let mut hasher = HotHasher::new();
        "hello world".hash(&mut hasher);
        let result = hasher.finish();

        // Should return a u64
        assert!(result > 0);
    }

    #[test]
    fn test_hash_value() {
        let value = "test string";
        let hash1 = hash_value(&value);
        let hash2 = hash_value(&value);

        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 64);
    }

    #[test]
    fn test_compute_source_files_hash() {
        use std::path::PathBuf;

        let files = vec![
            (PathBuf::from("file1.hot"), "content1".to_string()),
            (PathBuf::from("file2.hot"), "content2".to_string()),
        ];

        let hash1 = compute_source_files_hash(&files);
        let hash2 = compute_source_files_hash(&files);

        assert_eq!(hash1, hash2);
        assert_eq!(hash1.len(), 64);

        // Order shouldn't matter due to internal sorting
        let files_reversed = vec![
            (PathBuf::from("file2.hot"), "content2".to_string()),
            (PathBuf::from("file1.hot"), "content1".to_string()),
        ];

        let hash3 = compute_source_files_hash(&files_reversed);
        assert_eq!(hash1, hash3);
    }
}
