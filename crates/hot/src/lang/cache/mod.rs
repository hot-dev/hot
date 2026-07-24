//! Hot's compiled-artifact caching layer.
//!
//! * [`bytecode_cache`] — the on-disk bytecode/program cache (file format, IO).
//! * [`paths`]          — canonical filesystem layout used by the cache.
//! * [`unit_cache`]     — per-source-unit cache entries and invalidation.
//! * [`ast_cache`]      — pre-built `HotAst` caching for fast cached execution.

pub mod ast_cache;
pub mod bytecode_cache;
pub mod paths;
pub mod std_artifact;
pub mod unit_cache;

/// How long an untouched cache file survives before opportunistic pruning.
/// Cache keys embed the Hot version, git SHA, and format version, so any
/// release or format bump strands the previous generation of files forever —
/// nothing else ever deletes them.
const CACHE_PRUNE_AFTER: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);

/// Leftover atomic-write temp files are garbage after a crashed writer;
/// prune them on a much shorter fuse.
const CACHE_TMP_PRUNE_AFTER: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Best-effort sweep of stale files in a cache directory, called
/// opportunistically after a successful cache save (off any hot path).
///
/// Deletes regular files whose modification time is older than
/// [`CACHE_PRUNE_AFTER`] (or [`CACHE_TMP_PRUNE_AFTER`] for `.tmp*` leftovers),
/// skipping `keep` (the file just written). Errors are ignored — pruning is
/// housekeeping, never correctness.
pub(crate) fn prune_stale_cache_files(dir: &std::path::Path, keep: &std::path::Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep || !path.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let is_tmp = name.contains(".tmp");
        let max_age = if is_tmp {
            CACHE_TMP_PRUNE_AFTER
        } else {
            CACHE_PRUNE_AFTER
        };
        let stale = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|mtime| now.duration_since(mtime).ok())
            .is_some_and(|age| age > max_age);
        if stale {
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod prune_tests {
    use super::prune_stale_cache_files;

    #[test]
    fn prunes_old_files_keeps_fresh_and_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join("pkg-old-abc.ast.zst");
        let fresh = dir.path().join("pkg-fresh-def.ast.zst");
        let kept = dir.path().join("pkg-kept-123.ast.zst");
        let old_tmp = dir.path().join("pkg-x.ast.zst.tmp.999");
        for f in [&old, &fresh, &kept, &old_tmp] {
            std::fs::write(f, b"x").unwrap();
        }

        // Age `old` past the 30-day fuse and `old_tmp` past the 1-day fuse.
        let days = |n: u64| {
            filetime::FileTime::from_system_time(
                std::time::SystemTime::now() - std::time::Duration::from_secs(n * 24 * 3600),
            )
        };
        filetime::set_file_mtime(&old, days(31)).unwrap();
        filetime::set_file_mtime(&old_tmp, days(2)).unwrap();

        prune_stale_cache_files(dir.path(), &kept);

        assert!(!old.exists(), "31-day-old entry pruned");
        assert!(!old_tmp.exists(), "2-day-old tmp leftover pruned");
        assert!(fresh.exists(), "fresh entry kept");
        assert!(kept.exists(), "just-written entry kept");
    }

    #[test]
    fn keep_file_survives_even_when_old() {
        let dir = tempfile::tempdir().expect("tempdir");
        let kept = dir.path().join("pkg-kept.bc.zst");
        std::fs::write(&kept, b"x").unwrap();
        filetime::set_file_mtime(
            &kept,
            filetime::FileTime::from_system_time(
                std::time::SystemTime::now() - std::time::Duration::from_secs(90 * 24 * 3600),
            ),
        )
        .unwrap();

        prune_stale_cache_files(dir.path(), &kept);
        assert!(kept.exists(), "the file just written is never pruned");
    }
}
