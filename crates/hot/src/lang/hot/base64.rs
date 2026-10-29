use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;
use base64::{Engine as _, engine::general_purpose};

/// Encode a string (as UTF-8) or bytes to a base64 string
pub fn encode(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::base64/encode", args, 1);

    let bytes = match get_bytes_from_arg(&args[0]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    let encoded = general_purpose::STANDARD.encode(&bytes);
    HotResult::Ok(Val::from(encoded))
}

/// Decode base64 string to bytes
pub fn decode(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::base64/decode", args, 1);

    let base64_str = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from("Argument must be a string"));
        }
    };

    match general_purpose::STANDARD.decode(&**base64_str) {
        Ok(bytes) => HotResult::Ok(Val::Bytes(bytes)),
        Err(e) => HotResult::Err(Val::from(format!("Base64 decode error: {}", e))),
    }
}

/// Check if a string is valid base64
pub fn is_valid(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::base64/is-valid", args, 1);

    let input_str = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from("Argument must be a string"));
        }
    };

    let is_valid = general_purpose::STANDARD.decode(&**input_str).is_ok();
    HotResult::Ok(Val::Bool(is_valid))
}

/// Encode a string (as UTF-8) or bytes to a URL-safe base64 string (no padding)
pub fn encode_url(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::base64/encode-url", args, 1);

    let bytes = match get_bytes_from_arg(&args[0]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    let encoded = general_purpose::URL_SAFE_NO_PAD.encode(&bytes);
    HotResult::Ok(Val::from(encoded))
}

/// Decode URL-safe base64 string to bytes
pub fn decode_url(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::base64/decode-url", args, 1);

    let base64_str = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from("Argument must be a string"));
        }
    };

    match general_purpose::URL_SAFE_NO_PAD.decode(&**base64_str) {
        Ok(bytes) => HotResult::Ok(Val::Bytes(bytes)),
        Err(e) => HotResult::Err(Val::from(format!("Base64 URL decode error: {}", e))),
    }
}

/// Helper to extract bytes from a Val (Str as UTF-8, Bytes, or a vector of bytes)
fn get_bytes_from_arg(arg: &Val) -> Result<Vec<u8>, String> {
    match arg {
        Val::Str(s) => Ok(s.as_bytes().to_vec()),
        Val::Bytes(bytes) => Ok(bytes.clone()),
        Val::Vec(vec) => {
            let mut result = Vec::new();
            for val in vec {
                match val {
                    Val::Byte(b) => result.push(*b),
                    _ => {
                        return Err(
                            "All vector elements must be bytes for base64 encoding".to_string()
                        );
                    }
                }
            }
            Ok(result)
        }
        _ => Err("Argument must be string, bytes, or a vector of bytes".to_string()),
    }
}

/// Internal helper function for encoding bytes to base64 (used by serialization)
pub fn encode_bytes_to_base64(bytes: &[u8]) -> String {
    general_purpose::STANDARD.encode(bytes)
}

/// Internal helper function for decoding base64 to bytes (used by deserialization)
pub fn decode_base64_to_bytes(base64_str: &str) -> Result<Vec<u8>, String> {
    general_purpose::STANDARD
        .decode(base64_str)
        .map_err(|e| format!("Base64 decode error: {}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode_bytes() {
        let bytes = vec![72, 101, 108, 108, 111]; // "Hello"
        let result = encode(&[Val::Bytes(bytes)]);
        match result {
            HotResult::Ok(Val::Str(encoded)) => {
                assert_eq!(&*encoded, "SGVsbG8=");
            }
            _ => panic!("Expected encoded string"),
        }
    }

    #[test]
    fn test_encode_byte_vector() {
        let byte_vec = vec![
            Val::Byte(72),
            Val::Byte(101),
            Val::Byte(108),
            Val::Byte(108),
            Val::Byte(111),
        ];
        let result = encode(&[Val::Vec(byte_vec)]);
        match result {
            HotResult::Ok(Val::Str(encoded)) => {
                assert_eq!(&*encoded, "SGVsbG8=");
            }
            _ => panic!("Expected encoded string"),
        }
    }

    #[test]
    fn test_encode_str_as_utf8() {
        let result = encode(&[Val::from("hi")]);
        assert!(matches!(result, HotResult::Ok(Val::Str(ref s)) if &**s == "aGk="));

        let result = encode(&[Val::from("é")]);
        assert!(matches!(result, HotResult::Ok(Val::Str(ref s)) if &**s == "w6k="));

        let result = encode_url(&[Val::from("hi")]);
        assert!(matches!(result, HotResult::Ok(Val::Str(ref s)) if &**s == "aGk"));
    }

    #[test]
    fn test_encode_rejects_other_values() {
        assert!(matches!(encode(&[Val::Int(1)]), HotResult::Err(_)));
        assert!(matches!(encode_url(&[Val::Int(1)]), HotResult::Err(_)));
    }

    #[test]
    fn test_decode_base64() {
        let result = decode(&[Val::from("SGVsbG8=")]);
        match result {
            HotResult::Ok(Val::Bytes(bytes)) => {
                assert_eq!(bytes, vec![72, 101, 108, 108, 111]);
            }
            _ => panic!("Expected decoded bytes"),
        }
    }

    #[test]
    fn test_is_valid_base64() {
        let result = is_valid(&[Val::from("SGVsbG8=")]);
        assert!(matches!(result, HotResult::Ok(Val::Bool(true))));

        let result = is_valid(&[Val::from("invalid_base64!")]);
        assert!(matches!(result, HotResult::Ok(Val::Bool(false))));
    }

    #[test]
    fn test_helper_functions() {
        let bytes = vec![72, 101, 108, 108, 111];
        let encoded = encode_bytes_to_base64(&bytes);
        assert_eq!(&*encoded, "SGVsbG8=");

        let decoded = decode_base64_to_bytes("SGVsbG8=").unwrap();
        assert_eq!(decoded, bytes);
    }
}
