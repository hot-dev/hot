// Core language functions for  bytecode engine

use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;

/// VM-aware `do` that forces evaluation of lazy values (lambdas)
pub fn do_eval(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::core/do", args, 1);
    let value = &args[0];
    // `do` evaluates a lazy argument, or calls a function of no arguments,
    // as if written here: an Err value comes back as a value, and a halt
    // inside it propagates.
    if let Val::Box(boxed) = value
        && let Some(lambda) = boxed
            .as_any()
            .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
        && lambda.parameters.is_empty()
    {
        return match vm.execute_lambda(value, &[]) {
            Ok(result) => HotResult::Ok(result),
            Err(err) => {
                vm.record_halt(&err);
                HotResult::Err(Val::from(
                    crate::lang::runtime::vm::VirtualMachine::halt_message(&err),
                ))
            }
        };
    }
    HotResult::Ok(value.clone())
}

/// Check if a value is null
pub fn is_null(args: &[Val]) -> HotResult<Val> {
    validate_args!("is-null", args, 1);

    let result = matches!(args[0], Val::Null);
    HotResult::Ok(Val::Bool(result))
}
