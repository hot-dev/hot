// Core language functions for  bytecode engine

use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;

/// VM-aware `do` that forces evaluation of lazy values (lambdas)
pub fn do_eval(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::core/do", args, 1);
    let v = &args[0];

    // If it's a boxed lambda (zero-argument lambda for lazy evaluation), call it
    if let Val::Box(b) = v {
        // Try to downcast to LambdaInfo
        if let Some(lambda_info) = b
            .as_any()
            .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
        {
            if lambda_info.parameters.is_empty() {
                // Zero-argument lambda - call it
                // Note: We DO evaluate lazy parameter lambdas when `do` is explicitly called
                // The is_lazy_param flag prevents AUTO-evaluation, but `do` is EXPLICIT evaluation
                match vm.execute_lambda(v, &[]) {
                    Ok(result) => {
                        return HotResult::Ok(result);
                    }
                    Err(e) => {
                        return HotResult::Err(Val::from(e.to_string()));
                    }
                }
            } else {
                // Has parameters, not a lazy thunk
                return HotResult::Ok(v.clone());
            }
        }
    }

    // Not a lazy value: return as-is
    HotResult::Ok(v.clone())
}

/// Check if a value is null
pub fn is_null(args: &[Val]) -> HotResult<Val> {
    validate_args!("is-null", args, 1);

    let result = matches!(args[0], Val::Null);
    HotResult::Ok(Val::Bool(result))
}
