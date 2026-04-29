use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;
use aws_lc_rs::rand::{SecureRandom, SystemRandom};

/// Generate cryptographically secure random bytes
/// Returns as Bytes by default, or hex/base64 encoded string if encoding specified
pub fn random_bytes(args: &[Val]) -> HotResult<Val> {
    if args.is_empty() || args.len() > 2 {
        return HotResult::Err(Val::from(
            "::hot::random/random-bytes: Expected 1 or 2 arguments (length, [encoding])"
                .to_string(),
        ));
    }

    let length = match &args[0] {
        Val::Int(n) => {
            if *n <= 0 {
                return HotResult::Err(Val::from(
                    "::hot::random/random-bytes: length must be positive".to_string(),
                ));
            }
            *n as usize
        }
        _ => {
            return HotResult::Err(Val::from(
                "::hot::random/random-bytes: length must be an integer".to_string(),
            ));
        }
    };

    // Cap at 1MB to prevent memory issues
    if length > 1_048_576 {
        return HotResult::Err(Val::from(
            "::hot::random/random-bytes: length cannot exceed 1MB (1048576 bytes)".to_string(),
        ));
    }

    let mut bytes = vec![0u8; length];
    let rng = SystemRandom::new();
    if let Err(e) = rng.fill(&mut bytes) {
        return HotResult::Err(Val::from(format!(
            "::hot::random/random-bytes: Failed to generate random bytes: {:?}",
            e
        )));
    }

    // Check for encoding argument
    if args.len() == 2 {
        let encoding = match &args[1] {
            Val::Str(s) => &**s,
            _ => {
                return HotResult::Err(Val::from(
                    "::hot::random/random-bytes: encoding must be a string".to_string(),
                ));
            }
        };

        match encoding {
            "hex" => HotResult::Ok(Val::from(hex::encode(&bytes))),
            "base64" => {
                use base64::{Engine as _, engine::general_purpose};
                HotResult::Ok(Val::from(general_purpose::STANDARD.encode(&bytes)))
            }
            "bytes" => HotResult::Ok(Val::Bytes(bytes)),
            _ => HotResult::Err(Val::from(format!(
                "::hot::random/random-bytes: unknown encoding '{}'. Use 'hex', 'base64', or 'bytes'",
                encoding
            ))),
        }
    } else {
        HotResult::Ok(Val::Bytes(bytes))
    }
}

/// Generate a random alphanumeric string of specified length
/// Uses cryptographically secure random for character selection
pub fn random_string(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::random/random-string", args, 1);

    let length = match &args[0] {
        Val::Int(n) => {
            if *n <= 0 {
                return HotResult::Err(Val::from(
                    "::hot::random/random-string: length must be positive".to_string(),
                ));
            }
            *n as usize
        }
        _ => {
            return HotResult::Err(Val::from(
                "::hot::random/random-string: length must be an integer".to_string(),
            ));
        }
    };

    // Cap at 1MB
    if length > 1_048_576 {
        return HotResult::Err(Val::from(
            "::hot::random/random-string: length cannot exceed 1MB".to_string(),
        ));
    }

    const CHARSET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let charset_len = CHARSET.len();

    // Generate enough random bytes (using rejection sampling for uniformity)
    let rng = SystemRandom::new();
    let mut result = String::with_capacity(length);
    let mut random_byte = [0u8; 1];

    while result.len() < length {
        if let Err(e) = rng.fill(&mut random_byte) {
            return HotResult::Err(Val::from(format!(
                "::hot::random/random-string: Failed to generate random bytes: {:?}",
                e
            )));
        }

        // Use rejection sampling to avoid modulo bias
        let idx = random_byte[0] as usize;
        if idx < (256 / charset_len) * charset_len {
            result.push(CHARSET[idx % charset_len] as char);
        }
    }

    HotResult::Ok(Val::from(result))
}

/// Constant-time comparison of two strings/bytes to prevent timing attacks
pub fn secure_compare(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::random/secure-compare", args, 2);

    let a = match get_bytes_from_arg(&args[0]) {
        Ok(bytes) => bytes,
        Err(e) => {
            return HotResult::Err(Val::from(format!(
                "::hot::random/secure-compare first: {}",
                e
            )));
        }
    };
    let b = match get_bytes_from_arg(&args[1]) {
        Ok(bytes) => bytes,
        Err(e) => {
            return HotResult::Err(Val::from(format!(
                "::hot::random/secure-compare second: {}",
                e
            )));
        }
    };

    // Constant-time comparison
    if a.len() != b.len() {
        return HotResult::Ok(Val::Bool(false));
    }

    let mut result = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        result |= x ^ y;
    }

    HotResult::Ok(Val::Bool(result == 0))
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
    fn test_random_bytes() {
        let result = random_bytes(&[Val::Int(32)]);
        match result {
            HotResult::Ok(Val::Bytes(bytes)) => {
                assert_eq!(bytes.len(), 32);
            }
            _ => panic!("Expected bytes"),
        }
    }

    #[test]
    fn test_random_bytes_hex() {
        let result = random_bytes(&[Val::Int(16), Val::from("hex")]);
        match result {
            HotResult::Ok(Val::Str(hex)) => {
                assert_eq!(hex.len(), 32); // 16 bytes = 32 hex chars
            }
            _ => panic!("Expected hex string"),
        }
    }

    #[test]
    fn test_random_bytes_base64() {
        let result = random_bytes(&[Val::Int(12), Val::from("base64")]);
        match result {
            HotResult::Ok(Val::Str(b64)) => {
                assert_eq!(b64.len(), 16); // 12 bytes = 16 base64 chars
            }
            _ => panic!("Expected base64 string"),
        }
    }

    #[test]
    fn test_random_string() {
        let result = random_string(&[Val::Int(32)]);
        match result {
            HotResult::Ok(Val::Str(s)) => {
                assert_eq!(s.len(), 32);
                assert!(s.chars().all(|c| c.is_ascii_alphanumeric()));
            }
            _ => panic!("Expected string"),
        }
    }

    #[test]
    fn test_secure_compare_equal() {
        let result = secure_compare(&[Val::from("secret"), Val::from("secret")]);
        assert!(matches!(result, HotResult::Ok(Val::Bool(true))));
    }

    #[test]
    fn test_secure_compare_not_equal() {
        let result = secure_compare(&[Val::from("secret"), Val::from("different")]);
        assert!(matches!(result, HotResult::Ok(Val::Bool(false))));
    }

    #[test]
    fn test_secure_compare_bytes() {
        let result = secure_compare(&[Val::Bytes(vec![1, 2, 3, 4]), Val::Bytes(vec![1, 2, 3, 4])]);
        assert!(matches!(result, HotResult::Ok(Val::Bool(true))));
    }
}
