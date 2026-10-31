//! Core functions that run natively when called by their bare name.
//!
//! The interpreter's call fast paths, the JIT's intrinsics and HOF fusion run
//! some hot-std core functions (`add`, `length`, `first`, `map`, ...)
//! natively when code calls them by their bare name, without the lookup a
//! call by name otherwise makes. That is right only while the name reaches
//! hot-std's function. A project can bind the name in its namespace, as a
//! var import such as `first ::hot::coll/last` does, or declare a core
//! function of the same name, which the core registry then resolves the name
//! to. [`NativeCoreShadows`] records where that happens, so
//! every tier calls what the lookup reaches there.

use crate::lang::bytecode::{BytecodeProgram, FunctionId, VariableType};
use ahash::{AHashMap, AHashSet};
use indexmap::IndexMap;

/// Every core function name a tier runs natively when called by bare name.
const NATIVE_CORE_NAMES: &[&str] = &[
    "add", "sub", "mul", "div", "mod", "pow", "abs", "min", "max", "is-zero", "eq", "ne", "lt",
    "lte", "gt", "gte", "not", "and", "or", "is-null", "is-some", "is-empty", "length", "first",
    "rest", "range", "get", "concat", "map", "filter", "reduce", "some", "all", "pmap",
];

/// `name` as it appears in [`NATIVE_CORE_NAMES`], if it is one.
fn native_core_name(name: &str) -> Option<&'static str> {
    static NAMES: std::sync::OnceLock<AHashSet<&'static str>> = std::sync::OnceLock::new();
    NAMES
        .get_or_init(|| NATIVE_CORE_NAMES.iter().copied().collect())
        .get(name)
        .copied()
}

/// The names HOF fusion runs natively: its stages and the operations it
/// evaluates in their lambdas.
const FUSION_NAMES: &[&str] = &[
    "add", "sub", "mul", "div", "mod", "abs", "min", "max", "is-zero", "eq", "ne", "lt", "lte",
    "gt", "gte", "not", "concat", "range", "map", "filter", "reduce", "some", "all", "length",
];

/// Hot-std's own namespaces. A hot-std core function that shares a native
/// name (such as `::hot::str/length`) is the native one or an alias of it.
/// The `::hot::test::` namespaces are this repository's tests, not hot-std.
fn is_hot_std_namespace(namespace: &str) -> bool {
    namespace.starts_with("::hot::") && !namespace.starts_with("::hot::test::")
}

/// Where a bare call to a native core name reaches another function.
#[derive(Debug, Default)]
pub struct NativeCoreShadows {
    /// Names a non-hot-std core function redeclares: every bare call reaches
    /// that function.
    redeclared: AHashSet<&'static str>,
    /// Names bound in a namespace, with the namespaces binding them: a bare
    /// call from there reaches the binding.
    bound: AHashMap<&'static str, Vec<String>>,
}

impl NativeCoreShadows {
    pub fn new(program: &BytecodeProgram, core_functions: &IndexMap<String, FunctionId>) -> Self {
        let mut shadows = Self::default();
        // As `core_function_name` resolves a bare name: the qualified entry
        // registered last wins.
        let mut decided: AHashSet<&str> = AHashSet::new();
        for registered in core_functions.keys().rev() {
            let Some((namespace, name)) = registered.rsplit_once('/') else {
                continue;
            };
            let Some(native) = native_core_name(name) else {
                continue;
            };
            if decided.insert(native) && !is_hot_std_namespace(namespace) {
                shadows.redeclared.insert(native);
            }
        }
        for (path, namespace) in &program.namespaces.namespaces {
            for native in NATIVE_CORE_NAMES {
                // A function or type the namespace defines is called by id,
                // never by bare name, from that namespace. A binding of a
                // value (a var import, an alias, a lambda) is called by name.
                let binds = namespace
                    .variables
                    .get(*native)
                    .is_some_and(|variable| matches!(variable.var_type, VariableType::Value));
                if binds {
                    shadows.bound.entry(native).or_default().push(path.clone());
                }
            }
        }
        shadows
    }

    /// Whether a bare call to `name` from code in `namespace` reaches a
    /// function other than the native one.
    pub fn shadows(&self, namespace: &str, name: &str) -> bool {
        if self.is_empty() {
            return false;
        }
        self.redeclared.contains(name)
            || self
                .bound
                .get(name)
                .is_some_and(|namespaces| namespaces.iter().any(|bound| bound == namespace))
    }

    /// Whether HOF fusion would run a native name that code in `namespace`
    /// reaches another function by.
    pub fn shadows_fusion(&self, namespace: &str) -> bool {
        !self.is_empty()
            && FUSION_NAMES
                .iter()
                .any(|name| self.shadows(namespace, name))
    }

    fn is_empty(&self) -> bool {
        self.redeclared.is_empty() && self.bound.is_empty()
    }
}
