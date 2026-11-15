// Environment functions for  bytecode engine

use crate::lang::hot::r#type::HotResult;
use crate::lang::runtime::vm::VirtualMachine;
use crate::val::Val;
use crate::val::ValIndexMap;
use crate::validate_args;

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
            Val::Str(_) | Val::Null => return HotResult::Ok(default_value.clone()),
            _ => return HotResult::Err(Val::from("Default value must be a string or null")),
        }
    }

    match (name, default_value) {
        (Val::Str(n), Val::Str(_) | Val::Null) => match read(n, default_value) {
            Ok(value) => HotResult::Ok(value),
            Err(e) => HotResult::Err(Val::from(e)),
        },
        (_, Val::Str(_) | Val::Null) => {
            // If name is not a string, return the default
            HotResult::Ok(default_value.clone())
        }
        (_, _) => {
            // If default is neither a string nor null, return error
            HotResult::Err(Val::from(
                "Name must be a string and default value a string or null",
            ))
        }
    }
}

/// The environment variable `name`, or `default_value` when it is unset. A
/// `HOT_*` setting may come from `<NAME>_FILE`, as it does for the
/// configuration overlay.
pub(crate) fn read(name: &str, default_value: &Val) -> Result<Val, String> {
    if name.starts_with("HOT_") {
        return Ok(match crate::environment::read_optional(name)? {
            Some(value) => Val::from(value),
            None => default_value.clone(),
        });
    }
    Ok(match std::env::var(name) {
        Ok(value) => Val::from(value),
        Err(_) => default_value.clone(),
    })
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
    let mut map = ValIndexMap::default();

    for (key, value) in std::env::vars() {
        map.insert(Val::from(key), Val::from(value));
    }

    HotResult::Ok(Val::map(map))
}
