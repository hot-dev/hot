// Context functions for Hot library
// These functions manage execution context state (VM-specific)

use crate::lang::hot::r#type::HotResult;
use crate::lang::runtime::vm::VirtualMachine;
use crate::val::Val;
use crate::validate_args;

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
            tracing::debug!(
                "::hot::ctx/set: Set '{}' = {:?} (context size: {})",
                key,
                value,
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
            tracing::debug!(
                "::hot::ctx/get: Found '{}' = {:?} (context size: {}, is_secret: {})",
                key,
                value,
                vm.context_storage.len(),
                is_secret
            );

            // If this is a secret key, record value hashes for masking.
            // For scalar values, hash the value directly.
            // For Maps and Vecs, recursively hash all leaf values so that
            // individual extracted fields (via dot access) are also masked.
            if is_secret {
                hash_secret_value_recursive(&value, &mut vm.secret_value_hashes);
                vm.sync_secret_value_hashes_to_execution_context();
                tracing::debug!(
                    "::hot::ctx/get: Marked return value for key '{}' as secret ({} total hashes)",
                    key,
                    vm.secret_value_hashes.len()
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
    use std::hash::{Hash, Hasher};

    // Always hash the value itself (covers scalars and the container as a whole)
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    value.hash(&mut hasher);
    hashes.insert(hasher.finish());

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
