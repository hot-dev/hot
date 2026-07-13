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

/// Force a lazy-parameter thunk so its value (including an Err — Err is
/// falsy) reaches the truthiness check. Mirrors ::hot::type/is-err:
/// zero-arg LambdaInfo boxes evaluate under suppressed result checking;
/// a thunk whose evaluation halts counts as an Err (falsy).
fn force_for_truthiness(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    val: &Val,
) -> Option<Val> {
    if let Val::Box(b) = val
        && let Some(lambda_info) = b
            .as_any()
            .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
        && lambda_info.parameters.is_empty()
    {
        let saved = vm.get_suppress_result_checking();
        vm.set_suppress_result_checking(true);
        let result = vm.execute_lambda(val, &[]);
        vm.set_suppress_result_checking(saved);
        // A halted thunk evaluation counts like an Err value (falsy).
        return result.ok();
    }
    Some(val.clone())
}

/// Negate truthiness of a value. One truthiness for the whole language:
/// Val::is_truthy — only false, null, and Err are falsy. 0, "", [], and
/// {} are values, not falseness.
pub fn not(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::bool/not", args, 1);
    let truthy = force_for_truthiness(vm, &args[0]).is_some_and(|v| v.is_truthy());
    HotResult::Ok(Val::Bool(!truthy))
}

/// Check if a value is truthy: Val::is_truthy — only false, null, and Err
/// are falsy (matching if/cond/assert branch semantics).
pub fn is_truthy(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    validate_args!("::hot::bool/is-truthy", args, 1);
    let truthy = force_for_truthiness(vm, &args[0]).is_some_and(|v| v.is_truthy());
    HotResult::Ok(Val::Bool(truthy))
}

/// or: return the first truthy argument, or the last argument when none
/// is. Arguments are already evaluated (or is eager); the first may be a
/// lazy thunk from the wrapper's lazy parameter.
pub fn or(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.is_empty() {
        return HotResult::Err(Val::from("::hot::bool/or expects at least 1 argument"));
    }
    let mut last = Val::Null;
    for arg in args {
        let forced = match force_for_truthiness(vm, arg) {
            Some(v) => v,
            None => continue, // halted thunk: falsy, keep scanning
        };
        if forced.is_truthy() {
            return HotResult::Ok(forced);
        }
        last = forced;
    }
    HotResult::Ok(last)
}

/// and: return the first falsy argument, or the last argument when all
/// are truthy.
pub fn and(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.is_empty() {
        return HotResult::Err(Val::from("::hot::bool/and expects at least 1 argument"));
    }
    let mut last = Val::Null;
    for arg in args {
        let forced = match force_for_truthiness(vm, arg) {
            Some(v) => v,
            None => return HotResult::Ok(Val::Null), // halted thunk: falsy
        };
        if !forced.is_truthy() {
            return HotResult::Ok(forced);
        }
        last = forced;
    }
    HotResult::Ok(last)
}
