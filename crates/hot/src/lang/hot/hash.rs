use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;
use aws_lc_rs::digest::{self, SHA256, SHA384, SHA512};

/// SHA-256 hash - returns hex-encoded string
pub fn sha256(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::hash/sha256", args, 1);
    let data = match get_bytes_from_arg(&args[0]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(Val::from(format!("::hot::hash/sha256: {}", e))),
    };
    let hash = digest::digest(&SHA256, &data);
    HotResult::Ok(Val::from(hex::encode(hash.as_ref())))
}

/// SHA-384 hash - returns hex-encoded string
pub fn sha384(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::hash/sha384", args, 1);
    let data = match get_bytes_from_arg(&args[0]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(Val::from(format!("::hot::hash/sha384: {}", e))),
    };
    let hash = digest::digest(&SHA384, &data);
    HotResult::Ok(Val::from(hex::encode(hash.as_ref())))
}

/// SHA-512 hash - returns hex-encoded string
pub fn sha512(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::hash/sha512", args, 1);
    let data = match get_bytes_from_arg(&args[0]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(Val::from(format!("::hot::hash/sha512: {}", e))),
    };
    let hash = digest::digest(&SHA512, &data);
    HotResult::Ok(Val::from(hex::encode(hash.as_ref())))
}

/// BLAKE3 hash - returns hex-encoded string (fast, modern hash)
pub fn blake3(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::hash/blake3", args, 1);
    let data = match get_bytes_from_arg(&args[0]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(Val::from(format!("::hot::hash/blake3: {}", e))),
    };
    let hash = blake3::hash(&data);
    HotResult::Ok(Val::from(hash.to_hex().to_string()))
}

/// SHA-1 hash - returns hex-encoded string (legacy, for webhook verification)
pub fn sha1(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::hash/sha1", args, 1);
    let data = match get_bytes_from_arg(&args[0]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(Val::from(format!("::hot::hash/sha1: {}", e))),
    };
    // aws-lc-rs has SHA1 in digest module
    let hash = digest::digest(&digest::SHA1_FOR_LEGACY_USE_ONLY, &data);
    HotResult::Ok(Val::from(hex::encode(hash.as_ref())))
}

/// MD5 hash - returns hex-encoded string (legacy, for checksums only - NOT for security)
pub fn md5(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::hash/md5", args, 1);
    let data = match get_bytes_from_arg(&args[0]) {
        Ok(bytes) => bytes,
        Err(e) => return HotResult::Err(Val::from(format!("::hot::hash/md5: {}", e))),
    };
    let hash = md5::compute(&data);
    HotResult::Ok(Val::from(format!("{:x}", hash)))
}

/// Helper to extract bytes from a Val (Str or Bytes)
fn get_bytes_from_arg(arg: &Val) -> Result<Vec<u8>, String> {
    match arg {
        Val::Str(s) => Ok(s.as_bytes().to_vec()),
        Val::Bytes(b) => Ok(b.clone()),
        Val::Vec(vec) => {
            // Convert Vec<Val> to Vec<u8> if all elements are bytes
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
    fn test_sha256() {
        let result = sha256(&[Val::from("hello")]);
        match result {
            HotResult::Ok(Val::Str(hash)) => {
                assert_eq!(
                    &*hash,
                    "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
                );
            }
            _ => panic!("Expected hash string"),
        }
    }

    #[test]
    fn test_sha256_empty() {
        let result = sha256(&[Val::from("")]);
        match result {
            HotResult::Ok(Val::Str(hash)) => {
                assert_eq!(
                    &*hash,
                    "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
                );
            }
            _ => panic!("Expected hash string"),
        }
    }

    #[test]
    fn test_blake3() {
        let result = blake3(&[Val::from("hello")]);
        match result {
            HotResult::Ok(Val::Str(hash)) => {
                // BLAKE3 produces 256-bit (64 hex chars) output
                assert_eq!(hash.len(), 64);
            }
            _ => panic!("Expected hash string"),
        }
    }

    #[test]
    fn test_md5() {
        let result = md5(&[Val::from("hello")]);
        match result {
            HotResult::Ok(Val::Str(hash)) => {
                assert_eq!(&*hash, "5d41402abc4b2a76b9719d911017c592");
            }
            _ => panic!("Expected hash string"),
        }
    }
}
