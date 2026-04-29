use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;

/// Maximum nesting depth we'll traverse in either direction (build_json_value /
/// convert_json_value_to_val). Picked well below the default Rust thread stack
/// budget for a typical worker; deeply-nested user data above this returns a
/// runtime error rather than blowing the stack.
const JSON_MAX_DEPTH: usize = 256;

/// Convert a value to JSON string
pub fn to_json(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::json/to-json", args, 1);

    let value = &args[0];
    let json_val = match build_json_value(value, 0) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(e),
    };
    match serde_json::to_string(&json_val) {
        Ok(json_string) => {
            // Bound the produced string so a large nested user value can't
            // drive an unbounded String allocation.
            if let Err(e) =
                crate::lang::runtime::limits::check_string_bytes("to-json", json_string.len())
            {
                return HotResult::Err(e);
            }
            HotResult::Ok(Val::from(json_string))
        }
        Err(e) => HotResult::Err(Val::from(format!("Failed to serialize to JSON: {}", e))),
    }
}

/// Parse JSON string to value
pub fn from_json(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::json/from-json", args, 1);

    let json_str = match &args[0] {
        Val::Str(s) => s,
        _ => return HotResult::Err(Val::from("from-json expects a string argument")),
    };

    // Refuse to parse pathologically large input. serde_json can recursively
    // allocate well beyond input size on adversarial structures, and a panic
    // from an OOM here would abort the worker.
    if let Err(e) = crate::lang::runtime::limits::check_parse_input("from-json", json_str.len()) {
        return HotResult::Err(e);
    }

    match serde_json::from_str::<serde_json::Value>(json_str) {
        Ok(v) => match convert_json_value_to_val(&v, 0) {
            Ok(val) => HotResult::Ok(val),
            Err(e) => HotResult::Err(e),
        },
        Err(e) => HotResult::Err(Val::from(format!("Failed to parse JSON: {}", e))),
    }
}

fn depth_err(op: &str) -> Val {
    Val::from(format!(
        "{}: nesting depth exceeds {} levels",
        op, JSON_MAX_DEPTH
    ))
}

fn build_json_value(v: &Val, depth: usize) -> Result<serde_json::Value, Val> {
    if depth > JSON_MAX_DEPTH {
        return Err(depth_err("to-json"));
    }
    Ok(match v {
        Val::Null => serde_json::Value::Null,
        Val::Bool(b) => serde_json::Value::Bool(*b),
        Val::Int(i) => serde_json::Value::Number((*i).into()),
        Val::Dec(d) => {
            let s = d.to_string();
            // Parse the decimal string into a JSON Value to preserve exact digits
            match serde_json::from_str::<serde_json::Value>(&s) {
                Ok(serde_json::Value::Number(n)) => serde_json::Value::Number(n),
                _ => serde_json::Value::String(s),
            }
        }
        Val::Str(s) => serde_json::Value::String((**s).to_owned()),
        Val::Vec(items) => {
            let mut arr = Vec::with_capacity(items.len());
            for item in items.iter() {
                arr.push(build_json_value(item, depth + 1)?);
            }
            serde_json::Value::Array(arr)
        }
        Val::Map(m) => {
            let mut obj = serde_json::Map::new();
            for (k, v) in m.iter() {
                let key: String = match k {
                    Val::Str(s) => (**s).to_owned(),
                    _ => k.to_string(),
                };
                obj.insert(key, build_json_value(v, depth + 1)?);
            }
            serde_json::Value::Object(obj)
        }
        Val::Box(b) => serde_json::Value::String(b.to_string()),
        Val::Byte(b) => serde_json::json!({"$type":"Byte","data": b}),
        Val::Bytes(bytes) => serde_json::json!({"$type":"Bytes","data": bytes}),
    })
}

fn convert_json_value_to_val(v: &serde_json::Value, depth: usize) -> Result<Val, Val> {
    if depth > JSON_MAX_DEPTH {
        return Err(depth_err("from-json"));
    }
    Ok(match v {
        serde_json::Value::Null => Val::Null,
        serde_json::Value::Bool(b) => Val::Bool(*b),
        serde_json::Value::Number(num) => {
            if let Some(i) = num.as_i64() {
                Val::Int(i)
            } else {
                // Always parse via string to preserve decimal precision
                let s = num.to_string();
                if let Ok(d) = fastnum::D256::from_str(&s, fastnum::decimal::Context::default()) {
                    Val::Dec(d)
                } else {
                    Val::from(s)
                }
            }
        }
        serde_json::Value::String(s) => Val::from(s.clone()),
        serde_json::Value::Array(arr) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr.iter() {
                out.push(convert_json_value_to_val(item, depth + 1)?);
            }
            Val::Vec(out)
        }
        serde_json::Value::Object(map) => {
            let mut out = indexmap::IndexMap::new();
            for (k, v) in map.iter() {
                out.insert(
                    Val::from(k.clone()),
                    convert_json_value_to_val(v, depth + 1)?,
                );
            }
            Val::Map(Box::new(out))
        }
    })
}
