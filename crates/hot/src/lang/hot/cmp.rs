use crate::lang::hot::r#type::HotResult;
use crate::val::{Val, as_scalar};
use crate::validate_args_at_least;
use fastnum::D256;
use std::cmp::Ordering;

// ---------------------------------------------------------------------------
// Fast-path helpers for the VM hot loop (Int+Int only, zero overhead).
// ---------------------------------------------------------------------------

#[inline(always)]
pub fn fast_lt_int(a: i64, b: i64) -> Val {
    Val::Bool(a < b)
}

#[inline(always)]
pub fn fast_gt_int(a: i64, b: i64) -> Val {
    Val::Bool(a > b)
}

#[inline(always)]
pub fn fast_lte_int(a: i64, b: i64) -> Val {
    Val::Bool(a <= b)
}

#[inline(always)]
pub fn fast_gte_int(a: i64, b: i64) -> Val {
    Val::Bool(a >= b)
}

#[inline(always)]
pub fn fast_eq_int(a: i64, b: i64) -> Val {
    Val::Bool(a == b)
}

#[inline(always)]
pub fn fast_ne_int(a: i64, b: i64) -> Val {
    Val::Bool(a != b)
}

/// Check if all the values are equal
pub fn eq(args: &[Val]) -> HotResult<Val> {
    // Use the built-in PartialEq implementation which handles cross-type comparisons
    if let [a, b] = args {
        return HotResult::Ok(Val::Bool(a == b));
    }
    validate_args_at_least!("::hot::cmp", args, 2);
    HotResult::Ok(Val::Bool(args.windows(2).all(|pair| pair[0] == pair[1])))
}

/// Check if the values aren't all equal
pub fn ne(args: &[Val]) -> HotResult<Val> {
    if let [a, b] = args {
        return HotResult::Ok(Val::Bool(a != b));
    }
    validate_args_at_least!("::hot::cmp", args, 2);
    HotResult::Ok(Val::Bool(args.windows(2).any(|pair| pair[0] != pair[1])))
}

/// Check if each value is greater than the next
pub fn gt(args: &[Val]) -> HotResult<Val> {
    validate_args_at_least!("::hot::cmp", args, 2);
    answer("gt", args, Ordering::is_gt)
}

/// Check if each value is less than the next
pub fn lt(args: &[Val]) -> HotResult<Val> {
    validate_args_at_least!("::hot::cmp", args, 2);
    answer("lt", args, Ordering::is_lt)
}

/// Check if each value is greater than or equal to the next
pub fn gte(args: &[Val]) -> HotResult<Val> {
    validate_args_at_least!("::hot::cmp", args, 2);
    answer("gte", args, Ordering::is_ge)
}

/// Check if each value is less than or equal to the next
pub fn lte(args: &[Val]) -> HotResult<Val> {
    validate_args_at_least!("::hot::cmp", args, 2);
    answer("lte", args, Ordering::is_le)
}

/// What `name` says of how each argument orders with the next, stopping at
/// the first pair it doesn't hold for, or its Err when a pair has no order,
/// as `add` gives for a value that isn't a number.
fn answer(name: &str, args: &[Val], test: fn(Ordering) -> bool) -> HotResult<Val> {
    if let [a, b] = args {
        return match compare_vals(a, b) {
            Some(ordering) => HotResult::Ok(Val::Bool(test(ordering))),
            None => HotResult::Err(Val::from(format!("{name} requires numbers"))),
        };
    }
    for pair in args.windows(2) {
        match compare_vals(&pair[0], &pair[1]) {
            Some(ordering) if test(ordering) => {}
            Some(_) => return HotResult::Ok(Val::Bool(false)),
            None => return HotResult::Err(Val::from(format!("{name} requires numbers"))),
        }
    }
    HotResult::Ok(Val::Bool(true))
}

/// How two values order, reading a declared type over a scalar as that scalar.
/// Int and Dec order together, as do two strings and two bools. Nothing else
/// has an order to report.
fn compare_vals(a: &Val, b: &Val) -> Option<Ordering> {
    match (as_scalar(a), as_scalar(b)) {
        (Val::Int(a), Val::Int(b)) => Some(a.cmp(b)),
        (Val::Dec(a), Val::Dec(b)) => Some(a.partial_cmp(b).unwrap_or(Ordering::Equal)),
        (Val::Str(a), Val::Str(b)) => Some(a.cmp(b)),
        (Val::Bool(a), Val::Bool(b)) => Some(a.cmp(b)),
        // Type coercion for numbers
        (Val::Int(a), Val::Dec(b)) => {
            Some(D256::from(*a).partial_cmp(b).unwrap_or(Ordering::Equal))
        }
        (Val::Dec(a), Val::Int(b)) => {
            Some(a.partial_cmp(&D256::from(*b)).unwrap_or(Ordering::Equal))
        }
        _ => None,
    }
}
