// Bytes operations for Hot language - binary protocol support

use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;

/// Convert bytes to integer (big-endian by default)
/// to-int(bytes) or to-int(bytes, endian)
pub fn to_int(args: &[Val]) -> HotResult<Val> {
    if args.is_empty() || args.len() > 2 {
        return HotResult::Err(Val::from(
            "::hot::bytes/to-int: Expected 1 or 2 arguments (bytes, [endian])".to_string(),
        ));
    }

    let bytes = match &args[0] {
        Val::Bytes(b) => b,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::bytes/to-int: First argument must be Bytes".to_string(),
            ));
        }
    };

    let big_endian = if args.len() == 2 {
        match &args[1] {
            Val::Str(s) => match &**s {
                "be" | "big" | "big-endian" => true,
                "le" | "little" | "little-endian" => false,
                _ => {
                    return HotResult::Err(Val::from(format!(
                        "::hot::bytes/to-int: Invalid endian '{}'. Use 'be' or 'le'",
                        s
                    )));
                }
            },
            _ => {
                return HotResult::Err(Val::from(
                    "::hot::bytes/to-int: Endian must be a string ('be' or 'le')".to_string(),
                ));
            }
        }
    } else {
        true // Default to big-endian (network byte order)
    };

    // Convert bytes to i64 based on length
    let result = match bytes.len() {
        0 => 0i64,
        1 => bytes[0] as i8 as i64, // Endianness doesn't matter for single byte
        2 => {
            let arr: [u8; 2] = bytes[..2].try_into().unwrap();
            if big_endian {
                i16::from_be_bytes(arr) as i64
            } else {
                i16::from_le_bytes(arr) as i64
            }
        }
        3 | 4 => {
            // Pad to 4 bytes
            let mut arr = [0u8; 4];
            if big_endian {
                arr[4 - bytes.len()..].copy_from_slice(bytes);
                i32::from_be_bytes(arr) as i64
            } else {
                arr[..bytes.len()].copy_from_slice(bytes);
                i32::from_le_bytes(arr) as i64
            }
        }
        5..=8 => {
            // Pad to 8 bytes
            let mut arr = [0u8; 8];
            if big_endian {
                arr[8 - bytes.len()..].copy_from_slice(bytes);
                i64::from_be_bytes(arr)
            } else {
                arr[..bytes.len()].copy_from_slice(bytes);
                i64::from_le_bytes(arr)
            }
        }
        _ => {
            return HotResult::Err(Val::from(format!(
                "::hot::bytes/to-int: Bytes length {} exceeds maximum of 8",
                bytes.len()
            )));
        }
    };

    HotResult::Ok(Val::Int(result))
}

/// Convert bytes to unsigned integer (big-endian by default)
/// to-uint(bytes) or to-uint(bytes, endian)
pub fn to_uint(args: &[Val]) -> HotResult<Val> {
    if args.is_empty() || args.len() > 2 {
        return HotResult::Err(Val::from(
            "::hot::bytes/to-uint: Expected 1 or 2 arguments (bytes, [endian])".to_string(),
        ));
    }

    let bytes = match &args[0] {
        Val::Bytes(b) => b,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::bytes/to-uint: First argument must be Bytes".to_string(),
            ));
        }
    };

    let big_endian = if args.len() == 2 {
        match &args[1] {
            Val::Str(s) => match &**s {
                "be" | "big" | "big-endian" => true,
                "le" | "little" | "little-endian" => false,
                _ => {
                    return HotResult::Err(Val::from(format!(
                        "::hot::bytes/to-uint: Invalid endian '{}'. Use 'be' or 'le'",
                        s
                    )));
                }
            },
            _ => {
                return HotResult::Err(Val::from(
                    "::hot::bytes/to-uint: Endian must be a string ('be' or 'le')".to_string(),
                ));
            }
        }
    } else {
        true // Default to big-endian (network byte order)
    };

    // Convert bytes to u64, then to i64 (Hot's Int type)
    let result = match bytes.len() {
        0 => 0u64,
        1 => bytes[0] as u64,
        2 => {
            let arr: [u8; 2] = bytes[..2].try_into().unwrap();
            if big_endian {
                u16::from_be_bytes(arr) as u64
            } else {
                u16::from_le_bytes(arr) as u64
            }
        }
        3 | 4 => {
            let mut arr = [0u8; 4];
            if big_endian {
                arr[4 - bytes.len()..].copy_from_slice(bytes);
                u32::from_be_bytes(arr) as u64
            } else {
                arr[..bytes.len()].copy_from_slice(bytes);
                u32::from_le_bytes(arr) as u64
            }
        }
        5..=8 => {
            let mut arr = [0u8; 8];
            if big_endian {
                arr[8 - bytes.len()..].copy_from_slice(bytes);
                u64::from_be_bytes(arr)
            } else {
                arr[..bytes.len()].copy_from_slice(bytes);
                u64::from_le_bytes(arr)
            }
        }
        _ => {
            return HotResult::Err(Val::from(format!(
                "::hot::bytes/to-uint: Bytes length {} exceeds maximum of 8",
                bytes.len()
            )));
        }
    };

    // Note: This may overflow for values > i64::MAX
    // For those cases, the result will wrap to negative
    HotResult::Ok(Val::Int(result as i64))
}

/// Convert integer to bytes (big-endian by default)
/// from-int(int, size) or from-int(int, size, endian)
pub fn from_int(args: &[Val]) -> HotResult<Val> {
    if args.len() < 2 || args.len() > 3 {
        return HotResult::Err(Val::from(
            "::hot::bytes/from-int: Expected 2 or 3 arguments (int, size, [endian])".to_string(),
        ));
    }

    let value = match &args[0] {
        Val::Int(i) => *i,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::bytes/from-int: First argument must be Int".to_string(),
            ));
        }
    };

    let size = match &args[1] {
        Val::Int(s) => {
            if *s < 1 || *s > 8 {
                return HotResult::Err(Val::from(
                    "::hot::bytes/from-int: Size must be 1-8".to_string(),
                ));
            }
            *s as usize
        }
        _ => {
            return HotResult::Err(Val::from(
                "::hot::bytes/from-int: Size must be an Int".to_string(),
            ));
        }
    };

    let big_endian = if args.len() == 3 {
        match &args[2] {
            Val::Str(s) => match &**s {
                "be" | "big" | "big-endian" => true,
                "le" | "little" | "little-endian" => false,
                _ => {
                    return HotResult::Err(Val::from(format!(
                        "::hot::bytes/from-int: Invalid endian '{}'. Use 'be' or 'le'",
                        s
                    )));
                }
            },
            _ => {
                return HotResult::Err(Val::from(
                    "::hot::bytes/from-int: Endian must be a string ('be' or 'le')".to_string(),
                ));
            }
        }
    } else {
        true // Default to big-endian
    };

    let bytes = if big_endian {
        let all_bytes = value.to_be_bytes();
        all_bytes[8 - size..].to_vec()
    } else {
        let all_bytes = value.to_le_bytes();
        all_bytes[..size].to_vec()
    };

    HotResult::Ok(Val::Bytes(bytes))
}

/// CRC32 checksum (IEEE polynomial, same as used in AWS Event Stream)
pub fn crc32(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::bytes/crc32", args, 1);

    let bytes = match &args[0] {
        Val::Bytes(b) => b.clone(),
        Val::Str(s) => s.as_bytes().to_vec(),
        Val::Vec(v) => {
            let mut result = Vec::new();
            for val in v {
                match val {
                    Val::Int(i) => {
                        if *i >= 0 && *i <= 255 {
                            result.push(*i as u8);
                        } else {
                            return HotResult::Err(Val::from(format!(
                                "::hot::bytes/crc32: Int {} is out of byte range (0-255)",
                                i
                            )));
                        }
                    }
                    Val::Byte(b) => result.push(*b),
                    _ => {
                        return HotResult::Err(Val::from(
                            "::hot::bytes/crc32: Vec elements must be Int or Byte".to_string(),
                        ));
                    }
                }
            }
            result
        }
        _ => {
            return HotResult::Err(Val::from(
                "::hot::bytes/crc32: Argument must be Bytes, Str, or Vec".to_string(),
            ));
        }
    };

    let checksum = crc32fast::hash(&bytes);
    HotResult::Ok(Val::Int(checksum as i64))
}

/// Get a single byte from Bytes at index
pub fn get(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::bytes/get", args, 2);

    let bytes = match &args[0] {
        Val::Bytes(b) => b,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::bytes/get: First argument must be Bytes".to_string(),
            ));
        }
    };

    let index = match &args[1] {
        Val::Int(i) => {
            if *i < 0 {
                return HotResult::Err(Val::from(
                    "::hot::bytes/get: Index must be non-negative".to_string(),
                ));
            }
            *i as usize
        }
        _ => {
            return HotResult::Err(Val::from(
                "::hot::bytes/get: Index must be an Int".to_string(),
            ));
        }
    };

    if index >= bytes.len() {
        HotResult::Ok(Val::Null)
    } else {
        HotResult::Ok(Val::Int(bytes[index] as i64))
    }
}

/// Convert Bytes to Vec<Int> for easier manipulation
pub fn to_vec(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::bytes/to-vec", args, 1);

    let bytes = match &args[0] {
        Val::Bytes(b) => b,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::bytes/to-vec: Argument must be Bytes".to_string(),
            ));
        }
    };

    let vec: Vec<Val> = bytes.iter().map(|b| Val::Int(*b as i64)).collect();
    HotResult::Ok(Val::Vec(vec))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_to_int_be() {
        // 4-byte big-endian
        let bytes = vec![0x00, 0x00, 0x00, 0x10]; // 16
        let result = to_int(&[Val::Bytes(bytes)]);
        assert!(matches!(result, HotResult::Ok(Val::Int(16))));
    }

    #[test]
    fn test_to_int_le() {
        // 4-byte little-endian
        let bytes = vec![0x10, 0x00, 0x00, 0x00]; // 16
        let result = to_int(&[Val::Bytes(bytes), Val::from("le")]);
        assert!(matches!(result, HotResult::Ok(Val::Int(16))));
    }

    #[test]
    fn test_to_uint() {
        let bytes = vec![0xFF, 0xFF]; // 65535 unsigned
        let result = to_uint(&[Val::Bytes(bytes)]);
        assert!(matches!(result, HotResult::Ok(Val::Int(65535))));
    }

    #[test]
    fn test_from_int_be() {
        let result = from_int(&[Val::Int(256), Val::Int(2)]);
        match result {
            HotResult::Ok(Val::Bytes(bytes)) => {
                assert_eq!(bytes, vec![0x01, 0x00]);
            }
            _ => panic!("Expected bytes"),
        }
    }

    #[test]
    fn test_from_int_le() {
        let result = from_int(&[Val::Int(256), Val::Int(2), Val::from("le")]);
        match result {
            HotResult::Ok(Val::Bytes(bytes)) => {
                assert_eq!(bytes, vec![0x00, 0x01]);
            }
            _ => panic!("Expected bytes"),
        }
    }

    #[test]
    fn test_crc32() {
        let result = crc32(&[Val::from("hello")]);
        match result {
            HotResult::Ok(Val::Int(checksum)) => {
                // Known CRC32 of "hello"
                assert_eq!(checksum, 0x3610a686u32 as i64);
            }
            _ => panic!("Expected CRC32 checksum"),
        }
    }

    #[test]
    fn test_get() {
        let bytes = vec![10, 20, 30];
        let result = get(&[Val::Bytes(bytes), Val::Int(1)]);
        assert!(matches!(result, HotResult::Ok(Val::Int(20))));
    }

    #[test]
    fn test_to_vec() {
        let bytes = vec![1, 2, 3];
        let result = to_vec(&[Val::Bytes(bytes)]);
        match result {
            HotResult::Ok(Val::Vec(v)) => {
                assert_eq!(v.len(), 3);
                assert!(matches!(v[0], Val::Int(1)));
            }
            _ => panic!("Expected Vec"),
        }
    }
}
