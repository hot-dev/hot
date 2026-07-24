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
//! ## File format (`std-<key16>.hsc`)
//!
//! ```text
//! [0..8)   magic  b"HOTSTDA1"
//! [8..]    postcard header { artifact_key }
//!          followed by the zstd-compressed bytecode-cache payload
//!          (bytecode_cache::CacheFilePayload encoding)
//! ```
//!
//! `artifact_key` is a [`CacheKeyBuilder`] hash covering the Hot VERSION,
//! GIT_SHA, bytecode format version, and the blake3 hashes of every hot-std
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
use std::sync::OnceLock;

use crate::hasher::{CacheKeyBuilder, CacheType, compute_hot_file_hashes};

use super::bytecode_cache::{
    CacheMetadata, CachedBytecode, decode_cache_payload, encode_cache_payload,
};

const MAGIC: &[u8; 8] = b"HOTSTDA1";

#[derive(serde::Serialize, serde::Deserialize)]
struct ArtifactHeader {
    /// CacheKeyBuilder hash over VERSION + GIT_SHA + bytecode format version
    /// + root-relative hot-std source file hashes.
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
    cache_dir.join(format!(
        "std-{}.hsc",
        &artifact_key[..16.min(artifact_key.len())]
    ))
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

/// Per-process image slot: resolved at most once per process.
static LOADED: OnceLock<Option<Arc<CachedBytecode>>> = OnceLock::new();

/// Get the compiled hot-std image for the hot-std package at `std_root`:
/// load it from the system cache when present and valid, otherwise compile
/// hot-std now, persist the image (best-effort), and return the fresh build.
///
/// Returns `None` only if hot-std cannot be compiled at all — callers fall
/// back to the classic combined-compile path, which will report the error
/// with full diagnostics.
pub fn get_or_build(std_root: &Path) -> Option<Arc<CachedBytecode>> {
    LOADED
        .get_or_init(|| get_or_build_in(std_root, &std_cache_dir()))
        .clone()
}

fn get_or_build_in(std_root: &Path, cache_dir: &Path) -> Option<Arc<CachedBytecode>> {
    let artifact_key = match compute_artifact_key(std_root) {
        Ok(key) => key,
        Err(reason) => {
            tracing::debug!("hot-std image unavailable: {}", reason);
            return None;
        }
    };

    let path = image_path(cache_dir, &artifact_key);
    if path.exists() {
        match try_load(&path, &artifact_key) {
            Ok(cached) => return Some(cached),
            Err(reason) => {
                tracing::debug!("hot-std image at {} rejected: {}", path.display(), reason);
            }
        }
    }

    // First run (or stale/corrupt image): compile hot-std now and persist.
    match build_image(std_root, &artifact_key, &path) {
        Ok(cached) => Some(cached),
        Err(reason) => {
            tracing::debug!("hot-std image build failed: {}", reason);
            None
        }
    }
}

/// Compile hot-std and persist the image at `path`. Returns the in-memory
/// build even if persisting fails (the next run just recompiles).
fn build_image(
    std_root: &Path,
    artifact_key: &str,
    path: &Path,
) -> Result<Arc<CachedBytecode>, String> {
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

    let (compressed_payload, cached) = encode_cache_payload(
        metadata,
        &artifacts.program,
        &artifacts.function_mapping,
        &artifacts.core_functions,
        &artifacts.type_implementations,
        &artifacts.ast_program,
        &artifacts.hot_ast,
        &Default::default(),
        &Default::default(),
    )?;

    let header = postcard::to_allocvec(&ArtifactHeader {
        artifact_key: artifact_key.to_string(),
    })
    .map_err(|e| format!("Failed to encode image header: {}", e))?;

    let mut file_bytes = Vec::with_capacity(MAGIC.len() + header.len() + compressed_payload.len());
    file_bytes.extend_from_slice(MAGIC);
    file_bytes.extend_from_slice(&header);
    file_bytes.extend_from_slice(&compressed_payload);

    // Persisting is best-effort: a read-only cache dir or a race with
    // another first-run process must not fail the current run.
    if let Err(e) = persist(path, &file_bytes) {
        tracing::debug!("hot-std image not persisted: {}", e);
    }

    Ok(cached)
}

fn persist(path: &Path, file_bytes: &[u8]) -> Result<(), String> {
    let dir = path
        .parent()
        .ok_or_else(|| "image path has no parent".to_string())?;
    std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    let tmp = path.with_extension(format!("hsc.tmp.{}", std::process::id()));
    std::fs::write(&tmp, file_bytes).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        e.to_string()
    })?;
    super::prune_stale_cache_files(dir, path);
    Ok(())
}

fn try_load(path: &Path, expected_key: &str) -> Result<Arc<CachedBytecode>, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("read failed: {}", e))?;
    let rest = bytes
        .strip_prefix(MAGIC.as_slice())
        .ok_or_else(|| "bad magic".to_string())?;

    let (header, payload): (ArtifactHeader, &[u8]) =
        postcard::take_from_bytes(rest).map_err(|e| format!("bad header: {}", e))?;

    // The filename only carries a key prefix; the header holds the full key.
    if header.artifact_key != expected_key {
        return Err("key mismatch — stale or foreign image".to_string());
    }

    let decompressed =
        zstd::decode_all(payload).map_err(|e| format!("decompress failed: {}", e))?;
    let cached = decode_cache_payload(&decompressed)?;

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
    let path = match out {
        Some(p) => p.to_path_buf(),
        None => image_path(&std_cache_dir(), &artifact_key),
    };
    build_image(std_root, &artifact_key, &path)?;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Ok((path, size))
}

#[cfg(test)]
mod tests {
    use super::*;

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
