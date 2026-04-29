use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;
use indexmap::IndexMap;
use uuid::Uuid;

/// Generate a UUID v4 (random)
pub fn uuid_v4(args: &[Val]) -> HotResult<Val> {
    if !args.is_empty() {
        return HotResult::Err(Val::from("uuid-v4 expects no arguments"));
    }

    let uuid = Uuid::new_v4();
    let uuid_str = uuid.to_string();

    // Return Map with $type field (unified pattern)
    let mut result = IndexMap::new();
    result.insert(Val::from("$type"), Val::from("::hot::uuid/Uuid"));
    result.insert(Val::from("$val"), Val::from(uuid_str));
    HotResult::Ok(Val::Map(Box::new(result)))
}

/// Generate a UUID v7 (timestamp-based with counter for uniqueness)
pub fn uuid_v7(args: &[Val]) -> HotResult<Val> {
    if !args.is_empty() {
        return HotResult::Err(Val::from("uuid-v7 expects no arguments"));
    }

    // Generate a UUID v7 with timestamp-based uniqueness
    let uuid = Uuid::now_v7();
    let uuid_str = uuid.to_string();

    // Return Map with $type field (unified pattern)
    let mut result = IndexMap::new();
    result.insert(Val::from("$type"), Val::from("::hot::uuid/Uuid"));
    result.insert(Val::from("$val"), Val::from(uuid_str));
    HotResult::Ok(Val::Map(Box::new(result)))
}

/// Generate a nil UUID (all zeros)
pub fn uuid_nil(args: &[Val]) -> HotResult<Val> {
    if !args.is_empty() {
        return HotResult::Err(Val::from("uuid-nil expects no arguments"));
    }

    // Generate a nil UUID
    let uuid = uuid::Uuid::nil();
    HotResult::Ok(Val::from(uuid.to_string()))
}

/// Check if a value is a valid UUID
pub fn is_uuid(args: &[Val]) -> HotResult<Val> {
    validate_args!("is-uuid", args, 1);

    let is_valid = match &args[0] {
        Val::Str(s) => {
            // Try to parse as UUID
            uuid::Uuid::parse_str(s).is_ok()
        }
        Val::Map(m) => {
            // Check if this is a UUID type instance
            if let Some(Val::Str(type_name)) = m.get(&Val::from("$type")) {
                if &**type_name == "::hot::uuid/Uuid" {
                    // Check if the $val field is a valid UUID string
                    if let Some(Val::Str(uuid_str)) = m.get(&Val::from("$val")) {
                        uuid::Uuid::parse_str(uuid_str).is_ok()
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            }
        }
        _ => false,
    };

    HotResult::Ok(Val::Bool(is_valid))
}

/// Convert a Uuid typed value to its string representation.
/// Extracts the $val field from {$type: "::hot::uuid/Uuid", $val: "..."}
pub fn uuid_to_str(args: &[Val]) -> HotResult<Val> {
    validate_args!("uuid-to-str", args, 1);
    match &args[0] {
        Val::Map(m) => {
            if let Some(Val::Str(s)) = m.get(&Val::from("$val")) {
                HotResult::Ok(Val::from(s.clone()))
            } else {
                HotResult::Err(Val::from("Uuid has no $val string"))
            }
        }
        Val::Str(s) => HotResult::Ok(Val::from(s.clone())),
        _ => HotResult::Err(Val::from("uuid-to-str expects a Uuid value")),
    }
}

/// Uuid type constructor - creates UUID type instances
pub fn uuid_constructor(args: &[Val]) -> HotResult<Val> {
    let uuid_str = match args.len() {
        0 => {
            // Uuid() -> UUID v4
            let uuid = uuid::Uuid::new_v4();
            uuid.to_string()
        }
        1 => {
            match &args[0] {
                Val::Int(4) => {
                    // Uuid(4) -> UUID v4
                    let uuid = uuid::Uuid::new_v4();
                    uuid.to_string()
                }
                Val::Int(7) => {
                    // Uuid(7) -> UUID v7 with timestamp-based uniqueness
                    let uuid = Uuid::now_v7();
                    uuid.to_string()
                }
                Val::Null => {
                    // Uuid(null) -> nil UUID
                    let uuid = uuid::Uuid::nil();
                    uuid.to_string()
                }
                Val::Int(version) => {
                    return HotResult::Err(Val::from(format!("Invalid UUID version: {}", version)));
                }
                _ => {
                    return HotResult::Err(Val::from(
                        "UUID constructor expects Int version or null".to_string(),
                    ));
                }
            }
        }
        _ => {
            return HotResult::Err(Val::from(format!(
                "UUID constructor expects 0 or 1 arguments, got {}",
                args.len()
            )));
        }
    };

    // Create a UUID type instance
    let mut uuid_map = IndexMap::new();
    uuid_map.insert(Val::from("$type"), Val::from("::hot::uuid/Uuid"));
    uuid_map.insert(Val::from("$val"), Val::from(uuid_str));

    HotResult::Ok(Val::Map(Box::new(uuid_map)))
}
