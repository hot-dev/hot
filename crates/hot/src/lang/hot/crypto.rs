// Cryptographic key-derivation functions for Hot language
//
// Home for KDF-style primitives that must run natively: an interpreted
// PBKDF2 loop (thousands of HMAC rounds) would add seconds to every
// SCRAM authentication handshake.

use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use aws_lc_rs::pbkdf2;
use std::num::NonZeroU32;

/// Maximum derived-key length in bytes
const MAX_DK_LEN: usize = 1024;
/// Maximum iteration count (guards against accidental multi-minute hangs)
const MAX_ITERATIONS: i64 = 100_000_000;

fn get_bytes_from_arg(fn_name: &str, arg_name: &str, arg: &Val) -> Result<Vec<u8>, Val> {
    match arg {
        Val::Str(s) => Ok(s.as_bytes().to_vec()),
        Val::Bytes(b) => Ok(b.clone()),
        _ => Err(Val::from(format!(
            "{}: {} must be Str or Bytes",
            fn_name, arg_name
        ))),
    }
}

/// PBKDF2 key derivation with HMAC-SHA256 (the SCRAM-SHA-256 `Hi()` function).
/// pbkdf2-hmac-sha256(password, salt, iterations) or
/// pbkdf2-hmac-sha256(password, salt, iterations, length)
/// Returns the derived key as Bytes (default length 32).
pub fn pbkdf2_hmac_sha256(args: &[Val]) -> HotResult<Val> {
    const FN: &str = "::hot::crypto/pbkdf2-hmac-sha256";

    if args.len() < 3 || args.len() > 4 {
        return HotResult::Err(Val::from(format!(
            "{}: Expected 3 or 4 arguments (password, salt, iterations, [length])",
            FN
        )));
    }

    let password = match get_bytes_from_arg(FN, "password", &args[0]) {
        Ok(b) => b,
        Err(e) => return HotResult::Err(e),
    };
    let salt = match get_bytes_from_arg(FN, "salt", &args[1]) {
        Ok(b) => b,
        Err(e) => return HotResult::Err(e),
    };

    let iterations = match &args[2] {
        Val::Int(i) if *i >= 1 && *i <= MAX_ITERATIONS => {
            NonZeroU32::new(*i as u32).expect("validated non-zero")
        }
        Val::Int(i) => {
            return HotResult::Err(Val::from(format!(
                "{}: Iterations must be between 1 and {}, got {}",
                FN, MAX_ITERATIONS, i
            )));
        }
        _ => {
            return HotResult::Err(Val::from(format!("{}: Iterations must be an Int", FN)));
        }
    };

    let dk_len = if args.len() == 4 {
        match &args[3] {
            Val::Int(l) if *l >= 1 && *l <= MAX_DK_LEN as i64 => *l as usize,
            Val::Int(l) => {
                return HotResult::Err(Val::from(format!(
                    "{}: Length must be between 1 and {}, got {}",
                    FN, MAX_DK_LEN, l
                )));
            }
            _ => {
                return HotResult::Err(Val::from(format!("{}: Length must be an Int", FN)));
            }
        }
    } else {
        32
    };

    let mut out = vec![0u8; dk_len];
    pbkdf2::derive(
        pbkdf2::PBKDF2_HMAC_SHA256,
        iterations,
        &salt,
        &password,
        &mut out,
    );

    HotResult::Ok(Val::Bytes(out))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn derive_hex(password: &str, salt: &str, iterations: i64) -> String {
        let result =
            pbkdf2_hmac_sha256(&[Val::from(password), Val::from(salt), Val::Int(iterations)]);
        match result {
            HotResult::Ok(Val::Bytes(b)) => hex::encode(b),
            other => panic!("Expected bytes, got {:?}", other),
        }
    }

    // Standard PBKDF2-HMAC-SHA256 test vectors
    #[test]
    fn test_pbkdf2_vector_c1() {
        assert_eq!(
            derive_hex("password", "salt", 1),
            "120fb6cffcf8b32c43e7225256c4f837a86548c92ccc35480805987cb70be17b"
        );
    }

    #[test]
    fn test_pbkdf2_vector_c2() {
        assert_eq!(
            derive_hex("password", "salt", 2),
            "ae4d0c95af6b46d32d0adff928f06dd02a303f8ef3c251dfd6e2d85a95474c43"
        );
    }

    #[test]
    fn test_pbkdf2_vector_c4096() {
        assert_eq!(
            derive_hex("password", "salt", 4096),
            "c5e478d59288c841aa530db6845c4c8d962893a001ce4e11a4963873aa98134a"
        );
    }

    // RFC 7677 SCRAM-SHA-256 SaltedPassword:
    // Hi("pencil", base64("W22ZaJ0SNY7soEsUEjb6gQ=="), 4096)
    #[test]
    fn test_pbkdf2_scram_salted_password() {
        use base64::Engine;
        let salt = base64::engine::general_purpose::STANDARD
            .decode("W22ZaJ0SNY7soEsUEjb6gQ==")
            .unwrap();
        let result = pbkdf2_hmac_sha256(&[Val::from("pencil"), Val::Bytes(salt), Val::Int(4096)]);
        match result {
            HotResult::Ok(Val::Bytes(b)) => assert_eq!(
                hex::encode(b),
                "c4a49510323ab4f952cac1fa99441939e78ea74d6be81ddf7096e87513dc615d"
            ),
            other => panic!("Expected bytes, got {:?}", other),
        }
    }

    #[test]
    fn test_pbkdf2_custom_length() {
        let result = pbkdf2_hmac_sha256(&[
            Val::from("password"),
            Val::from("salt"),
            Val::Int(1),
            Val::Int(64),
        ]);
        match result {
            HotResult::Ok(Val::Bytes(b)) => assert_eq!(b.len(), 64),
            other => panic!("Expected bytes, got {:?}", other),
        }
    }

    #[test]
    fn test_pbkdf2_invalid_iterations() {
        let result = pbkdf2_hmac_sha256(&[Val::from("password"), Val::from("salt"), Val::Int(0)]);
        assert!(matches!(result, HotResult::Err(_)));
    }
}
