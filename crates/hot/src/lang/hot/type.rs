// Type constructor functions for Hot library
// These functions implement core type constructors and type conversion

use crate::val::Val;
use crate::{validate_args, validate_args_at_least};
use fastnum::D256;
use indexmap::IndexMap;

/// Hot Result type for Rust library functions
/// This enum is used by all hotlib functions to return success or failure.
#[derive(Debug, Clone, PartialEq)]
pub enum HotResult<T> {
    Ok(T),
    Err(T),
}

impl<T> HotResult<T> {
    pub fn ok(self) -> Option<T> {
        match self {
            HotResult::Ok(val) => Some(val),
            HotResult::Err(_) => None,
        }
    }

    pub fn err(self) -> Option<T> {
        match self {
            HotResult::Ok(_) => None,
            HotResult::Err(val) => Some(val),
        }
    }
}

/// Core Str type constructor
pub(crate) fn str_internal(args: &[Val]) -> HotResult<Val> {
    if args.is_empty() {
        return HotResult::Ok(Val::from(""));
    }

    if args.len() != 1 {
        return HotResult::Err(Val::from(format!(
            "Str constructor expects 0 or 1 arguments, got {}",
            args.len()
        )));
    }

    match &args[0] {
        Val::Str(s) => HotResult::Ok(Val::from(s.clone())),
        Val::Int(i) => HotResult::Ok(Val::from(i.to_string())),
        Val::Dec(d) => HotResult::Ok(Val::from(d.to_string())),
        Val::Bool(b) => HotResult::Ok(Val::from(b.to_string())),
        Val::Null => HotResult::Ok(Val::from("null")),
        Val::Map(m) => {
            // Typed object special-cases
            if let Some(Val::Str(type_name)) = m.get(&Val::from("$type")) {
                // For Fn and Namespace types, stringify to their $val
                if (&**type_name == "::hot::type/Fn" || &**type_name == "::hot::type/Namespace")
                    && m.get(&Val::from("$val")).is_some()
                    && let Some(Val::Str(val)) = m.get(&Val::from("$val"))
                {
                    return HotResult::Ok(Val::from(val.clone()));
                }

                // Delegate Hot time typed objects to ::hot::time/to-string for ISO formatting
                if type_name.starts_with("::hot::time/") {
                    return crate::lang::hot::time::to_string(std::slice::from_ref(&args[0]));
                }

                // Generic typed object default string representation
                return HotResult::Ok(Val::from(format!(
                    "{}({})",
                    type_name,
                    format_map_contents(m)
                )));
            }
            HotResult::Ok(Val::from(format_map_contents(m)))
        }
        Val::Vec(v) => {
            let items: Vec<String> = v.iter().map(format_val_for_string).collect();
            HotResult::Ok(Val::from(format!("[{}]", items.join(", "))))
        }
        Val::Bytes(bytes) => {
            // Convert bytes to UTF-8 string (lossy - replaces invalid UTF-8 with replacement char)
            match String::from_utf8(bytes.clone()) {
                Ok(s) => HotResult::Ok(Val::from(s)),
                Err(_) => {
                    // Fall back to lossy conversion if not valid UTF-8
                    HotResult::Ok(Val::from(String::from_utf8_lossy(bytes).to_string()))
                }
            }
        }
        Val::Box(b) => {
            // Try to downcast to FunctionRef for clean string conversion
            if let Some(func_ref) = b
                .as_any()
                .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>()
            {
                HotResult::Ok(Val::from(func_ref.name.clone()))
            } else {
                // Fallback to debug representation for other Box types
                HotResult::Ok(Val::from(format!("{:?}", args[0])))
            }
        }
        _ => HotResult::Ok(Val::from(format!("{:?}", args[0]))),
    }
}

thread_local! {
    static STR_DISPATCH_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

const MAX_STR_DISPATCH_DEPTH: u32 = 16;

/// VM-aware Str constructor with implements-dispatch
pub fn str_constructor(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    if args.is_empty() {
        return HotResult::Ok(Val::from(""));
    }
    if args.len() != 1 {
        return HotResult::Err(Val::from(format!(
            "Str constructor expects 0 or 1 arguments, got {}",
            args.len()
        )));
    }

    // If arg is a typed object, try user-defined implementation: SourceType -> Str
    if let Val::Map(map) = &args[0] {
        tracing::debug!("str_constructor_vm: Processing map argument: {:?}", map);
        let _has_type = map.contains_key(&Val::from("$type"));
        // If this is a typed Fn value, unwrap to its $val string before proceeding
        if let Some(Val::Str(tn)) = map.get(&Val::from("$type"))
            && &**tn == "::hot::type/Fn"
            && let Some(inner) = map.get(&Val::from("$val"))
        {
            // Stringify function refs by their $val (e.g., "::ns/fn")
            if let Val::Str(s) = inner {
                return HotResult::Ok(Val::from(s.clone()));
            }
        }
        if let Some(Val::Str(type_name_full)) = map.get(&Val::from("$type")) {
            let source_short = type_name_full
                .rsplit('/')
                .next()
                .unwrap_or(type_name_full)
                .to_string();
            let key = (source_short.clone(), "Str".to_string());
            let key_full = (type_name_full.clone(), "Str".to_string());

            tracing::debug!(
                "Type dispatch: Looking for implementation for type '{}' (short: '{}') -> Str",
                type_name_full,
                source_short
            );
            tracing::debug!("Type dispatch: Trying keys: {:?} and {:?}", key, key_full);
            tracing::debug!(
                "Type dispatch: Available implementations: {:?}",
                vm.get_type_implementations()
            );

            // Use scope-aware type implementation resolution
            if let Some(impl_fn_name) = vm
                .resolve_type_implementation(&source_short, "Str")
                .or_else(|| vm.resolve_type_implementation(type_name_full, "Str"))
            {
                // Check type-dispatch recursion depth before calling the
                // implementation. This catches buggy T -> Str implementations
                // that call Str() on the same type, preventing unbounded Rust
                // stack growth that would crash the worker process.
                let depth = STR_DISPATCH_DEPTH.get();
                if depth >= MAX_STR_DISPATCH_DEPTH {
                    return HotResult::Err(Val::from(format!(
                        "Str type dispatch recursion limit reached (depth {}) for type '{}'",
                        depth, type_name_full
                    )));
                }

                tracing::debug!(
                    "Type dispatch: Found implementation function '{}'",
                    impl_fn_name
                );

                // For objects with $val field, extract the inner data for the implementation
                let dispatch_arg = if let Some(inner_val) = map.get(&Val::from("$val")) {
                    tracing::debug!(
                        "Type dispatch: Extracting $val for implementation: {:?}",
                        inner_val
                    );
                    // Create a new typed object with the inner data and the type
                    if let Val::Map(inner_map) = inner_val {
                        let mut typed_inner = inner_map.clone();
                        typed_inner.insert(Val::from("$type"), Val::from(type_name_full.clone()));
                        Val::Map(typed_inner)
                    } else {
                        // For non-map $val, create a simple typed wrapper
                        let mut typed_wrapper = indexmap::IndexMap::new();
                        typed_wrapper.insert(Val::from("$type"), Val::from(type_name_full.clone()));
                        typed_wrapper.insert(Val::from("$val"), inner_val.clone());
                        Val::Map(Box::new(typed_wrapper))
                    }
                } else {
                    // No $val field, use the object as-is
                    args[0].clone()
                };
                // Try calling by qualified name first (current namespace), then type's namespace, then unqualified
                let qualified = format!("{}/{}", vm.get_current_namespace(), impl_fn_name);

                // Extract the type's namespace from the full type name
                let type_namespace = if let Some(slash_pos) = type_name_full.rfind('/') {
                    &type_name_full[..slash_pos]
                } else {
                    type_name_full
                };
                let type_qualified = format!("{}/{}", type_namespace, impl_fn_name);

                STR_DISPATCH_DEPTH.set(depth + 1);
                let call_res = vm
                    .execute_function_call_by_name(&qualified, std::slice::from_ref(&dispatch_arg))
                    .or_else(|_| {
                        vm.execute_function_call_by_name(
                            &type_qualified,
                            std::slice::from_ref(&dispatch_arg),
                        )
                    })
                    .or_else(|_| {
                        vm.execute_function_call_by_name(
                            &impl_fn_name,
                            std::slice::from_ref(&dispatch_arg),
                        )
                    });
                STR_DISPATCH_DEPTH.set(depth);

                match call_res {
                    Ok(val) => {
                        // Guard: if the implementation returned the same typed value,
                        // fall through to str_internal to avoid infinite recursion
                        if let Val::Map(result_map) = &val
                            && let Some(Val::Str(rt)) = result_map.get(&Val::from("$type"))
                            && **rt == **type_name_full
                        {
                            return str_internal(args);
                        }
                        return HotResult::Ok(val);
                    }
                    Err(e) => return HotResult::Err(Val::from(e.to_string())),
                }
            }
        }
    }

    // Fallback to regular constructor behavior (string conversion)
    str_internal(args)
}

/// Core Int type constructor
fn int_internal(args: &[Val]) -> HotResult<Val> {
    validate_args!("Int constructor", args, 1);

    match &args[0] {
        Val::Int(i) => HotResult::Ok(Val::Int(*i)),
        Val::Dec(d) => HotResult::Ok(Val::Int(d.to_i64().unwrap_or(0))),
        Val::Str(s) => match s.parse::<i64>() {
            Ok(i) => HotResult::Ok(Val::Int(i)),
            Err(_) => HotResult::Err(Val::from(format!("Cannot convert '{}' to Int", s))),
        },
        Val::Bool(b) => HotResult::Ok(Val::Int(if *b { 1 } else { 0 })),
        Val::Map(m) => {
            // Check if this is a typed object that implements Int
            if let Some(type_val) = m.get(&Val::from("$type"))
                && let Val::Str(_type_name) = type_val
            {
                // Look for $val field or type implementation
                if let Some(val_field) = m.get(&Val::from("$val")) {
                    return int_internal(std::slice::from_ref(val_field));
                }
            }
            HotResult::Err(Val::from("Cannot convert Map to Int"))
        }
        _ => HotResult::Err(Val::from(format!("Cannot convert {:?} to Int", args[0]))),
    }
}

/// VM-aware Int constructor with implements-dispatch
pub fn int_constructor(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from(format!(
            "Int constructor expects 1 argument, got {}",
            args.len()
        )));
    }
    if let Val::Map(map) = &args[0]
        && let Some(Val::Str(type_name_full)) = map.get(&Val::from("$type"))
    {
        let source_short = type_name_full
            .rsplit('/')
            .next()
            .unwrap_or(type_name_full)
            .to_string();

        // Use scope-aware type implementation resolution
        if let Some(impl_fn_name) = vm
            .resolve_type_implementation(&source_short, "Int")
            .or_else(|| vm.resolve_type_implementation(type_name_full, "Int"))
        {
            let qualified = format!("{}/{}", vm.get_current_namespace(), impl_fn_name);
            let call_res = vm
                .execute_function_call_by_name(&qualified, std::slice::from_ref(&args[0]))
                .or_else(|_| {
                    vm.execute_function_call_by_name(&impl_fn_name, std::slice::from_ref(&args[0]))
                });
            match call_res {
                Ok(val) => return HotResult::Ok(val),
                Err(e) => return HotResult::Err(Val::from(e.to_string())),
            }
        }
    }
    int_internal(args)
}

/// Core Dec type constructor
fn dec_internal(args: &[Val]) -> HotResult<Val> {
    validate_args!("Dec constructor", args, 1);

    match &args[0] {
        Val::Dec(d) => HotResult::Ok(Val::Dec(*d)),
        Val::Int(i) => HotResult::Ok(Val::Dec(D256::from(*i))),
        Val::Str(s) => match s.parse::<D256>() {
            Ok(d) => HotResult::Ok(Val::Dec(d)),
            Err(_) => HotResult::Err(Val::from(format!("Cannot convert '{}' to Dec", s))),
        },
        Val::Bool(b) => HotResult::Ok(Val::Dec(D256::from(if *b { 1 } else { 0 }))),
        Val::Map(m) => {
            // Check if this is a typed object that implements Dec
            if let Some(type_val) = m.get(&Val::from("$type"))
                && let Val::Str(_type_name) = type_val
            {
                // Look for $val field or type implementation
                if let Some(val_field) = m.get(&Val::from("$val")) {
                    return dec_internal(std::slice::from_ref(val_field));
                }
            }
            HotResult::Err(Val::from("Cannot convert Map to Dec"))
        }
        _ => HotResult::Err(Val::from(format!("Cannot convert {:?} to Dec", args[0]))),
    }
}

/// VM-aware Dec constructor with implements-dispatch
pub fn dec_constructor(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from(format!(
            "Dec constructor expects 1 argument, got {}",
            args.len()
        )));
    }
    if let Val::Map(map) = &args[0]
        && let Some(Val::Str(type_name_full)) = map.get(&Val::from("$type"))
    {
        let source_short = type_name_full
            .rsplit('/')
            .next()
            .unwrap_or(type_name_full)
            .to_string();

        // Use scope-aware type implementation resolution (same as str_constructor)
        if let Some(impl_fn_name) = vm
            .resolve_type_implementation(&source_short, "Dec")
            .or_else(|| vm.resolve_type_implementation(type_name_full, "Dec"))
        {
            let qualified = format!("{}/{}", vm.get_current_namespace(), impl_fn_name);
            let call_res = vm
                .execute_function_call_by_name(&qualified, std::slice::from_ref(&args[0]))
                .or_else(|_| {
                    vm.execute_function_call_by_name(&impl_fn_name, std::slice::from_ref(&args[0]))
                });
            match call_res {
                Ok(val) => return HotResult::Ok(val),
                Err(e) => return HotResult::Err(Val::from(e.to_string())),
            }
        }
    }
    dec_internal(args)
}

/// Core Bool type constructor
fn bool_internal(args: &[Val]) -> HotResult<Val> {
    validate_args!("Bool constructor", args, 1);

    match &args[0] {
        Val::Bool(b) => HotResult::Ok(Val::Bool(*b)),
        Val::Int(i) => HotResult::Ok(Val::Bool(*i != 0)),
        Val::Dec(d) => HotResult::Ok(Val::Bool(!d.is_zero())),
        Val::Str(s) => match s.to_lowercase().as_str() {
            "true" | "yes" | "1" => HotResult::Ok(Val::Bool(true)),
            "false" | "no" | "0" => HotResult::Ok(Val::Bool(false)),
            _ => HotResult::Err(Val::from(format!("Cannot convert '{}' to Bool", s))),
        },
        Val::Null => HotResult::Ok(Val::Bool(false)),
        _ => HotResult::Err(Val::from(format!("Cannot convert {:?} to Bool", args[0]))),
    }
}

/// VM-aware Bool constructor with implements-dispatch
pub fn bool_constructor(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from(format!(
            "Bool constructor expects 1 argument, got {}",
            args.len()
        )));
    }
    if let Val::Map(map) = &args[0]
        && let Some(Val::Str(type_name_full)) = map.get(&Val::from("$type"))
    {
        let source_short = type_name_full
            .rsplit('/')
            .next()
            .unwrap_or(type_name_full)
            .to_string();

        // Use scope-aware type implementation resolution
        if let Some(impl_fn_name) = vm
            .resolve_type_implementation(&source_short, "Bool")
            .or_else(|| vm.resolve_type_implementation(type_name_full, "Bool"))
        {
            let qualified = format!("{}/{}", vm.get_current_namespace(), impl_fn_name);
            let call_res = vm
                .execute_function_call_by_name(&qualified, std::slice::from_ref(&args[0]))
                .or_else(|_| {
                    vm.execute_function_call_by_name(&impl_fn_name, std::slice::from_ref(&args[0]))
                });
            match call_res {
                Ok(val) => return HotResult::Ok(val),
                Err(e) => return HotResult::Err(Val::from(e.to_string())),
            }
        }
    }
    bool_internal(args)
}

/// Core Byte type constructor
pub fn byte_constructor(args: &[Val]) -> HotResult<Val> {
    validate_args!("Byte constructor", args, 1);

    match &args[0] {
        Val::Byte(b) => HotResult::Ok(Val::Byte(*b)),
        Val::Int(i) => {
            if *i >= 0 && *i <= 255 {
                HotResult::Ok(Val::Byte(*i as u8))
            } else {
                HotResult::Err(Val::from(format!("Int {} is out of byte range (0-255)", i)))
            }
        }
        Val::Str(s) => match s.parse::<u8>() {
            Ok(b) => HotResult::Ok(Val::Byte(b)),
            Err(_) => HotResult::Err(Val::from(format!("Cannot convert '{}' to Byte", s))),
        },
        _ => HotResult::Err(Val::from(format!("Cannot convert {:?} to Byte", args[0]))),
    }
}

/// Core Bytes type constructor
pub fn bytes_constructor(args: &[Val]) -> HotResult<Val> {
    validate_args!("Bytes constructor", args, 1);

    match &args[0] {
        Val::Bytes(b) => HotResult::Ok(Val::Bytes(b.clone())),
        Val::Vec(v) => {
            // Convert Vec of Ints to Bytes
            let mut bytes = Vec::new();
            for val in v {
                match val {
                    Val::Int(i) => {
                        if *i >= 0 && *i <= 255 {
                            bytes.push(*i as u8);
                        } else {
                            return HotResult::Err(Val::from(format!(
                                "Int {} is out of byte range (0-255)",
                                i
                            )));
                        }
                    }
                    _ => {
                        return HotResult::Err(Val::from(format!(
                            "Cannot convert {:?} to byte",
                            val
                        )));
                    }
                }
            }
            HotResult::Ok(Val::Bytes(bytes))
        }
        Val::Str(s) => {
            // Convert string to UTF-8 bytes
            HotResult::Ok(Val::Bytes(s.as_bytes().to_vec()))
        }
        _ => HotResult::Err(Val::from(format!("Cannot convert {:?} to Bytes", args[0]))),
    }
}

/// Generic user-defined type constructor
/// Creates a typed object with $type and $val metadata
/// Following Hot's pattern: {"$type": "TypeName", "$val": {actual_data}}
pub fn user_type_constructor(type_name: &str, args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from(format!(
            "{} constructor expects 1 argument, got {}",
            type_name,
            args.len()
        )));
    }

    // Always create the Hot type pattern: {"$type": "TypeName", "$val": data}
    let mut result_map = IndexMap::new();
    result_map.insert(Val::from("$type"), Val::from(type_name.to_string()));
    result_map.insert(Val::from("$val"), args[0].clone());

    HotResult::Ok(Val::Map(Box::new(result_map)))
}

/// Helper function to format map contents for string representation
fn format_map_contents(map: &IndexMap<Val, Val>) -> String {
    let items: Vec<String> = map
        .iter()
        .filter(|(key, _)| !matches!(key, Val::Str(s) if s.starts_with('$'))) // Skip metadata
        .map(|(key, value)| {
            format!(
                "{}: {}",
                format_key_for_string(key),
                format_val_for_string(value)
            )
        })
        .collect();
    format!("{{{}}}", items.join(", "))
}

/// Helper function to format a map key for string representation (unquoted)
fn format_key_for_string(val: &Val) -> String {
    match val {
        Val::Str(s) => (**s).to_owned(), // Unquoted keys
        _ => format!("{:?}", val),
    }
}

/// Helper function to format a Val for string representation
fn format_val_for_string(val: &Val) -> String {
    match val {
        Val::Str(s) => format!("\"{}\"", s),
        Val::Int(i) => i.to_string(),
        Val::Dec(d) => d.to_string(),
        Val::Bool(b) => b.to_string(),
        Val::Null => "null".to_string(),
        _ => format!("{:?}", val),
    }
}

/// Type checking functions
pub fn is_str(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from("is-str expects 1 argument"));
    }
    HotResult::Ok(Val::Bool(matches!(args[0], Val::Str(_))))
}

pub fn is_int(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from("is-int expects 1 argument"));
    }
    HotResult::Ok(Val::Bool(matches!(args[0], Val::Int(_))))
}

pub fn is_dec(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from("is-dec expects 1 argument"));
    }
    HotResult::Ok(Val::Bool(matches!(args[0], Val::Dec(_))))
}

pub fn is_bool(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from("is-bool expects 1 argument"));
    }
    HotResult::Ok(Val::Bool(matches!(args[0], Val::Bool(_))))
}

pub fn is_byte(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from("is-byte expects 1 argument"));
    }
    HotResult::Ok(Val::Bool(matches!(args[0], Val::Byte(_))))
}

pub fn is_bytes(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from("is-bytes expects 1 argument"));
    }
    HotResult::Ok(Val::Bool(matches!(args[0], Val::Bytes(_))))
}

pub fn is_vec(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from("is-vec expects 1 argument"));
    }
    HotResult::Ok(Val::Bool(matches!(args[0], Val::Vec(_))))
}

pub fn is_map(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from("is-map expects 1 argument"));
    }
    HotResult::Ok(Val::Bool(matches!(args[0], Val::Map(_))))
}

pub fn is_null(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from("is-null expects 1 argument"));
    }
    HotResult::Ok(Val::Bool(matches!(args[0], Val::Null)))
}

/// Core Null constructor
pub fn null_constructor(args: &[Val]) -> HotResult<Val> {
    if !args.is_empty() {
        return HotResult::Err(Val::from(format!(
            "Null constructor expects 0 arguments, got {}",
            args.len()
        )));
    }
    HotResult::Ok(Val::Null)
}

/// Core Vec constructor
pub fn vec_constructor(args: &[Val]) -> HotResult<Val> {
    // Vec constructor can take any number of arguments
    HotResult::Ok(Val::Vec(args.to_vec()))
}

/// Core Map constructor
pub fn map_constructor(args: &[Val]) -> HotResult<Val> {
    // Map constructor can take pairs of arguments
    if !args.len().is_multiple_of(2) {
        return HotResult::Err(Val::from(
            "Map constructor expects an even number of arguments (key-value pairs)".to_string(),
        ));
    }

    let mut map = IndexMap::new();
    for chunk in args.chunks(2) {
        map.insert(chunk[0].clone(), chunk[1].clone());
    }

    HotResult::Ok(Val::Map(Box::new(map)))
}

/// Core Namespace constructor
pub fn namespace_constructor(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::type/Namespace", args, 1);

    match &args[0] {
        Val::Str(ns_path) => {
            // Return a real Hot typed object: {"$type": "::hot::type/Namespace", "$val": ns_path}
            user_type_constructor(
                "::hot::type/Namespace",
                std::slice::from_ref(&Val::from(ns_path.clone())),
            )
        }
        Val::Map(m) => {
            // If already a typed Namespace, return as-is
            if let Some(Val::Str(tn)) = m.get(&Val::from("$type"))
                && &**tn == "::hot::type/Namespace"
            {
                return HotResult::Ok(Val::Map(m.clone()));
            }
            HotResult::Err(Val::from(
                "Namespace constructor expects a string path".to_string(),
            ))
        }
        _ => HotResult::Err(Val::from(
            "Namespace constructor expects a string path".to_string(),
        )),
    }
}

/// Core Var constructor
pub fn var_constructor(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::type/Var", args, 1);

    // For now, just return the value as-is. A dedicated variable reference
    // type can be introduced when the runtime needs one.
    HotResult::Ok(args[0].clone())
}

/// Core Fn constructor
pub fn fn_constructor(args: &[Val]) -> HotResult<Val> {
    // Construct a real Hot typed Fn object
    if args.is_empty() {
        return HotResult::Err(Val::from(
            "Fn constructor expects at least 1 argument".to_string(),
        ));
    }
    let value = &args[0];
    let inner_val = match value {
        // Function by fully-qualified name
        Val::Str(s) => Val::from(s.clone()),
        // Boxed values: keep LambdaInfo or FunctionRef boxed; fallback to string for others
        Val::Box(b) => {
            if b.as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                .is_some()
                || b.as_any()
                    .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>()
                    .is_some()
            {
                Val::Box(b.clone_box())
            } else {
                Val::from(b.to_string())
            }
        }
        // Already a typed Fn – pass through
        Val::Map(m) => {
            if let Some(Val::Str(tn)) = m.get(&Val::from("$type"))
                && &**tn == "::hot::type/Fn"
            {
                return HotResult::Ok(Val::Map(m.clone()));
            }
            return HotResult::Err(Val::from("Fn constructor expects Str | Fn"));
        }
        _ => return HotResult::Err(Val::from("Fn constructor expects Str | Fn")),
    };

    user_type_constructor("::hot::type/Fn", std::slice::from_ref(&inner_val))
}

/// Core Any constructor
pub fn any_constructor(args: &[Val]) -> HotResult<Val> {
    // Any constructor can accept any value
    if args.is_empty() {
        return HotResult::Ok(Val::Null);
    }
    HotResult::Ok(args[0].clone())
}

/// Type checking function
/// Extract type name from type reference (for is-type function)
pub fn extract_type_name(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from("extract-type-name expects 1 argument"));
    }

    let type_ref = &args[0];

    match type_ref {
        // Already a string type name
        Val::Str(s) => HotResult::Ok(Val::from(s.clone())),

        // Function reference box (e.g., &Str -> "Str")
        Val::Box(boxed) => {
            let box_str = boxed.to_string();
            tracing::debug!("extract-type-name: Box string = '{}'", box_str);

            // The box_str is already the function name like "::hot::type/Str"
            // Extract type name from "::hot::type/Str" -> "Str"
            if let Some(type_name) = box_str.split('/').next_back() {
                tracing::debug!("extract-type-name: Final type_name = '{}'", type_name);
                return HotResult::Ok(Val::from(type_name.to_string()));
            }

            // Fallback
            tracing::debug!("extract-type-name: Falling back to Unknown");
            HotResult::Ok(Val::from("Unknown"))
        }

        // Function reference object (e.g., &Str -> "Str") - for map-based references
        Val::Map(m) => {
            if let Some(Val::Map(box_map)) = m.get(&Val::from("$box"))
                && let Some(Val::Str(fnref)) = box_map.get(&Val::from("$fnref"))
            {
                // Extract type name from "::hot::type/Str" -> "Str"
                if let Some(type_name) = fnref.split('/').next_back() {
                    return HotResult::Ok(Val::from(type_name.to_string()));
                }
            }
            // Fallback for other map structures
            HotResult::Ok(Val::from("Unknown"))
        }

        // Fallback: convert to string
        _ => HotResult::Ok(Val::from(format!("{:?}", type_ref))),
    }
}

/// Get the canonical type path for a value
/// Maps primitive Val types to their ::hot::type paths
fn get_value_type_path(value: &Val) -> Option<String> {
    match value {
        Val::Str(_) => Some("::hot::type/Str".to_string()),
        Val::Int(_) => Some("::hot::type/Int".to_string()),
        Val::Dec(_) => Some("::hot::type/Dec".to_string()),
        Val::Bool(_) => Some("::hot::type/Bool".to_string()),
        Val::Null => Some("::hot::type/Null".to_string()),
        Val::Vec(_) => Some("::hot::type/Vec".to_string()),
        Val::Byte(_) => Some("::hot::type/Byte".to_string()),
        Val::Bytes(_) => Some("::hot::type/Bytes".to_string()),
        Val::Map(map) => {
            // Check for $type field for custom types (including Result.Ok/Result.Err variants)
            if let Some(Val::Str(type_str)) = map.get(&Val::from("$type")) {
                Some((**type_str).to_owned())
            } else {
                // Plain map
                Some("::hot::type/Map".to_string())
            }
        }
        Val::Box(boxed) => {
            // Check for function ref, namespace ref, lambda, etc.
            if boxed
                .as_any()
                .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>()
                .is_some()
            {
                Some("::hot::type/Fn".to_string())
            } else if boxed
                .as_any()
                .downcast_ref::<crate::lang::refs::NamespaceRef>()
                .is_some()
            {
                Some("::hot::type/Namespace".to_string())
            } else if boxed
                .as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                .is_some()
            {
                Some("::hot::type/Fn".to_string())
            } else if boxed
                .as_any()
                .downcast_ref::<crate::lang::hot::iter::IteratorBox>()
                .is_some()
            {
                Some("::hot::iter/Iter".to_string())
            } else {
                None
            }
        }
    }
}

/// Check if a value is of a given type.
/// Handles primitives, custom types, enum variants, and type-level matching.
///
/// The first argument is lazy to prevent auto-unwrapping of Result.Err values.
pub fn is_type(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    tracing::debug!("is_type called with {} args: {:?}", args.len(), args);
    validate_args!("::hot::type/is-type", args, 2);

    // First, evaluate any lazy thunks to get the actual value (without auto-unwrapping)
    let value = match &args[0] {
        Val::Box(b) => {
            // Check if this is a zero-argument lambda (lazy evaluation)
            if let Some(lambda_info) = b
                .as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
            {
                if lambda_info.parameters.is_empty() {
                    // For lazy parameter lambdas, suppress Result checking during evaluation
                    // This allows them to evaluate and return Result types without triggering automatic failures
                    let saved_suppress = if lambda_info.is_lazy_param {
                        let current = vm.get_suppress_result_checking();
                        vm.set_suppress_result_checking(true);
                        Some(current)
                    } else {
                        None
                    };

                    // Evaluate the lazy thunk
                    let result = match vm.execute_lambda(&args[0], &[]) {
                        Ok(val) => val,
                        Err(err) => {
                            // Restore Result checking
                            if let Some(saved) = saved_suppress {
                                vm.set_suppress_result_checking(saved);
                            }
                            return HotResult::Err(Val::from(format!(
                                "Failed to evaluate lazy value: {:?}",
                                err
                            )));
                        }
                    };

                    // Restore Result checking
                    if let Some(saved) = saved_suppress {
                        vm.set_suppress_result_checking(saved);
                    }

                    result
                } else {
                    // Has parameters, use as-is
                    args[0].clone()
                }
            } else {
                // Not a lambda, use as-is
                args[0].clone()
            }
        }
        _ => {
            // Not boxed, use as-is
            args[0].clone()
        }
    };

    let type_ref = &args[1]; // Second arg: type reference

    // Extract type path from type_ref - keep full qualified path for proper comparison
    let type_path = match type_ref {
        // Direct string - normalize to full path if it's a short name
        Val::Str(s) => {
            if s.contains('/') || s.starts_with("::") {
                (**s).to_owned()
            } else {
                // Short name like "Str" -> assume it's in ::hot::type
                format!("::hot::type/{}", s)
            }
        }

        // Function reference box - get the full qualified name
        Val::Box(boxed) => {
            let box_str = boxed.to_string();
            if box_str.contains('/') || box_str.starts_with("::") {
                box_str
            } else {
                format!("::hot::type/{}", box_str)
            }
        }

        // Map-based type references or type descriptors
        Val::Map(map) => {
            if let Some(Val::Str(type_str)) = map.get(&Val::from("$type")) {
                (**type_str).to_owned()
            } else if let Some(Val::Str(fnref)) = map.get(&Val::from("$fnref")) {
                (**fnref).to_owned()
            } else {
                return HotResult::Err(Val::from("Invalid type reference map"));
            }
        }

        _ => {
            return HotResult::Err(Val::from(
                "Type reference must be a string, box, or map".to_string(),
            ));
        }
    };

    // Get the canonical type path for the value
    let value_type_path = get_value_type_path(&value);

    // Special case: Any type always matches
    if type_path == "::hot::type/Any" {
        return HotResult::Ok(Val::Bool(true));
    }

    // Compare paths - support both exact and type-level matching
    let matches = match value_type_path {
        Some(vtp) => {
            if type_path == vtp {
                // Exact match (e.g., Result.Ok matches Result.Ok)
                true
            } else if vtp.starts_with(&format!("{}.", type_path)) {
                // Type-level match: type_path is "Result", vtp is "Result.Ok"
                // This allows is-type(result_ok_value, Result) to return true
                true
            } else {
                false
            }
        }
        None => false,
    };

    HotResult::Ok(Val::Bool(matches))
}

/// Typed map constructor
pub fn typed_map(args: &[Val]) -> HotResult<Val> {
    validate_args_at_least!("::hot::type/typed-map", args, 1);

    let type_name = match &args[0] {
        Val::Str(s) => (**s).to_owned(),
        _ => return HotResult::Err(Val::from("typed-map expects a string type name")),
    };

    // Create a map with type information
    let mut map = IndexMap::new();
    map.insert(Val::from("$type"), Val::from(type_name));

    // Add any additional key-value pairs
    if args.len() > 1 {
        if !(args.len() - 1).is_multiple_of(2) {
            return HotResult::Err(Val::from(
                "typed-map expects pairs of key-value arguments after type name".to_string(),
            ));
        }

        for chunk in args[1..].chunks(2) {
            map.insert(chunk[0].clone(), chunk[1].clone());
        }
    }

    HotResult::Ok(Val::Map(Box::new(map)))
}

/// Untype function - removes type information from typed objects recursively
pub fn untype(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::type/untype", args, 1);

    let form = &args[0];
    untype_recursive(form)
}

/// Recursively remove type metadata from a structure.
///
/// Strips `$type`/`$val` wrapping at every level:
/// - `{$type: "Foo", $val: inner}` → recurse into `inner`
/// - `{$type: "Foo", field: …}` → new map without `$`-prefixed keys, values recursed
/// - Regular maps/vecs → recurse into values/elements
/// - Scalars → returned as-is
pub fn untype_recursive(form: &Val) -> HotResult<Val> {
    match form {
        Val::Map(map) => {
            // If this is a typed structure with $type, prefer $val if present
            if map.contains_key(&Val::from("$type")) {
                if let Some(val_content) = map.get(&Val::from("$val")) {
                    return untype_recursive(val_content);
                }

                // Otherwise, construct a new map excluding metadata keys (those starting with '$')
                let mut new_map = IndexMap::new();
                for (key, value) in map.iter() {
                    if let Val::Str(key_str) = key
                        && key_str.starts_with('$')
                    {
                        continue;
                    }
                    let untyped_value = match untype_recursive(value) {
                        HotResult::Ok(val) => val,
                        HotResult::Err(err) => return HotResult::Err(err),
                    };
                    new_map.insert(key.clone(), untyped_value);
                }
                HotResult::Ok(Val::Map(Box::new(new_map)))
            } else {
                // Regular map: recursively untype values (keys remain as-is)
                let mut new_map = IndexMap::new();
                for (key, value) in map.iter() {
                    let untyped_value = match untype_recursive(value) {
                        HotResult::Ok(val) => val,
                        HotResult::Err(err) => return HotResult::Err(err),
                    };
                    new_map.insert(key.clone(), untyped_value);
                }
                HotResult::Ok(Val::Map(Box::new(new_map)))
            }
        }
        Val::Vec(vec) => {
            // Recursively untype all vector elements
            let mut new_vec = Vec::new();
            for item in vec {
                match untype_recursive(item) {
                    HotResult::Ok(untyped_item) => new_vec.push(untyped_item),
                    HotResult::Err(err) => return HotResult::Err(err),
                }
            }
            HotResult::Ok(Val::Vec(new_vec))
        }
        _ => HotResult::Ok(form.clone()),
    }
}

// Result type functions (moved from result.rs since they map to ::hot::type/*)

/// Check if a result is Ok
pub fn result_is_ok(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    validate_args!("::hot::type/is-ok", args, 1);

    // First, evaluate any lazy thunks to get the actual result value
    let evaluated_val = match &args[0] {
        Val::Box(b) => {
            // Check if this is a zero-argument lambda (lazy evaluation)
            if let Some(lambda_info) = b
                .as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
            {
                if lambda_info.parameters.is_empty() {
                    // For lazy parameter lambdas, suppress Result checking during evaluation
                    // This allows them to evaluate and return Result types without triggering automatic failures
                    let saved_suppress = if lambda_info.is_lazy_param {
                        let current = vm.get_suppress_result_checking();
                        vm.set_suppress_result_checking(true);
                        Some(current)
                    } else {
                        None
                    };

                    // Evaluate the lazy thunk
                    let result = match vm.execute_lambda(&args[0], &[]) {
                        Ok(val) => val,
                        Err(_err) => {
                            // Restore Result checking
                            if let Some(saved) = saved_suppress {
                                vm.set_suppress_result_checking(saved);
                            }
                            // If evaluation failed, consider it an error result (not ok)
                            return HotResult::Ok(Val::Bool(false));
                        }
                    };

                    // Restore Result checking
                    if let Some(saved) = saved_suppress {
                        vm.set_suppress_result_checking(saved);
                    }

                    result
                } else {
                    // Has parameters, use as-is
                    args[0].clone()
                }
            } else {
                // Not a lambda, use as-is
                args[0].clone()
            }
        }
        _ => {
            // Not boxed, use as-is
            args[0].clone()
        }
    };

    // Now check if the evaluated result is Ok
    // Handle Raw wrapped values first - may be nested
    let mut actual_val = &evaluated_val;
    while actual_val.is_type("Raw") {
        match actual_val {
            Val::Map(map) => {
                if let Some(inner_val) = map.get(&Val::from("$val")) {
                    actual_val = inner_val;
                } else {
                    break;
                }
            }
            _ => {
                break;
            }
        }
    }

    match actual_val {
        // Check for Result variant format: {$type: "::hot::type/Result.Ok"|"::hot::type/Result.Err", $val: ...}
        Val::Map(map) => {
            if let Some(Val::Str(type_str)) = map.get(&Val::from("$type")) {
                if &**type_str == "::hot::type/Result.Ok" {
                    return HotResult::Ok(Val::Bool(true));
                }
                if &**type_str == "::hot::type/Result.Err" {
                    return HotResult::Ok(Val::Bool(false));
                }
            }
            // Non-Result maps are considered "ok" (successful values)
            HotResult::Ok(Val::Bool(true))
        }

        // All non-Result values are considered "ok" (successful values)
        _ => HotResult::Ok(Val::Bool(true)),
    }
}

/// Check if a result is Err
pub fn result_is_err(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    validate_args!("::hot::type/is-err", args, 1);

    // First, evaluate any lazy thunks to get the actual result value
    let evaluated_val = match &args[0] {
        Val::Box(b) => {
            // Check if this is a zero-argument lambda (lazy evaluation)
            if let Some(lambda_info) = b
                .as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
            {
                if lambda_info.parameters.is_empty() {
                    // For lazy parameter lambdas, suppress Result checking during evaluation
                    // This allows them to evaluate and return Result types without triggering automatic failures
                    let saved_suppress = if lambda_info.is_lazy_param {
                        let current = vm.get_suppress_result_checking();
                        vm.set_suppress_result_checking(true);
                        Some(current)
                    } else {
                        None
                    };

                    // Evaluate the lazy thunk
                    let result = match vm.execute_lambda(&args[0], &[]) {
                        Ok(val) => val,
                        Err(_err) => {
                            // Restore Result checking
                            if let Some(saved) = saved_suppress {
                                vm.set_suppress_result_checking(saved);
                            }
                            // If evaluation failed, consider it an error result (is err)
                            return HotResult::Ok(Val::Bool(true));
                        }
                    };

                    // Restore Result checking
                    if let Some(saved) = saved_suppress {
                        vm.set_suppress_result_checking(saved);
                    }

                    result
                } else {
                    // Has parameters, use as-is
                    args[0].clone()
                }
            } else {
                // Not a lambda, use as-is
                args[0].clone()
            }
        }
        _ => {
            // Not boxed, use as-is
            args[0].clone()
        }
    };

    // Now check if the evaluated result is Err
    // Handle Raw wrapped values first - may be nested
    let mut actual_val = &evaluated_val;
    while actual_val.is_type("Raw") {
        match actual_val {
            Val::Map(map) => {
                if let Some(inner_val) = map.get(&Val::from("$val")) {
                    actual_val = inner_val;
                } else {
                    break;
                }
            }
            _ => {
                break;
            }
        }
    }
    match actual_val {
        // Check for Result variant format: {$type: "::hot::type/Result.Ok"|"::hot::type/Result.Err", $val: ...}
        Val::Map(map) => {
            if let Some(Val::Str(type_str)) = map.get(&Val::from("$type")) {
                if &**type_str == "::hot::type/Result.Err" {
                    return HotResult::Ok(Val::Bool(true));
                }
                if &**type_str == "::hot::type/Result.Ok" {
                    return HotResult::Ok(Val::Bool(false));
                }
            }
            // Non-Result maps are not errors
            HotResult::Ok(Val::Bool(false))
        }

        // All non-Result values are not errors
        _ => HotResult::Ok(Val::Bool(false)),
    }
}

/// Create an Ok result
pub fn result_ok(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::type/ok", args, 1);
    HotResult::Ok(Val::ok(args[0].clone()))
}

/// Create an Err result
pub fn result_err(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::type/err", args, 1);
    HotResult::Ok(Val::err(args[0].clone()))
}

/// if-ok: if Result is Ok, apply fn or use value; if Err, pass through
/// if-ok(result, fn-or-value) — VM-aware for lazy eval + fn dispatch
pub fn result_if_ok(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(Val::from(
            "if-ok expects 2 arguments (result, fn-or-value)".to_string(),
        ));
    }

    // Evaluate lazy first arg (same pattern as result_is_ok)
    let evaluated_val = match &args[0] {
        Val::Box(b) => {
            if let Some(lambda_info) = b
                .as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
            {
                if lambda_info.parameters.is_empty() {
                    let saved_suppress = if lambda_info.is_lazy_param {
                        let current = vm.get_suppress_result_checking();
                        vm.set_suppress_result_checking(true);
                        Some(current)
                    } else {
                        None
                    };
                    let result = match vm.execute_lambda(&args[0], &[]) {
                        Ok(val) => val,
                        Err(_err) => {
                            if let Some(saved) = saved_suppress {
                                vm.set_suppress_result_checking(saved);
                            }
                            return HotResult::Ok(Val::err(Val::from("evaluation failed")));
                        }
                    };
                    if let Some(saved) = saved_suppress {
                        vm.set_suppress_result_checking(saved);
                    }
                    result
                } else {
                    args[0].clone()
                }
            } else {
                args[0].clone()
            }
        }
        _ => args[0].clone(),
    };

    // Unwrap Raw wrappers
    let mut actual_val = &evaluated_val;
    while actual_val.is_type("Raw") {
        match actual_val {
            Val::Map(map) => {
                if let Some(inner_val) = map.get(&Val::from("$val")) {
                    actual_val = inner_val;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }

    // Check if Ok or Err
    let (is_ok, payload) = match actual_val {
        Val::Map(map) => {
            if let Some(Val::Str(type_str)) = map.get(&Val::from("$type")) {
                if &**type_str == "::hot::type/Result.Ok" {
                    let val = map.get(&Val::from("$val")).cloned().unwrap_or(Val::Null);
                    (true, val)
                } else if &**type_str == "::hot::type/Result.Err" {
                    (false, actual_val.clone())
                } else {
                    (true, actual_val.clone())
                }
            } else {
                (true, actual_val.clone())
            }
        }
        _ => (true, actual_val.clone()),
    };

    if !is_ok {
        return HotResult::Ok(payload);
    }

    // Ok: apply fn or use value (lambdas, FunctionRef, typed Fn/FunctionAlias maps are callable)
    let handler = &args[1];
    if is_callable(handler) {
        match crate::lang::hot::coll::call_function_with_vm(vm, handler, &payload) {
            Ok(result) => HotResult::Ok(result),
            Err(e) => HotResult::Err(Val::from(e)),
        }
    } else {
        HotResult::Ok(handler.clone())
    }
}

/// if-err: if Result is Err, apply fn or use value; if Ok, pass through
/// if-err(result, fn-or-value) — VM-aware for lazy eval + fn dispatch
pub fn result_if_err(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(Val::from(
            "if-err expects 2 arguments (result, fn-or-value)".to_string(),
        ));
    }

    // Evaluate lazy first arg (same pattern as result_is_ok)
    let evaluated_val = match &args[0] {
        Val::Box(b) => {
            if let Some(lambda_info) = b
                .as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
            {
                if lambda_info.parameters.is_empty() {
                    let saved_suppress = if lambda_info.is_lazy_param {
                        let current = vm.get_suppress_result_checking();
                        vm.set_suppress_result_checking(true);
                        Some(current)
                    } else {
                        None
                    };
                    let result = match vm.execute_lambda(&args[0], &[]) {
                        Ok(val) => val,
                        Err(err) => {
                            if let Some(saved) = saved_suppress {
                                vm.set_suppress_result_checking(saved);
                            }
                            // if-err is a halt boundary for its lazy first arg:
                            // if evaluating the result expression halts via
                            // `fail()` / `cancel()`, capture the structured
                            // failure payload (preferring `fail()` data over
                            // the runtime error string), clear the halt state
                            // so the handler and downstream code can run, and
                            // dispatch the handler branch.
                            let err_val = if let Some(f) = vm.get_failure() {
                                vm.reset_failure_state();
                                vm.reset_cancellation_state();
                                if matches!(f.data, Val::Null) {
                                    Val::from(f.msg)
                                } else {
                                    f.data
                                }
                            } else if let Some(c) = vm.get_cancellation() {
                                vm.reset_failure_state();
                                vm.reset_cancellation_state();
                                if matches!(c.data, Val::Null) {
                                    Val::from(c.msg)
                                } else {
                                    c.data
                                }
                            } else {
                                Val::from(err.to_string())
                            };
                            return handle_err_branch(vm, &args[1], &err_val);
                        }
                    };
                    if let Some(saved) = saved_suppress {
                        vm.set_suppress_result_checking(saved);
                    }
                    result
                } else {
                    args[0].clone()
                }
            } else {
                args[0].clone()
            }
        }
        _ => args[0].clone(),
    };

    // Unwrap Raw wrappers
    let mut actual_val = &evaluated_val;
    while actual_val.is_type("Raw") {
        match actual_val {
            Val::Map(map) => {
                if let Some(inner_val) = map.get(&Val::from("$val")) {
                    actual_val = inner_val;
                } else {
                    break;
                }
            }
            _ => break,
        }
    }

    // Check if Ok or Err
    match actual_val {
        Val::Map(map) => {
            if let Some(Val::Str(type_str)) = map.get(&Val::from("$type")) {
                if &**type_str == "::hot::type/Result.Ok" {
                    return HotResult::Ok(actual_val.clone());
                }
                if &**type_str == "::hot::type/Result.Err" {
                    let err_payload = map.get(&Val::from("$val")).cloned().unwrap_or(Val::Null);
                    return handle_err_branch(vm, &args[1], &err_payload);
                }
            }
            HotResult::Ok(actual_val.clone())
        }
        _ => HotResult::Ok(actual_val.clone()),
    }
}

/// Check if a Val is callable (lambda, FunctionRef, typed Fn, or FunctionAlias)
fn is_callable(val: &Val) -> bool {
    match val {
        Val::Box(b) => {
            b.as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                .is_some()
                || b.as_any()
                    .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>()
                    .is_some()
        }
        Val::Map(m) => {
            if let Some(Val::Str(type_str)) = m.get(&Val::from("$type")) {
                &**type_str == "::hot::type/Fn" || &**type_str == "::hot::type/FunctionAlias"
            } else {
                false
            }
        }
        _ => false,
    }
}

/// Helper: handle the Err branch for if-err
fn handle_err_branch(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    handler: &Val,
    err_payload: &Val,
) -> HotResult<Val> {
    if is_callable(handler) {
        match crate::lang::hot::coll::call_function_with_vm(vm, handler, err_payload) {
            Ok(result) => HotResult::Ok(result),
            Err(e) => HotResult::Err(Val::from(e)),
        }
    } else {
        HotResult::Ok(handler.clone())
    }
}

#[cfg(test)]
mod tests {
    // Include Result unwrapping tests
    include!("result_unwrap_test.rs");
}
