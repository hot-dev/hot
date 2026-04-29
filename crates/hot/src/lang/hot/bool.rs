use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;

// ---------------------------------------------------------------------------
// Fast-path helpers for the VM hot loop.
// ---------------------------------------------------------------------------

#[inline(always)]
pub fn fast_not_bool(b: bool) -> Val {
    Val::Bool(!b)
}

/// Negate truthiness of a value.
pub fn not(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::bool/not", args, 1);

    let is_truthy = match &args[0] {
        Val::Null => false,
        Val::Bool(b) => *b,
        Val::Int(i) => *i != 0,
        Val::Dec(d) => !d.is_zero(),
        Val::Str(s) => !s.is_empty(),
        Val::Vec(v) => !v.is_empty(),
        Val::Map(m) => !m.is_empty(),
        _ => true,
    };

    HotResult::Ok(Val::Bool(!is_truthy))
}

/// Check if a value is truthy (non-null, non-false, non-empty)
pub fn is_truthy(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::bool/is-truthy", args, 1);

    let value = &args[0];
    let is_truthy = match value {
        Val::Null => false,
        Val::Bool(b) => *b,
        Val::Int(i) => *i != 0,
        Val::Dec(d) => !d.is_zero(),
        Val::Str(s) => !s.is_empty(),
        Val::Vec(v) => !v.is_empty(),
        Val::Map(m) => !m.is_empty(),
        _ => true, // Other types are considered truthy
    };

    HotResult::Ok(Val::Bool(is_truthy))
}
