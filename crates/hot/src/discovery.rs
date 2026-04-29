//! Uniform file discovery built on the `ignore` crate.
//!
//! Every site in the codebase that needs to enumerate user files (`.hot`
//! sources, resources, formatter targets, LSP workspace, package docs, etc.)
//! should route through this module so we get *consistent* honoring of
//! `.gitignore`, `.git/info/exclude`, global git ignores, `.ignore`, and the
//! Hot-specific `.hotignore`.
//!
//! Beyond the standard ignore-file machinery, every walk also applies a
//! built-in default-excludes set ([`DEFAULT_HARD_EXCLUDES`]) so a fresh Hot
//! project that has not yet authored a `.gitignore` doesn't accidentally try
//! to enumerate `target/`, `node_modules/`, or the local Hot build cache.
//!
//! ## Why not raw `walkdir`?
//!
//! `walkdir` is purely structural — every directory is descended into and the
//! caller has to re-implement all skip rules. That led to subtle drift across
//! the codebase: some sites skipped hidden dirs, some skipped `target/`, and
//! none of them honored `.gitignore`. The `ignore` crate solves all of that
//! at once and is what `ripgrep`, `fd`, and most modern Rust tooling already
//! use, so we get exactly the behavior users expect from any other modern CLI.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

/// Hard-coded directories that are *always* excluded from discovery, even when
/// the project has no ignore files.
///
/// Treat this as the "minimum sane default" — the things you'd put in every
/// `.gitignore` for a Hot project anyway. Users can still override by adding a
/// `!target` style negation in `.gitignore` / `.hotignore`, but the common
/// case (no ignore file → don't melt the disk on `node_modules`) is handled.
///
/// `.hot/` is the local Hot build cache. `.git/` is excluded by `ignore`'s
/// own filtering already, but we list it for clarity.
pub const DEFAULT_HARD_EXCLUDES: &[&str] = &[
    ".hot/",
    "target/",
    "node_modules/",
    ".git/",
    "dist/",
    "build/",
    ".next/",
    ".venv/",
    "__pycache__/",
];

/// Options for [`discover`].
#[derive(Debug, Clone)]
pub struct DiscoveryOpts {
    /// File extensions (without leading dot) to keep. Empty = keep all files.
    pub extensions: Vec<String>,

    /// Honor `.gitignore`, `.git/info/exclude`, and global git ignore files.
    /// `.hotignore` and `.ignore` are *always* honored regardless of this
    /// flag, since they are Hot-specific (or generic, non-git) and represent
    /// explicit user intent. Default: `true`.
    pub respect_gitignore: bool,

    /// Skip files whose names start with `.` *unless* they are explicitly
    /// listed by an ignore rule. This is the "hidden files" heuristic in
    /// `ignore`; we default to `false` so that things like `.skills/` and
    /// dotfiles inside a tracked tree are still discoverable.
    pub skip_hidden: bool,

    /// Extra patterns to *exclude* from discovery (gitignore syntax). For
    /// example, a build pipeline may want to add `*.test.hot` here.
    pub extra_excludes: Vec<String>,

    /// Apply the [`DEFAULT_HARD_EXCLUDES`] list. Default: `true`. Tests and
    /// other internal callers that want to walk a temporary tree without any
    /// implicit skips can disable this.
    pub apply_default_excludes: bool,
}

impl Default for DiscoveryOpts {
    fn default() -> Self {
        Self {
            extensions: Vec::new(),
            // Honor the process-wide `--no-gitignore` toggle when set, so that
            // a single CLI flag cleanly propagates to *every* discovery site
            // (engine, bundler, formatter, LSP, docs, …) without having to
            // thread the flag through dozens of function signatures.
            respect_gitignore: !global_no_gitignore(),
            skip_hidden: false,
            extra_excludes: Vec::new(),
            apply_default_excludes: true,
        }
    }
}

/// Process-wide switch matching the `--no-gitignore` CLI flag. When set to
/// `true`, every newly-constructed [`DiscoveryOpts`] starts with
/// `respect_gitignore = false`. Existing opts are unaffected.
///
/// Tests should restore the previous value when done.
static NO_GITIGNORE_GLOBAL: AtomicBool = AtomicBool::new(false);

/// Set the process-wide `--no-gitignore` switch.
pub fn set_no_gitignore_global(no_gitignore: bool) {
    NO_GITIGNORE_GLOBAL.store(no_gitignore, Ordering::Relaxed);
}

/// Read the process-wide `--no-gitignore` switch.
pub fn global_no_gitignore() -> bool {
    NO_GITIGNORE_GLOBAL.load(Ordering::Relaxed)
}

impl DiscoveryOpts {
    /// Convenience builder: discover only files with the given extension
    /// (e.g. `"hot"` for `.hot` source files).
    pub fn for_extension(ext: impl Into<String>) -> Self {
        Self {
            extensions: vec![ext.into()],
            ..Self::default()
        }
    }

    /// Disable `.gitignore`/`.hotignore` honoring (used by `--no-gitignore`).
    pub fn with_no_gitignore(mut self) -> Self {
        self.respect_gitignore = false;
        self
    }

    /// Add an exclude pattern (gitignore syntax).
    pub fn exclude(mut self, pattern: impl Into<String>) -> Self {
        self.extra_excludes.push(pattern.into());
        self
    }
}

/// A discovered file with both its absolute path and its path relative to the
/// root it was found under.
///
/// Most callers want both: the absolute path to read the file, and the
/// relative path to produce a stable bundle key, namespace label, or
/// user-facing display string. `rel_path` always uses forward slashes for
/// portability.
#[derive(Debug, Clone)]
pub struct Discovered {
    pub abs_path: PathBuf,
    pub rel_path: String,
}

/// Walk one or more roots and return all matching files, honoring the unified
/// ignore rules + extension filter described in [`DiscoveryOpts`].
///
/// Roots that don't exist are silently skipped (consistent with the previous
/// ad-hoc behavior across the codebase). If a root is itself a file with a
/// matching extension, it's returned as a single entry whose `rel_path` is
/// just the file name.
///
/// The returned list is sorted by `rel_path` for deterministic output across
/// platforms. When the same `rel_path` appears under multiple roots (e.g.
/// shadowed `src/foo.hot`), the first root wins — matching the existing
/// resource registry semantics.
pub fn discover<P: AsRef<Path>>(roots: &[P], opts: &DiscoveryOpts) -> Vec<Discovered> {
    let mut out: Vec<Discovered> = Vec::new();
    let mut seen_rel: ahash::AHashSet<String> = ahash::AHashSet::new();

    for root in roots {
        let root = root.as_ref();
        if !root.exists() {
            continue;
        }

        let abs_root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());

        if abs_root.is_file() {
            if matches_extension(&abs_root, &opts.extensions) {
                let rel = abs_root
                    .file_name()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                if !rel.is_empty() && seen_rel.insert(rel.clone()) {
                    out.push(Discovered {
                        abs_path: abs_root,
                        rel_path: rel,
                    });
                }
            }
            continue;
        }

        if !abs_root.is_dir() {
            continue;
        }

        let mut builder = ignore::WalkBuilder::new(&abs_root);
        builder
            .git_ignore(opts.respect_gitignore)
            .git_exclude(opts.respect_gitignore)
            .git_global(opts.respect_gitignore)
            .ignore(true)
            .add_custom_ignore_filename(".hotignore")
            .hidden(opts.skip_hidden)
            .parents(opts.respect_gitignore);

        // Apply hard-coded + caller-supplied excludes via an Override matcher.
        // Patterns are gitignore syntax; we negate each one so it becomes a
        // skip-this-path rule.
        let combined_excludes = collect_excludes(opts);
        if !combined_excludes.is_empty() {
            let mut overrides = ignore::overrides::OverrideBuilder::new(&abs_root);
            for pat in &combined_excludes {
                let neg = if pat.starts_with('!') {
                    pat.clone()
                } else {
                    format!("!{}", pat)
                };
                if let Err(e) = overrides.add(&neg) {
                    tracing::warn!("discovery: invalid exclude pattern '{}': {}", pat, e);
                }
            }
            if let Ok(matcher) = overrides.build() {
                builder.overrides(matcher);
            }
        }

        for result in builder.build() {
            let dent = match result {
                Ok(d) => d,
                Err(e) => {
                    tracing::warn!("discovery: walk error in {}: {}", abs_root.display(), e);
                    continue;
                }
            };
            let path = dent.path();
            if !path.is_file() {
                continue;
            }
            if !matches_extension(path, &opts.extensions) {
                continue;
            }
            let rel = match path.strip_prefix(&abs_root) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            if seen_rel.insert(rel.clone()) {
                out.push(Discovered {
                    abs_path: path.to_path_buf(),
                    rel_path: rel,
                });
            }
        }
    }

    out.sort_by(|a, b| a.rel_path.cmp(&b.rel_path));
    out
}

/// Convenience wrapper: discover files and return only their absolute paths
/// as strings — matches the `Vec<String>` return type of the legacy
/// `Engine::discover_hot_files`.
pub fn discover_paths<P: AsRef<Path>>(roots: &[P], opts: &DiscoveryOpts) -> Vec<String> {
    discover(roots, opts)
        .into_iter()
        .map(|d| d.abs_path.to_string_lossy().to_string())
        .collect()
}

fn matches_extension(path: &Path, extensions: &[String]) -> bool {
    if extensions.is_empty() {
        return true;
    }
    let Some(ext) = path.extension().and_then(|s| s.to_str()) else {
        return false;
    };
    extensions.iter().any(|e| e == ext)
}

fn collect_excludes(opts: &DiscoveryOpts) -> Vec<String> {
    let mut out = Vec::with_capacity(
        opts.extra_excludes.len()
            + if opts.apply_default_excludes {
                DEFAULT_HARD_EXCLUDES.len()
            } else {
                0
            },
    );
    if opts.apply_default_excludes {
        for p in DEFAULT_HARD_EXCLUDES {
            out.push((*p).to_string());
        }
    }
    out.extend(opts.extra_excludes.iter().cloned());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn touch(dir: &Path, rel: &str) {
        let p = dir.join(rel);
        if let Some(parent) = p.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&p, b"").unwrap();
    }

    fn rel_paths(items: &[Discovered]) -> Vec<String> {
        items.iter().map(|d| d.rel_path.clone()).collect()
    }

    #[test]
    fn extension_filter() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "a.hot");
        touch(tmp.path(), "b.txt");
        touch(tmp.path(), "sub/c.hot");

        let opts = DiscoveryOpts::for_extension("hot");
        let found = discover(&[tmp.path()], &opts);
        assert_eq!(rel_paths(&found), vec!["a.hot", "sub/c.hot"]);
    }

    #[test]
    fn hard_excludes_skip_target_and_node_modules() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "src/main.hot");
        touch(tmp.path(), "target/leaked.hot");
        touch(tmp.path(), "node_modules/pkg/x.hot");
        touch(tmp.path(), ".hot/cache.hot");

        let opts = DiscoveryOpts::for_extension("hot");
        let found = rel_paths(&discover(&[tmp.path()], &opts));
        assert_eq!(found, vec!["src/main.hot"]);
    }

    #[test]
    fn gitignore_is_honored_by_default() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "a.hot");
        touch(tmp.path(), "ignored.hot");
        fs::write(tmp.path().join(".gitignore"), "ignored.hot\n").unwrap();
        // `ignore` only honors .gitignore inside a git repo; emulate by
        // marking the root as one.
        fs::create_dir_all(tmp.path().join(".git")).unwrap();

        let found = rel_paths(&discover(
            &[tmp.path()],
            &DiscoveryOpts::for_extension("hot"),
        ));
        assert_eq!(found, vec!["a.hot"]);
    }

    #[test]
    fn no_gitignore_disables_gitignore() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "a.hot");
        touch(tmp.path(), "ignored.hot");
        fs::write(tmp.path().join(".gitignore"), "ignored.hot\n").unwrap();
        fs::create_dir_all(tmp.path().join(".git")).unwrap();

        let opts = DiscoveryOpts::for_extension("hot").with_no_gitignore();
        let found = rel_paths(&discover(&[tmp.path()], &opts));
        assert_eq!(found, vec!["a.hot", "ignored.hot"]);
    }

    #[test]
    fn hotignore_always_honored() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "a.hot");
        touch(tmp.path(), "secret.hot");
        fs::write(tmp.path().join(".hotignore"), "secret.hot\n").unwrap();

        let opts = DiscoveryOpts::for_extension("hot").with_no_gitignore();
        let found = rel_paths(&discover(&[tmp.path()], &opts));
        // .hotignore is honored even when --no-gitignore is set, because it
        // is Hot-specific user intent.
        assert_eq!(found, vec!["a.hot"]);
    }

    #[test]
    fn first_root_wins_on_rel_path_collision() {
        let tmp = TempDir::new().unwrap();
        let a = tmp.path().join("rootA");
        let b = tmp.path().join("rootB");
        fs::create_dir_all(&a).unwrap();
        fs::create_dir_all(&b).unwrap();
        fs::write(a.join("shared.hot"), b"a-version").unwrap();
        fs::write(b.join("shared.hot"), b"b-version").unwrap();

        let opts = DiscoveryOpts::for_extension("hot");
        let found = discover(&[&a, &b], &opts);
        assert_eq!(found.len(), 1);
        // Compare via canonicalize to tolerate /var → /private/var on macOS.
        let canon_a = a.canonicalize().unwrap();
        assert!(found[0].abs_path.starts_with(&canon_a));
    }

    #[test]
    fn single_file_root_returned_when_extension_matches() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "main.hot");

        let opts = DiscoveryOpts::for_extension("hot");
        let found = discover(&[tmp.path().join("main.hot")], &opts);
        assert_eq!(rel_paths(&found), vec!["main.hot"]);
    }

    #[test]
    fn missing_root_is_silently_skipped() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "exists.hot");
        let missing = tmp.path().join("does-not-exist");

        let opts = DiscoveryOpts::for_extension("hot");
        let found = rel_paths(&discover(&[tmp.path(), &missing], &opts));
        assert_eq!(found, vec!["exists.hot"]);
    }

    #[test]
    fn extra_excludes_apply() {
        let tmp = TempDir::new().unwrap();
        touch(tmp.path(), "src/main.hot");
        touch(tmp.path(), "src/main.test.hot");

        let opts = DiscoveryOpts::for_extension("hot").exclude("*.test.hot");
        let found = rel_paths(&discover(&[tmp.path()], &opts));
        assert_eq!(found, vec!["src/main.hot"]);
    }
}
