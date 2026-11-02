// Context functions for Hot library
// These functions manage execution context state (VM-specific)

use crate::lang::hot::r#type::HotResult;
use crate::lang::masking::value_hash;
use crate::lang::runtime::vm::VirtualMachine;
use crate::val::Val;
use crate::validate_args;

/// The context key under which a context loaded from files (`hot/ctx.hot`)
/// lists the keys it set with `set-secret`. Those files run once, outside any
/// run, so the flags travel with the values, and `hot dev`'s worker masks
/// those values in the runs it starts.
pub const LOADED_SECRET_KEYS: &str = "hot.ctx.secret-keys";

/// Set context variable(s) (uses VM-specific context storage)
/// Supports two arities:
/// - set(key: Str, value: Any) - set a single key-value pair
/// - set(ctx-map: Map) - set multiple key-value pairs from a map
pub fn set(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    match args.len() {
        // Single map argument: set multiple context variables
        1 => {
            let map = match &args[0] {
                Val::Map(m) => m,
                _ => {
                    return HotResult::Err(Val::from(
                        "::hot::ctx/set with 1 argument requires a Map".to_string(),
                    ));
                }
            };

            let mut count = 0;
            for (key, value) in map.iter() {
                let key_str = match key {
                    Val::Str(s) => s.clone(),
                    _ => {
                        return HotResult::Err(Val::from(format!(
                            "::hot::ctx/set map keys must be strings, got: {:?}",
                            key
                        )));
                    }
                };
                vm.context_storage
                    .insert((*key_str).to_owned(), value.clone());
                if vm.secret_keys.contains(key_str.as_ref()) {
                    vm.register_secret_value(value);
                }
                tracing::debug!("::hot::ctx/set: Set '{}' from map", key_str);
                count += 1;
            }

            tracing::debug!(
                "::hot::ctx/set: Set {} variables from map (context size: {})",
                count,
                vm.context_storage.len()
            );
            HotResult::Ok(args[0].clone())
        }
        // Two arguments: set a single key-value pair
        2 => {
            let key: String = match &args[0] {
                Val::Str(s) => (**s).to_owned(),
                _ => {
                    return HotResult::Err(Val::from("::hot::ctx/set key must be a string"));
                }
            };

            let value = args[1].clone();

            // Store in VM-specific context storage
            vm.context_storage.insert(key.clone(), value.clone());
            // A declared secret stays one however it got its value.
            if vm.secret_keys.contains(&key) {
                register_secret(vm, &value);
            }
            // Never the value: code may set a secret with `set`.
            tracing::debug!(
                "::hot::ctx/set: Set '{}' (context size: {})",
                key,
                vm.context_storage.len()
            );
            HotResult::Ok(value)
        }
        _ => HotResult::Err(Val::from(
            "::hot::ctx/set requires 1 (Map) or 2 (key, value) arguments".to_string(),
        )),
    }
}

/// Get a context variable (uses VM-specific context storage)
pub fn get(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::ctx/get", args, 1);

    let key: String = match &args[0] {
        Val::Str(s) => (**s).to_owned(),
        _ => {
            return HotResult::Err(Val::from("::hot::ctx/get key must be a string"));
        }
    };

    // Check if this key is marked as secret
    let is_secret = vm.secret_keys.contains(&key);

    // Retrieve from VM-specific context storage (clone to avoid borrow issues)
    let value = vm.context_storage.get(&key).cloned();

    match value {
        Some(value) => {
            // Never the value: whether it is secret depends on declarations
            // this run may not have.
            tracing::debug!(
                "::hot::ctx/get: Found '{}' (context size: {}, is_secret: {})",
                key,
                vm.context_storage.len(),
                is_secret
            );

            // If this is a secret key, register the value for masking, with
            // its leaves, so that fields extracted from it are masked too.
            // A default declared in source isn't one: anything that contains
            // it is only masked if it is the whole value.
            if is_secret {
                if vm.ctx_default_hashes.contains(&value_hash(&value)) {
                    vm.register_secret_value_exact(&value);
                } else {
                    register_secret(vm, &value);
                }
                tracing::debug!(
                    "::hot::ctx/get: Marked return value for key '{}' as secret",
                    key
                );
            }

            HotResult::Ok(value)
        }
        None => {
            tracing::debug!(
                "::hot::ctx/get: Key '{}' not found (context size: {}, keys: {:?})",
                key,
                vm.context_storage.len(),
                vm.context_storage.keys().collect::<Vec<_>>()
            );
            HotResult::Ok(Val::Null)
        }
    }
}

/// Recursively hash all leaf values in a Val for secret masking.
///
/// For scalar types (Str, Int, Dec, Bool, Bytes), the value's hash is recorded
/// directly. For Maps and Vecs, we recurse into children so that values
/// extracted via dot access (e.g. `req.auth.service-key.meta.api-key`) are
/// individually masked in call logs.
///
/// The top-level container's own hash is also recorded for completeness.
pub fn hash_secret_value_recursive(value: &Val, hashes: &mut ahash::AHashSet<u64>) {
    // `true`, `null` and "" are in every record, and hold no secret: masking
    // them because a secret Map has them only hides ordinary values.
    if matches!(value, Val::Null | Val::Bool(_)) || matches!(value, Val::Str(s) if s.is_empty()) {
        return;
    }

    // Always hash the value itself (covers scalars and the container as a whole)
    hashes.insert(value_hash(value));

    // Recurse into containers to hash leaf values individually
    match value {
        Val::Map(map) => {
            for (_key, child) in map.iter() {
                hash_secret_value_recursive(child, hashes);
            }
        }
        Val::Vec(vec) => {
            for child in vec {
                hash_secret_value_recursive(child, hashes);
            }
        }
        // Scalars: already hashed above
        _ => {}
    }
}

/// Register `value` for masking in this run's records from now on: as a whole
/// value, and, where long enough, inside strings. Registered during a call, it
/// also masks that call's and its parents' records, which aren't final until
/// they stop.
pub fn register_secret(vm: &mut VirtualMachine, value: &Val) {
    vm.register_secret_value(value);
}

/// Register a value for masking without storing it in the context, for a
/// credential that arrives as an ordinary argument.
pub fn mark_secret(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::ctx/mark-secret", args, 1);
    register_secret(vm, &args[0]);
    HotResult::Ok(args[0].clone())
}

/// Set a secret context variable (value is marked as secret for masking)
/// Supports two arities:
/// - set-secret(key: Str, value: Any) - set a single key-value pair as secret
/// - set-secret(ctx-map: Map) - set multiple key-value pairs from a map, all as secrets
pub fn set_secret(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    match args.len() {
        // Single map argument: set multiple secret context variables
        1 => {
            let map = match &args[0] {
                Val::Map(m) => m,
                _ => {
                    return HotResult::Err(Val::from(
                        "::hot::ctx/set-secret with 1 argument requires a Map".to_string(),
                    ));
                }
            };

            let mut count = 0;
            for (key, value) in map.iter() {
                let key_str = match key {
                    Val::Str(s) => s.clone(),
                    _ => {
                        return HotResult::Err(Val::from(format!(
                            "::hot::ctx/set-secret map keys must be strings, got: {:?}",
                            key
                        )));
                    }
                };
                vm.context_storage
                    .insert((*key_str).to_owned(), value.clone());
                vm.secret_keys.insert((*key_str).to_owned());
                register_secret(vm, value);
                tracing::debug!("::hot::ctx/set-secret: Set '{}' from map", key_str);
                count += 1;
            }

            tracing::debug!(
                "::hot::ctx/set-secret: Set {} secret variables from map (context size: {})",
                count,
                vm.context_storage.len()
            );
            HotResult::Ok(args[0].clone())
        }
        // Two arguments: set a single key-value pair as secret
        2 => {
            let key: String = match &args[0] {
                Val::Str(s) => (**s).to_owned(),
                _ => {
                    return HotResult::Err(Val::from("::hot::ctx/set-secret key must be a string"));
                }
            };

            let value = args[1].clone();

            // Store in VM-specific context storage and mark as secret
            vm.context_storage.insert(key.clone(), value.clone());
            vm.secret_keys.insert(key.clone());
            register_secret(vm, &value);
            tracing::debug!(
                "::hot::ctx/set-secret: Set '{}' = <secret> (context size: {})",
                key,
                vm.context_storage.len()
            );
            HotResult::Ok(value)
        }
        _ => HotResult::Err(Val::from(
            "::hot::ctx/set-secret requires 1 (Map) or 2 (key, value) arguments".to_string(),
        )),
    }
}
