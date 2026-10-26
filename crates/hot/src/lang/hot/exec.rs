//! Execution control functions (context-aware fail/cancel)
//!
//! These functions detect whether the VM is running inside a task or a run
//! and produce the appropriate typed value:
//! - Run context: `::hot::run/Failure` / `::hot::run/Cancellation`
//! - Task context: `::hot::task/Failure` / `::hot::task/Cancellation`

use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::validate_args;

fn is_task_context(vm: &crate::lang::runtime::vm::VirtualMachine) -> bool {
    vm.get_task_receiver().is_some()
}

fn failure_type(vm: &crate::lang::runtime::vm::VirtualMachine) -> &'static str {
    if is_task_context(vm) {
        "::hot::task/Failure"
    } else {
        "::hot::run/Failure"
    }
}

fn cancellation_type(vm: &crate::lang::runtime::vm::VirtualMachine) -> &'static str {
    if is_task_context(vm) {
        "::hot::task/Cancellation"
    } else {
        "::hot::run/Cancellation"
    }
}

/// The value of a lazy argument (`VirtualMachine::force_lazy`), including
/// a thunk that arrives in its serialized Map form. A halt while computing
/// it is returned, to propagate at the native's boundary.
pub(crate) fn evaluate_lazy(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    val: &Val,
) -> Result<Val, HotResult<Val>> {
    let halt = |vm: &mut crate::lang::runtime::vm::VirtualMachine,
                err: crate::lang::runtime::vm::VmError| {
        vm.record_halt(&err);
        HotResult::Err(Val::from(
            crate::lang::runtime::vm::VirtualMachine::halt_message(&err),
        ))
    };
    if let Val::Map(m) = val
        && m.contains_key(&Val::from("parameters"))
        && m.contains_key(&Val::from("instructions"))
        && m.contains_key(&Val::from("register_count"))
    {
        let lambda = serde_json::to_value(m).ok().and_then(|json| {
            serde_json::from_value::<crate::lang::bytecode::LambdaInfo>(json).ok()
        });
        return match lambda {
            Some(lambda) if lambda.parameters.is_empty() => {
                let boxed = Val::Box(Box::new(lambda));
                vm.execute_lambda(&boxed, &[]).map_err(|err| halt(vm, err))
            }
            _ => Ok(val.clone()),
        };
    }
    vm.force_lazy(val).map_err(|err| halt(vm, err))
}

/// Context-aware fail: produces `::hot::run/Failure` or `::hot::task/Failure`
pub fn fail(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    let (msg, err) = match args.len() {
        1 => {
            let err_data = match evaluate_lazy(vm, &args[0]) {
                Ok(value) => value,
                Err(halt) => return halt,
            };
            let msg: String = match &err_data {
                Val::Str(s) => (**s).to_owned(),
                Val::Map(m) => m
                    .get(&Val::from("message"))
                    .or_else(|| m.get(&Val::from("msg")))
                    .and_then(|v| match v {
                        Val::Str(s) => Some((**s).to_owned()),
                        _ => None,
                    })
                    .unwrap_or_else(|| "Execution failed".to_string()),
                _ => "Execution failed".to_string(),
            };
            (msg, err_data)
        }
        2 => {
            validate_args!("::hot::exec/fail", args, 2);
            let msg: String = match &args[0] {
                Val::Str(s) => (**s).to_owned(),
                _ => {
                    return HotResult::Err(Val::from(
                        "fail: first argument must be a string message",
                    ));
                }
            };
            let err_data = match evaluate_lazy(vm, &args[1]) {
                Ok(value) => value,
                Err(halt) => return halt,
            };
            (msg, err_data)
        }
        _ => {
            return HotResult::Err(Val::from(format!(
                "fail expects 1 or 2 arguments, got {}",
                args.len()
            )));
        }
    };

    tracing::debug!("VM: fail called with msg: '{}', err: {:?}", msg, err);

    let type_name = failure_type(vm);
    let origin = vm.current_origin();
    let failure = crate::val!({
        "$type": type_name,
        "$origin": origin,
        "$val": {
            "msg": msg.clone(),
            "err": err.clone()
        }
    });

    vm.set_failure(msg.clone(), failure.clone());

    HotResult::Err(failure)
}

/// Context-aware cancel: produces `::hot::run/Cancellation` or `::hot::task/Cancellation`
pub fn cancel(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    let (msg, data) = match args.len() {
        1 => {
            validate_args!("::hot::exec/cancel", args, 1);
            let msg: String = match &args[0] {
                Val::Str(s) => (**s).to_owned(),
                _ => {
                    return HotResult::Err(Val::from(
                        "cancel: first argument must be a string message",
                    ));
                }
            };
            (msg, Val::Null)
        }
        2 => {
            validate_args!("::hot::exec/cancel", args, 2);
            let msg: String = match &args[0] {
                Val::Str(s) => (**s).to_owned(),
                _ => {
                    return HotResult::Err(Val::from(
                        "cancel: first argument must be a string message",
                    ));
                }
            };
            let data = match evaluate_lazy(vm, &args[1]) {
                Ok(value) => value,
                Err(halt) => return halt,
            };
            (msg, data)
        }
        _ => {
            return HotResult::Err(Val::from(format!(
                "cancel expects 1 or 2 arguments, got {}",
                args.len()
            )));
        }
    };

    tracing::debug!("VM: cancel called with msg: '{}', data: {:?}", msg, data);

    let type_name = cancellation_type(vm);
    let origin = vm.current_origin();
    let cancellation = crate::val!({
        "$type": type_name,
        "$origin": origin,
        "$val": {
            "msg": msg.clone(),
            "data": data.clone()
        }
    });

    vm.set_cancellation(msg.clone(), cancellation.clone());

    HotResult::Err(cancellation)
}

/// Exit the process/run with a given exit code.
/// - CLI: exits the process
/// - Worker (isolation): exit(0) completes successfully, exit(N) delegates to fail
pub fn exit(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    let exit_code = if args.is_empty() {
        0
    } else {
        validate_args!("::hot::exec/exit", args, 1);
        match &args[0] {
            Val::Int(code) => *code as i32,
            _ => return HotResult::Err(Val::from("Exit code must be an integer")),
        }
    };

    if vm.is_isolation_enabled() {
        tracing::debug!("Isolation mode: exit({}) - harmonizing behavior", exit_code);

        if exit_code == 0 {
            return HotResult::Ok(Val::Null);
        } else {
            return fail(
                vm,
                &[
                    Val::from(format!("Process exited with code {}", exit_code)),
                    crate::val!({"exit_code": exit_code}),
                ],
            );
        }
    }

    std::process::exit(exit_code);
}
