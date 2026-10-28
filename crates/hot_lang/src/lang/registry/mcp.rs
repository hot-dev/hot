//! Tool specs (`meta {tool: ...}` / `meta {mcp: ...}`) the compiler harvests for
//! `::hot::internal::mcp/*`.
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

/// One entry in the tool-spec registry: schema + optional metadata
/// overrides harvested from `meta {tool:}` / `meta {mcp:}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolSchemaEntry {
    /// Fully-qualified function name, e.g. `"::myapp/search"`. Always
    /// the canonical key used to look up the entry.
    pub name: String,
    /// JSON-Schema input schema as a Hot Val, derived from typed
    /// parameters via `args_to_input_schema_with_registry`.
    #[serde(with = "crate::lang::cache::ast_cache::tagged_val_serde")]
    pub input_schema: Val,
    /// Optional output schema (only when a return type was annotated).
    #[serde(with = "crate::lang::cache::ast_cache::tagged_val_opt_serde")]
    pub output_schema: Option<Val>,
    /// Description chain: `meta {tool: {description}}` ->
    /// `meta {mcp: {description}}` -> `meta {doc: ...}` -> None.
    pub description: Option<String>,
    /// Optional display/dispatch name override from
    /// `meta {tool: {name}}` or `meta {mcp: {name}}`.
    /// Lookup is still keyed by `name` (the qualified Hot fn name);
    /// `display_name` is what `::ai::tool/from-fn` advertises to LLMs.
    pub display_name: Option<String>,
}

/// `ToolSpecEntry` is the new preferred name for `ToolSchemaEntry`.
/// Re-exported alias kept so downstream modules can adopt the
/// generalized terminology incrementally without a breaking rename.
pub type ToolSpecEntry = ToolSchemaEntry;

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct ToolSchemaRegistry {
    pub entries: IndexMap<String, ToolSchemaEntry>,
}

/// Generalized alias matching the new `extract_tool_specs` terminology.
pub type ToolSpecRegistry = ToolSchemaRegistry;

static REGISTRY: OnceLock<RwLock<ToolSchemaRegistry>> = OnceLock::new();

fn registry_lock() -> &'static RwLock<ToolSchemaRegistry> {
    REGISTRY.get_or_init(|| RwLock::new(ToolSchemaRegistry::default()))
}

thread_local! {
    static THREAD_REGISTRY: RefCell<Option<Arc<ToolSchemaRegistry>>> = const { RefCell::new(None) };
}

/// Exact execution metadata, restored on return or unwind. Even an empty
/// registry is authoritative and must never fall back to another build.
/// The guard cannot move between threads because its authority is thread-local.
pub struct ThreadRegistryGuard {
    previous: Option<Arc<ToolSchemaRegistry>>,
    _thread_bound: PhantomData<Rc<()>>,
}

impl ThreadRegistryGuard {
    pub fn install(registry: Arc<ToolSchemaRegistry>) -> Self {
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

pub fn with_registry<T>(read: impl FnOnce(&ToolSchemaRegistry) -> T) -> T {
    let scoped = THREAD_REGISTRY.with(|slot| slot.borrow().clone());
    match scoped {
        Some(registry) => read(&registry),
        None => read(&registry_lock().read()),
    }
}

fn update_registry(update: impl FnOnce(&mut ToolSchemaRegistry)) {
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
pub fn set_registry(registry: ToolSchemaRegistry) {
    update_registry(|current| current.entries.extend(registry.entries));
}

/// Replace the current scope's metadata (or the standalone tooling registry).
pub fn replace_registry(registry: ToolSchemaRegistry) {
    update_registry(|current| *current = registry);
}

pub fn get_registry() -> ToolSchemaRegistry {
    with_registry(Clone::clone)
}

pub fn put_entry(entry: ToolSchemaEntry) {
    update_registry(|current| {
        current.entries.insert(entry.name.clone(), entry);
    });
}

pub fn clear_registry() {
    replace_registry(ToolSchemaRegistry::default());
}
