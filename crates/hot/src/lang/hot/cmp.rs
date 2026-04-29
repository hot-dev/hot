use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;

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

/// Check if two values are equal
pub fn eq(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::cmp", args, 2);
    {
        let a = &args[0];
        let b = &args[1];
        // Use the built-in PartialEq implementation which handles cross-type comparisons
        let result = a == b;
        HotResult::Ok(Val::Bool(result))
    }
}

/// Check if two values are not equal
pub fn ne(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::cmp", args, 2);
    {
        let a = &args[0];
        let b = &args[1];
        let result = a != b;
        HotResult::Ok(Val::Bool(result))
    }
}

/// Check if first value is greater than second
pub fn gt(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::cmp", args, 2);
    {
        let a = &args[0];
        let b = &args[1];
        let result = compare_vals(a, b) == std::cmp::Ordering::Greater;
        HotResult::Ok(Val::Bool(result))
    }
}

/// Check if first value is less than second
pub fn lt(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::cmp", args, 2);
    {
        let a = &args[0];
        let b = &args[1];
        let result = compare_vals(a, b) == std::cmp::Ordering::Less;
        HotResult::Ok(Val::Bool(result))
    }
}

/// Check if first value is greater than or equal to second
pub fn gte(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::cmp", args, 2);
    {
        let a = &args[0];
        let b = &args[1];
        let ordering = compare_vals(a, b);
        let result =
            ordering == std::cmp::Ordering::Greater || ordering == std::cmp::Ordering::Equal;
        HotResult::Ok(Val::Bool(result))
    }
}

/// Check if first value is less than or equal to second
pub fn lte(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::cmp", args, 2);
    {
        let a = &args[0];
        let b = &args[1];
        let ordering = compare_vals(a, b);
        let result = ordering == std::cmp::Ordering::Less || ordering == std::cmp::Ordering::Equal;
        HotResult::Ok(Val::Bool(result))
    }
}

/// Compare two values for ordering
fn compare_vals(a: &Val, b: &Val) -> std::cmp::Ordering {
    use crate::val::Val;
    use std::cmp::Ordering;

    match (a, b) {
        (Val::Int(a), Val::Int(b)) => a.cmp(b),
        (Val::Dec(a), Val::Dec(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        (Val::Str(a), Val::Str(b)) => a.cmp(b),
        (Val::Bool(a), Val::Bool(b)) => a.cmp(b),
        // Type coercion for numbers
        (Val::Int(a), Val::Dec(b)) => {
            use fastnum::D256;
            D256::from(*a).partial_cmp(b).unwrap_or(Ordering::Equal)
        }
        (Val::Dec(a), Val::Int(b)) => {
            use fastnum::D256;
            a.partial_cmp(&D256::from(*b)).unwrap_or(Ordering::Equal)
        }
        // For other types, use string comparison as fallback
        _ => format!("{:?}", a).cmp(&format!("{:?}", b)),
    }
}
