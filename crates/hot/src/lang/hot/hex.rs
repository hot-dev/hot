use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;

/// Encode bytes to hex string
pub fn encode(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::hex/encode", args, 1);

    let bytes = match get_bytes_from_arg(&args[0]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(Val::from(format!("::hot::hex/encode: {}", e))),
    };
    HotResult::Ok(Val::from(hex::encode(&bytes)))
}

/// Decode hex string to bytes
pub fn decode(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::hex/decode", args, 1);

    let hex_str = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::hex/decode: Argument must be a string".to_string(),
            ));
        }
    };

    match hex::decode(&**hex_str) {
        Ok(bytes) => HotResult::Ok(Val::Bytes(bytes)),
        Err(e) => HotResult::Err(Val::from(format!("::hot::hex/decode: Invalid hex: {}", e))),
    }
}

/// Check if a string is valid hex
pub fn is_valid(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::hex/is-valid", args, 1);

    let hex_str = match &args[0] {
        Val::Str(s) => s,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::hex/is-valid: Argument must be a string".to_string(),
            ));
        }
    };

    // Valid hex: even length, all chars are 0-9, a-f, A-F
    let is_valid = hex_str.len() % 2 == 0 && hex_str.chars().all(|c| c.is_ascii_hexdigit());

    HotResult::Ok(Val::Bool(is_valid))
}

/// Helper to extract bytes from a Val (Str or Bytes)
fn get_bytes_from_arg(arg: &Val) -> Result<Vec<u8>, String> {
    match arg {
        Val::Str(s) => Ok(s.as_bytes().to_vec()),
        Val::Bytes(b) => Ok(b.clone()),
        Val::Vec(vec) => {
            let mut result = Vec::new();
            for val in vec {
                match val {
                    Val::Byte(b) => result.push(*b),
                    _ => {
                        return Err("Vector elements must be bytes".to_string());
                    }
                }
            }
            Ok(result)
        }
        _ => Err("Argument must be a string or bytes".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_encode() {
        let bytes = vec![0x48, 0x65, 0x6c, 0x6c, 0x6f]; // "Hello"
        let result = encode(&[Val::Bytes(bytes)]);
        match result {
            HotResult::Ok(Val::Str(hex)) => {
                assert_eq!(&*hex, "48656c6c6f");
            }
            _ => panic!("Expected hex string"),
        }
    }

    #[test]
    fn test_encode_string() {
        let result = encode(&[Val::from("Hello")]);
        match result {
            HotResult::Ok(Val::Str(hex)) => {
                assert_eq!(&*hex, "48656c6c6f");
            }
            _ => panic!("Expected hex string"),
        }
    }

    #[test]
    fn test_decode() {
        let result = decode(&[Val::from("48656c6c6f")]);
        match result {
            HotResult::Ok(Val::Bytes(bytes)) => {
                assert_eq!(bytes, vec![0x48, 0x65, 0x6c, 0x6c, 0x6f]);
            }
            _ => panic!("Expected bytes"),
        }
    }

    #[test]
    fn test_is_valid() {
        let result = is_valid(&[Val::from("48656c6c6f")]);
        assert!(matches!(result, HotResult::Ok(Val::Bool(true))));

        let result = is_valid(&[Val::from("48656c6c6")]); // Odd length
        assert!(matches!(result, HotResult::Ok(Val::Bool(false))));

        let result = is_valid(&[Val::from("ghijkl")]); // Invalid chars
        assert!(matches!(result, HotResult::Ok(Val::Bool(false))));
    }
}
