// Hot Library Function Map
//
// This module provides the centralized mapping of Hot library functions for the engine.

use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use std::sync::OnceLock;

// Function types for hotlib
type LibFnType = fn(&[Val]) -> HotResult<Val>;
type VmAwareFnType = fn(&mut crate::lang::runtime::vm::VirtualMachine, &[Val]) -> HotResult<Val>;

/// hotlib function types - encodes whether function needs VM access
#[derive(Clone)]
pub enum HotLibFn {
    /// Regular function that doesn't need VM access
    LibFn(LibFnType),
    /// VM-aware function that needs mutable access to the VM
    VmAwareFn(VmAwareFnType),
    /// VM-aware function with an explicit JIT safety policy.
    VmAwareJitFn(VmAwareFnType, HotLibJitPolicy),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HotLibJitPolicy {
    JitSafe,
    JitViaVmCallback,
    JitBailout,
}

impl HotLibFn {
    fn vm_callback(func: VmAwareFnType) -> Self {
        HotLibFn::VmAwareJitFn(func, HotLibJitPolicy::JitViaVmCallback)
    }

    pub fn jit_policy(&self) -> HotLibJitPolicy {
        match self {
            HotLibFn::LibFn(_) => HotLibJitPolicy::JitSafe,
            HotLibFn::VmAwareFn(_) => HotLibJitPolicy::JitBailout,
            HotLibFn::VmAwareJitFn(_, policy) => *policy,
        }
    }
}

// ---------------------------------------------------------------------------
// JIT classification metadata
//
// The JIT recognizes fusible higher-order functions and lowerable scalar ops by
// the declared capability metadata below, never by matching function-name
// strings in JIT logic. The registry is the single source of truth: a literal
// name appears only as the lookup key in these tables. Adding or renaming a
// hotlib updates one entry here and requires no JIT-side changes.
// ---------------------------------------------------------------------------

/// Semantic role a collection HOF plays inside a fusible pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HofStage {
    Map,
    Filter,
    Reduce,
    Some,
    All,
    Length,
}

impl HofStage {
    /// Terminal stages consume a stream and yield a scalar/aggregate; transform
    /// stages produce another stream.
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            HofStage::Reduce | HofStage::Some | HofStage::All | HofStage::Length
        )
    }
}

/// Whether a HOF runs sequentially (fusible) or in parallel (never fused).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HofExecution {
    Sequential,
    Parallel,
}

/// Declared pipeline capability of a collection HOF.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HofRole {
    pub stage: HofStage,
    pub execution: HofExecution,
}

/// A pure scalar operation the JIT can lower directly into Cranelift IR inside a
/// fused pipeline lambda.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PureOp {
    Add,
    Sub,
    Mul,
    Div,
    Mod,
    Neg,
    Abs,
    Min,
    Max,
    IsZero,
    Eq,
    Ne,
    Lt,
    Lte,
    Gt,
    Gte,
    Not,
    /// String/collection concatenation (`::hot::coll/concat`).
    Concat,
}

/// Declared lowering capability of a pure scalar op.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PureOpDescriptor {
    pub op: PureOp,
    /// Fully-qualified registry key of the backing hotlib `LibFn`, used by the
    /// fused evaluator to call the exact implementation for non-`Int` operands
    /// (guaranteeing parity with the interpreter).
    pub full_name: &'static str,
    /// True if the op traps on a zero divisor and therefore needs a runtime
    /// zero-check (or a proven non-zero divisor) before fusion.
    pub traps_on_zero_divisor: bool,
}

static HOF_ROLE_MAP: OnceLock<ahash::AHashMap<&'static str, HofRole>> = OnceLock::new();
static PURE_OP_MAP: OnceLock<ahash::AHashMap<&'static str, PureOpDescriptor>> = OnceLock::new();

/// Look up the pipeline role of a hotlib by its stable registry key.
pub fn hof_role(name: &str) -> Option<HofRole> {
    HOF_ROLE_MAP
        .get_or_init(|| {
            use HofExecution::*;
            use HofStage::*;
            let seq = |stage| HofRole {
                stage,
                execution: Sequential,
            };
            let mut m: ahash::AHashMap<&'static str, HofRole> = ahash::AHashMap::new();
            // Both the fully-qualified registry key and the bare name are mapped.
            // A bare name in a `Call` instruction is safe to treat as the core
            // hotlib: a user-defined function with the same name compiles to
            // `CallUserFunction` with a resolved id, so a bare `Call` name here
            // is the core function. This mirrors `known_core_call` in the JIT.
            m.insert("::hot::coll/map", seq(Map));
            m.insert("map", seq(Map));
            m.insert("::hot::coll/filter", seq(Filter));
            m.insert("filter", seq(Filter));
            m.insert("::hot::coll/reduce", seq(Reduce));
            m.insert("reduce", seq(Reduce));
            m.insert("::hot::coll/some", seq(Some));
            m.insert("some", seq(Some));
            m.insert("::hot::coll/all", seq(All));
            m.insert("all", seq(All));
            m.insert("::hot::coll/length", seq(Length));
            m.insert("length", seq(Length));
            // pmap is true-parallel and must never be fused.
            let parallel_map = HofRole {
                stage: Map,
                execution: Parallel,
            };
            m.insert("::hot::coll/pmap", parallel_map);
            m.insert("pmap", parallel_map);
            m
        })
        .get(name)
        .copied()
}

/// Look up the pure-op lowering descriptor of a hotlib by its stable registry key.
pub fn pure_op(name: &str) -> Option<PureOpDescriptor> {
    PURE_OP_MAP
        .get_or_init(|| {
            use PureOp::*;
            let d = |op, full_name: &'static str, traps_on_zero_divisor| PureOpDescriptor {
                op,
                full_name,
                traps_on_zero_divisor,
            };
            // Fully-qualified registry keys plus bare names (see hof_role for the
            // bare-name safety rationale). Both keys carry the same `full_name`
            // so the evaluator can fetch the backing `LibFn`.
            let entries: &[(&'static str, &'static str, PureOpDescriptor)] = &[
                ("::hot::math/add", "add", d(Add, "::hot::math/add", false)),
                ("::hot::math/sub", "sub", d(Sub, "::hot::math/sub", false)),
                ("::hot::math/mul", "mul", d(Mul, "::hot::math/mul", false)),
                ("::hot::math/div", "div", d(Div, "::hot::math/div", true)),
                ("::hot::math/mod", "mod", d(Mod, "::hot::math/mod", true)),
                ("::hot::math/abs", "abs", d(Abs, "::hot::math/abs", false)),
                ("::hot::math/min", "min", d(Min, "::hot::math/min", false)),
                ("::hot::math/max", "max", d(Max, "::hot::math/max", false)),
                (
                    "::hot::math/is-zero",
                    "is-zero",
                    d(IsZero, "::hot::math/is-zero", false),
                ),
                ("::hot::cmp/eq", "eq", d(Eq, "::hot::cmp/eq", false)),
                ("::hot::cmp/ne", "ne", d(Ne, "::hot::cmp/ne", false)),
                ("::hot::cmp/lt", "lt", d(Lt, "::hot::cmp/lt", false)),
                ("::hot::cmp/lte", "lte", d(Lte, "::hot::cmp/lte", false)),
                ("::hot::cmp/gt", "gt", d(Gt, "::hot::cmp/gt", false)),
                ("::hot::cmp/gte", "gte", d(Gte, "::hot::cmp/gte", false)),
                ("::hot::bool/not", "not", d(Not, "::hot::bool/not", false)),
                (
                    "::hot::coll/concat",
                    "concat",
                    d(Concat, "::hot::coll/concat", false),
                ),
            ];
            let mut m: ahash::AHashMap<&'static str, PureOpDescriptor> = ahash::AHashMap::new();
            for (full, bare, desc) in entries {
                m.insert(full, *desc);
                m.insert(bare, *desc);
            }
            // `modulo` is an alternate registry key for the same op.
            m.insert("::hot::math/modulo", d(Mod, "::hot::math/mod", true));
            m
        })
        .get(name)
        .copied()
}

pub type HotLibMap = ahash::AHashMap<String, HotLibFn>;

static HOTLIB_MAP: OnceLock<HotLibMap> = OnceLock::new();

/// Get the list of all hotlib function names
pub fn get_hotlib_functions() -> Vec<String> {
    get_hotlib_map().keys().cloned().collect()
}

/// Get the hotlib map with functions (cached, built only once)
/// Call a pure (non-VM) hotlib `LibFn` by its fully-qualified registry name.
///
/// Returns `None` when the name is not registered or is VM-aware (the caller
/// must then fall back to the interpreter). Used by the fused HOF evaluator to
/// run the exact registered implementation for operands it does not handle on
/// its inlined fast path, guaranteeing semantic parity.
pub fn call_pure_lib(
    full_name: &str,
    args: &[Val],
) -> Option<crate::lang::hot::r#type::HotResult<Val>> {
    match get_hotlib_map().get(full_name)? {
        HotLibFn::LibFn(f) => Some(f(args)),
        _ => None,
    }
}

pub fn get_hotlib_map() -> &'static HotLibMap {
    HOTLIB_MAP.get_or_init(|| {
        let mut map = ahash::AHashMap::new();

        // Import all the hotlib modules
        use crate::lang::hot::*;

        // Test assertions are implemented in Hot std; no override here

        // Collection functions (simplified versions)
        map.insert(
            "::hot::coll/count".to_string(),
            HotLibFn::LibFn(coll::count),
        );
        map.insert(
            "::hot::coll/first".to_string(),
            HotLibFn::LibFn(coll::first),
        );
        map.insert("::hot::coll/last".to_string(), HotLibFn::LibFn(coll::last));
        map.insert("::hot::coll/nth".to_string(), HotLibFn::LibFn(coll::nth));
        map.insert(
            "::hot::coll/first".to_string(),
            HotLibFn::LibFn(coll::first),
        );
        map.insert("::hot::coll/last".to_string(), HotLibFn::LibFn(coll::last));
        map.insert("::hot::coll/nth".to_string(), HotLibFn::LibFn(coll::nth));
        map.insert(
            "::hot::coll/mapcat".to_string(),
            HotLibFn::vm_callback(coll::mapcat),
        );
        map.insert(
            "::hot::coll/filter".to_string(),
            HotLibFn::vm_callback(coll::filter),
        );
        map.insert(
            "::hot::coll/map".to_string(),
            HotLibFn::vm_callback(coll::map),
        );
        map.insert(
            "::hot::coll/pmap".to_string(),
            HotLibFn::vm_callback(coll::pmap),
        );
        map.insert(
            "::hot::coll/map-indexed".to_string(),
            HotLibFn::vm_callback(coll::map_indexed),
        );
        map.insert(
            "::hot::coll/mapcat".to_string(),
            HotLibFn::vm_callback(coll::mapcat),
        );
        map.insert(
            "::hot::coll/length".to_string(),
            HotLibFn::LibFn(coll::length),
        );
        map.insert("::hot::coll/get".to_string(), HotLibFn::LibFn(coll::get));
        map.insert(
            "::hot::coll/get-in".to_string(),
            HotLibFn::LibFn(coll::get_in),
        );
        map.insert(
            "::hot::coll/assoc".to_string(),
            HotLibFn::LibFn(coll::assoc),
        );
        map.insert(
            "::hot::coll/assoc-in".to_string(),
            HotLibFn::LibFn(coll::assoc_in),
        );
        map.insert(
            "::hot::coll/update-in".to_string(),
            HotLibFn::vm_callback(coll::update_in),
        );
        map.insert(
            "::hot::coll/concat".to_string(),
            HotLibFn::LibFn(coll::concat),
        );
        map.insert(
            "::hot::coll/merge".to_string(),
            HotLibFn::LibFn(coll::merge),
        );
        map.insert(
            "::hot::coll/reduce".to_string(),
            HotLibFn::vm_callback(coll::reduce),
        );
        map.insert(
            "::hot::coll/some".to_string(),
            HotLibFn::vm_callback(coll::some),
        );
        map.insert(
            "::hot::coll/find_first".to_string(),
            HotLibFn::vm_callback(coll::find_first),
        );
        map.insert(
            "::hot::coll/remove".to_string(),
            HotLibFn::vm_callback(coll::remove),
        );
        map.insert(
            "::hot::coll/delete".to_string(),
            HotLibFn::LibFn(coll::delete),
        );
        map.insert("::hot::coll/rest".to_string(), HotLibFn::LibFn(coll::rest));
        map.insert(
            "::hot::coll/butlast".to_string(),
            HotLibFn::LibFn(coll::butlast),
        );
        map.insert(
            "::hot::coll/is-empty".to_string(),
            HotLibFn::LibFn(coll::is_empty),
        );
        map.insert(
            "::hot::coll/partition".to_string(),
            HotLibFn::vm_callback(coll::partition),
        );
        map.insert(
            "::hot::coll/partition-by".to_string(),
            HotLibFn::vm_callback(coll::partition_by),
        );
        map.insert(
            "::hot::coll/distinct".to_string(),
            HotLibFn::LibFn(coll::distinct),
        );
        map.insert(
            "::hot::coll/reverse".to_string(),
            HotLibFn::LibFn(coll::reverse),
        );
        map.insert("::hot::coll/sort".to_string(), HotLibFn::LibFn(coll::sort));
        map.insert(
            "::hot::coll/sort-by".to_string(),
            HotLibFn::vm_callback(coll::sort_by),
        );
        map.insert(
            "::hot::coll/shuffle".to_string(),
            HotLibFn::LibFn(coll::shuffle),
        );
        map.insert(
            "::hot::coll/interleave".to_string(),
            HotLibFn::LibFn(coll::interleave),
        );
        map.insert(
            "::hot::coll/interpose".to_string(),
            HotLibFn::LibFn(coll::interpose),
        );
        map.insert(
            "::hot::coll/flatten".to_string(),
            HotLibFn::LibFn(coll::flatten),
        );
        map.insert(
            "::hot::coll/zipmap".to_string(),
            HotLibFn::LibFn(coll::zipmap),
        );
        map.insert(
            "::hot::coll/all".to_string(),
            HotLibFn::vm_callback(coll::all),
        );
        map.insert(
            "::hot::coll/slice".to_string(),
            HotLibFn::LibFn(coll::slice),
        );
        map.insert(
            "::hot::str/slice".to_string(),
            HotLibFn::LibFn(coll::slice), // Alias for coll::slice
        );
        map.insert("::hot::coll/keys".to_string(), HotLibFn::LibFn(coll::keys));
        map.insert(
            "::hot::coll/values".to_string(),
            HotLibFn::LibFn(coll::values),
        );
        map.insert(
            "::hot::coll/vals".to_string(),
            HotLibFn::LibFn(coll::values), // Alias for values
        );
        map.insert(
            "::hot::coll/range".to_string(),
            HotLibFn::LibFn(coll::range),
        );
        map.insert(
            "::hot::coll/walk".to_string(),
            HotLibFn::vm_callback(coll::walk),
        );
        map.insert(
            "::hot::coll/prewalk".to_string(),
            HotLibFn::vm_callback(coll::prewalk),
        );
        map.insert(
            "::hot::coll/postwalk".to_string(),
            HotLibFn::vm_callback(coll::postwalk),
        );
        map.insert(
            "::hot::coll/postwalk-replace".to_string(),
            HotLibFn::vm_callback(coll::postwalk_replace),
        );

        // String functions
        map.insert("::hot::str/split".to_string(), HotLibFn::LibFn(str::split));
        map.insert("::hot::str/join".to_string(), HotLibFn::LibFn(str::join));
        map.insert("::hot::str/trim".to_string(), HotLibFn::LibFn(str::trim));
        map.insert(
            "::hot::str/lowercase".to_string(),
            HotLibFn::LibFn(str::lowercase),
        );
        map.insert(
            "::hot::str/uppercase".to_string(),
            HotLibFn::LibFn(str::uppercase),
        );
        map.insert(
            "::hot::str/contains".to_string(),
            HotLibFn::LibFn(str::contains),
        );
        map.insert(
            "::hot::str/starts-with".to_string(),
            HotLibFn::LibFn(str::starts_with),
        );
        map.insert(
            "::hot::str/ends-with".to_string(),
            HotLibFn::LibFn(str::ends_with),
        );

        // Environment functions
        map.insert(
            "::hot::env/get".to_string(),
            HotLibFn::vm_callback(env::get),
        );
        map.insert(
            "::hot::env/get-all".to_string(),
            HotLibFn::vm_callback(env::get_all),
        );

        // Result functions (now in ::hot::type)
        map.insert(
            "::hot::type/is-ok".to_string(),
            HotLibFn::vm_callback(r#type::result_is_ok),
        );
        map.insert(
            "::hot::type/is-err".to_string(),
            HotLibFn::vm_callback(r#type::result_is_err),
        );
        map.insert(
            "::hot::type/if-ok".to_string(),
            HotLibFn::vm_callback(r#type::result_if_ok),
        );
        map.insert(
            "::hot::type/if-err".to_string(),
            HotLibFn::vm_callback(r#type::result_if_err),
        );
        map.insert(
            "::hot::type/ok".to_string(),
            HotLibFn::LibFn(r#type::result_ok),
        );
        map.insert(
            "::hot::type/err".to_string(),
            HotLibFn::LibFn(r#type::result_err),
        );

        // HTTP functions
        map.insert(
            "::hot::http/request".to_string(),
            HotLibFn::LibFn(http::request),
        );
        map.insert("::hot::http/get".to_string(), HotLibFn::LibFn(http::get));
        map.insert("::hot::http/post".to_string(), HotLibFn::LibFn(http::post));
        map.insert("::hot::http/put".to_string(), HotLibFn::LibFn(http::put));
        map.insert(
            "::hot::http/delete".to_string(),
            HotLibFn::LibFn(http::delete),
        );
        map.insert(
            "::hot::http/request-stream".to_string(),
            HotLibFn::LibFn(http::request_stream),
        );

        // Iterator functions
        map.insert(
            "::hot::iter/next".to_string(),
            HotLibFn::vm_callback(iter::next),
        );
        map.insert(
            "::hot::iter/collect".to_string(),
            HotLibFn::vm_callback(iter::collect),
        );
        map.insert("::hot::iter/Iter".to_string(), HotLibFn::LibFn(iter::iter));
        map.insert(
            "::hot::iter/range".to_string(),
            HotLibFn::LibFn(iter::range),
        );

        // Event functions
        map.insert(
            "::hot::event/send".to_string(),
            HotLibFn::VmAwareFn(event::send_event),
        );
        map.insert(
            "::hot::event/listen".to_string(),
            HotLibFn::LibFn(event::listen),
        );
        map.insert(
            "::hot::event/create-event".to_string(),
            HotLibFn::LibFn(event::create_event),
        );

        // Alert functions
        map.insert(
            "::hot::alert/alert".to_string(),
            HotLibFn::VmAwareFn(alert::alert),
        );

        // Schedule functions (for hot:schedule:new and hot:schedule:cancel events)
        map.insert(
            "::hot::schedule/create".to_string(),
            HotLibFn::VmAwareFn(schedule::create_schedule),
        );
        map.insert(
            "::hot::schedule/cancel".to_string(),
            HotLibFn::VmAwareFn(schedule::cancel_schedule),
        );

        // String functions
        map.insert("::hot::str/split".to_string(), HotLibFn::LibFn(str::split));
        map.insert("::hot::str/join".to_string(), HotLibFn::LibFn(str::join));
        map.insert("::hot::str/trim".to_string(), HotLibFn::LibFn(str::trim));
        map.insert(
            "::hot::str/lowercase".to_string(),
            HotLibFn::LibFn(str::lowercase),
        );
        map.insert(
            "::hot::str/uppercase".to_string(),
            HotLibFn::LibFn(str::uppercase),
        );
        map.insert(
            "::hot::str/contains".to_string(),
            HotLibFn::LibFn(str::contains),
        );
        map.insert(
            "::hot::str/starts-with".to_string(),
            HotLibFn::LibFn(str::starts_with),
        );
        map.insert(
            "::hot::str/ends-with".to_string(),
            HotLibFn::LibFn(str::ends_with),
        );

        // IO functions
        map.insert("::hot::io/print".to_string(), HotLibFn::LibFn(io::print));
        map.insert(
            "::hot::io/println".to_string(),
            HotLibFn::LibFn(io::println),
        );
        map.insert(
            "::hot::io/print-line".to_string(),
            HotLibFn::LibFn(io::println),
        );
        map.insert("::hot::io/eprint".to_string(), HotLibFn::LibFn(io::eprint));
        map.insert(
            "::hot::io/eprintln".to_string(),
            HotLibFn::LibFn(io::eprintln),
        );
        map.insert(
            "::hot::io/capture-stdout".to_string(),
            HotLibFn::LibFn(io::capture_stdout),
        );
        map.insert(
            "::hot::io/capture-stderr".to_string(),
            HotLibFn::LibFn(io::capture_stderr),
        );
        map.insert(
            "::hot::io/release".to_string(),
            HotLibFn::LibFn(io::release),
        );
        map.insert(
            "::hot::io/discard".to_string(),
            HotLibFn::LibFn(io::discard),
        );
        map.insert(
            "::hot::io/get-captured-content".to_string(),
            HotLibFn::LibFn(io::get_captured_content),
        );
        map.insert(
            "::hot::io/clear-captured-content".to_string(),
            HotLibFn::LibFn(io::clear_captured_content),
        );
        map.insert("::hot::io/tap".to_string(), HotLibFn::LibFn(io::tap));
        // Add direct println alias for compatibility with hot-std
        map.insert("println".to_string(), HotLibFn::LibFn(io::println));

        // Language introspection functions (VM-aware)
        map.insert(
            "::hot::lang/namespaces".to_string(),
            HotLibFn::vm_callback(lang::namespaces),
        );
        map.insert(
            "::hot::lang/functions-in-namespace".to_string(),
            HotLibFn::vm_callback(lang::functions_in_namespace),
        );

        // Math functions
        map.insert("::hot::math/add".to_string(), HotLibFn::LibFn(math::add));
        map.insert("::hot::math/sub".to_string(), HotLibFn::LibFn(math::sub));
        map.insert("::hot::math/mul".to_string(), HotLibFn::LibFn(math::mul));
        map.insert("::hot::math/div".to_string(), HotLibFn::LibFn(math::div));
        map.insert(
            "::hot::math/modulo".to_string(),
            HotLibFn::LibFn(math::modulo),
        );
        map.insert("::hot::math/pow".to_string(), HotLibFn::LibFn(math::pow));
        map.insert("::hot::math/abs".to_string(), HotLibFn::LibFn(math::abs));
        map.insert("::hot::math/max".to_string(), HotLibFn::LibFn(math::max));
        map.insert("::hot::math/min".to_string(), HotLibFn::LibFn(math::min));
        map.insert("::hot::math/ceil".to_string(), HotLibFn::LibFn(math::ceil));
        map.insert(
            "::hot::math/floor".to_string(),
            HotLibFn::LibFn(math::floor),
        );
        map.insert(
            "::hot::math/round".to_string(),
            HotLibFn::LibFn(math::round),
        );
        map.insert("::hot::math/rand".to_string(), HotLibFn::LibFn(math::rand));
        map.insert(
            "::hot::math/is-zero".to_string(),
            HotLibFn::LibFn(math::is_zero),
        );

        // Execution control (context-aware fail/cancel)
        map.insert(
            "::hot::exec/fail".to_string(),
            HotLibFn::VmAwareFn(exec::fail),
        );
        map.insert(
            "::hot::exec/cancel".to_string(),
            HotLibFn::VmAwareFn(exec::cancel),
        );
        map.insert(
            "::hot::exec/exit".to_string(),
            HotLibFn::VmAwareFn(exec::exit),
        );
        map.insert(
            "::hot::run/info".to_string(),
            HotLibFn::vm_callback(run::info),
        );
        // Info functions
        map.insert(
            "::hot::info/version".to_string(),
            HotLibFn::LibFn(info::version),
        );

        // Stream functions
        map.insert(
            "::hot::stream/data".to_string(),
            HotLibFn::VmAwareFn(stream::data),
        );

        // Base64 functions
        map.insert(
            "::hot::base64/encode".to_string(),
            HotLibFn::LibFn(base64::encode),
        );
        map.insert(
            "::hot::base64/decode".to_string(),
            HotLibFn::LibFn(base64::decode),
        );
        map.insert(
            "::hot::base64/is-valid".to_string(),
            HotLibFn::LibFn(base64::is_valid),
        );
        map.insert(
            "::hot::base64/encode-url".to_string(),
            HotLibFn::LibFn(base64::encode_url),
        );
        map.insert(
            "::hot::base64/decode-url".to_string(),
            HotLibFn::LibFn(base64::decode_url),
        );

        // URI functions
        map.insert(
            "::hot::uri/Uri".to_string(),
            HotLibFn::LibFn(uri::uri_constructor),
        );
        map.insert(
            "::hot::uri/encode".to_string(),
            HotLibFn::LibFn(uri::encode),
        );
        map.insert(
            "::hot::uri/decode".to_string(),
            HotLibFn::LibFn(uri::decode),
        );
        map.insert(
            "::hot::uri/encode-query".to_string(),
            HotLibFn::LibFn(uri::encode_query),
        );
        map.insert(
            "::hot::uri/decode-query".to_string(),
            HotLibFn::LibFn(uri::decode_query),
        );
        map.insert("::hot::uri/parse".to_string(), HotLibFn::LibFn(uri::parse));
        map.insert("::hot::uri/build".to_string(), HotLibFn::LibFn(uri::build));
        map.insert("::hot::uri/join".to_string(), HotLibFn::LibFn(uri::join));
        map.insert(
            "::hot::uri/is-valid".to_string(),
            HotLibFn::LibFn(uri::is_valid),
        );
        map.insert(
            "::hot::uri/to-str".to_string(),
            HotLibFn::LibFn(uri::uri_to_str),
        );

        // Resource registry
        map.insert(
            "::hot::resource/load".to_string(),
            HotLibFn::LibFn(resource::load),
        );
        map.insert(
            "::hot::resource/load-str".to_string(),
            HotLibFn::LibFn(resource::load_str),
        );
        map.insert(
            "::hot::resource/path".to_string(),
            HotLibFn::LibFn(resource::path),
        );
        map.insert(
            "::hot::resource/exists".to_string(),
            HotLibFn::LibFn(resource::exists),
        );
        map.insert(
            "::hot::resource/list".to_string(),
            HotLibFn::LibFn(resource::list),
        );
        map.insert(
            "::hot::resource/list-matching".to_string(),
            HotLibFn::LibFn(resource::list_matching),
        );

        // Markdown frontmatter parser
        map.insert(
            "::hot::md/parse-frontmatter".to_string(),
            HotLibFn::LibFn(md::parse_frontmatter),
        );

        // Internal: tool/MCP schema introspection (no-doc; called by ::ai::tool).
        map.insert(
            "::hot::internal::mcp/schema-from-fn".to_string(),
            HotLibFn::LibFn(internal_mcp::schema_from_fn),
        );
        map.insert(
            "::hot::internal::mcp/all-tool-schemas".to_string(),
            HotLibFn::LibFn(internal_mcp::all_tool_schemas),
        );
        map.insert(
            "::hot::internal::mcp/invoke-with-input".to_string(),
            HotLibFn::VmAwareFn(internal_mcp::invoke_with_input),
        );

        // Internal: skill spec introspection (no-doc; called by ::ai::skill).
        map.insert(
            "::hot::internal::skill/meta-from-fn".to_string(),
            HotLibFn::LibFn(internal_skill::meta_from_fn),
        );
        map.insert(
            "::hot::internal::skill/all-skill-metas".to_string(),
            HotLibFn::LibFn(internal_skill::all_skill_metas),
        );

        // Internal: BPE tokenizer (no-doc; called by ::ai::tokenizer).
        map.insert(
            "::hot::internal::tokenizer/count".to_string(),
            HotLibFn::LibFn(internal_tokenizer::count),
        );
        map.insert(
            "::hot::internal::tokenizer/encodings".to_string(),
            HotLibFn::LibFn(internal_tokenizer::encodings),
        );

        // Hash functions
        map.insert(
            "::hot::hash/sha256".to_string(),
            HotLibFn::LibFn(hash::sha256),
        );
        map.insert(
            "::hot::hash/sha384".to_string(),
            HotLibFn::LibFn(hash::sha384),
        );
        map.insert(
            "::hot::hash/sha512".to_string(),
            HotLibFn::LibFn(hash::sha512),
        );
        map.insert(
            "::hot::hash/blake3".to_string(),
            HotLibFn::LibFn(hash::blake3),
        );
        map.insert("::hot::hash/sha1".to_string(), HotLibFn::LibFn(hash::sha1));
        map.insert("::hot::hash/md5".to_string(), HotLibFn::LibFn(hash::md5));

        // HMAC functions
        map.insert(
            "::hot::hmac/hmac-sha256".to_string(),
            HotLibFn::LibFn(hmac::hmac_sha256),
        );
        map.insert(
            "::hot::hmac/hmac-sha256-bytes".to_string(),
            HotLibFn::LibFn(hmac::hmac_sha256_bytes),
        );
        map.insert(
            "::hot::hmac/hmac-sha512".to_string(),
            HotLibFn::LibFn(hmac::hmac_sha512),
        );
        map.insert(
            "::hot::hmac/hmac-sha1".to_string(),
            HotLibFn::LibFn(hmac::hmac_sha1),
        );
        map.insert(
            "::hot::hmac/hmac-verify".to_string(),
            HotLibFn::LibFn(hmac::hmac_verify),
        );

        // Hex encoding functions
        map.insert(
            "::hot::hex/encode".to_string(),
            HotLibFn::LibFn(hex::encode),
        );
        map.insert(
            "::hot::hex/decode".to_string(),
            HotLibFn::LibFn(hex::decode),
        );
        map.insert(
            "::hot::hex/is-valid".to_string(),
            HotLibFn::LibFn(hex::is_valid),
        );

        // Bitwise operations
        map.insert("::hot::bit/and".to_string(), HotLibFn::LibFn(bit::and));
        map.insert("::hot::bit/or".to_string(), HotLibFn::LibFn(bit::or));
        map.insert("::hot::bit/xor".to_string(), HotLibFn::LibFn(bit::xor));
        map.insert("::hot::bit/not".to_string(), HotLibFn::LibFn(bit::not));
        map.insert(
            "::hot::bit/shift-left".to_string(),
            HotLibFn::LibFn(bit::shift_left),
        );
        map.insert(
            "::hot::bit/shift-right".to_string(),
            HotLibFn::LibFn(bit::shift_right),
        );

        // Bytes operations
        map.insert(
            "::hot::bytes/to-int".to_string(),
            HotLibFn::LibFn(bytes::to_int),
        );
        map.insert(
            "::hot::bytes/to-uint".to_string(),
            HotLibFn::LibFn(bytes::to_uint),
        );
        map.insert(
            "::hot::bytes/from-int".to_string(),
            HotLibFn::LibFn(bytes::from_int),
        );
        map.insert(
            "::hot::bytes/crc32".to_string(),
            HotLibFn::LibFn(bytes::crc32),
        );
        map.insert("::hot::bytes/get".to_string(), HotLibFn::LibFn(bytes::get));
        map.insert(
            "::hot::bytes/to-vec".to_string(),
            HotLibFn::LibFn(bytes::to_vec),
        );

        // Secure random functions
        map.insert(
            "::hot::random/random-bytes".to_string(),
            HotLibFn::LibFn(random::random_bytes),
        );
        map.insert(
            "::hot::random/random-string".to_string(),
            HotLibFn::LibFn(random::random_string),
        );
        map.insert(
            "::hot::random/secure-compare".to_string(),
            HotLibFn::LibFn(random::secure_compare),
        );

        // Type constructor functions
        map.insert(
            "::hot::type/Str".to_string(),
            HotLibFn::vm_callback(r#type::str_constructor),
        );
        map.insert(
            "::hot::type/Int".to_string(),
            HotLibFn::vm_callback(r#type::int_constructor),
        );
        map.insert(
            "::hot::type/Dec".to_string(),
            HotLibFn::vm_callback(r#type::dec_constructor),
        );
        map.insert(
            "::hot::type/Bool".to_string(),
            HotLibFn::vm_callback(r#type::bool_constructor),
        );
        map.insert(
            "::hot::type/Null".to_string(),
            HotLibFn::LibFn(r#type::null_constructor),
        );
        map.insert(
            "::hot::type/Vec".to_string(),
            HotLibFn::LibFn(r#type::vec_constructor),
        );
        map.insert(
            "::hot::type/Map".to_string(),
            HotLibFn::LibFn(r#type::map_constructor),
        );
        map.insert(
            "::hot::type/Byte".to_string(),
            HotLibFn::LibFn(r#type::byte_constructor),
        );
        map.insert(
            "::hot::type/Bytes".to_string(),
            HotLibFn::LibFn(r#type::bytes_constructor),
        );
        map.insert(
            "::hot::type/Namespace".to_string(),
            HotLibFn::LibFn(r#type::namespace_constructor),
        );
        map.insert(
            "::hot::type/Var".to_string(),
            HotLibFn::LibFn(r#type::var_constructor),
        );
        map.insert(
            "::hot::type/Fn".to_string(),
            HotLibFn::LibFn(r#type::fn_constructor),
        );
        map.insert(
            "::hot::type/Any".to_string(),
            HotLibFn::LibFn(r#type::any_constructor),
        );
        // is-type implementation (VM-aware for lazy argument evaluation)
        map.insert(
            "::hot::type/is-type".to_string(),
            HotLibFn::vm_callback(r#type::is_type),
        );
        map.insert(
            "::hot::type/typed-map".to_string(),
            HotLibFn::LibFn(r#type::typed_map),
        );
        map.insert(
            "::hot::type/untype".to_string(),
            HotLibFn::LibFn(r#type::untype),
        );

        // Meta functions (VM-aware)
        map.insert(
            "::hot::meta/get".to_string(),
            HotLibFn::vm_callback(meta::get),
        );

        // Test functions (VM-aware) - override Hot language implementations
        map.insert(
            "::hot::test/is-test".to_string(),
            HotLibFn::vm_callback(meta::is_test),
        );
        map.insert(
            "::hot::meta/source".to_string(),
            HotLibFn::vm_callback(meta::source),
        );

        // Function execution functions
        map.insert(
            "::hot::lang/call".to_string(),
            HotLibFn::VmAwareFn(lang::call),
        );
        map.insert(
            "::hot::lang/call-internal".to_string(),
            HotLibFn::VmAwareFn(lang::call),
        );
        map.insert(
            "::hot::lang/try-call".to_string(),
            HotLibFn::VmAwareFn(lang::try_call),
        );
        map.insert(
            "::hot::lang/resolve".to_string(),
            HotLibFn::vm_callback(lang::resolve),
        );

        // Lambda execution functions
        map.insert(
            "::hot::lambda/execute-with-param".to_string(),
            HotLibFn::LibFn(lambda::execute_lambda_with_param),
        );
        map.insert(
            "::hot::lambda/identity".to_string(),
            HotLibFn::LibFn(lambda::identity_lambda),
        );

        // Time functions
        map.insert("::hot::time/now".to_string(), HotLibFn::LibFn(time::now));
        map.insert(
            "::hot::time/epoch-millis".to_string(),
            HotLibFn::LibFn(time::epoch_millis),
        );
        map.insert(
            "::hot::time/instant-from-millis".to_string(),
            HotLibFn::LibFn(time::instant_from_millis),
        );
        // Removed instant_constructor - let Hot type-based overloads handle dispatch
        map.insert(
            "::hot::time/to-string".to_string(),
            HotLibFn::LibFn(time::to_string),
        );
        map.insert(
            "::hot::time/PlainDate".to_string(),
            HotLibFn::LibFn(time::plain_date_constructor),
        );
        // String and now constructors for PlainDate/PlainTime/PlainDateTime
        map.insert(
            "::hot::time/parse-plain-date".to_string(),
            HotLibFn::LibFn(time::parse_plain_date),
        );
        map.insert(
            "::hot::time/parse-plain-time".to_string(),
            HotLibFn::LibFn(time::parse_plain_time),
        );
        map.insert(
            "::hot::time/parse-plain-date-time".to_string(),
            HotLibFn::LibFn(time::parse_plain_date_time),
        );
        map.insert(
            "::hot::time/now-plain-date".to_string(),
            HotLibFn::LibFn(time::now_plain_date),
        );
        map.insert(
            "::hot::time/now-plain-time".to_string(),
            HotLibFn::LibFn(time::now_plain_time),
        );
        map.insert(
            "::hot::time/now-plain-date-time".to_string(),
            HotLibFn::LibFn(time::now_plain_date_time),
        );
        map.insert(
            "::hot::time/PlainTime".to_string(),
            HotLibFn::LibFn(time::plain_time_constructor),
        );
        map.insert(
            "::hot::time/PlainDateTime".to_string(),
            HotLibFn::LibFn(time::plain_datetime_constructor),
        );
        map.insert(
            "::hot::time/Duration".to_string(),
            HotLibFn::LibFn(time::duration_constructor),
        );
        map.insert(
            "::hot::time/epoch-nanos".to_string(),
            HotLibFn::LibFn(time::epoch_nanos),
        );
        map.insert("::hot::time/year".to_string(), HotLibFn::LibFn(time::year));
        map.insert(
            "::hot::time/month".to_string(),
            HotLibFn::LibFn(time::month),
        );
        map.insert("::hot::time/day".to_string(), HotLibFn::LibFn(time::day));
        map.insert("::hot::time/hour".to_string(), HotLibFn::LibFn(time::hour));
        map.insert(
            "::hot::time/minute".to_string(),
            HotLibFn::LibFn(time::minute),
        );
        map.insert(
            "::hot::time/second".to_string(),
            HotLibFn::LibFn(time::second),
        );
        map.insert(
            "::hot::time/millisecond".to_string(),
            HotLibFn::LibFn(time::millisecond),
        );
        map.insert(
            "::hot::time/microsecond".to_string(),
            HotLibFn::LibFn(time::microsecond),
        );
        map.insert(
            "::hot::time/nanosecond".to_string(),
            HotLibFn::LibFn(time::nanosecond),
        );
        map.insert(
            "::hot::time/parse".to_string(),
            HotLibFn::LibFn(time::parse),
        );
        map.insert(
            "::hot::time/format".to_string(),
            HotLibFn::LibFn(time::format_temporal),
        );
        map.insert("::hot::time/add".to_string(), HotLibFn::LibFn(time::add));
        map.insert(
            "::hot::time/subtract".to_string(),
            HotLibFn::LibFn(time::subtract),
        );
        map.insert(
            "::hot::time/until".to_string(),
            HotLibFn::LibFn(time::until),
        );
        map.insert(
            "::hot::time/since".to_string(),
            HotLibFn::LibFn(time::since),
        );
        // Missing time functions
        map.insert(
            "::hot::time/instant-from-micros".to_string(),
            HotLibFn::LibFn(time::instant_from_micros),
        );
        map.insert(
            "::hot::time/instant-from-nanos".to_string(),
            HotLibFn::LibFn(time::instant_from_nanos),
        );
        map.insert(
            "::hot::time/instant-from-plain-date".to_string(),
            HotLibFn::LibFn(time::instant_from_plain_date),
        );
        map.insert(
            "::hot::time/instant-from-plain-time".to_string(),
            HotLibFn::LibFn(time::instant_from_plain_time),
        );
        map.insert(
            "::hot::time/instant-from-plain-date-time".to_string(),
            HotLibFn::LibFn(time::instant_from_plain_date_time),
        );
        map.insert(
            "::hot::time/parse-duration".to_string(),
            HotLibFn::LibFn(time::parse_duration),
        );
        map.insert(
            "::hot::time/duration".to_string(),
            HotLibFn::LibFn(time::duration),
        );
        map.insert(
            "::hot::time/plain-date".to_string(),
            HotLibFn::LibFn(time::plain_date),
        );
        map.insert(
            "::hot::time/plain-time".to_string(),
            HotLibFn::LibFn(time::plain_time),
        );
        map.insert(
            "::hot::time/plain-date-time".to_string(),
            HotLibFn::LibFn(time::plain_date_time),
        );
        // ZonedDateTime functions
        map.insert(
            "::hot::time/zoned-date-time-from-string".to_string(),
            HotLibFn::LibFn(time::zoned_date_time_from_string),
        );
        map.insert(
            "::hot::time/zoned-date-time-from-instant".to_string(),
            HotLibFn::LibFn(time::zoned_date_time_from_instant),
        );
        map.insert(
            "::hot::time/zoned-date-time-from-plain-date-time".to_string(),
            HotLibFn::LibFn(time::zoned_date_time_from_plain_date_time),
        );
        map.insert(
            "::hot::time/now-zoned".to_string(),
            HotLibFn::LibFn(time::now_zoned),
        );
        map.insert(
            "::hot::time/with-timezone".to_string(),
            HotLibFn::LibFn(time::with_timezone),
        );
        map.insert(
            "::hot::time/to-plain-date-time".to_string(),
            HotLibFn::LibFn(time::to_plain_date_time_from_zdt),
        );
        map.insert(
            "::hot::time/to-plain-date".to_string(),
            HotLibFn::LibFn(time::to_plain_date_from_zdt),
        );
        map.insert(
            "::hot::time/to-plain-time".to_string(),
            HotLibFn::LibFn(time::to_plain_time_from_zdt),
        );
        map.insert(
            "::hot::time/to-instant".to_string(),
            HotLibFn::LibFn(time::to_instant_from_zdt),
        );

        // Regex functions (simple names, Rust functions keep regex_ prefix since `match` is reserved)
        map.insert(
            "::hot::regex/is-match".to_string(),
            HotLibFn::LibFn(regex::is_match),
        );
        map.insert(
            "::hot::regex/match".to_string(),
            HotLibFn::LibFn(regex::regex_match),
        );
        map.insert(
            "::hot::regex/find".to_string(),
            HotLibFn::LibFn(regex::find),
        );
        map.insert(
            "::hot::regex/find-all".to_string(),
            HotLibFn::LibFn(regex::find_all),
        );
        map.insert(
            "::hot::regex/replace".to_string(),
            HotLibFn::LibFn(regex::replace),
        );
        map.insert(
            "::hot::regex/replace-all".to_string(),
            HotLibFn::LibFn(regex::replace_all),
        );
        map.insert(
            "::hot::regex/split".to_string(),
            HotLibFn::LibFn(regex::split),
        );
        map.insert(
            "::hot::regex/capture".to_string(),
            HotLibFn::LibFn(regex::capture),
        );
        map.insert(
            "::hot::regex/capture-all".to_string(),
            HotLibFn::LibFn(regex::capture_all),
        );
        map.insert(
            "::hot::regex/escape".to_string(),
            HotLibFn::LibFn(regex::escape),
        );

        // JSON functions
        map.insert(
            "::hot::json/to-json".to_string(),
            HotLibFn::LibFn(json::to_json),
        );
        map.insert(
            "::hot::json/from-json".to_string(),
            HotLibFn::LibFn(json::from_json),
        );

        // XML functions
        map.insert(
            "::hot::xml/from-xml".to_string(),
            HotLibFn::LibFn(xml::from_xml),
        );
        map.insert(
            "::hot::xml/to-xml".to_string(),
            HotLibFn::LibFn(xml::to_xml),
        );
        map.insert("::hot::xml/child".to_string(), HotLibFn::LibFn(xml::child));
        map.insert(
            "::hot::xml/children".to_string(),
            HotLibFn::LibFn(xml::children),
        );
        map.insert("::hot::xml/text".to_string(), HotLibFn::LibFn(xml::text));
        map.insert("::hot::xml/attr".to_string(), HotLibFn::LibFn(xml::attr));
        map.insert("::hot::xml/at".to_string(), HotLibFn::LibFn(xml::at));

        // Bool functions
        map.insert(
            "::hot::bool/is-truthy".to_string(),
            HotLibFn::LibFn(bool::is_truthy),
        );
        map.insert("::hot::bool/not".to_string(), HotLibFn::LibFn(bool::not));

        // Additional string functions
        map.insert(
            "::hot::str/trim-start".to_string(),
            HotLibFn::LibFn(str::trim_start),
        );
        map.insert(
            "::hot::str/trim-end".to_string(),
            HotLibFn::LibFn(str::trim_end),
        );
        map.insert(
            "::hot::str/replace".to_string(),
            HotLibFn::LibFn(str::replace),
        );
        map.insert(
            "::hot::str/pad-start".to_string(),
            HotLibFn::LibFn(str::pad_start),
        );
        map.insert(
            "::hot::str/pad-end".to_string(),
            HotLibFn::LibFn(str::pad_end),
        );
        // Math functions
        map.insert(
            "::hot::math/is-zero".to_string(),
            HotLibFn::LibFn(math::is_zero),
        );
        map.insert("::hot::math/add".to_string(), HotLibFn::LibFn(math::add));
        map.insert("::hot::math/sub".to_string(), HotLibFn::LibFn(math::sub));
        map.insert("::hot::math/mul".to_string(), HotLibFn::LibFn(math::mul));
        map.insert("::hot::math/div".to_string(), HotLibFn::LibFn(math::div));
        map.insert("::hot::math/mod".to_string(), HotLibFn::LibFn(math::modulo));
        map.insert("::hot::math/pow".to_string(), HotLibFn::LibFn(math::pow));
        map.insert("::hot::math/abs".to_string(), HotLibFn::LibFn(math::abs));
        map.insert("::hot::math/max".to_string(), HotLibFn::LibFn(math::max));
        map.insert("::hot::math/min".to_string(), HotLibFn::LibFn(math::min));
        map.insert("::hot::math/ceil".to_string(), HotLibFn::LibFn(math::ceil));
        map.insert(
            "::hot::math/floor".to_string(),
            HotLibFn::LibFn(math::floor),
        );
        map.insert(
            "::hot::math/round".to_string(),
            HotLibFn::LibFn(math::round),
        );

        // Context functions (VM-specific context storage)
        map.insert(
            "::hot::ctx/get".to_string(),
            HotLibFn::vm_callback(ctx::get),
        );
        map.insert("::hot::ctx/set".to_string(), HotLibFn::VmAwareFn(ctx::set));
        map.insert(
            "::hot::ctx/set-secret".to_string(),
            HotLibFn::VmAwareFn(ctx::set_secret),
        );

        // Comparison functions
        map.insert("::hot::cmp/eq".to_string(), HotLibFn::LibFn(cmp::eq));
        map.insert("::hot::cmp/ne".to_string(), HotLibFn::LibFn(cmp::ne));
        map.insert("::hot::cmp/gt".to_string(), HotLibFn::LibFn(cmp::gt));
        map.insert("::hot::cmp/lt".to_string(), HotLibFn::LibFn(cmp::lt));
        map.insert("::hot::cmp/gte".to_string(), HotLibFn::LibFn(cmp::gte));
        map.insert("::hot::cmp/lte".to_string(), HotLibFn::LibFn(cmp::lte));

        // Function utilities (resolve already registered above)
        // Core do: force-evaluate lazy thunks once
        map.insert(
            "::hot::core/do".to_string(),
            HotLibFn::vm_callback(core::do_eval),
        );

        // Map functions
        map.insert("::hot::map/key".to_string(), HotLibFn::LibFn(map::key));
        map.insert("::hot::map/value".to_string(), HotLibFn::LibFn(map::value));

        // Core type constructors - VM-aware (for implements dispatch)
        map.insert(
            "::hot::type/Str".to_string(),
            HotLibFn::vm_callback(r#type::str_constructor),
        );
        map.insert(
            "::hot::type/Int".to_string(),
            HotLibFn::vm_callback(r#type::int_constructor),
        );
        map.insert(
            "::hot::type/Dec".to_string(),
            HotLibFn::vm_callback(r#type::dec_constructor),
        );
        map.insert(
            "::hot::type/Bool".to_string(),
            HotLibFn::vm_callback(r#type::bool_constructor),
        );
        map.insert(
            "::hot::type/Byte".to_string(),
            HotLibFn::LibFn(r#type::byte_constructor),
        );
        map.insert(
            "::hot::type/Bytes".to_string(),
            HotLibFn::LibFn(r#type::bytes_constructor),
        );

        // Time type constructors
        map.insert(
            "::hot::time/Millisecond".to_string(),
            HotLibFn::LibFn(time::millisecond_constructor),
        );
        map.insert(
            "::hot::time/Second".to_string(),
            HotLibFn::LibFn(time::second_constructor),
        );
        map.insert(
            "::hot::time/Minute".to_string(),
            HotLibFn::LibFn(time::minute_constructor),
        );
        map.insert(
            "::hot::time/Hour".to_string(),
            HotLibFn::LibFn(time::hour_constructor),
        );
        map.insert(
            "::hot::time/Day".to_string(),
            HotLibFn::LibFn(time::day_constructor),
        );
        map.insert(
            "::hot::time/Week".to_string(),
            HotLibFn::LibFn(time::week_constructor),
        );
        map.insert(
            "::hot::time/Month".to_string(),
            HotLibFn::LibFn(time::month_constructor),
        );
        map.insert(
            "::hot::time/Year".to_string(),
            HotLibFn::LibFn(time::year_constructor),
        );
        map.insert(
            "::hot::time/Nanosecond".to_string(),
            HotLibFn::LibFn(time::nanosecond_constructor),
        );
        map.insert(
            "::hot::time/Microsecond".to_string(),
            HotLibFn::LibFn(time::microsecond_constructor),
        );

        // Type checking functions
        map.insert(
            "::hot::type/is-str".to_string(),
            HotLibFn::LibFn(r#type::is_str),
        );
        map.insert(
            "::hot::type/is-int".to_string(),
            HotLibFn::LibFn(r#type::is_int),
        );
        map.insert(
            "::hot::type/is-dec".to_string(),
            HotLibFn::LibFn(r#type::is_dec),
        );
        map.insert(
            "::hot::type/is-bool".to_string(),
            HotLibFn::LibFn(r#type::is_bool),
        );
        map.insert(
            "::hot::type/is-byte".to_string(),
            HotLibFn::LibFn(r#type::is_byte),
        );
        map.insert(
            "::hot::type/is-bytes".to_string(),
            HotLibFn::LibFn(r#type::is_bytes),
        );
        map.insert(
            "::hot::type/is-vec".to_string(),
            HotLibFn::LibFn(r#type::is_vec),
        );
        map.insert(
            "::hot::type/is-map".to_string(),
            HotLibFn::LibFn(r#type::is_map),
        );
        map.insert(
            "::hot::type/is-null".to_string(),
            HotLibFn::LibFn(r#type::is_null),
        );

        // UUID functions
        map.insert(
            "::hot::uuid/uuid-v4".to_string(),
            HotLibFn::LibFn(uuid::uuid_v4),
        );
        map.insert(
            "::hot::uuid/uuid-v7".to_string(),
            HotLibFn::LibFn(uuid::uuid_v7),
        );
        map.insert(
            "::hot::uuid/uuid-nil".to_string(),
            HotLibFn::LibFn(uuid::uuid_nil),
        );
        map.insert(
            "::hot::uuid/is-uuid".to_string(),
            HotLibFn::LibFn(uuid::is_uuid),
        );
        map.insert(
            "::hot::uuid/Uuid".to_string(),
            HotLibFn::LibFn(uuid::uuid_constructor),
        );
        map.insert(
            "::hot::uuid/uuid-to-str".to_string(),
            HotLibFn::LibFn(uuid::uuid_to_str),
        );

        // MIME type functions
        map.insert(
            "::hot::mime/from-ext".to_string(),
            HotLibFn::LibFn(mime::from_ext),
        );
        map.insert(
            "::hot::mime/from-path".to_string(),
            HotLibFn::LibFn(mime::from_path),
        );
        map.insert(
            "::hot::mime/to-ext".to_string(),
            HotLibFn::LibFn(mime::to_ext),
        );
        map.insert(
            "::hot::mime/to-exts".to_string(),
            HotLibFn::LibFn(mime::to_exts),
        );
        map.insert(
            "::hot::mime/is-image".to_string(),
            HotLibFn::LibFn(mime::is_image),
        );
        map.insert(
            "::hot::mime/is-audio".to_string(),
            HotLibFn::LibFn(mime::is_audio),
        );
        map.insert(
            "::hot::mime/is-video".to_string(),
            HotLibFn::LibFn(mime::is_video),
        );
        map.insert(
            "::hot::mime/is-text".to_string(),
            HotLibFn::LibFn(mime::is_text),
        );
        map.insert(
            "::hot::mime/is-application".to_string(),
            HotLibFn::LibFn(mime::is_application),
        );
        map.insert(
            "::hot::mime/type".to_string(),
            HotLibFn::LibFn(mime::get_type),
        );
        map.insert(
            "::hot::mime/subtype".to_string(),
            HotLibFn::LibFn(mime::get_subtype),
        );

        // String function aliases
        map.insert(
            "::hot::str/length".to_string(),
            HotLibFn::LibFn(coll::length),
        );

        // Box (container) functions
        map.insert(
            "::hot::box/start".to_string(),
            HotLibFn::VmAwareFn(r#box::start),
        );
        map.insert(
            "::hot::box/allowed-images".to_string(),
            HotLibFn::LibFn(r#box::allowed_images),
        );
        map.insert(
            "::hot::box/sizes".to_string(),
            HotLibFn::LibFn(r#box::sizes),
        );
        map.insert(
            "::hot::box/stats".to_string(),
            HotLibFn::vm_callback(r#box::stats),
        );
        map.insert(
            "::hot::box/enabled".to_string(),
            HotLibFn::vm_callback(r#box::enabled),
        );
        map.insert(
            "::hot::box/quota".to_string(),
            HotLibFn::vm_callback(r#box::quota),
        );
        map.insert(
            "::hot::box/limits".to_string(),
            HotLibFn::vm_callback(r#box::limits),
        );

        // File functions
        map.insert(
            "::hot::file/read-file".to_string(),
            HotLibFn::VmAwareFn(file::read_file),
        );
        map.insert(
            "::hot::file/read-file-bytes".to_string(),
            HotLibFn::VmAwareFn(file::read_file_bytes),
        );
        map.insert(
            "::hot::file/write-file".to_string(),
            HotLibFn::VmAwareFn(file::write_file),
        );
        map.insert(
            "::hot::file/write-file-bytes".to_string(),
            HotLibFn::VmAwareFn(file::write_file_bytes),
        );
        map.insert(
            "::hot::file/delete-file".to_string(),
            HotLibFn::VmAwareFn(file::delete_file),
        );
        map.insert(
            "::hot::file/file-exists".to_string(),
            HotLibFn::VmAwareFn(file::file_exists),
        );
        map.insert(
            "::hot::file/file-info".to_string(),
            HotLibFn::VmAwareFn(file::file_info),
        );
        map.insert(
            "::hot::file/list-files".to_string(),
            HotLibFn::VmAwareFn(file::list_files),
        );

        // Task functions
        map.insert(
            "::hot::task/start".to_string(),
            HotLibFn::VmAwareFn(task::start),
        );
        map.insert(
            "::hot::task/send".to_string(),
            HotLibFn::VmAwareFn(task::send),
        );
        map.insert(
            "::hot::task/receive".to_string(),
            HotLibFn::VmAwareFn(task::receive),
        );
        map.insert(
            "::hot::task/cancel".to_string(),
            HotLibFn::VmAwareFn(task::cancel),
        );
        map.insert(
            "::hot::task/await".to_string(),
            HotLibFn::VmAwareFn(task::await_task),
        );
        map.insert(
            "::hot::task/checkpoint".to_string(),
            HotLibFn::VmAwareFn(task::checkpoint),
        );
        map.insert(
            "::hot::task/restore".to_string(),
            HotLibFn::VmAwareFn(task::restore),
        );

        // WebSocket functions
        map.insert(
            "::hot::ws/connect".to_string(),
            HotLibFn::LibFn(ws::connect),
        );
        map.insert("::hot::ws/send".to_string(), HotLibFn::LibFn(ws::send));
        map.insert(
            "::hot::ws/receive".to_string(),
            HotLibFn::LibFn(ws::receive),
        );
        map.insert("::hot::ws/close".to_string(), HotLibFn::LibFn(ws::close));
        map.insert(
            "::hot::ws/is-open".to_string(),
            HotLibFn::LibFn(ws::is_open),
        );

        // Store functions
        map.insert(
            "::hot::store/put".to_string(),
            HotLibFn::VmAwareFn(store::put),
        );
        map.insert(
            "::hot::store/get".to_string(),
            HotLibFn::VmAwareFn(store::get),
        );
        map.insert(
            "::hot::store/delete".to_string(),
            HotLibFn::VmAwareFn(store::delete),
        );
        map.insert(
            "::hot::store/keys".to_string(),
            HotLibFn::VmAwareFn(store::keys),
        );
        map.insert(
            "::hot::store/vals".to_string(),
            HotLibFn::VmAwareFn(store::vals),
        );
        map.insert(
            "::hot::store/length".to_string(),
            HotLibFn::VmAwareFn(store::length),
        );
        map.insert(
            "::hot::store/is-empty".to_string(),
            HotLibFn::VmAwareFn(store::is_empty),
        );
        map.insert(
            "::hot::store/first".to_string(),
            HotLibFn::VmAwareFn(store::first),
        );
        map.insert(
            "::hot::store/last".to_string(),
            HotLibFn::VmAwareFn(store::last),
        );
        map.insert(
            "::hot::store/merge".to_string(),
            HotLibFn::VmAwareFn(store::merge),
        );
        map.insert(
            "::hot::store/put-many".to_string(),
            HotLibFn::VmAwareFn(store::put_many),
        );
        map.insert(
            "::hot::store/list".to_string(),
            HotLibFn::VmAwareFn(store::list),
        );
        map.insert(
            "::hot::store/search".to_string(),
            HotLibFn::VmAwareFn(store::search),
        );
        map.insert(
            "::hot::store/filter".to_string(),
            HotLibFn::VmAwareFn(store::filter),
        );
        map.insert(
            "::hot::store/find-first".to_string(),
            HotLibFn::VmAwareFn(store::find_first),
        );
        map.insert(
            "::hot::store/some".to_string(),
            HotLibFn::VmAwareFn(store::some),
        );
        map.insert(
            "::hot::store/all".to_string(),
            HotLibFn::VmAwareFn(store::all),
        );
        map.insert(
            "::hot::store/reduce".to_string(),
            HotLibFn::VmAwareFn(store::reduce),
        );
        map.insert(
            "::hot::store/slice".to_string(),
            HotLibFn::VmAwareFn(store::slice),
        );
        map.insert(
            "::hot::store/clear".to_string(),
            HotLibFn::VmAwareFn(store::clear),
        );
        map.insert(
            "::hot::store/destroy".to_string(),
            HotLibFn::VmAwareFn(store::destroy),
        );

        map
    })
}
