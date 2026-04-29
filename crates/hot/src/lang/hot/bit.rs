// Bitwise operations for Hot language

use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;

/// Bitwise AND
pub fn and(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::bit/and", args, 2);

    match (&args[0], &args[1]) {
        (Val::Int(a), Val::Int(b)) => HotResult::Ok(Val::Int(a & b)),
        (Val::Byte(a), Val::Byte(b)) => HotResult::Ok(Val::Byte(a & b)),
        (Val::Byte(a), Val::Int(b)) => {
            if *b >= 0 && *b <= 255 {
                HotResult::Ok(Val::Byte(a & (*b as u8)))
            } else {
                HotResult::Err(Val::from(format!(
                    "::hot::bit/and: Int {} is out of byte range (0-255)",
                    b
                )))
            }
        }
        (Val::Int(a), Val::Byte(b)) => HotResult::Ok(Val::Int(a & (*b as i64))),
        _ => HotResult::Err(Val::from(
            "::hot::bit/and: Arguments must be Int or Byte".to_string(),
        )),
    }
}

/// Bitwise OR
pub fn or(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::bit/or", args, 2);

    match (&args[0], &args[1]) {
        (Val::Int(a), Val::Int(b)) => HotResult::Ok(Val::Int(a | b)),
        (Val::Byte(a), Val::Byte(b)) => HotResult::Ok(Val::Byte(a | b)),
        (Val::Byte(a), Val::Int(b)) => {
            if *b >= 0 && *b <= 255 {
                HotResult::Ok(Val::Byte(a | (*b as u8)))
            } else {
                HotResult::Err(Val::from(format!(
                    "::hot::bit/or: Int {} is out of byte range (0-255)",
                    b
                )))
            }
        }
        (Val::Int(a), Val::Byte(b)) => HotResult::Ok(Val::Int(a | (*b as i64))),
        _ => HotResult::Err(Val::from(
            "::hot::bit/or: Arguments must be Int or Byte".to_string(),
        )),
    }
}

/// Bitwise XOR
pub fn xor(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::bit/xor", args, 2);

    match (&args[0], &args[1]) {
        (Val::Int(a), Val::Int(b)) => HotResult::Ok(Val::Int(a ^ b)),
        (Val::Byte(a), Val::Byte(b)) => HotResult::Ok(Val::Byte(a ^ b)),
        (Val::Byte(a), Val::Int(b)) => {
            if *b >= 0 && *b <= 255 {
                HotResult::Ok(Val::Byte(a ^ (*b as u8)))
            } else {
                HotResult::Err(Val::from(format!(
                    "::hot::bit/xor: Int {} is out of byte range (0-255)",
                    b
                )))
            }
        }
        (Val::Int(a), Val::Byte(b)) => HotResult::Ok(Val::Int(a ^ (*b as i64))),
        _ => HotResult::Err(Val::from(
            "::hot::bit/xor: Arguments must be Int or Byte".to_string(),
        )),
    }
}

/// Bitwise NOT (ones' complement)
pub fn not(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::bit/not", args, 1);

    match &args[0] {
        Val::Int(a) => HotResult::Ok(Val::Int(!a)),
        Val::Byte(a) => HotResult::Ok(Val::Byte(!a)),
        _ => HotResult::Err(Val::from(
            "::hot::bit/not: Argument must be Int or Byte".to_string(),
        )),
    }
}

/// Bit shift left
pub fn shift_left(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::bit/shift-left", args, 2);

    let shift_amount = match &args[1] {
        Val::Int(n) => {
            if *n < 0 {
                return HotResult::Err(Val::from(
                    "::hot::bit/shift-left: Shift amount must be non-negative".to_string(),
                ));
            }
            *n as u32
        }
        _ => {
            return HotResult::Err(Val::from(
                "::hot::bit/shift-left: Shift amount must be an Int".to_string(),
            ));
        }
    };

    match &args[0] {
        Val::Int(a) => {
            if shift_amount >= 64 {
                HotResult::Ok(Val::Int(0))
            } else {
                HotResult::Ok(Val::Int(a << shift_amount))
            }
        }
        Val::Byte(a) => {
            if shift_amount >= 8 {
                HotResult::Ok(Val::Byte(0))
            } else {
                HotResult::Ok(Val::Byte(a << shift_amount))
            }
        }
        _ => HotResult::Err(Val::from(
            "::hot::bit/shift-left: First argument must be Int or Byte".to_string(),
        )),
    }
}

/// Bit shift right (arithmetic for signed, logical for unsigned)
pub fn shift_right(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::bit/shift-right", args, 2);

    let shift_amount = match &args[1] {
        Val::Int(n) => {
            if *n < 0 {
                return HotResult::Err(Val::from(
                    "::hot::bit/shift-right: Shift amount must be non-negative".to_string(),
                ));
            }
            *n as u32
        }
        _ => {
            return HotResult::Err(Val::from(
                "::hot::bit/shift-right: Shift amount must be an Int".to_string(),
            ));
        }
    };

    match &args[0] {
        Val::Int(a) => {
            if shift_amount >= 64 {
                // For signed integers, shift right fills with sign bit
                if *a < 0 {
                    HotResult::Ok(Val::Int(-1))
                } else {
                    HotResult::Ok(Val::Int(0))
                }
            } else {
                HotResult::Ok(Val::Int(a >> shift_amount))
            }
        }
        Val::Byte(a) => {
            if shift_amount >= 8 {
                HotResult::Ok(Val::Byte(0))
            } else {
                HotResult::Ok(Val::Byte(a >> shift_amount))
            }
        }
        _ => HotResult::Err(Val::from(
            "::hot::bit/shift-right: First argument must be Int or Byte".to_string(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_and() {
        let result = and(&[Val::Int(0b1100), Val::Int(0b1010)]);
        assert!(matches!(result, HotResult::Ok(Val::Int(0b1000))));

        let result = and(&[Val::Byte(0b1100), Val::Byte(0b1010)]);
        assert!(matches!(result, HotResult::Ok(Val::Byte(0b1000))));
    }

    #[test]
    fn test_or() {
        let result = or(&[Val::Int(0b1100), Val::Int(0b1010)]);
        assert!(matches!(result, HotResult::Ok(Val::Int(0b1110))));
    }

    #[test]
    fn test_xor() {
        let result = xor(&[Val::Int(0b1100), Val::Int(0b1010)]);
        assert!(matches!(result, HotResult::Ok(Val::Int(0b0110))));
    }

    #[test]
    fn test_not() {
        let result = not(&[Val::Byte(0b11110000)]);
        assert!(matches!(result, HotResult::Ok(Val::Byte(0b00001111))));
    }

    #[test]
    fn test_shift_left() {
        let result = shift_left(&[Val::Int(1), Val::Int(4)]);
        assert!(matches!(result, HotResult::Ok(Val::Int(16))));

        let result = shift_left(&[Val::Byte(1), Val::Int(4)]);
        assert!(matches!(result, HotResult::Ok(Val::Byte(16))));
    }

    #[test]
    fn test_shift_right() {
        let result = shift_right(&[Val::Int(16), Val::Int(2)]);
        assert!(matches!(result, HotResult::Ok(Val::Int(4))));

        // Test arithmetic shift for negative numbers
        let result = shift_right(&[Val::Int(-8), Val::Int(1)]);
        assert!(matches!(result, HotResult::Ok(Val::Int(-4))));
    }
}
