//! Configuration made only of literal settings, read without an engine.
//!
//! A `hot.hot` file is Hot code, so loading it runs an engine, which loads,
//! checks and compiles hot-std first: most of a short command's startup.
//! Most configuration files only set values (`hot.set.profile "dev"`), and
//! what those produce depends on nothing but their text. For those this
//! builds the `hot` value directly, as the deep-path assignments would: each
//! key creates the Map it names when it is missing, and the last segment
//! sets its value. `::hot::env/get` with literal arguments reads the variable
//! as the function does. Anything else (another call, a reference, a
//! template, an index or append segment, another binding) is the engine's,
//! unchanged.

use crate::lang::ast::{DeepPath, FnCall, Namespace, Program, Ref, Value};
use crate::val::{Val, ValIndexMap};

/// The `hot` value of configuration code made only of literal settings of
/// `hot`, or `None` when the engine must run it.
pub fn literal_conf_value(code: &str) -> Option<Val> {
    let program = crate::lang::parser::parse_hot(code).ok()?;
    hot_value(&program)
}

fn hot_value(program: &Program) -> Option<Val> {
    // The parser keeps an empty root namespace beside `::hot::conf`.
    let mut namespaces = program
        .namespaces
        .values()
        .filter(|namespace| !namespace.scope.vars.is_empty());
    let namespace = namespaces.next()?;
    if namespaces.next().is_some()
        || namespace.path.to_string() != "::hot::conf"
        || !namespace
            .aliases
            .values()
            .all(|target| target.to_string() == ENV_NS)
        || namespace.meta.is_some()
    {
        return None;
    }
    let mut hot: Option<Val> = None;
    for (var, value) in &namespace.scope.vars {
        // The namespace declaration itself.
        if var.sym.name() == "ns"
            && var.deep_set.is_none()
            && matches!(value, Value::Ref(Ref::Ns(ns))
                if ns.ns.to_string() == "::hot::conf" && ns.function_name.is_none())
        {
            continue;
        }
        if var.sym.name() != "hot"
            || var.deep_path.is_some()
            || var.meta.is_some()
            || var.type_annotation.is_some()
        {
            return None;
        }
        let value = literal(value, namespace)?;
        match &var.deep_set {
            None => hot = Some(value),
            Some(path) => {
                let keys = keys(path)?;
                let root = hot.take().unwrap_or_else(Val::map_empty);
                hot = Some(set_in(root, &keys, value)?);
            }
        }
    }
    hot
}

const ENV_NS: &str = "::hot::env";

/// A value with no expression in it, or an environment variable read with
/// literal arguments.
fn literal(value: &Value, namespace: &Namespace) -> Option<Val> {
    match value {
        Value::FnCall(call) => env_get(call, namespace),
        Value::Val(val, _) if is_plain(val) => Some(val.clone()),
        Value::MapWithSpread {
            base_entries,
            spread_entries,
        } if spread_entries.is_empty()
            && base_entries.iter().all(|(k, v)| is_plain(k) && is_plain(v)) =>
        {
            Some(Val::map(base_entries.clone()))
        }
        _ => None,
    }
}

/// `::hot::env/get("NAME", "default")`, through the namespace or an alias
/// of it, read as the function reads it.
fn env_get(call: &FnCall, namespace: &Namespace) -> Option<Val> {
    let Value::Ref(Ref::Ns(target)) = call.function.as_ref() else {
        return None;
    };
    let path = target.ns.to_string();
    let resolves_to_env = path == ENV_NS
        || namespace
            .aliases
            .iter()
            .any(|(alias, target)| alias.to_string() == path && target.to_string() == ENV_NS);
    if !resolves_to_env
        || target.function_name.as_deref() != Some("get")
        || call.result_path.is_some()
        || call.args.len() != 2
        || call
            .args
            .iter()
            .any(|arg| arg.lazy || arg.spread || arg.name.is_some())
    {
        return None;
    }
    let (Value::Val(Val::Str(name), _), Value::Val(default @ (Val::Str(_) | Val::Null), _)) =
        (&call.args[0].value, &call.args[1].value)
    else {
        return None;
    };
    crate::lang::hot::env::read(name, default).ok()
}

/// Whether a parsed value is data only: no boxed expression (an AST node or
/// a call) anywhere in it.
fn is_plain(val: &Val) -> bool {
    match val {
        Val::Box(_) => false,
        Val::Vec(items) => items.iter().all(is_plain),
        Val::Map(entries) => entries.iter().all(|(k, v)| is_plain(k) && is_plain(v)),
        _ => true,
    }
}

/// The keys of a path made only of keys.
fn keys(path: &DeepPath) -> Option<Vec<&str>> {
    match path {
        DeepPath::Key(key) => Some(vec![key.as_str()]),
        DeepPath::Chain(left, right) => {
            let mut keys = self::keys(left)?;
            keys.extend(self::keys(right)?);
            Some(keys)
        }
        DeepPath::Index(_) | DeepPath::DynamicIndex(_) | DeepPath::Append => None,
    }
}

/// `root` with `value` at `keys`, creating each missing Map on the way, as
/// a deep-path assignment does. `None` where a key on the way holds
/// something other than a Map: the engine decides what that does.
fn set_in(root: Val, keys: &[&str], value: Val) -> Option<Val> {
    let Val::Map(entries) = root else {
        return None;
    };
    let mut entries: ValIndexMap = std::sync::Arc::unwrap_or_clone(entries).into_inner();
    let (first, rest) = keys.split_first()?;
    let key = Val::from(*first);
    let new = if rest.is_empty() {
        value
    } else {
        let child = entries.get(&key).cloned().unwrap_or_else(Val::map_empty);
        set_in(child, rest, value)?
    };
    entries.insert(key, new);
    Some(Val::map(entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_settings_build_the_hot_value() {
        let code = r#"::hot::conf ns
::env ::hot::env

hot.set.profile "dev"
hot.project.app.src.paths ["./hot/src"]
hot.db.uri ""
hot.set.profile "prod"
hot.limits {max: 3, names: ["a", "b"]}
hot.remote.url ::env/get("HOT_LITERAL_CONF_UNSET", "https://api.hot.dev")
hot.remote.key ::hot::env/get("HOT_LITERAL_CONF_UNSET", null)
"#;
        let value = literal_conf_value(code).expect("literal configuration");
        assert_eq!(
            value,
            crate::val!({
                "set": {"profile": "prod"},
                "project": {"app": {"src": {"paths": ["./hot/src"]}}},
                "db": {"uri": ""},
                "limits": {"max": 3, "names": ["a", "b"]},
                "remote": {"url": "https://api.hot.dev", "key": null}
            })
        );
    }

    #[test]
    fn anything_but_literal_settings_goes_to_the_engine() {
        for code in [
            "::hot::conf ns\nhot.db.uri ::hot::env/get(concat(\"D\", \"B\"), \"\")\n",
            "::hot::conf ns\nuri \"x\"\nhot.db.uri uri\n",
            "::hot::conf ns\nhot.paths[] \"a\"\n",
            "::hot::conf ns\nhot.name `n${1}`\n",
            "::hot::conf ns\nhot.a 1\nhot.a.b 2\n",
            "::hot::conf ns\n::http ::hot::http\nhot.a 1\n",
            "::hot::conf ns\n::env ::hot::env\nhot.a ::env/get-all()\n",
            "::hot::conf ns\n::env ::hot::env\nhot.a ::env/get(\"X\", 1)\n",
            "::hot::conf ns\n::env ::hot::env\nhot.a ::env/get(\"X\")\n",
            "::hot::conf ns\nhot.a ::env/get(\"X\", \"\")\n",
        ] {
            assert_eq!(literal_conf_value(code), None, "{code}");
        }
    }
}
