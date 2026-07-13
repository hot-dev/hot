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

/// Decode a PEM body: strip the BEGIN/END lines and base64-decode the rest.
/// Returns the label (e.g. "PRIVATE KEY", "RSA PRIVATE KEY") and DER bytes.
fn pem_to_der(pem: &str) -> Result<(String, Vec<u8>), String> {
    use base64::Engine;
    let mut label = String::new();
    let mut body = String::new();
    let mut in_body = false;
    for line in pem.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("-----BEGIN ") {
            label = rest.trim_end_matches('-').trim().to_string();
            in_body = true;
        } else if line.starts_with("-----END ") {
            in_body = false;
        } else if in_body {
            body.push_str(line);
        }
    }
    if label.is_empty() || body.is_empty() {
        return Err("not a PEM-encoded key".to_string());
    }
    let der = base64::engine::general_purpose::STANDARD
        .decode(&body)
        .map_err(|e| format!("invalid PEM base64: {}", e))?;
    Ok((label, der))
}

/// RSASSA-PKCS1-v1_5 with SHA-256 signing.
/// rsa-sha256-sign(private-key-pem, message) -> Bytes (raw signature)
/// Accepts PKCS#8 ("BEGIN PRIVATE KEY") and PKCS#1 ("BEGIN RSA PRIVATE KEY")
/// PEM keys — the formats used by Google service accounts and GitHub Apps.
pub fn rsa_sha256_sign(args: &[Val]) -> HotResult<Val> {
    use aws_lc_rs::rand::SystemRandom;
    use aws_lc_rs::signature::{RSA_PKCS1_SHA256, RsaKeyPair};

    const FN: &str = "::hot::crypto/rsa-sha256-sign";
    crate::validate_args!(FN, args, 2);

    let pem = match &args[0] {
        Val::Str(s) => s.clone(),
        _ => {
            return HotResult::Err(Val::from(format!(
                "{}: private key must be a PEM string",
                FN
            )));
        }
    };
    let message = match get_bytes_from_arg(FN, "message", &args[1]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(e),
    };

    let (label, der) = match pem_to_der(&pem) {
        Ok(v) => v,
        Err(e) => return HotResult::Err(Val::from(format!("{}: {}", FN, e))),
    };
    let key_pair = match label.as_str() {
        "PRIVATE KEY" => RsaKeyPair::from_pkcs8(&der),
        "RSA PRIVATE KEY" => RsaKeyPair::from_der(&der),
        other => {
            return HotResult::Err(Val::from(format!(
                "{}: unsupported PEM label '{}' (expected PRIVATE KEY or RSA PRIVATE KEY)",
                FN, other
            )));
        }
    };
    let key_pair = match key_pair {
        Ok(kp) => kp,
        Err(e) => return HotResult::Err(Val::from(format!("{}: invalid RSA key: {}", FN, e))),
    };

    let rng = SystemRandom::new();
    let mut signature = vec![0u8; key_pair.public_modulus_len()];
    match key_pair.sign(&RSA_PKCS1_SHA256, &rng, &message, &mut signature) {
        Ok(()) => HotResult::Ok(Val::Bytes(signature)),
        Err(e) => HotResult::Err(Val::from(format!("{}: signing failed: {}", FN, e))),
    }
}

/// RSASSA-PKCS1-v1_5 with SHA-256 verification from public key components.
/// rsa-sha256-verify(n, e, message, signature) -> Bool
/// `n` (modulus) and `e` (exponent) are big-endian Bytes — the JWKS `n`/`e`
/// fields after base64url decoding. Returns false for a bad signature.
pub fn rsa_sha256_verify(args: &[Val]) -> HotResult<Val> {
    use aws_lc_rs::signature::{RSA_PKCS1_2048_8192_SHA256, RsaPublicKeyComponents};

    const FN: &str = "::hot::crypto/rsa-sha256-verify";
    crate::validate_args!(FN, args, 4);

    let n = match get_bytes_from_arg(FN, "n (modulus)", &args[0]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(e),
    };
    let e = match get_bytes_from_arg(FN, "e (exponent)", &args[1]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(e),
    };
    let message = match get_bytes_from_arg(FN, "message", &args[2]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(e),
    };
    let signature = match get_bytes_from_arg(FN, "signature", &args[3]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(e),
    };

    let public_key = RsaPublicKeyComponents { n: &n, e: &e };
    HotResult::Ok(Val::Bool(
        public_key
            .verify(&RSA_PKCS1_2048_8192_SHA256, &message, &signature)
            .is_ok(),
    ))
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

    // Test key generated with openssl; the signature vector below was
    // produced independently by `openssl dgst -sha256 -sign`.
    const RSA_PKCS8_PEM: &str = r#"-----BEGIN PRIVATE KEY-----
MIIEvQIBADANBgkqhkiG9w0BAQEFAASCBKcwggSjAgEAAoIBAQCrYPuXclov+8mi
QdHLEqzPRBYk680SEdPgKvJYB3ennG7GuR1KXk3i9pvVHqtN+g2sfvSYxd2IZubt
Z0cfBDG6a8idkH9tZDqfOB48BABntG3Kxkjjm5OUXcLu48VN+TLpl0j1jVr9jy/a
Zhci+YJNOox+GqZJ7YiDJKXgMrRGMwChPuK0A++p1DJteS2Kv9lGZdYcH6I4CHLR
xOXuyFYL/3KWWxCWf3xORTXgHCLcTT+gdKm1rnkUAkVE5J7fgWBcT8tvwJ44lZ/U
mzhbId2El1TmTkjfil+813QxZRJbkPDrDQWJT/9Zh389hcP5jffaLI6fzjXMX27/
e1yKC/dfAgMBAAECggEADywl/TzEZRTtDBgfEaDEu0RwLQxrp0rI9iktmgNU1OCu
fAB3UK/nRFpustNx+Uegi0hIF/hrvWY8a2f/1DkKVmwX1djgamTIiqasvCwWHxhK
huLyhz556W8HN+zi+TbAcUBecLYQu3vZ3p0em9v47PGe7sRZB+WA9vjsVVxLBbkP
O76TRYU+kQU6gxOCS7bZ2TbIxvCTFzzi4AObeDy493jQe49HJJF31BWfWQnd61aC
Fe5ipcmfEVuuzqWpNIYfq8f/O/+4EKRrxPNAkF9cWDMLUiUFr6uRrIDHqwjfPtb5
jVuyfaJpgCrshVIc5KSd38GERzQfGoQMpzgtBNaP9QKBgQDhS/Orc+2wMt9Knx1J
tee9+GcqT88p3prgI2abKI3bMTegnrkbJUfxjmh6p94JqvFHUQ97nM4qucvaOXXD
MX8Yf/HvKc4NiNnJRzGvi41I+J5ewK1xKfEJhuCT+iZulTPpTZpZXTs1Wz0WVQa+
xGmY/RL405kpR57t8aYEF+dHdQKBgQDCu/g545bEVKbtZWjXS5wNzOxkSeD0dr5g
mkoJUmU5ntv+p7mhTLXAxAHoNUpIR5CLbbwZ6+CpN9JZ4B/UOChrgbgwzWCkaSKD
h4VhEJEIN/k/VkQ1C5g6NBLVMYiVqOOX3syYaJgm25HsVRDKM2l8oktrXV0zFpzG
iMTkBmp9AwKBgAJoCUY/ir5jDLeDU5DB/KOuz4tIJvJPil/ygNoPaNR6hhmVGG0N
zOLrnnoQ6PI5fTJBz2SGnD0glujRzEw1byIX28GuNKE4YPshY4p4dx3cafShIjkf
NY/gfO2Xsmlj9pX7WjayJDvBqblfVx8agfY8XcOCnxQty6jG9/r7NmExAoGBAKUG
zNaqI5OgNZKLVSzW+5nKZy6aWVRy6OVO+50owXXyRXdqxmqGaqTAtukFeD0GwN0Y
EsdY2RwunUnjILYlHUP2O5TIB7VtD32ttH/MnUv8u3JMdiMJ/2ibxrX7c0d5R97l
RjeGtCKCAwjoEgF5TlT76LRE+/2WlSe+sjSXp1jnAoGAYC+BiprgvwdVfGuWzvcf
KU1nCqCFUaY4zu6FYeHCss2skbLb2fJf5XcNRc6KxdTwc0iVyxVTtth6YqraCPyG
vpgguFcjN8C7xIYtsVn/Vl0ehcacqupv7NdBeFbLXR3dalZHhnQyOInT8+b6BGLO
iv1mq913odV3grjJZoH4zkk=
-----END PRIVATE KEY-----"#;

    const RSA_PKCS1_PEM: &str = r#"-----BEGIN RSA PRIVATE KEY-----
MIIEowIBAAKCAQEAq2D7l3JaL/vJokHRyxKsz0QWJOvNEhHT4CryWAd3p5xuxrkd
Sl5N4vab1R6rTfoNrH70mMXdiGbm7WdHHwQxumvInZB/bWQ6nzgePAQAZ7RtysZI
45uTlF3C7uPFTfky6ZdI9Y1a/Y8v2mYXIvmCTTqMfhqmSe2IgySl4DK0RjMAoT7i
tAPvqdQybXktir/ZRmXWHB+iOAhy0cTl7shWC/9yllsQln98TkU14Bwi3E0/oHSp
ta55FAJFROSe34FgXE/Lb8CeOJWf1Js4WyHdhJdU5k5I34pfvNd0MWUSW5Dw6w0F
iU//WYd/PYXD+Y332iyOn841zF9u/3tcigv3XwIDAQABAoIBAA8sJf08xGUU7QwY
HxGgxLtEcC0Ma6dKyPYpLZoDVNTgrnwAd1Cv50RabrLTcflHoItISBf4a71mPGtn
/9Q5ClZsF9XY4GpkyIqmrLwsFh8YSobi8oc+eelvBzfs4vk2wHFAXnC2ELt72d6d
Hpvb+Ozxnu7EWQflgPb47FVcSwW5Dzu+k0WFPpEFOoMTgku22dk2yMbwkxc84uAD
m3g8uPd40HuPRySRd9QVn1kJ3etWghXuYqXJnxFbrs6lqTSGH6vH/zv/uBCka8Tz
QJBfXFgzC1IlBa+rkayAx6sI3z7W+Y1bsn2iaYAq7IVSHOSknd/BhEc0HxqEDKc4
LQTWj/UCgYEA4Uvzq3PtsDLfSp8dSbXnvfhnKk/PKd6a4CNmmyiN2zE3oJ65GyVH
8Y5oeqfeCarxR1EPe5zOKrnL2jl1wzF/GH/x7ynODYjZyUcxr4uNSPieXsCtcSnx
CYbgk/ombpUz6U2aWV07NVs9FlUGvsRpmP0S+NOZKUee7fGmBBfnR3UCgYEAwrv4
OeOWxFSm7WVo10ucDczsZEng9Ha+YJpKCVJlOZ7b/qe5oUy1wMQB6DVKSEeQi228
GevgqTfSWeAf1Dgoa4G4MM1gpGkig4eFYRCRCDf5P1ZENQuYOjQS1TGIlajjl97M
mGiYJtuR7FUQyjNpfKJLa11dMxacxojE5AZqfQMCgYACaAlGP4q+Ywy3g1OQwfyj
rs+LSCbyT4pf8oDaD2jUeoYZlRhtDczi6556EOjyOX0yQc9khpw9IJbo0cxMNW8i
F9vBrjShOGD7IWOKeHcd3Gn0oSI5HzWP4Hztl7JpY/aV+1o2siQ7wam5X1cfGoH2
PF3Dgp8ULcuoxvf6+zZhMQKBgQClBszWqiOToDWSi1Us1vuZymcumllUcujlTvud
KMF18kV3asZqhmqkwLbpBXg9BsDdGBLHWNkcLp1J4yC2JR1D9juUyAe1bQ99rbR/
zJ1L/LtyTHYjCf9om8a1+3NHeUfe5UY3hrQiggMI6BIBeU5U++i0RPv9lpUnvrI0
l6dY5wKBgGAvgYqa4L8HVXxrls73HylNZwqghVGmOM7uhWHhwrLNrJGy29nyX+V3
DUXOisXU8HNIlcsVU7bYemKq2gj8hr6YILhXIzfAu8SGLbFZ/1ZdHoXGnKrqb+zX
QXhWy10d3WpWR4Z0MjiJ0/Pm+gRizor9Zqvdd6HVd4K4yWaB+M5J
-----END RSA PRIVATE KEY-----"#;

    const RSA_MODULUS_HEX: &str = "AB60FB97725A2FFBC9A241D1CB12ACCF441624EBCD1211D3E02AF2580777A79C6EC6B91D4A5E4DE2F69BD51EAB4DFA0DAC7EF498C5DD8866E6ED67471F0431BA6BC89D907F6D643A9F381E3C040067B46DCAC648E39B93945DC2EEE3C54DF932E99748F58D5AFD8F2FDA661722F9824D3A8C7E1AA649ED888324A5E032B4463300A13EE2B403EFA9D4326D792D8ABFD94665D61C1FA2380872D1C4E5EEC8560BFF72965B10967F7C4E4535E01C22DC4D3FA074A9B5AE7914024544E49EDF81605C4FCB6FC09E38959FD49B385B21DD849754E64E48DF8A5FBCD7743165125B90F0EB0D05894FFF59877F3D85C3F98DF7DA2C8E9FCE35CC5F6EFF7B5C8A0BF75F";
    const RSA_TEST_MESSAGE: &str = "test message for rsa";
    const OPENSSL_SIGNATURE_HEX: &str = "38e0d068bcfa88078bc0fbe2643b02eff1a14e7cb794af4151143c7ac30a38064cc9462ebf8e3faf998ddf4efb871c9c9c6d6f379901db5e64f436eb56dc06bd6a3c02c1d768ec62cdf89554effd0b9542ab35ac9a1a0b463183b36c98dcecbab52489bd89fc9c42ae4a28f06c990bea099137a15cc3a6a95554fb30db8e26be697c4bf5652f89713814080c9dc8dd6860b7a2ba09f56d814230284b8f4be179e217590d7a8b34e70da9c9bbb3432113807050a13f56e5fafaf1e0bdd98c8706b6126038e4dc2e753b429ec5d200d0b0b7d385db1b90f62c667a24c3407e47f715139d1ed919712b3ba7f029e1813859a9be1bc33c491c20ee665a409c4e4fc3";

    fn sign_with(pem: &str) -> Vec<u8> {
        match rsa_sha256_sign(&[Val::from(pem), Val::from(RSA_TEST_MESSAGE)]) {
            HotResult::Ok(Val::Bytes(b)) => b,
            other => panic!("Expected signature bytes, got {:?}", other),
        }
    }

    fn verify(message: &str, signature: Vec<u8>) -> bool {
        let n = hex::decode(RSA_MODULUS_HEX.to_lowercase()).unwrap();
        let e = vec![0x01, 0x00, 0x01];
        match rsa_sha256_verify(&[
            Val::Bytes(n),
            Val::Bytes(e),
            Val::from(message),
            Val::Bytes(signature),
        ]) {
            HotResult::Ok(Val::Bool(ok)) => ok,
            other => panic!("Expected bool, got {:?}", other),
        }
    }

    #[test]
    fn test_rsa_sign_matches_openssl_vector() {
        let signature = sign_with(RSA_PKCS8_PEM);
        assert_eq!(hex::encode(&signature), OPENSSL_SIGNATURE_HEX);
    }

    #[test]
    fn test_rsa_pkcs1_pem_produces_same_signature() {
        assert_eq!(sign_with(RSA_PKCS1_PEM), sign_with(RSA_PKCS8_PEM));
    }

    #[test]
    fn test_rsa_verify_roundtrip() {
        let signature = sign_with(RSA_PKCS8_PEM);
        assert!(verify(RSA_TEST_MESSAGE, signature));
    }

    #[test]
    fn test_rsa_verify_rejects_tampered_message() {
        let signature = sign_with(RSA_PKCS8_PEM);
        assert!(!verify("tampered message", signature));
    }

    #[test]
    fn test_rsa_sign_rejects_non_pem() {
        let result = rsa_sha256_sign(&[Val::from("not a key"), Val::from("m")]);
        assert!(matches!(result, HotResult::Err(_)));
    }
}
