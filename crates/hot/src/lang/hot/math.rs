// Math functions for bytecode engine

use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;
use fastnum::D256;

/// Check if a D256 decimal represents an integer (no fractional part)
fn is_d256_integer(dec: &D256) -> bool {
    // Check if the decimal has no fractional part by checking if dec % 1 == 0
    // Handle both positive and negative zero by using is_zero() method
    let remainder = *dec % D256::ONE;
    remainder.is_zero()
}

/// Convert a D256 decimal to i64 if it represents a whole number
fn d256_to_i64(dec: &D256) -> Option<i64> {
    if is_d256_integer(dec) {
        // Get the string representation and handle the .0 case
        let s = dec.to_string();
        if let Ok(result) = s.parse::<i64>() {
            Some(result)
        } else if s.ends_with(".0") {
            // Handle case like "3.0" -> "3"
            let trimmed = &s[..s.len() - 2];
            trimmed.parse::<i64>().ok()
        } else {
            None
        }
    } else {
        None
    }
}

/// Check if a number is zero
pub fn is_zero(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::math/is-zero", args, 1);

    match &args[0] {
        Val::Int(i) => HotResult::Ok(Val::Bool(*i == 0)),
        Val::Dec(d) => HotResult::Ok(Val::Bool(d.is_zero())),
        other => {
            let type_name = match other {
                Val::Null => "null",
                Val::Str(_) => "string",
                Val::Bool(_) => "boolean",
                Val::Vec(_) => "vector",
                Val::Map(_) => "map",
                Val::Box(_) => "boxed value",
                _ => "unknown",
            };
            HotResult::Err(Val::from(format!(
                "is-zero requires a number, got {}",
                type_name
            )))
        }
    }
}

/// The Err an Int result gets when it doesn't fit in an Int, as `div(1, 0)`
/// gets one for a zero divisor.
#[cold]
pub fn int_overflow() -> Val {
    Val::err(Val::from("Integer overflow"))
}

/// An Int result, or the overflow Err when it doesn't fit.
#[inline(always)]
fn int_or_overflow(result: Option<i64>) -> Val {
    match result {
        Some(n) => Val::Int(n),
        None => int_overflow(),
    }
}

/// `base` to a non-negative Int power, exactly.
fn int_pow(base: i64, exp: i64) -> Val {
    match u32::try_from(exp) {
        Ok(exp) => int_or_overflow(base.checked_pow(exp)),
        // Only 0, 1 and -1 stay in range for an exponent this large.
        Err(_) => match base {
            0 | 1 => Val::Int(base),
            -1 => Val::Int(if exp % 2 == 0 { 1 } else { -1 }),
            _ => int_overflow(),
        },
    }
}

// ---------------------------------------------------------------------------
// Fast-path helpers for the VM hot loop.
// These handle common type specializations with zero overhead (no
// validate_args, no HotResult wrapping). The VM calls these before falling
// through to the general-purpose functions below.
// ---------------------------------------------------------------------------

#[inline(always)]
pub fn fast_add_int(a: i64, b: i64) -> Val {
    int_or_overflow(a.checked_add(b))
}

#[inline(always)]
pub fn fast_sub_int(a: i64, b: i64) -> Val {
    int_or_overflow(a.checked_sub(b))
}

#[inline(always)]
pub fn fast_mul_int(a: i64, b: i64) -> Val {
    int_or_overflow(a.checked_mul(b))
}

#[inline(always)]
pub fn fast_mod_int(a: i64, b: i64) -> Option<Val> {
    // Only `i64::MIN % -1` wraps, and its remainder is 0.
    if b != 0 {
        Some(Val::Int(a.wrapping_rem(b)))
    } else {
        None
    }
}

#[inline(always)]
pub fn fast_is_zero_int(a: i64) -> Val {
    Val::Bool(a == 0)
}

#[inline(always)]
pub fn fast_div_int(a: i64, b: i64) -> Option<Val> {
    if b == 0 {
        return None;
    }
    // `i64::MIN / -1` doesn't fit in an Int; `div` returns it as a Dec.
    if a.wrapping_rem(b) == 0 {
        a.checked_div(b).map(Val::Int)
    } else {
        None
    }
}

#[inline(always)]
pub fn fast_pow_int(a: i64, b: i64) -> Option<Val> {
    if b < 0 {
        return None;
    }
    Some(int_pow(a, b))
}

/// Add two numbers
pub fn add(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::math/add", args, 2);

    match (&args[0], &args[1]) {
        (Val::Int(a), Val::Int(b)) => HotResult::Ok(int_or_overflow(a.checked_add(*b))),
        (Val::Dec(a), Val::Dec(b)) => HotResult::Ok(Val::Dec(*a + *b)),
        (Val::Int(a), Val::Dec(b)) => {
            use fastnum::decimal::Decimal;
            let a_dec = Decimal::from(*a);
            HotResult::Ok(Val::Dec(a_dec + *b))
        }
        (Val::Dec(a), Val::Int(b)) => {
            use fastnum::decimal::Decimal;
            let b_dec = Decimal::from(*b);
            HotResult::Ok(Val::Dec(*a + b_dec))
        }
        _ => HotResult::Err(Val::from("add requires numbers")),
    }
}

/// Subtract two numbers
pub fn sub(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::math/sub", args, 2);

    match (&args[0], &args[1]) {
        (Val::Int(a), Val::Int(b)) => HotResult::Ok(int_or_overflow(a.checked_sub(*b))),
        (Val::Dec(a), Val::Dec(b)) => HotResult::Ok(Val::Dec(*a - *b)),
        (Val::Int(a), Val::Dec(b)) => {
            use fastnum::decimal::Decimal;
            let a_dec = Decimal::from(*a);
            HotResult::Ok(Val::Dec(a_dec - *b))
        }
        (Val::Dec(a), Val::Int(b)) => {
            use fastnum::decimal::Decimal;
            let b_dec = Decimal::from(*b);
            HotResult::Ok(Val::Dec(*a - b_dec))
        }
        _ => HotResult::Err(Val::from("sub requires numbers")),
    }
}

/// Multiply two numbers
pub fn mul(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::math/mul", args, 2);

    match (&args[0], &args[1]) {
        (Val::Int(a), Val::Int(b)) => HotResult::Ok(int_or_overflow(a.checked_mul(*b))),
        (Val::Dec(a), Val::Dec(b)) => HotResult::Ok(Val::Dec(*a * *b)),
        (Val::Int(a), Val::Dec(b)) => {
            use fastnum::decimal::Decimal;
            let a_dec = Decimal::from(*a);
            HotResult::Ok(Val::Dec(a_dec * *b))
        }
        (Val::Dec(a), Val::Int(b)) => {
            use fastnum::decimal::Decimal;
            let b_dec = Decimal::from(*b);
            HotResult::Ok(Val::Dec(*a * b_dec))
        }
        _ => HotResult::Err(Val::from("mul requires numbers")),
    }
}

/// Divide two numbers
pub fn div(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::math/div", args, 2);

    match (&args[0], &args[1]) {
        (Val::Int(a), Val::Int(b)) => {
            if *b == 0 {
                return HotResult::Ok(Val::err(Val::from("Division by zero")));
            }
            // Integer division that can result in decimal
            let result = D256::from(*a) / D256::from(*b);
            // For int/int division, convert back to integer if the result is a whole number
            if let Some(int_result) = d256_to_i64(&result) {
                HotResult::Ok(Val::Int(int_result))
            } else {
                HotResult::Ok(Val::Dec(result))
            }
        }
        (Val::Dec(a), Val::Dec(b)) => {
            if b.is_zero() {
                return HotResult::Ok(Val::err(Val::from("Division by zero")));
            }
            HotResult::Ok(Val::Dec(*a / *b))
        }
        (Val::Int(a), Val::Dec(b)) => {
            if b.is_zero() {
                return HotResult::Ok(Val::err(Val::from("Division by zero")));
            }
            use fastnum::decimal::Decimal;
            let a_dec = Decimal::from(*a);
            HotResult::Ok(Val::Dec(a_dec / *b))
        }
        (Val::Dec(a), Val::Int(b)) => {
            if *b == 0 {
                return HotResult::Ok(Val::err(Val::from("Division by zero")));
            }
            use fastnum::decimal::Decimal;
            let b_dec = Decimal::from(*b);
            HotResult::Ok(Val::Dec(*a / b_dec))
        }
        _ => HotResult::Err(Val::from("div requires numbers")),
    }
}

/// The remainder of `a / b` with the sign of `a` (truncated division), as an
/// Int `mod` returns it.
fn dec_remainder(a: fastnum::D256, b: fastnum::D256) -> HotResult<Val> {
    if b.is_zero() {
        return HotResult::Ok(Val::err(Val::from("Modulo by zero")));
    }
    HotResult::Ok(Val::Dec(a % b))
}

/// Modulo operation: the remainder of truncated division, which has the sign
/// of the dividend for every Int and Dec combination (`mod(-7, 2)` is `-1`,
/// `mod(-7.0, 2)` is `-1.0`).
pub fn modulo(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::math/modulo", args, 2);

    match (&args[0], &args[1]) {
        (Val::Int(a), Val::Int(b)) => {
            if *b == 0 {
                return HotResult::Ok(Val::err(Val::from("Modulo by zero")));
            }
            // Only `i64::MIN % -1` wraps, and its remainder is 0.
            HotResult::Ok(Val::Int(a.wrapping_rem(*b)))
        }
        // A Dec remainder truncates like an Int one: it takes the sign of
        // `a`, and `%` computes it exactly (no rounded quotient).
        (Val::Dec(a), Val::Dec(b)) => dec_remainder(*a, *b),
        (Val::Int(a), Val::Dec(b)) => dec_remainder(fastnum::D256::from(*a), *b),
        (Val::Dec(a), Val::Int(b)) => dec_remainder(*a, fastnum::D256::from(*b)),
        _ => HotResult::Err(Val::from("mod requires numbers")),
    }
}

/// Power operation
pub fn pow(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::math/pow", args, 2);

    match (&args[0], &args[1]) {
        (Val::Int(a), Val::Int(b)) => {
            if *b < 0 {
                // Negative exponent, use decimal
                use fastnum::decimal::Decimal;
                let a_dec = Decimal::from(*a);
                let b_dec = Decimal::from(*b);
                HotResult::Ok(Val::Dec(a_dec.pow(b_dec)))
            } else {
                HotResult::Ok(int_pow(*a, *b))
            }
        }
        (Val::Dec(a), Val::Dec(b)) => HotResult::Ok(Val::Dec(a.pow(*b))),
        (Val::Int(a), Val::Dec(b)) => {
            use fastnum::decimal::Decimal;
            let a_dec = Decimal::from(*a);
            HotResult::Ok(Val::Dec(a_dec.pow(*b)))
        }
        (Val::Dec(a), Val::Int(b)) => {
            use fastnum::decimal::Decimal;
            let b_dec = Decimal::from(*b);
            HotResult::Ok(Val::Dec(a.pow(b_dec)))
        }
        _ => HotResult::Err(Val::from("pow requires numbers")),
    }
}

/// Ceiling function
pub fn ceil(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::math/ceil", args, 1);

    match &args[0] {
        Val::Int(i) => HotResult::Ok(Val::Int(*i)), // Integer ceiling is itself
        Val::Dec(d) => HotResult::Ok(int_or_overflow(d.ceil().to_i64().ok())),
        _ => HotResult::Err(Val::from("ceil requires a number")),
    }
}

/// Floor function
pub fn floor(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::math/floor", args, 1);

    match &args[0] {
        Val::Int(i) => HotResult::Ok(Val::Int(*i)), // Integer floor is itself
        Val::Dec(d) => HotResult::Ok(int_or_overflow(d.floor().to_i64().ok())),
        _ => HotResult::Err(Val::from("floor requires a number")),
    }
}

/// Absolute value
pub fn abs(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::math/abs", args, 1);

    match &args[0] {
        Val::Int(i) => HotResult::Ok(int_or_overflow(i.checked_abs())),
        Val::Dec(d) => HotResult::Ok(Val::Dec(d.abs())),
        _ => HotResult::Err(Val::from("abs requires a number")),
    }
}

/// Maximum of two numbers
pub fn max(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::math/max", args, 2);

    match (&args[0], &args[1]) {
        (Val::Int(a), Val::Int(b)) => HotResult::Ok(Val::Int(*a.max(b))),
        (Val::Dec(a), Val::Dec(b)) => HotResult::Ok(Val::Dec(if a >= b { *a } else { *b })),
        (Val::Int(a), Val::Dec(b)) => {
            use fastnum::decimal::Decimal;
            let a_dec = Decimal::from(*a);
            HotResult::Ok(Val::Dec(if a_dec >= *b { a_dec } else { *b }))
        }
        (Val::Dec(a), Val::Int(b)) => {
            use fastnum::decimal::Decimal;
            let b_dec = Decimal::from(*b);
            HotResult::Ok(Val::Dec(if *a >= b_dec { *a } else { b_dec }))
        }
        _ => HotResult::Err(Val::from("max requires numbers")),
    }
}

/// Minimum of two numbers
pub fn min(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::math/min", args, 2);

    match (&args[0], &args[1]) {
        (Val::Int(a), Val::Int(b)) => HotResult::Ok(Val::Int(*a.min(b))),
        (Val::Dec(a), Val::Dec(b)) => HotResult::Ok(Val::Dec(if a <= b { *a } else { *b })),
        (Val::Int(a), Val::Dec(b)) => {
            use fastnum::decimal::Decimal;
            let a_dec = Decimal::from(*a);
            HotResult::Ok(Val::Dec(if a_dec <= *b { a_dec } else { *b }))
        }
        (Val::Dec(a), Val::Int(b)) => {
            use fastnum::decimal::Decimal;
            let b_dec = Decimal::from(*b);
            HotResult::Ok(Val::Dec(if *a <= b_dec { *a } else { b_dec }))
        }
        _ => HotResult::Err(Val::from("min requires numbers")),
    }
}

/// Round function
pub fn round(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::math/round", args, 1);

    match &args[0] {
        Val::Int(i) => HotResult::Ok(Val::Int(*i)), // Integer round is itself
        Val::Dec(d) => HotResult::Ok(int_or_overflow(d.round(0).to_i64().ok())),
        _ => HotResult::Err(Val::from("round requires a number")),
    }
}

/// A random Int: from 0 to the largest Int, or from 0 to `max - 1`.
pub fn rand_int(args: &[Val]) -> HotResult<Val> {
    use rand::RngExt;

    match args {
        [] => HotResult::Ok(Val::Int(rand::rng().random_range(0..=i64::MAX))),
        [Val::Int(max)] if *max > 0 => HotResult::Ok(Val::Int(rand::rng().random_range(0..*max))),
        [Val::Int(_)] => HotResult::Err(Val::from("rand-int max must be positive")),
        [other] => HotResult::Err(Val::from(format!(
            "rand-int expects an Int max, got {other}"
        ))),
        _ => HotResult::Err(Val::from(format!(
            "::hot::math/rand-int expects 0 or 1 arguments, got {}",
            args.len()
        ))),
    }
}

/// Random number generator
pub fn rand(args: &[Val]) -> HotResult<Val> {
    use rand::RngExt;

    if args.is_empty() {
        // Return random decimal between 0 and 1
        let mut rng = rand::rng();
        let random_val: f64 = rng.random();
        HotResult::Ok(Val::Dec(fastnum::D256::from(random_val)))
    } else {
        validate_args!("::hot::math/rand", args, 1);

        match &args[0] {
            Val::Int(max) => {
                if *max <= 0 {
                    return HotResult::Err(Val::from("rand max must be positive"));
                }
                let mut rng = rand::rng();
                let random_val = rng.random_range(0.0..(*max as f64));
                HotResult::Ok(Val::Dec(fastnum::D256::from(random_val)))
            }
            Val::Dec(d) => {
                // Coerce decimal to positive f64 range
                let max_f = d.to_string().parse::<f64>().unwrap_or(0.0);
                if max_f <= 0.0 {
                    return HotResult::Err(Val::from("rand max must be positive"));
                }
                let mut rng = rand::rng();
                let random_val = rng.random_range(0.0..max_f);
                HotResult::Ok(Val::Dec(fastnum::D256::from(random_val)))
            }
            _ => HotResult::Err(Val::from("rand expects a numeric max value")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // `is-zero` takes an Int or a Dec. A Null or a Map gave true, for an old
    // test runner, while compiled code gave false.
    #[test]
    fn is_zero_takes_numbers_only() {
        let dec = |s: &str| Val::Dec(s.parse().expect("a Dec"));
        assert_eq!(is_zero(&[Val::Int(0)]), HotResult::Ok(Val::Bool(true)));
        assert_eq!(is_zero(&[dec("0.00")]), HotResult::Ok(Val::Bool(true)));
        assert_eq!(is_zero(&[dec("-0.0")]), HotResult::Ok(Val::Bool(true)));
        assert_eq!(is_zero(&[dec("1.5")]), HotResult::Ok(Val::Bool(false)));
        assert_eq!(
            is_zero(&[Val::Null]),
            HotResult::Err(Val::from("is-zero requires a number, got null"))
        );
        assert_eq!(
            is_zero(&[Val::map_empty()]),
            HotResult::Err(Val::from("is-zero requires a number, got map"))
        );
    }
}
