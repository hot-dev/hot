// Environment functions for  bytecode engine

use crate::lang::hot::r#type::HotResult;
use crate::lang::runtime::vm::VirtualMachine;
use crate::val::Val;
use crate::validate_args;
use indexmap::IndexMap;

/// Get environment variable with default value (VM-aware for isolation mode)
pub fn get(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::env/get", args, 2);

    let (name, default_value) = (&args[0], &args[1]);

    // In isolation mode, always return the default value
    if vm.is_isolation_enabled() {
        if let Val::Str(n) = name {
            tracing::debug!(
                "Isolation mode: ::hot::env/get('{}') blocked, returning default",
                n
            );
        }
        match default_value {
            Val::Str(default) => return HotResult::Ok(Val::from(default.clone())),
            _ => return HotResult::Err(Val::from("Default value must be a string")),
        }
    }

    // Normal mode: read from environment
    match (name, default_value) {
        (Val::Str(n), Val::Str(default)) => match std::env::var(&**n) {
            Ok(value) => HotResult::Ok(Val::from(value)),
            Err(_) => HotResult::Ok(Val::from(default.clone())),
        },
        (_, Val::Str(default)) => {
            // If name is not a string, return the default
            HotResult::Ok(Val::from(default.clone()))
        }
        (_, _) => {
            // If default is not a string, return error
            HotResult::Err(Val::from("Both name and default value must be strings"))
        }
    }
}

/// Get all environment variables as a map (VM-aware for isolation mode)
pub fn get_all(vm: &mut VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if !args.is_empty() {
        return HotResult::Err(Val::from("::hot::env/get-all expects 0 arguments"));
    }

    // In isolation mode, return an empty map
    if vm.is_isolation_enabled() {
        tracing::debug!("Isolation mode: ::hot::env/get-all() blocked, returning empty map");
        return HotResult::Ok(Val::map_empty());
    }

    // Normal mode: return all environment variables
    let mut map = IndexMap::new();

    for (key, value) in std::env::vars() {
        map.insert(Val::from(key), Val::from(value));
    }

    HotResult::Ok(Val::Map(Box::new(map)))
}
