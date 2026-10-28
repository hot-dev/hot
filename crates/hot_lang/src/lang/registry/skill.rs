//! Skill specs (`meta {skill: ...}`) the compiler harvests for
//! `::hot::internal::skill/*`.
//!
//! The compiler fills this registry while it compiles; the runtime natives in
//! `hot` read it. Execution scopes install an exact registry per thread with
//! [`ThreadRegistryGuard`]; the process-global one serves standalone tooling.

use crate::val::Val;
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

pub fn with_registry<T>(read: impl FnOnce(&SkillSpecRegistry) -> T) -> T {
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
