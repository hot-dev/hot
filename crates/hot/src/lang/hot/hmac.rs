use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;
use aws_lc_rs::hmac;

/// HMAC-SHA256 - returns hex-encoded string
pub fn hmac_sha256(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::hmac/hmac-sha256", args, 2);
    let key = match get_bytes_from_arg(&args[0]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(Val::from(format!("::hot::hmac/hmac-sha256 key: {}", e))),
    };
    let data = match get_bytes_from_arg(&args[1]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(Val::from(format!("::hot::hmac/hmac-sha256 data: {}", e))),
    };

    let signing_key = hmac::Key::new(hmac::HMAC_SHA256, &key);
    let tag = hmac::sign(&signing_key, &data);
    HotResult::Ok(Val::from(hex::encode(tag.as_ref())))
}

/// HMAC-SHA256 returning raw bytes (useful for chained HMAC like AWS Sig v4)
pub fn hmac_sha256_bytes(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::hmac/hmac-sha256-bytes", args, 2);
    let key = match get_bytes_from_arg(&args[0]) {
        Ok(bytes) => bytes,
        Err(e) => {
            return HotResult::Err(Val::from(format!(
                "::hot::hmac/hmac-sha256-bytes key: {}",
                e
            )));
        }
    };
    let data = match get_bytes_from_arg(&args[1]) {
        Ok(bytes) => bytes,
        Err(e) => {
            return HotResult::Err(Val::from(format!(
                "::hot::hmac/hmac-sha256-bytes data: {}",
                e
            )));
        }
    };

    let signing_key = hmac::Key::new(hmac::HMAC_SHA256, &key);
    let tag = hmac::sign(&signing_key, &data);
    HotResult::Ok(Val::Bytes(tag.as_ref().to_vec()))
}

/// HMAC-SHA512 - returns hex-encoded string
pub fn hmac_sha512(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::hmac/hmac-sha512", args, 2);
    let key = match get_bytes_from_arg(&args[0]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(Val::from(format!("::hot::hmac/hmac-sha512 key: {}", e))),
    };
    let data = match get_bytes_from_arg(&args[1]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(Val::from(format!("::hot::hmac/hmac-sha512 data: {}", e))),
    };

    let signing_key = hmac::Key::new(hmac::HMAC_SHA512, &key);
    let tag = hmac::sign(&signing_key, &data);
    HotResult::Ok(Val::from(hex::encode(tag.as_ref())))
}

/// HMAC-SHA1 - returns hex-encoded string (legacy, for webhook verification)
pub fn hmac_sha1(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::hmac/hmac-sha1", args, 2);
    let key = match get_bytes_from_arg(&args[0]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(Val::from(format!("::hot::hmac/hmac-sha1 key: {}", e))),
    };
    let data = match get_bytes_from_arg(&args[1]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(Val::from(format!("::hot::hmac/hmac-sha1 data: {}", e))),
    };

    let signing_key = hmac::Key::new(hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY, &key);
    let tag = hmac::sign(&signing_key, &data);
    HotResult::Ok(Val::from(hex::encode(tag.as_ref())))
}

/// Verify HMAC in constant time to prevent timing attacks
pub fn hmac_verify(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::hmac/hmac-verify", args, 4);
    let key = match get_bytes_from_arg(&args[0]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(Val::from(format!("::hot::hmac/hmac-verify key: {}", e))),
    };
    let data = match get_bytes_from_arg(&args[1]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(Val::from(format!("::hot::hmac/hmac-verify data: {}", e))),
    };
    let expected = match &args[2] {
        Val::Str(s) => s.clone(),
        _ => {
            return HotResult::Err(Val::from(
                "::hot::hmac/hmac-verify: expected must be a string".to_string(),
            ));
        }
    };
    let algorithm = match &args[3] {
        Val::Str(s) => s.clone(),
        _ => {
            return HotResult::Err(Val::from(
                "::hot::hmac/hmac-verify: algorithm must be a string".to_string(),
            ));
        }
    };

    let hmac_algorithm = match &*algorithm {
        "sha256" | "SHA256" => hmac::HMAC_SHA256,
        "sha512" | "SHA512" => hmac::HMAC_SHA512,
        "sha1" | "SHA1" => hmac::HMAC_SHA1_FOR_LEGACY_USE_ONLY,
        _ => {
            return HotResult::Err(Val::from(format!(
                "::hot::hmac/hmac-verify: unsupported algorithm '{}'. Use 'sha256', 'sha512', or 'sha1'",
                algorithm
            )));
        }
    };

    // Decode expected from hex
    let expected_bytes = match hex::decode(&*expected) {
        Ok(bytes) => bytes,
        Err(e) => {
            return HotResult::Err(Val::from(format!(
                "::hot::hmac/hmac-verify: invalid hex in expected: {}",
                e
            )));
        }
    };

    let verification_key = hmac::Key::new(hmac_algorithm, &key);
    let result = hmac::verify(&verification_key, &data, &expected_bytes);

    HotResult::Ok(Val::Bool(result.is_ok()))
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
    fn test_hmac_sha256() {
        let result = hmac_sha256(&[Val::from("secret-key"), Val::from("hello")]);
        match result {
            HotResult::Ok(Val::Str(hash)) => {
                // HMAC-SHA256 produces 256-bit (64 hex chars) output
                assert_eq!(hash.len(), 64);
            }
            _ => panic!("Expected hash string"),
        }
    }

    #[test]
    fn test_hmac_sha256_bytes() {
        let result = hmac_sha256_bytes(&[Val::from("secret-key"), Val::from("hello")]);
        match result {
            HotResult::Ok(Val::Bytes(bytes)) => {
                // HMAC-SHA256 produces 32 bytes
                assert_eq!(bytes.len(), 32);
            }
            _ => panic!("Expected bytes"),
        }
    }

    #[test]
    fn test_hmac_verify_valid() {
        // First compute the HMAC
        let hmac_result = hmac_sha256(&[Val::from("secret-key"), Val::from("hello")]);
        let expected = match hmac_result {
            HotResult::Ok(Val::Str(s)) => s,
            _ => panic!("Expected HMAC string"),
        };

        // Now verify it
        let result = hmac_verify(&[
            Val::from("secret-key"),
            Val::from("hello"),
            Val::Str(expected),
            Val::from("sha256"),
        ]);
        match result {
            HotResult::Ok(Val::Bool(valid)) => {
                assert!(valid);
            }
            _ => panic!("Expected boolean"),
        }
    }

    #[test]
    fn test_hmac_verify_invalid() {
        let result = hmac_verify(&[
            Val::from("secret-key"),
            Val::from("hello"),
            Val::from("0000000000000000000000000000000000000000000000000000000000000000"),
            Val::from("sha256"),
        ]);
        match result {
            HotResult::Ok(Val::Bool(valid)) => {
                assert!(!valid);
            }
            _ => panic!("Expected boolean"),
        }
    }
}
