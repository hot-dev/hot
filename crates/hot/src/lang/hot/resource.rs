// Resource Registry and Hot bindings for `::hot::resource`
//
// A resource is a project-bundled file or directory tree, made available
// to Hot code at runtime. Resources are configured in `hot.hot` via:
//
//   hot.project.<name>.resources {
//     paths: ["./hot/resources", "./prompts"],
//     excludes: ["**/*.tmp"],
//   }
//
// The registry uses a "classpath" model: the union of files under all
// `paths` is exposed by relative path. If two roots provide the same
// relative path, the first listed wins (a warning is recorded).
//
// At runtime, `::hot::resource/load("foo/bar.txt")` returns Bytes,
// `load-str` returns Str, `path` returns the canonical hot:// URI,
// `exists` returns Bool, `list` returns a Vec<ResourceRef>, and
// `list-matching(glob)` filters by glob.

use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;
use indexmap::IndexMap;
use parking_lot::RwLock;
use std::cell::RefCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use uuid::Uuid;

/// One resource entry in the project-global registry.
#[derive(Debug, Clone)]
pub struct ResourceEntry {
    /// Path relative to the resource root (e.g. "fetcher/index.js").
    pub rel_path: String,
    /// Absolute path on disk (dev mode) or virtual path under the build
    /// root (deploy mode).
    pub abs_path: PathBuf,
    /// Blake3 content hash, populated during `hot build`/`deploy`. In
    /// dev mode this is empty until the first `load`.
    pub hash: String,
    /// File size in bytes, populated lazily.
    pub size: u64,
    /// True when read from a baked deploy manifest; false in dev mode.
    pub from_manifest: bool,
}

/// Project-global resource registry.
#[derive(Debug, Default, Clone)]
pub struct ResourceRegistry {
    /// rel_path -> ResourceEntry. Order preserved for `list()`.
    pub entries: IndexMap<String, ResourceEntry>,
    /// Conflict warnings recorded during scanning.
    pub conflicts: Vec<String>,
    /// Resource source roots (absolute paths), in priority order.
    pub roots: Vec<PathBuf>,
}

static REGISTRY: OnceLock<RwLock<ResourceRegistry>> = OnceLock::new();

fn registry_lock() -> &'static RwLock<ResourceRegistry> {
    REGISTRY.get_or_init(|| RwLock::new(ResourceRegistry::default()))
}

/// Per-build registry cache. Workers populate this once per bundle build
/// (keyed by `build_id`) when the bundle is first extracted; task threads
/// then install the matching `Arc<ResourceRegistry>` as a thread-local for
/// the lifetime of a single task execution. Sharing the `Arc` across
/// concurrent tasks running the same build avoids re-walking the bundle on
/// every invocation.
static BUILD_REGISTRIES: OnceLock<RwLock<HashMap<Uuid, Arc<ResourceRegistry>>>> = OnceLock::new();

fn build_registries_lock() -> &'static RwLock<HashMap<Uuid, Arc<ResourceRegistry>>> {
    BUILD_REGISTRIES.get_or_init(|| RwLock::new(HashMap::new()))
}

thread_local! {
    /// Per-thread registry override. When set, `get_registry()` reads this
    /// instead of the process-global registry. The task worker sets this
    /// inside its `spawn_blocking` closure so concurrent tasks see their own
    /// build's resources without racing on shared global state.
    static THREAD_REGISTRY: RefCell<Option<Arc<ResourceRegistry>>> = const { RefCell::new(None) };
}

/// Replace the project-global registry. Called by the engine during
/// startup, by `hot dev` after a watcher refresh, and by tests.
pub fn set_registry(registry: ResourceRegistry) {
    *registry_lock().write() = registry;
}

/// Read-only snapshot of the current registry. Honors the thread-local
/// override (set by the task worker during task execution) before falling
/// back to the process-global registry.
pub fn get_registry() -> ResourceRegistry {
    let thread_local = THREAD_REGISTRY.with(|r| r.borrow().clone());
    if let Some(reg) = thread_local {
        return (*reg).clone();
    }
    registry_lock().read().clone()
}

/// Install or clear the thread-local registry override. Pass `None` to
/// clear. The task worker uses this to scope each task to its bundle's
/// resources.
pub fn set_thread_registry(registry: Option<Arc<ResourceRegistry>>) {
    THREAD_REGISTRY.with(|r| *r.borrow_mut() = registry);
}

/// RAII guard that installs a thread-local registry override and restores
/// the previous value (typically `None`) on drop. Use from `spawn_blocking`
/// closures so panics still clean up.
pub struct ThreadRegistryGuard {
    prev: Option<Arc<ResourceRegistry>>,
}

impl ThreadRegistryGuard {
    pub fn install(registry: Arc<ResourceRegistry>) -> Self {
        let prev = THREAD_REGISTRY.with(|r| r.replace(Some(registry)));
        ThreadRegistryGuard { prev }
    }
}

impl Drop for ThreadRegistryGuard {
    fn drop(&mut self) {
        let prev = self.prev.take();
        THREAD_REGISTRY.with(|r| *r.borrow_mut() = prev);
    }
}

/// Cache a per-build registry so task threads can install it without
/// re-walking the bundle. Idempotent — overwriting an existing entry is
/// supported (e.g. when a new bundle replaces the old one for the same
/// build_id).
pub fn set_build_registry(build_id: Uuid, registry: Arc<ResourceRegistry>) {
    build_registries_lock().write().insert(build_id, registry);
}

/// Look up the per-build registry for a given build_id, if previously
/// installed via `set_build_registry`.
pub fn get_build_registry(build_id: &Uuid) -> Option<Arc<ResourceRegistry>> {
    build_registries_lock().read().get(build_id).cloned()
}

/// Remove a per-build registry entry (e.g. on bundle eviction).
pub fn remove_build_registry(build_id: &Uuid) -> Option<Arc<ResourceRegistry>> {
    build_registries_lock().write().remove(build_id)
}

/// Clear the registry (for tests).
pub fn clear_registry() {
    *registry_lock().write() = ResourceRegistry::default();
    build_registries_lock().write().clear();
    THREAD_REGISTRY.with(|r| *r.borrow_mut() = None);
}

/// Build a `ResourceRegistry` from a deployed bundle's manifest and the
/// extracted bundle directory. Resources live under
/// `<extract_dir>/resources/<rel_path>` (matching
/// [`crate::bundle::RESOURCE_BUNDLE_PREFIX`]); each manifest entry carries
/// the canonical `path` / `hash` / `size` values produced at build time.
///
/// `manifest_resources` is the `Val::Vec` value found at
/// `manifest.<bundle_key>.resources`. Entries with non-string `path` are
/// silently skipped with a warning.
pub fn build_registry_from_manifest(
    manifest_resources: &Val,
    extract_dir: &Path,
) -> ResourceRegistry {
    let resources_root = extract_dir.join(crate::bundle::RESOURCE_BUNDLE_PREFIX);
    let mut entries: IndexMap<String, ResourceEntry> = IndexMap::new();

    if let Val::Vec(items) = manifest_resources {
        for item in items {
            let Val::Map(map) = item else {
                tracing::warn!("resource: manifest entry is not a Map: {:?}", item);
                continue;
            };
            let rel_path = match map.get(&Val::from("path")) {
                Some(Val::Str(s)) => (**s).to_owned(),
                _ => {
                    tracing::warn!("resource: manifest entry missing or non-Str `path`");
                    continue;
                }
            };
            let hash = match map.get(&Val::from("hash")) {
                Some(Val::Str(s)) => (**s).to_owned(),
                _ => String::new(),
            };
            let size = match map.get(&Val::from("size")) {
                Some(Val::Int(n)) => (*n).max(0) as u64,
                _ => 0,
            };
            let abs_path = resources_root.join(&rel_path);
            entries.insert(
                rel_path.clone(),
                ResourceEntry {
                    rel_path,
                    abs_path,
                    hash,
                    size,
                    from_manifest: true,
                },
            );
        }
    } else if !matches!(manifest_resources, Val::Null) {
        tracing::warn!(
            "resource: manifest `resources` is not a Vec: {:?}",
            manifest_resources
        );
    }

    ResourceRegistry {
        entries,
        conflicts: Vec::new(),
        roots: vec![resources_root],
    }
}

/// Build a registry by walking the supplied roots, honoring .gitignore /
/// .hotignore (when `respect_gitignore` is true) and the supplied extra
/// excludes.
pub fn build_registry(
    roots: &[PathBuf],
    respect_gitignore: bool,
    extra_excludes: &[String],
) -> ResourceRegistry {
    let mut entries: IndexMap<String, ResourceEntry> = IndexMap::new();
    let mut conflicts: Vec<String> = Vec::new();
    let mut absolute_roots: Vec<PathBuf> = Vec::new();

    for root in roots {
        if !root.exists() {
            continue;
        }
        let abs_root = match root.canonicalize() {
            Ok(p) => p,
            Err(_) => root.clone(),
        };
        absolute_roots.push(abs_root.clone());

        let mut builder = ignore::WalkBuilder::new(&abs_root);
        builder
            .git_ignore(respect_gitignore)
            .git_exclude(respect_gitignore)
            .git_global(respect_gitignore)
            .ignore(true)
            .add_custom_ignore_filename(".hotignore")
            .hidden(false);

        // Apply explicit excludes via override matcher.
        if !extra_excludes.is_empty() {
            let mut overrides = ignore::overrides::OverrideBuilder::new(&abs_root);
            for pat in extra_excludes {
                let neg = if pat.starts_with('!') {
                    pat.clone()
                } else {
                    format!("!{}", pat)
                };
                if let Err(e) = overrides.add(&neg) {
                    tracing::warn!("resource: invalid exclude pattern '{}': {}", pat, e);
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
                    tracing::warn!("resource walk error in {}: {}", abs_root.display(), e);
                    continue;
                }
            };
            let path = dent.path();
            if !path.is_file() {
                continue;
            }
            let rel = match path.strip_prefix(&abs_root) {
                Ok(r) => r.to_string_lossy().replace('\\', "/"),
                Err(_) => continue,
            };
            if entries.contains_key(&rel) {
                conflicts.push(format!(
                    "resource '{}' shadowed by earlier root (kept first; new root: {})",
                    rel,
                    abs_root.display()
                ));
                continue;
            }
            // Hash and size are computed up-front so `::hot::resource/list`
            // returns the same schema in dev (live disk) and prod (bundled
            // manifest). Resource trees are expected to be small (prompts,
            // templates, skill markdown); revisit lazy-hashing if/when a
            // project ships gigabyte-scale assets.
            let (size, hash) = match std::fs::read(path) {
                Ok(content) => {
                    let size = content.len() as u64;
                    let hash = crate::hasher::HotHasher::hash_content(&content);
                    (size, hash)
                }
                Err(e) => {
                    tracing::warn!(
                        "resource: failed to read {} for hashing: {}",
                        path.display(),
                        e
                    );
                    (path.metadata().map(|m| m.len()).unwrap_or(0), String::new())
                }
            };
            entries.insert(
                rel.clone(),
                ResourceEntry {
                    rel_path: rel,
                    abs_path: path.to_path_buf(),
                    hash,
                    size,
                    from_manifest: false,
                },
            );
        }
    }

    for w in &conflicts {
        tracing::warn!("{}", w);
    }

    ResourceRegistry {
        entries,
        conflicts,
        roots: absolute_roots,
    }
}

fn rel_arg(fn_name: &str, arg: &Val) -> Result<String, Val> {
    match arg {
        Val::Str(s) => Ok((**s).to_owned()),
        _ => Err(Val::from(format!(
            "{}: relative path must be a string",
            fn_name
        ))),
    }
}

fn entry_to_val(entry: &ResourceEntry) -> Val {
    crate::val!({
        "path": entry.rel_path.clone(),
        "hash": entry.hash.clone(),
        "size": entry.size as i64
    })
}

/// `::hot::resource/load(rel-path: Str): Bytes`
pub fn load(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::resource/load", args, 1);
    let rel = match rel_arg("::hot::resource/load", &args[0]) {
        Ok(r) => r,
        Err(e) => return HotResult::Err(e),
    };
    let registry = get_registry();
    match registry.entries.get(&rel) {
        Some(entry) => match std::fs::read(&entry.abs_path) {
            Ok(bytes) => HotResult::Ok(Val::Bytes(bytes)),
            Err(e) => HotResult::Err(Val::from(format!(
                "::hot::resource/load: failed to read '{}': {}",
                rel, e
            ))),
        },
        None => HotResult::Err(Val::from(format!(
            "::hot::resource/load: resource not found: '{}'",
            rel
        ))),
    }
}

/// `::hot::resource/load-str(rel-path: Str): Str`
pub fn load_str(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::resource/load-str", args, 1);
    let rel = match rel_arg("::hot::resource/load-str", &args[0]) {
        Ok(r) => r,
        Err(e) => return HotResult::Err(e),
    };
    let registry = get_registry();
    match registry.entries.get(&rel) {
        Some(entry) => match std::fs::read_to_string(&entry.abs_path) {
            Ok(s) => HotResult::Ok(Val::from(s)),
            Err(e) => HotResult::Err(Val::from(format!(
                "::hot::resource/load-str: failed to read '{}': {}",
                rel, e
            ))),
        },
        None => HotResult::Err(Val::from(format!(
            "::hot::resource/load-str: resource not found: '{}'",
            rel
        ))),
    }
}

/// `::hot::resource/path(rel-path: Str): Str`
/// Returns the canonical `hot://res/<rel-path>` URI.
pub fn path(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::resource/path", args, 1);
    let rel = match rel_arg("::hot::resource/path", &args[0]) {
        Ok(r) => r,
        Err(e) => return HotResult::Err(e),
    };
    let registry = get_registry();
    match registry.entries.get(&rel) {
        Some(_) => HotResult::Ok(Val::from(format!("hot://res/{}", rel))),
        None => HotResult::Err(Val::from(format!(
            "::hot::resource/path: resource not found: '{}'",
            rel
        ))),
    }
}

/// `::hot::resource/exists(rel-path: Str): Bool`
pub fn exists(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::resource/exists", args, 1);
    let rel = match rel_arg("::hot::resource/exists", &args[0]) {
        Ok(r) => r,
        Err(e) => return HotResult::Err(e),
    };
    let registry = get_registry();
    HotResult::Ok(Val::Bool(registry.entries.contains_key(&rel)))
}

/// `::hot::resource/list(): Vec<{path, hash, size}>`
pub fn list(args: &[Val]) -> HotResult<Val> {
    if !args.is_empty() {
        return HotResult::Err(Val::from(
            "::hot::resource/list takes no arguments".to_string(),
        ));
    }
    let registry = get_registry();
    let entries: Vec<Val> = registry.entries.values().map(entry_to_val).collect();
    HotResult::Ok(Val::Vec(entries))
}

/// `::hot::resource/list-matching(glob: Str): Vec<{path, hash, size}>`
pub fn list_matching(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::resource/list-matching", args, 1);
    let pattern = match rel_arg("::hot::resource/list-matching", &args[0]) {
        Ok(s) => s,
        Err(e) => return HotResult::Err(e),
    };
    let mut builder = ignore::overrides::OverrideBuilder::new(Path::new("."));
    if let Err(e) = builder.add(&pattern) {
        return HotResult::Err(Val::from(format!(
            "::hot::resource/list-matching: invalid glob '{}': {}",
            pattern, e
        )));
    }
    let matcher = match builder.build() {
        Ok(m) => m,
        Err(e) => {
            return HotResult::Err(Val::from(format!(
                "::hot::resource/list-matching: glob compile failed: {}",
                e
            )));
        }
    };
    let registry = get_registry();
    let mut out = Vec::new();
    for entry in registry.entries.values() {
        let m = matcher.matched(&entry.rel_path, false);
        if m.is_whitelist() {
            out.push(entry_to_val(entry));
        }
    }
    HotResult::Ok(Val::Vec(out))
}

#[cfg(test)]
mod tests {
    use super::*;
    use parking_lot::Mutex;
    use std::io::Write;

    /// Serialize tests that touch the process-global registry.
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn setup_tmp() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let res_root = dir.path().join("res");
        std::fs::create_dir_all(res_root.join("nested")).unwrap();
        let mut f = std::fs::File::create(res_root.join("hello.txt")).unwrap();
        f.write_all(b"world").unwrap();
        let mut f = std::fs::File::create(res_root.join("nested/deep.md")).unwrap();
        f.write_all(b"# nested").unwrap();
        dir
    }

    #[test]
    fn test_build_registry_basic() {
        let dir = setup_tmp();
        let registry = build_registry(&[dir.path().join("res")], false, &[]);
        assert_eq!(registry.entries.len(), 2);
        assert!(registry.entries.contains_key("hello.txt"));
        assert!(registry.entries.contains_key("nested/deep.md"));
    }

    #[test]
    fn test_load_and_load_str() {
        let _g = TEST_LOCK.lock();
        let dir = setup_tmp();
        set_registry(build_registry(&[dir.path().join("res")], false, &[]));

        let bytes = load(&[Val::from("hello.txt")]);
        match bytes {
            HotResult::Ok(Val::Bytes(b)) => assert_eq!(b, b"world".to_vec()),
            other => panic!("unexpected: {:?}", other),
        }

        let s = load_str(&[Val::from("nested/deep.md")]);
        match s {
            HotResult::Ok(Val::Str(s)) => assert_eq!(&*s, "# nested"),
            other => panic!("unexpected: {:?}", other),
        }

        clear_registry();
    }

    #[test]
    fn test_exists_and_path() {
        let _g = TEST_LOCK.lock();
        let dir = setup_tmp();
        set_registry(build_registry(&[dir.path().join("res")], false, &[]));

        match exists(&[Val::from("hello.txt")]) {
            HotResult::Ok(Val::Bool(true)) => {}
            other => panic!("unexpected: {:?}", other),
        }
        match exists(&[Val::from("nope.txt")]) {
            HotResult::Ok(Val::Bool(false)) => {}
            other => panic!("unexpected: {:?}", other),
        }
        match path(&[Val::from("hello.txt")]) {
            HotResult::Ok(Val::Str(s)) => assert_eq!(&*s, "hot://res/hello.txt"),
            other => panic!("unexpected: {:?}", other),
        }

        clear_registry();
    }

    #[test]
    fn test_list_and_list_matching() {
        let _g = TEST_LOCK.lock();
        let dir = setup_tmp();
        set_registry(build_registry(&[dir.path().join("res")], false, &[]));

        match list(&[]) {
            HotResult::Ok(Val::Vec(v)) => assert_eq!(v.len(), 2),
            other => panic!("unexpected: {:?}", other),
        }
        match list_matching(&[Val::from("**/*.md")]) {
            HotResult::Ok(Val::Vec(v)) => {
                assert_eq!(v.len(), 1);
            }
            other => panic!("unexpected: {:?}", other),
        }

        clear_registry();
    }

    #[test]
    fn test_conflict_first_wins() {
        let dir = tempfile::tempdir().unwrap();
        let r1 = dir.path().join("a");
        let r2 = dir.path().join("b");
        std::fs::create_dir_all(&r1).unwrap();
        std::fs::create_dir_all(&r2).unwrap();
        std::fs::write(r1.join("dup.txt"), b"first").unwrap();
        std::fs::write(r2.join("dup.txt"), b"second").unwrap();
        let registry = build_registry(&[r1.clone(), r2.clone()], false, &[]);
        assert_eq!(registry.entries.len(), 1);
        assert_eq!(registry.conflicts.len(), 1);
        assert_eq!(
            std::fs::read(&registry.entries["dup.txt"].abs_path).unwrap(),
            b"first"
        );
    }

    #[test]
    fn test_excludes() {
        let dir = setup_tmp();
        let registry = build_registry(&[dir.path().join("res")], false, &["**/*.md".to_string()]);
        assert_eq!(registry.entries.len(), 1);
        assert!(registry.entries.contains_key("hello.txt"));
        assert!(!registry.entries.contains_key("nested/deep.md"));
    }

    #[test]
    fn test_load_missing() {
        let _g = TEST_LOCK.lock();
        clear_registry();
        match load(&[Val::from("nope.txt")]) {
            HotResult::Err(_) => {}
            other => panic!("expected err, got: {:?}", other),
        }
    }

    #[test]
    fn test_thread_registry_overrides_global() {
        let _g = TEST_LOCK.lock();
        clear_registry();

        let dir = setup_tmp();
        let registry = Arc::new(build_registry(&[dir.path().join("res")], false, &[]));

        match load(&[Val::from("hello.txt")]) {
            HotResult::Err(_) => {}
            other => panic!("expected err before install, got: {:?}", other),
        }

        let _guard = ThreadRegistryGuard::install(registry.clone());
        match load(&[Val::from("hello.txt")]) {
            HotResult::Ok(Val::Bytes(b)) => assert_eq!(b, b"world".to_vec()),
            other => panic!("expected ok via thread-local, got: {:?}", other),
        }
        drop(_guard);

        match load(&[Val::from("hello.txt")]) {
            HotResult::Err(_) => {}
            other => panic!("expected err after guard drop, got: {:?}", other),
        }

        clear_registry();
    }

    #[test]
    fn test_build_registry_cache_roundtrip() {
        let _g = TEST_LOCK.lock();
        clear_registry();

        let dir = setup_tmp();
        let registry = Arc::new(build_registry(&[dir.path().join("res")], false, &[]));

        let build_id = Uuid::new_v4();
        set_build_registry(build_id, registry.clone());

        let fetched = get_build_registry(&build_id).expect("registry present");
        assert_eq!(fetched.entries.len(), 2);

        let removed = remove_build_registry(&build_id).expect("removable");
        assert_eq!(removed.entries.len(), 2);
        assert!(get_build_registry(&build_id).is_none());

        clear_registry();
    }
}
