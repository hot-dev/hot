// Map functions for  bytecode engine

use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;

/// Extract key from a key-value pair or map entry
pub fn key(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::map/key", args, 1);

    match &args[0] {
        // If it's a map with a single key-value pair, return the key
        Val::Map(map) => {
            if let Some((key, _)) = map.iter().next() {
                HotResult::Ok(key.clone())
            } else {
                HotResult::Err(Val::from("Cannot extract key from empty map"))
            }
        }
        // For other types, this might be used in a different context
        // For now, return the value as-is (identity function)
        _ => HotResult::Ok(args[0].clone()),
    }
}

/// Extract value from a key-value pair or map entry
pub fn value(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::map/value", args, 1);

    match &args[0] {
        // If it's a map with a single key-value pair, return the value
        Val::Map(map) => {
            if let Some((_, value)) = map.iter().next() {
                HotResult::Ok(value.clone())
            } else {
                HotResult::Err(Val::from("Cannot extract value from empty map"))
            }
        }
        // For other types, return the value as-is (identity function)
        _ => HotResult::Ok(args[0].clone()),
    }
}
