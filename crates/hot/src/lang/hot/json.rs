use crate::lang::hot::r#type::HotResult;
use crate::val::ValIndexMap;
use crate::val::{HOT_BYTE_TYPE, HOT_BYTES_TYPE, Val};
use crate::validate_args;

/// Maximum nesting depth we'll traverse in either direction (build_json_value /
/// convert_json_value_to_val). Picked well below the default Rust thread stack
/// budget for a typical worker; deeply-nested user data above this returns a
/// runtime error rather than blowing the stack.
const JSON_MAX_DEPTH: usize = 256;

/// Convert a value to JSON string
pub fn to_json(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::json/to-json", args, 1);

    crate::validate_expansion!("to-json", args);
    let value = &args[0];
    // Written straight from the value, without building a `serde_json::Value`
    // first (see `JsonOut`).
    match serde_json::to_string(&JsonOut { value, depth: 0 }) {
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
        Err(e) if is_depth_error(&e) => HotResult::Err(depth_err("to-json")),
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

    // Parsed straight into values, without a `serde_json::Value` between
    // (see `JsonIn`).
    let mut deserializer = serde_json::Deserializer::from_str(json_str);
    let parsed = serde::de::DeserializeSeed::deserialize(JsonIn { depth: 0 }, &mut deserializer)
        .and_then(|value| deserializer.end().map(|()| value));
    match parsed {
        Ok(val) => HotResult::Ok(val),
        Err(e) if is_depth_error(&e) => HotResult::Err(depth_err("from-json")),
        Err(e) => HotResult::Err(Val::from(format!("Failed to parse JSON: {}", e))),
    }
}

fn depth_err(op: &str) -> Val {
    Val::from(format!(
        "{}: nesting depth exceeds {} levels",
        op, JSON_MAX_DEPTH
    ))
}

/// The message of the serde error that stands for the depth limit, which
/// `to_json` and `from_json` turn back into `depth_err`.
const DEPTH_ERROR: &str = "hot json: nesting depth limit";

fn is_depth_error(error: &serde_json::Error) -> bool {
    error.to_string().starts_with(DEPTH_ERROR)
}

/// A value written as JSON, as `build_json_value` converts it, but streamed
/// to the output instead of built as a `serde_json::Value` first. The rare
/// shapes with a conversion of their own (a Dec's exact digits, a Box, a
/// Byte or Bytes, a Map with a key that isn't a Str) take that conversion.
struct JsonOut<'a> {
    value: &'a Val,
    depth: usize,
}

impl serde::Serialize for JsonOut<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::{Error as _, SerializeMap, SerializeSeq};
        if self.depth > JSON_MAX_DEPTH {
            return Err(S::Error::custom(DEPTH_ERROR));
        }
        let converted = |value: &Val| {
            build_json_value(value, self.depth).map_err(|_| S::Error::custom(DEPTH_ERROR))
        };
        match self.value {
            Val::Null => serializer.serialize_unit(),
            Val::Bool(b) => serializer.serialize_bool(*b),
            Val::Int(i) => serializer.serialize_i64(*i),
            Val::Str(s) => serializer.serialize_str(s),
            Val::Vec(items) => {
                let mut seq = serializer.serialize_seq(Some(items.len()))?;
                for item in items.iter() {
                    seq.serialize_element(&JsonOut {
                        value: item,
                        depth: self.depth + 1,
                    })?;
                }
                seq.end()
            }
            Val::Map(entries) if entries.keys().all(|key| matches!(key, Val::Str(_))) => {
                let mut map = serializer.serialize_map(Some(entries.len()))?;
                for (key, value) in entries.iter() {
                    let Val::Str(key) = key else { unreachable!() };
                    map.serialize_entry(
                        &**key,
                        &JsonOut {
                            value,
                            depth: self.depth + 1,
                        },
                    )?;
                }
                map.end()
            }
            // Keys that print alike merge, as in `build_json_value`.
            Val::Map(_) | Val::Dec(_) | Val::Box(_) | Val::Byte(_) | Val::Bytes(_) => {
                converted(self.value)?.serialize(serializer)
            }
        }
    }
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
        Val::Byte(b) => serde_json::json!({"$type": HOT_BYTE_TYPE, "$val": b}),
        Val::Bytes(bytes) => serde_json::json!({"$type": HOT_BYTES_TYPE, "$val": bytes}),
    })
}

/// The key serde_json gives a number when `arbitrary_precision` is on: it
/// hands every number to a visitor as a map of this one key to its digits.
const NUMBER_TOKEN: &str = "$serde_json::private::Number";

/// Check whether `s` is syntactically a JSON number (RFC 8259).
fn is_valid_json_number(s: &str) -> bool {
    let bytes = s.as_bytes();
    let mut i = 0;
    if bytes.get(i) == Some(&b'-') {
        i += 1;
    }
    match bytes.get(i) {
        Some(b'0') => i += 1,
        Some(b'1'..=b'9') => {
            i += 1;
            while matches!(bytes.get(i), Some(b'0'..=b'9')) {
                i += 1;
            }
        }
        _ => return false,
    }
    if bytes.get(i) == Some(&b'.') {
        i += 1;
        if !matches!(bytes.get(i), Some(b'0'..=b'9')) {
            return false;
        }
        while matches!(bytes.get(i), Some(b'0'..=b'9')) {
            i += 1;
        }
    }
    if matches!(bytes.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(bytes.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if !matches!(bytes.get(i), Some(b'0'..=b'9')) {
            return false;
        }
        while matches!(bytes.get(i), Some(b'0'..=b'9')) {
            i += 1;
        }
    }
    i == bytes.len()
}

/// A Val read from its digits, as `serde_json::Value` numbers were read: an
/// Int when they fit one, else a Dec, keeping every digit, else the digits.
fn number_val(digits: &str) -> Val {
    if let Ok(i) = digits.parse::<i64>() {
        return Val::Int(i);
    }
    match fastnum::D256::from_str(digits, fastnum::decimal::Context::default()) {
        Ok(d) => Val::Dec(d),
        Err(_) => Val::from(digits),
    }
}

/// A byte, as a Byte's or Bytes' `$val` holds one.
fn val_to_byte(value: &Val) -> Option<u8> {
    match value {
        Val::Int(n) => u8::try_from(*n).ok(),
        _ => None,
    }
}

/// A JSON value parsed straight into a Val. Objects become Maps in their
/// order, a later duplicate key replacing the value of the first, and the
/// objects `to_json` writes for a Byte or Bytes become those again.
struct JsonIn {
    depth: usize,
}

impl<'de> serde::de::DeserializeSeed<'de> for JsonIn {
    type Value = Val;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Val, D::Error> {
        if self.depth > JSON_MAX_DEPTH {
            return Err(serde::de::Error::custom(DEPTH_ERROR));
        }
        deserializer.deserialize_any(self)
    }
}

/// A Map key, read as a Str without an intermediate String.
struct JsonKey;

impl<'de> serde::de::DeserializeSeed<'de> for JsonKey {
    type Value = Val;

    fn deserialize<D: serde::Deserializer<'de>>(self, deserializer: D) -> Result<Val, D::Error> {
        deserializer.deserialize_str(self)
    }
}

impl serde::de::Visitor<'_> for JsonKey {
    type Value = Val;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON object key")
    }

    fn visit_str<E>(self, key: &str) -> Result<Val, E> {
        Ok(Val::from(key))
    }

    fn visit_string<E>(self, key: String) -> Result<Val, E> {
        Ok(Val::from(key))
    }
}

impl<'de> serde::de::Visitor<'de> for JsonIn {
    type Value = Val;

    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("a JSON value")
    }

    fn visit_unit<E>(self) -> Result<Val, E> {
        Ok(Val::Null)
    }

    fn visit_bool<E>(self, b: bool) -> Result<Val, E> {
        Ok(Val::Bool(b))
    }

    fn visit_i64<E>(self, i: i64) -> Result<Val, E> {
        Ok(Val::Int(i))
    }

    fn visit_u64<E>(self, u: u64) -> Result<Val, E> {
        Ok(i64::try_from(u).map_or_else(|_| number_val(&u.to_string()), Val::Int))
    }

    fn visit_f64<E>(self, f: f64) -> Result<Val, E> {
        Ok(number_val(&f.to_string()))
    }

    fn visit_str<E>(self, s: &str) -> Result<Val, E> {
        Ok(Val::from(s))
    }

    fn visit_string<E>(self, s: String) -> Result<Val, E> {
        Ok(Val::from(s))
    }

    fn visit_seq<A: serde::de::SeqAccess<'de>>(self, mut seq: A) -> Result<Val, A::Error> {
        let mut items = Vec::with_capacity(seq.size_hint().unwrap_or(0));
        while let Some(item) = seq.next_element_seed(JsonIn {
            depth: self.depth + 1,
        })? {
            items.push(item);
        }
        Ok(Val::vec(items))
    }

    fn visit_map<A: serde::de::MapAccess<'de>>(self, mut map: A) -> Result<Val, A::Error> {
        let mut entries =
            ValIndexMap::with_capacity_and_hasher(map.size_hint().unwrap_or(0), Default::default());
        while let Some(key) = map.next_key_seed(JsonKey)? {
            let value = map.next_value_seed(JsonIn {
                depth: self.depth + 1,
            })?;
            if entries.is_empty()
                && matches!(&key, Val::Str(k) if &**k == NUMBER_TOKEN)
                && let Val::Str(ref digits) = value
                && is_valid_json_number(digits)
            {
                if let Some(next_key) = map.next_key_seed(JsonKey)? {
                    entries.insert(key, value);
                    let next_value = map.next_value_seed(JsonIn {
                        depth: self.depth + 1,
                    })?;
                    entries.insert(next_key, next_value);
                    continue;
                }
                return Ok(number_val(digits));
            }
            entries.insert(key, value);
        }
        if entries.len() == 2
            && let Some(Val::Str(type_name)) = entries.get(&Val::from("$type"))
        {
            if &**type_name == HOT_BYTE_TYPE
                && let Some(byte) = entries.get(&Val::from("$val")).and_then(val_to_byte)
            {
                return Ok(Val::Byte(byte));
            }
            if &**type_name == HOT_BYTES_TYPE
                && let Some(Val::Vec(items)) = entries.get(&Val::from("$val"))
                && let Some(bytes) = items.iter().map(val_to_byte).collect()
            {
                return Ok(Val::bytes(bytes));
            }
        }
        Ok(Val::map(entries))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dec(digits: &str) -> Val {
        Val::Dec(fastnum::D256::from_str(digits, fastnum::decimal::Context::default()).unwrap())
    }

    fn parse(json: &str) -> Val {
        from_json(&[Val::from(json)]).ok().expect("parses")
    }

    fn write(value: Val) -> String {
        match to_json(&[value]).ok().expect("writes") {
            Val::Str(s) => s.to_string(),
            other => panic!("to-json returned {other:?}"),
        }
    }

    /// `to_json` streams what `build_json_value` built: the same bytes.
    #[test]
    fn to_json_writes_what_the_value_conversion_wrote() {
        let mut int_keys = ValIndexMap::default();
        int_keys.insert(Val::Int(1), Val::from("one"));
        int_keys.insert(Val::from("1"), Val::from("string one"));
        int_keys.insert(Val::Bool(true), Val::Null);
        let mut nested = ValIndexMap::default();
        nested.insert(
            Val::from("quote\"and\\slash/"),
            Val::from("tab\tnewline\n\u{1F600}"),
        );
        nested.insert(Val::from("dec"), dec("1.50"));
        nested.insert(
            Val::from("big"),
            dec("123456789012345678901234567890.000001"),
        );
        nested.insert(Val::from("int keys"), Val::map(int_keys));
        nested.insert(Val::from("byte"), Val::Byte(7));
        nested.insert(Val::from("bytes"), Val::bytes(vec![0, 255]));
        nested.insert(
            Val::from("list"),
            Val::vec(vec![
                Val::Int(-3),
                Val::Bool(false),
                Val::Null,
                Val::vec(vec![]),
            ]),
        );
        let values = [
            Val::Null,
            Val::Int(i64::MIN),
            dec("-0.000"),
            Val::from(""),
            Val::map(ValIndexMap::default()),
            Val::map(nested),
        ];
        for value in values {
            let expected = serde_json::to_string(&build_json_value(&value, 0).unwrap()).unwrap();
            assert_eq!(write(value), expected);
        }
    }

    #[test]
    fn from_json_reads_numbers_as_ints_and_exact_decimals() {
        assert_eq!(parse("42"), Val::Int(42));
        assert_eq!(parse("-7"), Val::Int(-7));
        assert_eq!(parse("9223372036854775807"), Val::Int(i64::MAX));
        assert_eq!(parse("9223372036854775808"), dec("9223372036854775808"));
        assert_eq!(parse("1.50"), dec("1.50"));
        assert_eq!(write(parse("1.50")), "1.50");
        assert_eq!(parse("1e2"), dec("1e2"));
        assert_eq!(parse("[0.1, 2]"), Val::vec(vec![dec("0.1"), Val::Int(2)]));
    }

    #[test]
    fn from_json_keeps_key_order_and_the_last_duplicate() {
        let value = parse(r#"{"b": 1, "a": {"y": [true, null], "x": "s"}, "b": 3}"#);
        assert_eq!(write(value), r#"{"b":3,"a":{"y":[true,null],"x":"s"}}"#);
    }

    #[test]
    fn json_errors_read_as_before() {
        let deep_write = (0..300).fold(Val::Int(1), |inner, _| Val::vec(vec![inner]));
        assert_eq!(
            to_json(&[deep_write]),
            HotResult::Err(Val::from("to-json: nesting depth exceeds 256 levels"))
        );
        let HotResult::Err(Val::Str(bad)) = from_json(&[Val::from("{\"a\": }")]) else {
            panic!("invalid JSON parsed");
        };
        assert!(bad.starts_with("Failed to parse JSON: "), "{bad}");
        let HotResult::Err(Val::Str(deep)) = from_json(&[Val::from(format!(
            "{}1{}",
            "[".repeat(300),
            "]".repeat(300)
        ))]) else {
            panic!("deep JSON parsed");
        };
        assert!(
            deep.starts_with("Failed to parse JSON: recursion limit"),
            "{deep}"
        );
        let HotResult::Err(Val::Str(trailing)) = from_json(&[Val::from("[1] x")]) else {
            panic!("trailing characters parsed");
        };
        assert!(
            trailing.starts_with("Failed to parse JSON: trailing"),
            "{trailing}"
        );
    }

    #[test]
    fn test_to_json_uses_qualified_byte_type_names() {
        let byte = to_json(&[Val::Byte(42)]).ok().unwrap();
        assert_eq!(byte, Val::from(r#"{"$type":"::hot::type/Byte","$val":42}"#));

        let bytes = to_json(&[Val::bytes(vec![1, 2, 3])]).ok().unwrap();
        assert_eq!(
            bytes,
            Val::from(r#"{"$type":"::hot::type/Bytes","$val":[1,2,3]}"#)
        );
    }

    #[test]
    fn test_from_json_restores_byte_types() {
        let byte = from_json(&[Val::from(r#"{"$type":"::hot::type/Byte","$val":42}"#)])
            .ok()
            .unwrap();
        assert_eq!(byte, Val::Byte(42));

        let bytes = from_json(&[Val::from(r#"{"$type":"::hot::type/Bytes","$val":[1,2,3]}"#)])
            .ok()
            .unwrap();
        assert_eq!(bytes, Val::bytes(vec![1, 2, 3]));
    }

    #[test]
    fn from_json_does_not_hijack_non_number_token_or_drop_extra_type_keys() {
        let non_num = parse(r#"{"$serde_json::private::Number": "hello"}"#);
        assert!(matches!(non_num, Val::Map(_)), "got {non_num:?}");
        assert_eq!(
            non_num.get("$serde_json::private::Number"),
            Some(Val::from("hello"))
        );

        let extra_num = parse(r#"{"$serde_json::private::Number": "42", "other": 1}"#);
        assert!(matches!(extra_num, Val::Map(_)), "got {extra_num:?}");
        assert_eq!(extra_num.get("other"), Some(Val::Int(1)));

        let extra_type = parse(r#"{"$type":"::hot::type/Byte","$val":42,"extra":"keep"}"#);
        assert!(matches!(extra_type, Val::Map(_)), "got {extra_type:?}");
        assert_eq!(extra_type.get("extra"), Some(Val::from("keep")));
    }
}
