// Hidden Rust-backed bindings for `::hot::internal::skill/*`.
//
// Mirrors `internal_mcp.rs` but for `meta {skill: {...}}` annotations.
// The compiler walks every Hot fn def, harvests the skill meta as a
// raw `Val::Map`, and parks it in an execution-scoped registry keyed by
// fully qualified function name. `::ai::skill/from-fn` consults this
// registry to reconstruct a `Skill` record at runtime without
// re-parsing source.
//
// Like `internal_mcp`, this namespace is intentionally undocumented
// (no-doc) and is meant to be called only from the `::ai::skill`
// Hot module.

use crate::lang::hot::r#type::HotResult;
use crate::lang::runtime::function_ref::extract_function_ref;
use crate::val::Val;
use crate::validate_args;
use indexmap::IndexMap;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::{Arc, OnceLock};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillSpecEntry {
    /// Fully-qualified function name, e.g. `"::myapp/customer-tone"`.
    pub name: String,
    /// Raw `meta {skill: {...}}` map captured by the compiler.
    #[serde(with = "crate::lang::cache::ast_cache::tagged_val_serde")]
    pub skill_meta: Val,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct SkillSpecRegistry {
    pub entries: IndexMap<String, SkillSpecEntry>,
}

static REGISTRY: OnceLock<RwLock<SkillSpecRegistry>> = OnceLock::new();

fn registry_lock() -> &'static RwLock<SkillSpecRegistry> {
    REGISTRY.get_or_init(|| RwLock::new(SkillSpecRegistry::default()))
}

thread_local! {
    static THREAD_REGISTRY: RefCell<Option<Arc<SkillSpecRegistry>>> = const { RefCell::new(None) };
}

/// Exact execution metadata, restored on return or unwind. Even an empty
/// registry is authoritative and must never fall back to another build.
/// The guard cannot move between threads because its authority is thread-local.
pub struct ThreadRegistryGuard {
    previous: Option<Arc<SkillSpecRegistry>>,
    _thread_bound: PhantomData<Rc<()>>,
}

impl ThreadRegistryGuard {
    pub fn install(registry: Arc<SkillSpecRegistry>) -> Self {
        Self {
            previous: THREAD_REGISTRY.with(|slot| slot.replace(Some(registry))),
            _thread_bound: PhantomData,
        }
    }
}

impl Drop for ThreadRegistryGuard {
    fn drop(&mut self) {
        THREAD_REGISTRY.with(|slot| *slot.borrow_mut() = self.previous.take());
    }
}

fn with_registry<T>(read: impl FnOnce(&SkillSpecRegistry) -> T) -> T {
    let scoped = THREAD_REGISTRY.with(|slot| slot.borrow().clone());
    match scoped {
        Some(registry) => read(&registry),
        None => read(&registry_lock().read()),
    }
}

fn update_registry(update: impl FnOnce(&mut SkillSpecRegistry)) {
    let mut update = Some(update);
    THREAD_REGISTRY.with(|slot| {
        if let Some(registry) = slot.borrow_mut().as_mut() {
            update.take().unwrap()(Arc::make_mut(registry));
        }
    });
    if let Some(update) = update {
        update(&mut registry_lock().write());
    }
}

/// Merge metadata into the current scope. The process-global fallback is for
/// standalone compiler tooling; runtime entry points install exact registries.
pub fn set_registry(registry: SkillSpecRegistry) {
    update_registry(|current| current.entries.extend(registry.entries));
}

/// Replace the current scope's metadata (or the standalone tooling registry).
pub fn replace_registry(registry: SkillSpecRegistry) {
    update_registry(|current| *current = registry);
}

pub fn get_registry() -> SkillSpecRegistry {
    with_registry(Clone::clone)
}

pub fn put_entry(entry: SkillSpecEntry) {
    update_registry(|current| {
        current.entries.insert(entry.name.clone(), entry);
    });
}

pub fn clear_registry() {
    replace_registry(SkillSpecRegistry::default());
}

fn extract_fn_name(val: &Val) -> Option<String> {
    if let Some(fr) = extract_function_ref(val) {
        return Some(fr.name.clone());
    }
    if let Val::Str(s) = val {
        return Some((**s).to_owned());
    }
    if let Val::Map(m) = val
        && let Some(Val::Str(tn)) = m.get(&Val::from("$type"))
        && &**tn == "::hot::type/Fn"
        && let Some(inner) = m.get(&Val::from("$val"))
    {
        return extract_fn_name(inner);
    }
    None
}

/// `::hot::internal::skill/meta-from-fn(f: Fn): Map`
///
/// Returns the raw `meta {skill: {...}}` map associated with `f`,
/// merged with `{name: <fq-fn-name>}` for convenience. Returns an
/// `Err` when `f` has no `meta {skill: ...}` annotation.
pub fn meta_from_fn(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::internal::skill/meta-from-fn", args, 1);
    let name = match extract_fn_name(&args[0]) {
        Some(n) => n,
        None => {
            return HotResult::Err(Val::from(
                "::hot::internal::skill/meta-from-fn: argument must be a function reference \
                 or a fully qualified function name string"
                    .to_string(),
            ));
        }
    };
    with_registry(|g| match g.entries.get(&name) {
        Some(entry) => {
            // Always include the canonical Hot fn name so callers can
            // round-trip a Skill back to its source fn even when no
            // `name:` was provided in the annotation.
            let mut merged: IndexMap<Val, Val> = match &entry.skill_meta {
                Val::Map(m) => (**m).clone(),
                _ => IndexMap::new(),
            };
            merged
                .entry(Val::from("fn-name"))
                .or_insert_with(|| Val::from(entry.name.as_str()));
            HotResult::Ok(Val::Map(Box::new(merged)))
        }
        None => HotResult::Err(Val::from(format!(
            "::hot::internal::skill/meta-from-fn: no skill registered for '{}' (function may \
             have no `meta {{skill: ...}}` annotation)",
            name
        ))),
    })
}

/// `::hot::internal::skill/all-skill-metas(): Vec<Map>`
///
/// Returns every registered skill meta as a list of maps. Used by
/// `::ai::skill/all` and any admin tooling that needs to enumerate
/// known skills.
pub fn all_skill_metas(args: &[Val]) -> HotResult<Val> {
    if !args.is_empty() {
        return HotResult::Err(Val::from(
            "::hot::internal::skill/all-skill-metas takes no arguments".to_string(),
        ));
    }
    with_registry(|g| {
        let out: Vec<Val> = g
            .entries
            .values()
            .map(|e| {
                let mut merged: IndexMap<Val, Val> = match &e.skill_meta {
                    Val::Map(m) => (**m).clone(),
                    _ => IndexMap::new(),
                };
                merged
                    .entry(Val::from("fn-name"))
                    .or_insert_with(|| Val::from(e.name.as_str()));
                Val::Map(Box::new(merged))
            })
            .collect();
        HotResult::Ok(Val::Vec(out))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::runtime::function_ref::function_ref;
    use parking_lot::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn fixture_entry(name: &str, desc: &str) -> SkillSpecEntry {
        let mut m = IndexMap::new();
        m.insert(Val::from("description"), Val::from(desc));
        m.insert(Val::from("when"), Val::Vec(vec![Val::from("trigger")]));
        SkillSpecEntry {
            name: name.to_string(),
            skill_meta: Val::Map(Box::new(m)),
        }
    }

    #[test]
    fn test_meta_from_fn_lookup() {
        let _g = TEST_LOCK.lock();
        clear_registry();
        put_entry(fixture_entry("::demo/tone", "How we talk"));
        let v = meta_from_fn(&[function_ref("::demo/tone".to_string())]);
        match v {
            HotResult::Ok(Val::Map(m)) => {
                assert_eq!(
                    m.get(&Val::from("description")),
                    Some(&Val::from("How we talk"))
                );
                assert_eq!(
                    m.get(&Val::from("fn-name")),
                    Some(&Val::from("::demo/tone"))
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
        clear_registry();
    }

    #[test]
    fn test_meta_from_fn_unknown() {
        let _g = TEST_LOCK.lock();
        clear_registry();
        match meta_from_fn(&[function_ref("::nope/nope".to_string())]) {
            HotResult::Err(_) => {}
            other => panic!("expected err, got: {:?}", other),
        }
    }

    #[test]
    fn test_all_skill_metas() {
        let _g = TEST_LOCK.lock();
        clear_registry();
        put_entry(fixture_entry("::demo/a", "A"));
        put_entry(fixture_entry("::demo/b", "B"));
        match all_skill_metas(&[]) {
            HotResult::Ok(Val::Vec(v)) => assert_eq!(v.len(), 2),
            other => panic!("unexpected: {:?}", other),
        }
        clear_registry();
    }

    #[test]
    fn scoped_mutations_restore_outer_registry() {
        let mut outer = SkillSpecRegistry::default();
        outer.entries.insert(
            "::outer/skill".into(),
            fixture_entry("::outer/skill", "outer"),
        );
        let outer = Arc::new(outer);
        let _outer = ThreadRegistryGuard::install(outer.clone());
        {
            let _inner = ThreadRegistryGuard::install(Arc::new(SkillSpecRegistry::default()));
            assert!(matches!(
                meta_from_fn(&[Val::from("::outer/skill")]),
                HotResult::Err(_)
            ));
            put_entry(fixture_entry("::inner/one", "one"));
            assert!(get_registry().entries.contains_key("::inner/one"));
            let mut additional = SkillSpecRegistry::default();
            additional
                .entries
                .insert("::inner/two".into(), fixture_entry("::inner/two", "two"));
            set_registry(additional);
            assert_eq!(get_registry().entries.len(), 2);
            replace_registry((*outer).clone());
            assert!(!get_registry().entries.contains_key("::inner/one"));
            clear_registry();
            assert!(get_registry().entries.is_empty());
        }
        assert_eq!(get_registry().entries.len(), 1);
        assert!(get_registry().entries.contains_key("::outer/skill"));
    }
}
