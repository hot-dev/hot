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

/// The value whose truthiness is tested: the argument, forced if it arrives
/// as a lazy thunk. An Err value is falsy; a halt while computing the value
/// isn't caught here but returned to propagate at the native's boundary.
fn force_for_truthiness(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    val: &Val,
) -> Result<Val, HotResult<Val>> {
    vm.force_lazy(val).map_err(|err| {
        vm.record_halt(&err);
        HotResult::Err(Val::from(
            crate::lang::runtime::vm::VirtualMachine::halt_message(&err),
        ))
    })
}

/// Negate truthiness of a value. One truthiness for the whole language:
/// Val::is_truthy — only false, null, and Err are falsy. 0, "", [], and
/// {} are values, not falseness.
pub fn not(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::bool/not", args, 1);
    match force_for_truthiness(vm, &args[0]) {
        Ok(value) => HotResult::Ok(Val::Bool(!value.is_truthy())),
        Err(halt) => halt,
    }
}

/// Check if a value is truthy: Val::is_truthy — only false, null, and Err
/// are falsy (matching if/cond/assert branch semantics).
pub fn is_truthy(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    validate_args!("::hot::bool/is-truthy", args, 1);
    match force_for_truthiness(vm, &args[0]) {
        Ok(value) => HotResult::Ok(Val::Bool(value.is_truthy())),
        Err(halt) => halt,
    }
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
            Ok(value) => value,
            Err(halt) => return halt,
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
            Ok(value) => value,
            Err(halt) => return halt,
        };
        if !forced.is_truthy() {
            return HotResult::Ok(forced);
        }
        last = forced;
    }
    HotResult::Ok(last)
}
