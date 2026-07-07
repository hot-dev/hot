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

/// Parse an endian argument ("be"/"big"/"big-endian" or "le"/"little"/"little-endian")
fn parse_endian(fn_name: &str, arg: &Val) -> Result<bool, Val> {
    match arg {
        Val::Str(s) => match &**s {
            "be" | "big" | "big-endian" => Ok(true),
            "le" | "little" | "little-endian" => Ok(false),
            _ => Err(Val::from(format!(
                "{}: Invalid endian '{}'. Use 'be' or 'le'",
                fn_name, s
            ))),
        },
        _ => Err(Val::from(format!(
            "{}: Endian must be a string ('be' or 'le')",
            fn_name
        ))),
    }
}

/// Find the first occurrence of a byte or byte sequence within Bytes.
/// index-of(haystack, needle) or index-of(haystack, needle, from)
/// Returns the index as Int, or Null when not found.
pub fn index_of(args: &[Val]) -> HotResult<Val> {
    if args.len() < 2 || args.len() > 3 {
        return HotResult::Err(Val::from(
            "::hot::bytes/index-of: Expected 2 or 3 arguments (haystack, needle, [from])"
                .to_string(),
        ));
    }

    let haystack = match &args[0] {
        Val::Bytes(b) => b,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::bytes/index-of: First argument must be Bytes".to_string(),
            ));
        }
    };

    let needle: Vec<u8> = match &args[1] {
        Val::Bytes(b) => {
            if b.is_empty() {
                return HotResult::Err(Val::from(
                    "::hot::bytes/index-of: Needle must not be empty".to_string(),
                ));
            }
            b.clone()
        }
        Val::Byte(b) => vec![*b],
        Val::Int(i) => {
            if *i < 0 || *i > 255 {
                return HotResult::Err(Val::from(format!(
                    "::hot::bytes/index-of: Int needle {} is out of byte range (0-255)",
                    i
                )));
            }
            vec![*i as u8]
        }
        _ => {
            return HotResult::Err(Val::from(
                "::hot::bytes/index-of: Needle must be Bytes, Byte, or Int".to_string(),
            ));
        }
    };

    let from = if args.len() == 3 {
        match &args[2] {
            Val::Int(i) => {
                if *i < 0 {
                    return HotResult::Err(Val::from(
                        "::hot::bytes/index-of: From index must be non-negative".to_string(),
                    ));
                }
                *i as usize
            }
            _ => {
                return HotResult::Err(Val::from(
                    "::hot::bytes/index-of: From index must be an Int".to_string(),
                ));
            }
        }
    } else {
        0
    };

    if from >= haystack.len() || needle.len() > haystack.len() - from {
        return HotResult::Ok(Val::Null);
    }

    match haystack[from..]
        .windows(needle.len())
        .position(|w| w == needle)
    {
        Some(pos) => HotResult::Ok(Val::Int((from + pos) as i64)),
        None => HotResult::Ok(Val::Null),
    }
}

/// XOR two equal-length byte sequences
pub fn xor(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::bytes/xor", args, 2);

    let (a, b) = match (&args[0], &args[1]) {
        (Val::Bytes(a), Val::Bytes(b)) => (a, b),
        _ => {
            return HotResult::Err(Val::from(
                "::hot::bytes/xor: Both arguments must be Bytes".to_string(),
            ));
        }
    };

    if a.len() != b.len() {
        return HotResult::Err(Val::from(format!(
            "::hot::bytes/xor: Length mismatch ({} vs {} bytes)",
            a.len(),
            b.len()
        )));
    }

    let result: Vec<u8> = a.iter().zip(b.iter()).map(|(x, y)| x ^ y).collect();
    HotResult::Ok(Val::Bytes(result))
}

/// Decode IEEE 754 bytes (4 = f32, 8 = f64) to a Dec value.
/// to-float(bytes) or to-float(bytes, endian). Big-endian by default.
pub fn to_float(args: &[Val]) -> HotResult<Val> {
    if args.is_empty() || args.len() > 2 {
        return HotResult::Err(Val::from(
            "::hot::bytes/to-float: Expected 1 or 2 arguments (bytes, [endian])".to_string(),
        ));
    }

    let bytes = match &args[0] {
        Val::Bytes(b) => b,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::bytes/to-float: First argument must be Bytes".to_string(),
            ));
        }
    };

    let big_endian = if args.len() == 2 {
        match parse_endian("::hot::bytes/to-float", &args[1]) {
            Ok(be) => be,
            Err(e) => return HotResult::Err(e),
        }
    } else {
        true
    };

    let value: f64 = match bytes.len() {
        4 => {
            let arr: [u8; 4] = bytes[..4].try_into().unwrap();
            let f = if big_endian {
                f32::from_be_bytes(arr)
            } else {
                f32::from_le_bytes(arr)
            };
            f as f64
        }
        8 => {
            let arr: [u8; 8] = bytes[..8].try_into().unwrap();
            if big_endian {
                f64::from_be_bytes(arr)
            } else {
                f64::from_le_bytes(arr)
            }
        }
        n => {
            return HotResult::Err(Val::from(format!(
                "::hot::bytes/to-float: Bytes length must be 4 (f32) or 8 (f64), got {}",
                n
            )));
        }
    };

    HotResult::Ok(Val::from(value))
}

/// Encode a Dec or Int value as IEEE 754 bytes (size 4 = f32, 8 = f64).
/// from-float(value, size) or from-float(value, size, endian). Big-endian by default.
/// Values that are not exactly representable are rounded to the nearest float.
pub fn from_float(args: &[Val]) -> HotResult<Val> {
    if args.len() < 2 || args.len() > 3 {
        return HotResult::Err(Val::from(
            "::hot::bytes/from-float: Expected 2 or 3 arguments (value, size, [endian])"
                .to_string(),
        ));
    }

    let value: f64 = match &args[0] {
        Val::Int(i) => *i as f64,
        Val::Dec(d) => {
            // Round-trip through the canonical string form; D256 covers a wider
            // range than f64, so out-of-range values become +/-Infinity.
            let s = d.to_string();
            match s.parse::<f64>() {
                Ok(f) => f,
                Err(_) => {
                    return HotResult::Err(Val::from(format!(
                        "::hot::bytes/from-float: Cannot encode '{}' as a float",
                        s
                    )));
                }
            }
        }
        _ => {
            return HotResult::Err(Val::from(
                "::hot::bytes/from-float: First argument must be Dec or Int".to_string(),
            ));
        }
    };

    let size = match &args[1] {
        Val::Int(4) => 4usize,
        Val::Int(8) => 8usize,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::bytes/from-float: Size must be 4 (f32) or 8 (f64)".to_string(),
            ));
        }
    };

    let big_endian = if args.len() == 3 {
        match parse_endian("::hot::bytes/from-float", &args[2]) {
            Ok(be) => be,
            Err(e) => return HotResult::Err(e),
        }
    } else {
        true
    };

    let bytes = match (size, big_endian) {
        (4, true) => (value as f32).to_be_bytes().to_vec(),
        (4, false) => (value as f32).to_le_bytes().to_vec(),
        (8, true) => value.to_be_bytes().to_vec(),
        (8, false) => value.to_le_bytes().to_vec(),
        _ => unreachable!(),
    };

    HotResult::Ok(Val::Bytes(bytes))
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

    #[test]
    fn test_index_of_int_needle() {
        let haystack = Val::Bytes(vec![0x52, 0x00, 0x00, 0x08, 0x00]);
        // Null terminator scan: first 0x00 is at index 1
        let result = index_of(&[haystack.clone(), Val::Int(0)]);
        assert!(matches!(result, HotResult::Ok(Val::Int(1))));
        // With from: next 0x00 at index 2
        let result = index_of(&[haystack, Val::Int(0), Val::Int(2)]);
        assert!(matches!(result, HotResult::Ok(Val::Int(2))));
    }

    #[test]
    fn test_index_of_bytes_needle() {
        let haystack = Val::Bytes(b"SCRAM-SHA-256\0".to_vec());
        let result = index_of(&[haystack.clone(), Val::Bytes(b"SHA".to_vec())]);
        assert!(matches!(result, HotResult::Ok(Val::Int(6))));
        // Not found returns Null
        let result = index_of(&[haystack, Val::Bytes(b"MD5".to_vec())]);
        assert!(matches!(result, HotResult::Ok(Val::Null)));
    }

    #[test]
    fn test_index_of_bounds() {
        let haystack = Val::Bytes(vec![1, 2, 3]);
        // From beyond the end returns Null (not an error)
        let result = index_of(&[haystack.clone(), Val::Int(1), Val::Int(10)]);
        assert!(matches!(result, HotResult::Ok(Val::Null)));
        // Needle longer than remaining haystack returns Null
        let result = index_of(&[haystack, Val::Bytes(vec![2, 3, 4, 5])]);
        assert!(matches!(result, HotResult::Ok(Val::Null)));
    }

    #[test]
    fn test_xor() {
        let a = Val::Bytes(vec![0b1010, 0xFF, 0x00]);
        let b = Val::Bytes(vec![0b0110, 0x0F, 0x00]);
        let result = xor(&[a, b]);
        match result {
            HotResult::Ok(Val::Bytes(bytes)) => assert_eq!(bytes, vec![0b1100, 0xF0, 0x00]),
            _ => panic!("Expected bytes"),
        }
    }

    #[test]
    fn test_xor_length_mismatch() {
        let result = xor(&[Val::Bytes(vec![1, 2]), Val::Bytes(vec![1])]);
        assert!(matches!(result, HotResult::Err(_)));
    }

    #[test]
    fn test_to_float_f64_be() {
        // 1.5 as big-endian f64
        let bytes = 1.5f64.to_be_bytes().to_vec();
        let result = to_float(&[Val::Bytes(bytes)]);
        match result {
            HotResult::Ok(Val::Dec(d)) => assert_eq!(d.to_string(), "1.5"),
            other => panic!("Expected Dec, got {:?}", other),
        }
    }

    #[test]
    fn test_to_float_f32_le() {
        let bytes = 0.25f32.to_le_bytes().to_vec();
        let result = to_float(&[Val::Bytes(bytes), Val::from("le")]);
        match result {
            HotResult::Ok(Val::Dec(d)) => assert_eq!(d.to_string(), "0.25"),
            other => panic!("Expected Dec, got {:?}", other),
        }
    }

    #[test]
    fn test_to_float_shortest_roundtrip() {
        // 0.1 is not exactly representable in binary; the decoded Dec should be
        // the shortest-roundtrip form "0.1", not the expanded binary expansion.
        let bytes = 0.1f64.to_be_bytes().to_vec();
        let result = to_float(&[Val::Bytes(bytes)]);
        match result {
            HotResult::Ok(Val::Dec(d)) => assert_eq!(d.to_string(), "0.1"),
            other => panic!("Expected Dec, got {:?}", other),
        }
    }

    #[test]
    fn test_to_float_invalid_length() {
        let result = to_float(&[Val::Bytes(vec![0, 1, 2])]);
        assert!(matches!(result, HotResult::Err(_)));
    }

    #[test]
    fn test_from_float_f64_roundtrip() {
        let encoded = from_float(&[Val::from(1.5f64), Val::Int(8)]);
        match encoded {
            HotResult::Ok(Val::Bytes(bytes)) => {
                assert_eq!(bytes, 1.5f64.to_be_bytes().to_vec());
            }
            other => panic!("Expected bytes, got {:?}", other),
        }
    }

    #[test]
    fn test_from_float_int_input() {
        let encoded = from_float(&[Val::Int(2), Val::Int(4), Val::from("le")]);
        match encoded {
            HotResult::Ok(Val::Bytes(bytes)) => {
                assert_eq!(bytes, 2.0f32.to_le_bytes().to_vec());
            }
            other => panic!("Expected bytes, got {:?}", other),
        }
    }

    #[test]
    fn test_float_dec_roundtrip_via_val() {
        // Dec -> bytes -> Dec must round-trip for values representable in f64
        for v in ["0.1", "-273.15", "12345.6789", "0", "1e10"] {
            let dec: fastnum::D256 = v.parse().unwrap();
            let encoded = from_float(&[Val::Dec(dec), Val::Int(8)]);
            let bytes = match encoded {
                HotResult::Ok(Val::Bytes(b)) => b,
                other => panic!("Expected bytes, got {:?}", other),
            };
            let decoded = to_float(&[Val::Bytes(bytes)]);
            match decoded {
                HotResult::Ok(Val::Dec(d)) => {
                    assert_eq!(d, dec, "round-trip failed for {}", v)
                }
                other => panic!("Expected Dec, got {:?}", other),
            }
        }
    }

    #[test]
    fn test_float_special_values() {
        // NaN and infinities are legal IEEE 754 values (e.g. Postgres float8);
        // decoding must not error.
        for special in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            let bytes = special.to_be_bytes().to_vec();
            let result = to_float(&[Val::Bytes(bytes)]);
            assert!(
                matches!(result, HotResult::Ok(_)),
                "decoding {:?} bytes should not error",
                special
            );
        }
    }
}
