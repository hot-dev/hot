// Hidden Rust-backed bindings for `::hot::internal::mcp/*`.
//
// These are intentionally undocumented (no-doc) and are meant to be
// called only from the `::ai::tool` Hot module. They expose the
// existing JSON Schema generation in `crates/hot/src/lang/json_schema.rs`
// to Hot code so that there is a single source of truth for tool
// schemas at compile time and at runtime.
//
// Because Hot functions are dropped to bytecode + a function table at
// runtime, schemas are computed at compile time (by the compiler) and
// stored in an execution-scoped registry keyed by fully qualified
// function name (`::ns/name`). At runtime, `::hot::internal::mcp/schema-from-fn`
// receives a `FunctionRef`, extracts the name, and returns the cached
// `{input-schema: ..., output-schema: ...}` map.

use crate::lang::hot::r#type::HotResult;
use crate::lang::runtime::function_ref::{FunctionRef, extract_function_ref};
use crate::val::Val;
use crate::validate_args;
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

fn with_registry<T>(read: impl FnOnce(&ToolSchemaRegistry) -> T) -> T {
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

fn entry_to_val(entry: &ToolSchemaEntry) -> Val {
    let mut map = IndexMap::new();
    map.insert(Val::from("name"), Val::from(entry.name.as_str()));
    map.insert(Val::from("input-schema"), entry.input_schema.clone());
    map.insert(
        Val::from("output-schema"),
        entry.output_schema.clone().unwrap_or(Val::Null),
    );
    if let Some(d) = &entry.description {
        map.insert(Val::from("description"), Val::from(d.as_str()));
    }
    if let Some(dn) = &entry.display_name {
        map.insert(Val::from("display-name"), Val::from(dn.as_str()));
    }
    Val::Map(Box::new(map))
}

/// `::hot::internal::mcp/schema-from-fn(f: Fn): Map`
///
/// Returns `{name, input-schema, output-schema, description?}` for the
/// given function reference, or an Err if the function is unknown to
/// the compile-time registry (e.g. the function has no typed
/// parameters and no annotation).
pub fn schema_from_fn(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::internal::mcp/schema-from-fn", args, 1);
    let name = match extract_function_ref(&args[0]) {
        Some(fr) => fr.name.clone(),
        None => match &args[0] {
            Val::Str(s) => (**s).to_owned(),
            _ => {
                return HotResult::Err(Val::from(
                    "::hot::internal::mcp/schema-from-fn: argument must be a function reference \
                     or a fully qualified function name string"
                        .to_string(),
                ));
            }
        },
    };
    with_registry(|g| match g.entries.get(&name) {
        Some(entry) => HotResult::Ok(entry_to_val(entry)),
        None => HotResult::Err(Val::from(format!(
            "::hot::internal::mcp/schema-from-fn: no schema registered for '{}' (function may \
             have no typed parameters or may not have been compiled)",
            name
        ))),
    })
}

/// Look up a function's input-schema parameter names in the same order
/// they appear in the function signature.
fn input_param_names(name: &str) -> Option<Vec<String>> {
    with_registry(|g| {
        let entry = g.entries.get(name)?;
        let Val::Map(schema) = &entry.input_schema else {
            return None;
        };
        let Val::Map(props) = schema.get(&Val::from("properties"))? else {
            return None;
        };
        let mut names = Vec::with_capacity(props.len());
        for (k, _) in props.iter() {
            if let Val::Str(s) = k {
                names.push((**s).to_owned());
            }
        }
        Some(names)
    })
}

fn extract_fn_name(val: &Val) -> Option<String> {
    if let Some(fr) = extract_function_ref(val) {
        return Some(fr.name.clone());
    }
    if let Val::Str(s) = val {
        return Some((**s).to_owned());
    }
    // Unwrap typed Fn shape: {"$type": "::hot::type/Fn", "$val": inner}
    if let Val::Map(m) = val
        && let Some(Val::Str(tn)) = m.get(&Val::from("$type"))
        && &**tn == "::hot::type/Fn"
        && let Some(inner) = m.get(&Val::from("$val"))
    {
        return extract_fn_name(inner);
    }
    None
}

/// `::hot::internal::mcp/invoke-with-input(f: Fn, input: Map): Any`
///
/// Reorders fields from `input` (a Map keyed by parameter name) into
/// positional arguments matching the function's signature, then
/// dispatches to the function via the VM. Missing params are passed
/// as `null`. Extra fields in `input` are ignored.
pub fn invoke_with_input(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::internal::mcp/invoke-with-input expects (f, input), got {} args",
            args.len()
        )));
    }
    let fn_val = &args[0];
    let input = &args[1];

    let input_map = match input {
        Val::Map(m) => m.clone(),
        Val::Null => Box::new(IndexMap::new()),
        other => {
            return HotResult::Err(Val::from(format!(
                "::hot::internal::mcp/invoke-with-input: input must be a Map (got {:?})",
                other
            )));
        }
    };

    // Lambda path: anonymous closures (e.g. tools whose `fn` field is
    // an inline lambda like `(name: Str): Str { ... }`). LambdaInfo
    // carries the parameter names directly, so reorder by those.
    if let Some(lambda) = unwrap_lambda(fn_val) {
        let positional: Vec<Val> = lambda
            .parameters
            .iter()
            .map(|n| {
                input_map
                    .get(&Val::from(n.as_str()))
                    .cloned()
                    .unwrap_or(Val::Null)
            })
            .collect();
        return match crate::lang::hot::coll::call_function_with_vm_multi_args(
            vm,
            lambda.value,
            &positional,
        ) {
            Ok(v) => HotResult::Ok(v),
            Err(e) => HotResult::Err(Val::from(format!(
                "::hot::internal::mcp/invoke-with-input: {}",
                e
            ))),
        };
    }

    let fn_name = match extract_fn_name(fn_val) {
        Some(n) => n,
        None => {
            return HotResult::Err(Val::from(
                "::hot::internal::mcp/invoke-with-input: first argument must be a function \
                 reference, function name, typed Fn, or lambda"
                    .to_string(),
            ));
        }
    };

    let positional: Vec<Val> = match input_param_names(&fn_name) {
        Some(names) => names
            .into_iter()
            .map(|n| {
                input_map
                    .get(&Val::from(n.as_str()))
                    .cloned()
                    .unwrap_or(Val::Null)
            })
            .collect(),
        None => {
            // No registered schema: fall back to passing the map as a
            // single argument (back-compat for fns declared as
            // `(input: Map): Any`).
            vec![Val::Map(input_map)]
        }
    };

    let f_for_call = Val::Box(Box::new(FunctionRef::new(fn_name)));
    match crate::lang::hot::coll::call_function_with_vm_multi_args(vm, &f_for_call, &positional) {
        Ok(v) => HotResult::Ok(v),
        Err(e) => HotResult::Err(Val::from(format!(
            "::hot::internal::mcp/invoke-with-input: {}",
            e
        ))),
    }
}

struct UnwrappedLambda<'a> {
    parameters: Vec<String>,
    /// The actual Val to dispatch — either the original `fn_val` (if it
    /// is itself a Box<LambdaInfo>) or the inner `$val` of a typed-Fn
    /// wrapper. Always something `call_function_with_vm_multi_args`
    /// will recognize as a lambda.
    value: &'a Val,
}

fn unwrap_lambda(val: &Val) -> Option<UnwrappedLambda<'_>> {
    if let Val::Box(boxed) = val
        && let Some(lambda) = boxed
            .as_any()
            .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
    {
        return Some(UnwrappedLambda {
            parameters: lambda.parameters.clone(),
            value: val,
        });
    }
    if let Val::Map(m) = val
        && let Some(Val::Str(tn)) = m.get(&Val::from("$type"))
        && &**tn == "::hot::type/Fn"
        && let Some(inner) = m.get(&Val::from("$val"))
    {
        return unwrap_lambda(inner);
    }
    None
}

/// `::hot::internal::mcp/all-tool-schemas(): Vec<Map>`
///
/// Returns every entry in the registry as a vector of maps. Useful for
/// listing tools in agent boot or admin tooling.
pub fn all_tool_schemas(args: &[Val]) -> HotResult<Val> {
    if !args.is_empty() {
        return HotResult::Err(Val::from(
            "::hot::internal::mcp/all-tool-schemas takes no arguments".to_string(),
        ));
    }
    with_registry(|g| {
        let out: Vec<Val> = g.entries.values().map(entry_to_val).collect();
        HotResult::Ok(Val::Vec(out))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lang::runtime::function_ref::function_ref;
    use parking_lot::Mutex;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn fixture_entry(name: &str) -> ToolSchemaEntry {
        let mut props = IndexMap::new();
        props.insert(Val::from("query"), {
            let mut p = IndexMap::new();
            p.insert(Val::from("type"), Val::from("string"));
            Val::Map(Box::new(p))
        });
        let mut input_schema = IndexMap::new();
        input_schema.insert(Val::from("type"), Val::from("object"));
        input_schema.insert(Val::from("properties"), Val::Map(Box::new(props)));
        ToolSchemaEntry {
            name: name.to_string(),
            input_schema: Val::Map(Box::new(input_schema)),
            output_schema: None,
            description: Some("a search tool".to_string()),
            display_name: None,
        }
    }

    #[test]
    fn test_schema_from_fn_lookup() {
        let _g = TEST_LOCK.lock();
        clear_registry();
        put_entry(fixture_entry("::demo/search"));
        let v = schema_from_fn(&[function_ref("::demo/search".to_string())]);
        match v {
            HotResult::Ok(Val::Map(m)) => {
                assert_eq!(m.get(&Val::from("name")), Some(&Val::from("::demo/search")));
                assert!(m.get(&Val::from("input-schema")).is_some());
                assert_eq!(
                    m.get(&Val::from("description")),
                    Some(&Val::from("a search tool"))
                );
            }
            other => panic!("unexpected: {:?}", other),
        }
        clear_registry();
    }

    #[test]
    fn test_schema_from_fn_str_arg() {
        let _g = TEST_LOCK.lock();
        clear_registry();
        put_entry(fixture_entry("::demo/search"));
        let v = schema_from_fn(&[Val::from("::demo/search")]);
        match v {
            HotResult::Ok(Val::Map(_)) => {}
            other => panic!("unexpected: {:?}", other),
        }
        clear_registry();
    }

    #[test]
    fn test_schema_from_fn_unknown() {
        let _g = TEST_LOCK.lock();
        clear_registry();
        match schema_from_fn(&[function_ref("::nope/nope".to_string())]) {
            HotResult::Err(_) => {}
            other => panic!("expected err, got: {:?}", other),
        }
    }

    #[test]
    fn test_all_tool_schemas() {
        let _g = TEST_LOCK.lock();
        clear_registry();
        put_entry(fixture_entry("::demo/search"));
        put_entry(fixture_entry("::demo/read"));
        match all_tool_schemas(&[]) {
            HotResult::Ok(Val::Vec(v)) => assert_eq!(v.len(), 2),
            other => panic!("unexpected: {:?}", other),
        }
        clear_registry();
    }

    #[test]
    fn scoped_mutations_restore_outer_registry() {
        let mut outer = ToolSchemaRegistry::default();
        outer
            .entries
            .insert("::outer/tool".into(), fixture_entry("::outer/tool"));
        let outer = Arc::new(outer);
        let _outer = ThreadRegistryGuard::install(outer.clone());
        {
            let _inner = ThreadRegistryGuard::install(Arc::new(ToolSchemaRegistry::default()));
            assert!(matches!(
                schema_from_fn(&[Val::from("::outer/tool")]),
                HotResult::Err(_)
            ));
            put_entry(fixture_entry("::inner/one"));
            assert!(get_registry().entries.contains_key("::inner/one"));
            let mut additional = ToolSchemaRegistry::default();
            additional
                .entries
                .insert("::inner/two".into(), fixture_entry("::inner/two"));
            set_registry(additional);
            assert_eq!(get_registry().entries.len(), 2);
            replace_registry((*outer).clone());
            assert!(!get_registry().entries.contains_key("::inner/one"));
            clear_registry();
            assert!(get_registry().entries.is_empty());
        }
        assert_eq!(get_registry().entries.len(), 1);
        assert!(get_registry().entries.contains_key("::outer/tool"));
    }
}
