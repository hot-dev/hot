//! Read Hot files that hold only literal data.
//!
//! A package's `pkg.hot` binds its metadata with literal maps, vectors,
//! strings, numbers, booleans and null:
//!
//! ```text
//! ::hot::pkg ns
//!
//! hot.pkg.hot-std { name: "hot.dev/hot-std", src-paths: ["src/"] }
//! ```
//!
//! `hot` used to run such a file on the VM with no packages loaded, which is
//! why it can only hold literals. Its value is the last binding's, and a dot
//! path builds nested maps under its root name, so folding the bindings gives
//! the same value without executing anything: `{pkg: {hot-std: {...}}}` here.

use crate::lang::ast::{DeepPath, Ref, Value};
use crate::val::Val;
use indexmap::IndexMap;

/// Evaluate a file of literal bindings and return the last binding's value
/// (for a dot-path binding, the whole value now bound to its root name).
///
/// Function calls, references, templates and spreads are errors: they need
/// the runtime.
pub fn eval_data(code: &str) -> Result<Val, String> {
    let program = crate::lang::parser::parse_hot_file(code, "<data>").map_err(|e| {
        e.format_error(code, false)
            .unwrap_or_else(|| format!("Parse error: {}", e))
    })?;

    let mut bindings: IndexMap<String, Val> = IndexMap::new();
    let mut last: Option<String> = None;
    for namespace in program.namespaces.values() {
        for (var, value) in &namespace.scope.vars {
            // `::ns ns` declarations and `::alias ::target` imports.
            if let Value::Ref(Ref::Ns(_)) = value {
                continue;
            }
            let name = var.sym.name().to_string();
            let literal =
                literal_value(value).ok_or_else(|| format!("'{}' is not literal data", name))?;
            let bound = match &var.deep_set {
                None => literal,
                Some(path) => {
                    let current = bindings.get(&name).cloned().unwrap_or(Val::Null);
                    set_path(current, &key_path(path, &name)?, literal)
                        .map_err(|e| format!("'{}': {}", name, e))?
                }
            };
            bindings.insert(name.clone(), bound);
            last = Some(name);
        }
    }

    Ok(last
        .and_then(|name| bindings.swap_remove(&name))
        .unwrap_or(Val::Null))
}

fn literal_value(value: &Value) -> Option<Val> {
    match value {
        Value::Val(val, _) if is_literal(val) => Some(val.clone()),
        _ => None,
    }
}

fn is_literal(val: &Val) -> bool {
    match val {
        Val::Int(_) | Val::Dec(_) | Val::Str(_) | Val::Bool(_) | Val::Null => true,
        Val::Vec(items) => items.iter().all(is_literal),
        Val::Map(map) => map.iter().all(|(k, v)| is_literal(k) && is_literal(v)),
        Val::Box(_) | Val::Byte(_) | Val::Bytes(_) => false,
    }
}

fn key_path(path: &DeepPath, name: &str) -> Result<Vec<String>, String> {
    match path {
        DeepPath::Key(key) => Ok(vec![key.clone()]),
        DeepPath::Chain(head, tail) => {
            let mut keys = key_path(head, name)?;
            keys.extend(key_path(tail, name)?);
            Ok(keys)
        }
        DeepPath::Index(_) | DeepPath::DynamicIndex(_) | DeepPath::Append => Err(format!(
            "'{}' sets an index; only map keys are literal data",
            name
        )),
    }
}

fn set_path(current: Val, keys: &[String], value: Val) -> Result<Val, String> {
    let Some((key, rest)) = keys.split_first() else {
        return Ok(value);
    };
    let mut map = match current {
        Val::Map(map) => *map,
        Val::Null => IndexMap::new(),
        other => return Err(format!("'{}' is set on a {}, not a map", key, other)),
    };
    let key = Val::from(key.as_str());
    let child = map.get(&key).cloned().unwrap_or(Val::Null);
    map.insert(key, set_path(child, rest, value)?);
    Ok(Val::Map(Box::new(map)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::val;

    #[test]
    fn a_dot_path_binding_evaluates_to_its_root_value() {
        let code = r#"::hot::pkg ns

hot.pkg.hot-std {
  name: "hot.dev/hot-std",
  deps: {},
  src-paths: ["src/"],
}
"#;
        assert_eq!(
            eval_data(code).unwrap(),
            val!({"pkg": {"hot-std": {"name": "hot.dev/hot-std", "deps": {}, "src-paths": ["src/"]}}})
        );
    }

    #[test]
    fn later_dot_paths_add_to_the_same_root() {
        let code = "a 1\nhot.pkg.x {name: \"a\"}\nhot.pkg.y {name: \"b\"}\n";
        assert_eq!(
            eval_data(code).unwrap(),
            val!({"pkg": {"x": {"name": "a"}, "y": {"name": "b"}}})
        );
    }

    #[test]
    fn code_is_not_data() {
        let err = eval_data("hot.pkg.x {name: concat(\"a\", \"b\")}\n").unwrap_err();
        assert!(err.contains("not literal data"), "{err}");
        let err = eval_data("::env ::hot::env\nhot.url ::env/get(\"X\", \"y\")\n").unwrap_err();
        assert!(err.contains("not literal data"), "{err}");
    }
}
