//! Precompiled hot-std release artifact.
//!
//! hot-std is immutable per install, yet every CLI invocation used to parse
//! and compile all ~50 of its files before touching user code. This module
//! defines `hot-std.hsc` — a compiled hot-std image built once (at release
//! time, or on demand via the hidden `hot build-std-artifact` command) and
//! shipped next to the hot-std sources.
//!
//! ## File format
//!
//! ```text
//! [0..8)   magic  b"HOTSTDA1"
//! [8..]    postcard header { artifact_key }
//!          followed by the zstd-compressed bytecode-cache payload
//!          (same encoding as bytecode_cache::CacheFilePayload)
//! ```
//!
//! `artifact_key` is a [`CacheKeyBuilder`] hash covering the Hot VERSION,
//! GIT_SHA, bytecode format version, and the blake3 hashes of every hot-std
//! source file. The loader recomputes the key from the installed sources and
//! compares — any mismatch (different runtime build, locally modified
//! hot-std, dev override) silently falls back to the classic source-compile
//! path. There is no partial reuse and no migration: the artifact either
//! matches this exact binary and source tree, or it is ignored.
//!
//! The same wire-format rules as the caches apply (see ast_cache.rs module
//! docs); the artifact reuses the bytecode cache payload encoding, so its
//! layout is covered by `CacheType::Bytecode.format_version()` via the key.
//!
//! ## Status: infrastructure only — not yet wired into execution
//!
//! A pipeline fast path that extended this artifact with ad hoc eval code
//! was prototyped and measured SLOWER than the classic path (134ms vs 72ms
//! for `hot eval 'add(1,1)'`): eagerly decoding the full payload costs
//! ~60ms, dominated by the AST namespaces / HotAst / var_index sections the
//! eval path barely uses. Two things are needed before integration:
//!
//! 1. **Per-section lazy decoding** — split the payload so the eval path
//!    decodes only program + registries + derived ctx data (small), and the
//!    AST sections decode on demand (error enrichment, tooling).
//! 2. **Resolver-aware eval extension** — `eval_code_with_cached_bytecode`
//!    compiles the eval snippet against registered function IDs only; it
//!    never runs `resolve_all_variable_references`, so unqualified calls
//!    (e.g. `add(1,1)`) fail to resolve. Worker eval snippets use qualified
//!    names, which is why this gap was invisible until now.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::OnceLock;

use crate::hasher::{CacheKeyBuilder, CacheType, compute_hot_file_hashes};

use super::bytecode_cache::{
    CacheMetadata, CachedBytecode, decode_cache_payload, encode_cache_payload,
};

/// Artifact file name, resolved relative to the hot-std package root.
pub const ARTIFACT_FILE_NAME: &str = "hot-std.hsc";

const MAGIC: &[u8; 8] = b"HOTSTDA1";

#[derive(serde::Serialize, serde::Deserialize)]
struct ArtifactHeader {
    /// CacheKeyBuilder hash over VERSION + GIT_SHA + bytecode format version
    /// + hot-std source file hashes.
    artifact_key: String,
}

/// Compute the expected artifact key for the hot-std sources at `std_root`.
///
/// File paths are relativized to `std_root` before hashing: the artifact is
/// built on a release machine and validated at whatever prefix the user
/// installed to, so absolute paths must never influence the key.
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

/// Build the artifact from the hot-std sources at `std_root` and write it to
/// `out_path` (defaults to `<std_root>/hot-std.hsc`). Returns the written
/// path and the artifact size in bytes.
pub fn build_artifact(std_root: &Path, out_path: Option<&Path>) -> Result<(PathBuf, u64), String> {
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

    let artifact_key = compute_artifact_key(std_root)?;
    let metadata = CacheMetadata {
        project_name: "hot-std".to_string(),
        hot_version: crate::build_info::VERSION.to_string(),
        git_sha: crate::build_info::GIT_SHA.to_string(),
        cache_format_version: CacheType::Bytecode.format_version(),
        created_at: chrono::Utc::now().timestamp(),
        file_hashes: Vec::new(),
        cache_key: artifact_key.clone(),
    };

    let (compressed_payload, _) = encode_cache_payload(
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

    let header = postcard::to_allocvec(&ArtifactHeader { artifact_key })
        .map_err(|e| format!("Failed to encode artifact header: {}", e))?;

    let mut file_bytes = Vec::with_capacity(MAGIC.len() + header.len() + compressed_payload.len());
    file_bytes.extend_from_slice(MAGIC);
    file_bytes.extend_from_slice(&header);
    file_bytes.extend_from_slice(&compressed_payload);

    let out = out_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| std_root.join(ARTIFACT_FILE_NAME));
    let tmp = out.with_extension("hsc.tmp");
    std::fs::write(&tmp, &file_bytes)
        .map_err(|e| format!("Failed to write artifact {}: {}", tmp.display(), e))?;
    std::fs::rename(&tmp, &out)
        .map_err(|e| format!("Failed to finalize artifact {}: {}", out.display(), e))?;

    Ok((out, file_bytes.len() as u64))
}

/// Per-process artifact slot: loaded and validated at most once.
static LOADED: OnceLock<Option<Arc<CachedBytecode>>> = OnceLock::new();

/// Load the hot-std artifact for the hot-std package at `std_root`, if one
/// exists and matches this binary and the installed sources exactly.
/// Returns `None` (with a debug log) on any mismatch or error — callers fall
/// back to the classic source-compile path.
pub fn load_for(std_root: &Path) -> Option<Arc<CachedBytecode>> {
    LOADED
        .get_or_init(|| match try_load(std_root) {
            Ok(cached) => Some(cached),
            Err(reason) => {
                tracing::debug!("hot-std artifact unavailable: {}", reason);
                None
            }
        })
        .clone()
}

fn try_load(std_root: &Path) -> Result<Arc<CachedBytecode>, String> {
    let path = std_root.join(ARTIFACT_FILE_NAME);
    if !path.exists() {
        return Err(format!("{} not present", path.display()));
    }

    let bytes = std::fs::read(&path).map_err(|e| format!("read failed: {}", e))?;
    let rest = bytes
        .strip_prefix(MAGIC.as_slice())
        .ok_or_else(|| "bad magic".to_string())?;

    let (header, payload): (ArtifactHeader, &[u8]) =
        postcard::take_from_bytes(rest).map_err(|e| format!("bad header: {}", e))?;

    // The key covers VERSION, GIT_SHA, bytecode format version, and the
    // hashes of the installed hot-std sources. Recompute and compare: any
    // difference (upgraded runtime, dirty hot-std, dev override) rejects
    // the artifact.
    let expected = compute_artifact_key(std_root)?;
    if header.artifact_key != expected {
        return Err(format!(
            "key mismatch (artifact {}…, sources {}…) — stale or foreign artifact",
            &header.artifact_key[..12.min(header.artifact_key.len())],
            &expected[..12.min(expected.len())],
        ));
    }

    let decompressed =
        zstd::decode_all(payload).map_err(|e| format!("decompress failed: {}", e))?;
    let cached = decode_cache_payload(&decompressed)?;

    tracing::debug!(
        "hot-std artifact loaded from {} ({} functions, {} namespaces)",
        path.display(),
        cached.function_mapping.len(),
        cached.ast_program.namespaces.len()
    );
    Ok(cached)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build + load round-trip against the installed hot-std, exercising the
    /// key validation both ways. Ignored by default: needs an installed
    /// hot-std and does a full compile (~100ms release / ~1s debug).
    #[test]
    #[ignore]
    fn artifact_roundtrip_and_validation() {
        let installed = Path::new("/usr/local/share/hot/pkg/hot-std");
        if !installed.exists() {
            eprintln!("hot-std not installed, skipping");
            return;
        }

        // Copy hot-std into a temp root so the test never writes to the
        // (root-owned) install prefix.
        let dir = tempfile::tempdir().expect("tempdir");
        let root = dir.path().join("hot-std");
        copy_dir(installed, &root);

        let (path, size) = build_artifact(&root, None).expect("build must succeed");
        assert!(path.exists());
        assert!(size > 0);

        let loaded = try_load(&root).expect("load must succeed");
        assert!(!loaded.function_mapping.is_empty());
        assert!(!loaded.ast_program.namespaces.is_empty());

        // The artifact is built on a release machine and installed at an
        // arbitrary prefix: moving the whole tree must not invalidate it
        // (the key hashes root-relative paths, never absolute ones).
        let moved = dir.path().join("relocated").join("hot-std");
        std::fs::create_dir_all(moved.parent().unwrap()).unwrap();
        std::fs::rename(&root, &moved).unwrap();
        let reloaded = try_load(&moved).expect("load must survive relocation");
        assert!(!reloaded.function_mapping.is_empty());
        std::fs::rename(&moved, &root).unwrap();

        // Modifying a source file must invalidate the artifact.
        let victim = std::fs::read_dir(root.join("src").join("hot"))
            .unwrap()
            .flatten()
            .map(|e| e.path())
            .find(|p| p.extension().is_some_and(|e| e == "hot"))
            .expect("a hot-std source file");
        let mut content = std::fs::read_to_string(&victim).unwrap();
        content.push_str("\n// modified\n");
        std::fs::write(&victim, content).unwrap();

        let err = try_load(&root).expect_err("stale artifact must be rejected");
        assert!(err.contains("key mismatch"), "{err}");
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
