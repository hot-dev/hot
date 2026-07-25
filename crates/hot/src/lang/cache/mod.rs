//! Hot's compiled-artifact caching layer.
//!
//! * [`bytecode_cache`] — the on-disk bytecode/program cache (file format, IO).
//! * [`paths`]          — canonical filesystem layout used by the cache.
//! * [`unit_cache`]     — per-source-unit cache entries and invalidation.
//! * [`ast_cache`]      — pre-built `HotAst` caching for fast cached execution.

pub mod ast_cache;
pub mod bytecode_cache;
pub mod paths;
pub mod unit_cache;

/// How long an untouched cache file survives before opportunistic pruning.
/// Cache keys embed the Hot version, git SHA, and format version, so any
/// release or format bump strands the previous generation of files forever —
/// nothing else ever deletes them.
const CACHE_PRUNE_AFTER: std::time::Duration = std::time::Duration::from_secs(30 * 24 * 60 * 60);

/// Leftover atomic-write temp files are garbage after a crashed writer;
/// prune them on a much shorter fuse.
const CACHE_TMP_PRUNE_AFTER: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);

/// Mark a cache entry as recently used (best-effort mtime bump). Reads
/// don't update mtime, so without this an entry hit daily would still look
/// "untouched" to the pruner after 30 days and get deleted, forcing a
/// periodic rebuild.
pub(crate) fn touch_cache_entry(path: &std::path::Path) {
    if let Ok(file) = std::fs::OpenOptions::new().write(true).open(path) {
        let _ = file.set_modified(std::time::SystemTime::now());
    }
}

/// Remove a cache entry known to be stale or corrupt. This is deliberately
/// separate from read-error handling: permission and other transient read
/// failures must leave the entry in place.
pub(crate) fn remove_invalid_cache_entry(path: &std::path::Path) {
    if let Err(error) = std::fs::remove_file(path)
        && error.kind() != std::io::ErrorKind::NotFound
    {
        tracing::debug!(
            "Failed to remove invalid cache entry {}: {}",
            path.display(),
            error
        );
    }
}

fn is_lower_hex(value: &str, expected_len: usize) -> bool {
    value.len() == expected_len
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_unit_cache_stem(stem: &str) -> bool {
    if !stem.starts_with("pkg-") && !stem.starts_with("src-") {
        return false;
    }
    stem.rsplit_once('-')
        .is_some_and(|(_, hash)| is_lower_hex(hash, 16))
}

/// Whether a filename is owned by Hot's cache layer and is therefore safe
/// for opportunistic deletion.
pub(crate) fn is_managed_cache_file_name(name: &str) -> bool {
    if let Some(hash) = name.strip_suffix(".bc.zst")
        && is_lower_hex(hash, 64)
    {
        return true;
    }
    if let Some(stem) = name.strip_suffix(".ast.zst")
        && is_unit_cache_stem(stem)
    {
        return true;
    }
    if let Some(hash) = name
        .strip_prefix("std-")
        .and_then(|name| name.strip_suffix(".hsc"))
        && (is_lower_hex(hash, 64) || is_lower_hex(hash, 16))
    {
        return true;
    }

    // Atomic-write leftovers normally append `.tmp.<writer>`.
    if let Some((base, _)) = name.split_once(".tmp")
        && is_managed_cache_file_name(base)
    {
        return true;
    }
    // Historical writers replaced the final extension rather than appending.
    if let Some(hash) = name.split_once(".bc.bytecode.tmp.").map(|(hash, _)| hash)
        && is_lower_hex(hash, 64)
    {
        return true;
    }
    if let Some(stem) = name.split_once(".ast.ast.zst.tmp").map(|(stem, _)| stem)
        && is_unit_cache_stem(stem)
    {
        return true;
    }

    false
}

/// Drop superseded generations of a single cache entry.
///
/// Time-based pruning only reclaims *abandoned* entries; it does nothing about
/// churn, where each edit to a unit's sources writes a new key and strands the
/// previous one. A mutable unit (project sources, or a `local:` path
/// dependency under active development) can therefore accumulate hundreds of
/// dead entries long before any of them ages out.
///
/// `prefix` must be scoped to one unit *at one location* (see
/// `CompilationUnit::scoped_fs_id`) so this never evicts a same-named unit
/// belonging to another project sharing the directory. Immutable units
/// (a released package version) simply have nothing to sweep.
pub(crate) fn prune_superseded_entries(
    dir: &std::path::Path,
    prefix: &str,
    suffix: &str,
    keep: &std::path::Path,
) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path == keep || !path.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        // `{prefix}-{key}{suffix}` — the trailing `-` keeps `pkg-lib-<hash>`
        // from matching a longer unit name that merely starts the same way.
        if name.starts_with(prefix)
            && name[prefix.len()..].starts_with('-')
            && name.ends_with(suffix)
            && is_managed_cache_file_name(&name)
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// Remove entries written before unit cache names were scoped to a location.
///
/// Legacy names are exactly `{unit}-{16 hex key}{suffix}`. The loader now
/// derives a location-scoped name, so no legacy file can ever be read again —
/// they are unreachable for every project sharing the directory, not just this
/// one. Matching the exact legacy shape (rather than a prefix) is what keeps
/// this from touching a scoped entry belonging to another location.
pub(crate) fn prune_legacy_unscoped_entries(dir: &std::path::Path, unit_fs_id: &str, suffix: &str) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let Some(rest) = name
            .strip_prefix(unit_fs_id)
            .and_then(|r| r.strip_prefix('-'))
        else {
            continue;
        };
        if rest
            .strip_suffix(suffix)
            .is_some_and(|key| is_lower_hex(key, 16))
        {
            let _ = std::fs::remove_file(&path);
        }
    }
}

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
        // Never prune lock files: they are opened once and their mtime is
        // never refreshed, so an actively used lock would look stale — and
        // deleting one while a process holds its fd splits later lockers
        // onto a fresh inode, silently breaking mutual exclusion.
        if name.ends_with(".lock") {
            continue;
        }
        if !is_managed_cache_file_name(&name) {
            continue;
        }
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
    use super::{is_managed_cache_file_name, prune_stale_cache_files};

    const BYTECODE_KEY: &str = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[test]
    fn prunes_old_files_keeps_fresh_and_kept() {
        let dir = tempfile::tempdir().expect("tempdir");
        let old = dir.path().join("pkg-old-0123456789abcdef.ast.zst");
        let fresh = dir.path().join("pkg-fresh-fedcba9876543210.ast.zst");
        let kept = dir.path().join("pkg-kept-1111111111111111.ast.zst");
        let old_tmp = dir.path().join("pkg-x-2222222222222222.ast.zst.tmp.999");
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
        let kept = dir.path().join(format!("{}.bc.zst", BYTECODE_KEY));
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

    #[test]
    fn pruning_never_deletes_unrelated_or_lock_files() {
        let dir = tempfile::tempdir().expect("tempdir");
        let keep = dir
            .path()
            .join("std-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.hsc");
        let managed = dir.path().join(format!("{}.bc.zst", BYTECODE_KEY));
        let unrelated = dir.path().join("customer-data.cache");
        let unrelated_tmp = dir.path().join("notes.tmp");
        let lock = dir.path().join("pkg-old-0123456789abcdef.lock");
        for path in [&keep, &managed, &unrelated, &unrelated_tmp, &lock] {
            std::fs::write(path, b"x").unwrap();
            filetime::set_file_mtime(
                path,
                filetime::FileTime::from_system_time(
                    std::time::SystemTime::now() - std::time::Duration::from_secs(90 * 24 * 3600),
                ),
            )
            .unwrap();
        }

        prune_stale_cache_files(dir.path(), &keep);

        assert!(!managed.exists(), "recognized Hot cache entry is pruned");
        assert!(unrelated.exists(), "unrelated file is preserved");
        assert!(unrelated_tmp.exists(), "unrelated temp file is preserved");
        assert!(lock.exists(), "lock file is always preserved");
    }

    #[test]
    fn recognizes_only_hot_managed_cache_patterns() {
        assert!(is_managed_cache_file_name(&format!(
            "{}.bc.zst",
            BYTECODE_KEY
        )));
        assert!(is_managed_cache_file_name(
            "pkg-hot-std-0123456789abcdef.ast.zst"
        ));
        assert!(is_managed_cache_file_name(
            "std-0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef.hsc"
        ));
        assert!(!is_managed_cache_file_name("foreign.cache"));
        assert!(!is_managed_cache_file_name("notes.tmp"));
        assert!(!is_managed_cache_file_name("short.bc.zst"));
    }
}
