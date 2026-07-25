//! Precompiled hot-std image, populated on first run.
//!
//! hot-std is immutable per install, yet every CLI invocation used to parse
//! and compile all ~50 of its files before touching user code. This module
//! maintains a compiled hot-std image in the **system-level** cache: the
//! first run that needs it compiles hot-std once and persists the image;
//! every later run (from any directory, any project) loads it instead of
//! recompiling.
//!
//! ## Location
//!
//! Always system-level (`$HOT_HOME/cache/std/` when HOT_HOME is set,
//! otherwise the platform cache dir, e.g. `~/Library/Caches/hot/cache/std/`)
//! — never the cwd-dependent project cache. hot-std is per-install: one
//! image serves every project, and a per-project copy would recompile and
//! duplicate ~1.5MB per project for nothing.
//!
//! ## File format (`std-<full-key>.hsc`)
//!
//! ```text
//! [0..8)   magic  b"HOTSTDA1"
//! [8..]    postcard header { artifact_key }
//!          followed by the zstd-compressed bytecode-cache payload
//!          (bytecode_cache::CacheFilePayload encoding)
//! ```
//!
//! `artifact_key` is a [`CacheKeyBuilder`] hash covering the Hot VERSION,
//! GIT_SHA, build fingerprint, bytecode format version, and the blake3 hashes of every hot-std
//! source file (**root-relative** paths — the image must survive any install
//! prefix). The key is both the filename discriminator and revalidated from
//! the header after load. Any mismatch — runtime upgrade, modified hot-std,
//! dev override — misses and triggers a fresh first-run compile; superseded
//! images are swept by the stale-cache pruner.
//!
//! The hidden `hot build-std-artifact` command prewarms the image (Docker
//! layers, CI runners) using the same code path.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::LazyLock;

use crate::hasher::{CacheKeyBuilder, CacheType, compute_hot_file_hashes};
use parking_lot::Mutex;

use super::bytecode_cache::{
    CacheMetadata, CachedBytecode, decode_cache_payload, encode_cache_payload,
    validate_cache_metadata,
};

const MAGIC: &[u8; 8] = b"HOTSTDA1";

#[derive(serde::Serialize, serde::Deserialize)]
struct ArtifactHeader {
    /// CacheKeyBuilder hash over VERSION + GIT_SHA + build fingerprint +
    /// bytecode format version + root-relative hot-std source file hashes.
    artifact_key: String,
}

/// System-level directory holding compiled hot-std images.
pub fn std_cache_dir() -> PathBuf {
    if let Ok(hot_home) = std::env::var("HOT_HOME") {
        return PathBuf::from(hot_home).join("cache").join("std");
    }
    super::paths::get_system_cache_dir()
        .join("cache")
        .join("std")
}

fn image_path(cache_dir: &Path, artifact_key: &str) -> PathBuf {
    cache_dir.join(format!("std-{}.hsc", artifact_key))
}

/// Compute the expected artifact key for the hot-std sources at `std_root`.
///
/// File paths are relativized to `std_root` before hashing: the image is
/// validated at whatever prefix hot-std is installed to, so absolute paths
/// must never influence the key.
fn compute_artifact_key(std_root: &Path) -> Result<String, String> {
    let root = std_root
        .canonicalize()
        .unwrap_or_else(|_| std_root.to_path_buf());
    let file_hashes: Vec<(String, String)> = compute_hot_file_hashes(std_root)?
        .into_iter()
        .map(|(path, hash)| {
            let rel = Path::new(&path)
                .strip_prefix(&root)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or(path);
            (rel, hash)
        })
        .collect();
    if file_hashes.is_empty() {
        return Err(format!(
            "no hot-std sources found at {}",
            std_root.display()
        ));
    }
    Ok(CacheKeyBuilder::new(CacheType::Bytecode)
        .with_prefix("std-artifact")
        .with_file_hashes(&file_hashes)
        .finalize())
}

struct KeyedSlot<T> {
    loaded: Mutex<Option<(String, Arc<T>)>>,
}

impl<T> Default for KeyedSlot<T> {
    fn default() -> Self {
        Self {
            loaded: Mutex::new(None),
        }
    }
}

impl<T> KeyedSlot<T> {
    fn get_or_try_build(
        &self,
        key: &str,
        build: impl FnOnce() -> Option<Arc<T>>,
    ) -> Option<Arc<T>> {
        let mut loaded = self.loaded.lock();
        if let Some((loaded_key, value)) = loaded.as_ref()
            && loaded_key == key
        {
            return Some(Arc::clone(value));
        }

        // A different key invalidates the old image before attempting the
        // replacement. Failed builds remain retryable.
        *loaded = None;
        let value = build()?;
        *loaded = Some((key.to_string(), Arc::clone(&value)));
        Some(value)
    }

    fn clear(&self) {
        *self.loaded.lock() = None;
    }
}

/// Per-process image slot keyed by the current source/build fingerprint.
static LOADED: LazyLock<KeyedSlot<CachedBytecode>> = LazyLock::new(KeyedSlot::default);

/// Get the compiled hot-std image for the hot-std package at `std_root`:
/// load it from the system cache when present and valid, otherwise compile
/// hot-std now, persist the image (best-effort), and return the fresh build.
///
/// Returns `None` only if hot-std cannot be compiled at all — callers fall
/// back to the classic combined-compile path, which will report the error
/// with full diagnostics.
pub fn get_or_build(std_root: &Path) -> Option<Arc<CachedBytecode>> {
    let artifact_key = match compute_artifact_key(std_root) {
        Ok(key) => key,
        Err(reason) => {
            LOADED.clear();
            tracing::debug!("hot-std image unavailable: {}", reason);
            return None;
        }
    };
    LOADED.get_or_try_build(&artifact_key, || {
        load_or_build(std_root, &std_cache_dir(), &artifact_key)
    })
}

#[cfg(test)]
fn get_or_build_in(std_root: &Path, cache_dir: &Path) -> Option<Arc<CachedBytecode>> {
    let artifact_key = match compute_artifact_key(std_root) {
        Ok(key) => key,
        Err(reason) => {
            tracing::debug!("hot-std image unavailable: {}", reason);
            return None;
        }
    };
    load_or_build(std_root, cache_dir, &artifact_key)
}

fn load_or_build(
    std_root: &Path,
    cache_dir: &Path,
    artifact_key: &str,
) -> Option<Arc<CachedBytecode>> {
    let path = image_path(cache_dir, artifact_key);
    if path.exists() {
        match try_load(&path, artifact_key) {
            Ok(cached) => return Some(cached),
            Err(reason) => {
                tracing::debug!("hot-std image at {} rejected: {}", path.display(), reason);
                remove_rejected_canonical_image(&path, &reason);
            }
        }
    }

    // First run (or stale/corrupt image): compile hot-std now and persist.
    match compile_image(std_root, artifact_key) {
        Ok((file_bytes, cached)) => {
            // Runtime use is allowed to proceed when the cache directory is
            // read-only; a later process can retry persistence.
            if let Err(error) = persist(&path, &file_bytes, PrunePolicy::ManagedCanonical) {
                tracing::debug!("hot-std image not persisted: {}", error);
            }
            Some(cached)
        }
        Err(reason) => {
            tracing::debug!("hot-std image build failed: {}", reason);
            None
        }
    }
}

/// Compile and encode hot-std without performing filesystem persistence.
fn compile_image(
    std_root: &Path,
    artifact_key: &str,
) -> Result<(Vec<u8>, Arc<CachedBytecode>), String> {
    let src_dir = std_root.join("src");
    let compile_root = if src_dir.is_dir() {
        src_dir
    } else {
        std_root.to_path_buf()
    };

    let (_, artifacts) = crate::lang::engine::Engine::compile_project_for_cache(
        &[compile_root.to_string_lossy().to_string()],
        None,
        Some("hot-std"),
        None,
        None,
        None,
        None,
        None,
        None,
    )?;

    let metadata = CacheMetadata {
        project_name: "hot-std".to_string(),
        hot_version: crate::build_info::VERSION.to_string(),
        git_sha: crate::build_info::GIT_SHA.to_string(),
        cache_format_version: CacheType::Bytecode.format_version(),
        created_at: chrono::Utc::now().timestamp(),
        file_hashes: Vec::new(),
        cache_key: artifact_key.to_string(),
    };

    let spec_compiler = crate::lang::compiler::Compiler::new();
    let tool_specs = spec_compiler.build_tool_specs(&artifacts.ast_program);
    let skill_specs = spec_compiler.build_skill_specs(&artifacts.ast_program);
    let (compressed_payload, cached) = encode_cache_payload(
        metadata,
        &artifacts.program,
        &artifacts.function_mapping,
        &artifacts.core_functions,
        &artifacts.type_implementations,
        &artifacts.ast_program,
        &artifacts.hot_ast,
        &tool_specs,
        &skill_specs,
    )?;

    let header = postcard::to_allocvec(&ArtifactHeader {
        artifact_key: artifact_key.to_string(),
    })
    .map_err(|e| format!("Failed to encode image header: {}", e))?;

    let mut file_bytes = Vec::with_capacity(MAGIC.len() + header.len() + compressed_payload.len());
    file_bytes.extend_from_slice(MAGIC);
    file_bytes.extend_from_slice(&header);
    file_bytes.extend_from_slice(&compressed_payload);

    Ok((file_bytes, cached))
}

#[derive(Clone, Copy)]
enum PrunePolicy {
    ManagedCanonical,
    Never,
}

#[cfg(not(windows))]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    // POSIX rename atomically replaces an existing destination.
    std::fs::rename(from, to)
}

#[cfg(windows)]
fn replace_file(from: &Path, to: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;

    #[link(name = "kernel32")]
    unsafe extern "system" {
        fn MoveFileExW(
            existing_file_name: *const u16,
            new_file_name: *const u16,
            flags: u32,
        ) -> i32;
    }

    const MOVEFILE_REPLACE_EXISTING: u32 = 0x1;
    const MOVEFILE_WRITE_THROUGH: u32 = 0x8;

    let from: Vec<u16> = from.as_os_str().encode_wide().chain(Some(0)).collect();
    let to: Vec<u16> = to.as_os_str().encode_wide().chain(Some(0)).collect();
    // SAFETY: both pointers refer to NUL-terminated UTF-16 buffers that remain
    // alive for the duration of this synchronous Win32 call.
    let result = unsafe {
        MoveFileExW(
            from.as_ptr(),
            to.as_ptr(),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    };
    if result == 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

fn persist(path: &Path, file_bytes: &[u8], prune: PrunePolicy) -> Result<(), String> {
    if file_bytes.is_empty() {
        return Err("refusing to persist an empty hot-std image".to_string());
    }
    let dir = path
        .parent()
        .ok_or_else(|| "image path has no parent".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let unique = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or(0);
    let tmp = path.with_extension(format!("hsc.tmp.{}.{}", std::process::id(), unique));
    std::fs::write(&tmp, file_bytes).map_err(|e| e.to_string())?;
    replace_file(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })?;
    if matches!(prune, PrunePolicy::ManagedCanonical) {
        super::prune_stale_cache_files(dir, path);
    }
    Ok(())
}

fn persist_prewarmed_image(
    path: &Path,
    file_bytes: &[u8],
    prune: PrunePolicy,
) -> Result<u64, String> {
    persist(path, file_bytes, prune)?;
    let size = std::fs::metadata(path)
        .map_err(|e| format!("failed to verify persisted image: {}", e))?
        .len();
    if size == 0 {
        super::remove_invalid_cache_entry(path);
        return Err("persisted hot-std image is empty".to_string());
    }
    Ok(size)
}

#[derive(Debug)]
enum ArtifactLoadError {
    Read(String),
    Invalid(String),
}

impl std::fmt::Display for ArtifactLoadError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ArtifactLoadError::Read(reason) | ArtifactLoadError::Invalid(reason) => {
                formatter.write_str(reason)
            }
        }
    }
}

fn remove_rejected_canonical_image(path: &Path, error: &ArtifactLoadError) {
    if matches!(error, ArtifactLoadError::Invalid(_)) {
        super::remove_invalid_cache_entry(path);
    }
}

fn try_load(path: &Path, expected_key: &str) -> Result<Arc<CachedBytecode>, ArtifactLoadError> {
    let invalid = |reason: String| ArtifactLoadError::Invalid(reason);
    let bytes =
        std::fs::read(path).map_err(|e| ArtifactLoadError::Read(format!("read failed: {}", e)))?;
    let rest = bytes
        .strip_prefix(MAGIC.as_slice())
        .ok_or_else(|| invalid("bad magic".to_string()))?;

    let (header, payload): (ArtifactHeader, &[u8]) =
        postcard::take_from_bytes(rest).map_err(|e| invalid(format!("bad header: {}", e)))?;

    if header.artifact_key != expected_key {
        return Err(invalid("key mismatch — stale or foreign image".to_string()));
    }

    let decompressed =
        zstd::decode_all(payload).map_err(|e| invalid(format!("decompress failed: {}", e)))?;
    let cached = decode_cache_payload(&decompressed).map_err(invalid)?;
    validate_cache_metadata(&cached.metadata, expected_key).map_err(invalid)?;

    // Only a completely decoded and metadata-validated image is live.
    super::touch_cache_entry(path);
    tracing::debug!(
        "hot-std image loaded from {} ({} functions, {} namespaces)",
        path.display(),
        cached.function_mapping.len(),
        cached.ast_program.namespaces.len()
    );
    Ok(cached)
}

/// Prewarm entry point for the hidden `hot build-std-artifact` command.
/// Builds (or refreshes) the image for the hot-std at `std_root`; `out`
/// overrides the destination file.
pub fn prewarm(std_root: &Path, out: Option<&Path>) -> Result<(PathBuf, u64), String> {
    let artifact_key = compute_artifact_key(std_root)?;
    let (path, prune) = match out {
        Some(path) => (path.to_path_buf(), PrunePolicy::Never),
        None => (
            image_path(&std_cache_dir(), &artifact_key),
            PrunePolicy::ManagedCanonical,
        ),
    };
    let (file_bytes, _) = compile_image(std_root, &artifact_key)?;
    let size = persist_prewarmed_image(&path, &file_bytes, prune)?;
    Ok((path, size))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    const ARTIFACT_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    fn encoded_test_image(header_key: &str, metadata_key: &str) -> Vec<u8> {
        let ast_program = crate::lang::ast::Program {
            namespaces: indexmap::IndexMap::new(),
            current_namespace: crate::lang::ast::NsPath::new(),
        };
        let hot_ast = crate::lang::ast::HotAst::new();
        let metadata = CacheMetadata {
            project_name: "hot-std".to_string(),
            hot_version: crate::build_info::VERSION.to_string(),
            git_sha: crate::build_info::GIT_SHA.to_string(),
            cache_format_version: CacheType::Bytecode.format_version(),
            created_at: 0,
            file_hashes: Vec::new(),
            cache_key: metadata_key.to_string(),
        };
        let (payload, _) = encode_cache_payload(
            metadata,
            &crate::lang::bytecode::BytecodeProgram::new(),
            &indexmap::IndexMap::new(),
            &indexmap::IndexMap::new(),
            &indexmap::IndexMap::new(),
            &ast_program,
            &hot_ast,
            &Default::default(),
            &Default::default(),
        )
        .unwrap();
        let header = postcard::to_allocvec(&ArtifactHeader {
            artifact_key: header_key.to_string(),
        })
        .unwrap();
        [MAGIC.as_slice(), header.as_slice(), payload.as_slice()].concat()
    }

    #[test]
    fn keyed_slot_invalidates_and_does_not_memoize_failures() {
        let slot = KeyedSlot::<usize>::default();
        let builds = Cell::new(0);
        let first = slot
            .get_or_try_build("first", || {
                builds.set(builds.get() + 1);
                Some(Arc::new(1))
            })
            .unwrap();
        let reused = slot
            .get_or_try_build("first", || {
                builds.set(builds.get() + 1);
                Some(Arc::new(2))
            })
            .unwrap();
        assert!(Arc::ptr_eq(&first, &reused));
        assert_eq!(builds.get(), 1);

        let second = slot
            .get_or_try_build("second", || {
                builds.set(builds.get() + 1);
                Some(Arc::new(2))
            })
            .unwrap();
        assert_eq!(*second, 2);
        assert_eq!(builds.get(), 2);

        assert!(slot.get_or_try_build("third", || None).is_none());
        let retried = slot
            .get_or_try_build("third", || {
                builds.set(builds.get() + 1);
                Some(Arc::new(3))
            })
            .unwrap();
        assert_eq!(*retried, 3);
        assert_eq!(builds.get(), 3);
    }

    #[test]
    fn load_validates_payload_key_and_removes_rejected_canonical_image() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = image_path(dir.path(), ARTIFACT_KEY);
        std::fs::write(
            &path,
            encoded_test_image(ARTIFACT_KEY, "different-payload-key"),
        )
        .unwrap();

        let error = try_load(&path, ARTIFACT_KEY).expect_err("payload key must be checked");
        assert!(matches!(error, ArtifactLoadError::Invalid(_)));
        remove_rejected_canonical_image(&path, &error);
        assert!(!path.exists(), "rejected canonical image must be removed");
    }

    #[test]
    fn strict_prewarm_rejects_empty_or_unwritable_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let empty_path = dir.path().join("empty.hsc");
        assert!(persist_prewarmed_image(&empty_path, &[], PrunePolicy::Never).is_err());
        assert!(!empty_path.exists());

        let parent_file = dir.path().join("not-a-directory");
        std::fs::write(&parent_file, b"x").unwrap();
        let blocked_path = parent_file.join("artifact.hsc");
        assert!(persist_prewarmed_image(&blocked_path, b"image", PrunePolicy::Never).is_err());
        assert!(!blocked_path.exists());
    }

    #[test]
    fn persistence_atomically_replaces_an_existing_image() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("artifact.hsc");
        std::fs::write(&path, b"old image").unwrap();

        persist(&path, b"new image", PrunePolicy::Never).unwrap();

        assert_eq!(std::fs::read(&path).unwrap(), b"new image");
        assert!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .flatten()
                .all(|entry| !entry.file_name().to_string_lossy().contains(".tmp.")),
            "successful replacement must not leave a temp file"
        );
    }

    #[test]
    fn custom_output_persistence_never_prunes_parent() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old_managed = dir.path().join("pkg-old-0123456789abcdef.ast.zst");
        std::fs::write(&old_managed, b"old").unwrap();
        filetime::set_file_mtime(
            &old_managed,
            filetime::FileTime::from_system_time(
                std::time::SystemTime::now() - std::time::Duration::from_secs(90 * 24 * 3600),
            ),
        )
        .unwrap();

        let custom = dir.path().join("custom-output.bin");
        persist(&custom, b"image", PrunePolicy::Never).unwrap();
        assert!(custom.exists());
        assert!(
            old_managed.exists(),
            "custom --out must not prune its parent directory"
        );
    }

    /// First-run build + reload round-trip against a relocatable hot-std
    /// copy, exercising key validation, relocation, and staleness. Ignored
    /// by default: needs an installed hot-std and does a full compile.
    #[test]
    #[ignore]
    fn image_roundtrip_relocation_and_staleness() {
        let installed = Path::new("/usr/local/share/hot/pkg/hot-std");
        if !installed.exists() {
            eprintln!("hot-std not installed, skipping");
            return;
        }

        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("hot-std");
        copy_dir(installed, &root);
        let cache_dir = dir.path().join("cache-std");

        // First run: no image — compiles and persists.
        let built = get_or_build_in(&root, &cache_dir).expect("first run must build");
        assert!(!built.function_mapping.is_empty());
        assert!(
            !built.tool_specs.entries.is_empty(),
            "tool specs must be derived from the compiled AST"
        );
        let key = compute_artifact_key(&root).unwrap();
        let path = image_path(&cache_dir, &key);
        assert!(path.exists(), "image persisted at {}", path.display());

        // Second run: loads the persisted image.
        let loaded = try_load(&path, &key).expect("reload must succeed");
        assert!(!loaded.ast_program.namespaces.is_empty());

        // Relocation: moving the hot-std tree must not change the key.
        let moved = dir.path().join("relocated").join("hot-std");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        std::fs::rename(&root, &moved).unwrap();
        assert_eq!(
            compute_artifact_key(&moved).unwrap(),
            key,
            "key must be prefix-independent"
        );
        std::fs::rename(&moved, &root).unwrap();

        // Staleness: modifying a source changes the key — old image misses.
        let victim = std::fs::read_dir(root.join("src").join("hot"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "hot"))
            .expect("a hot-std source file");
        let mut content = std::fs::read_to_string(&victim).unwrap();
        content.push_str("\n// modified\n");
        std::fs::write(&victim, content).unwrap();
        let new_key = compute_artifact_key(&root).unwrap();
        assert_ne!(new_key, key, "modified sources must change the key");
        assert!(
            !image_path(&cache_dir, &new_key).exists(),
            "no image exists for the new key yet"
        );
    }

    fn copy_dir(from: &Path, to: &Path) {
        std::fs::create_dir_all(to).unwrap();
        for entry in std::fs::read_dir(from).unwrap().flatten() {
            let src = entry.path();
            let dst = to.join(entry.file_name());
            if src.is_dir() {
                copy_dir(&src, &dst);
            } else {
                let _ = std::fs::copy(&src, &dst);
            }
        }
    }
}
