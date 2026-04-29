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

/// Evaluate lazy thunks (handles both Val::Box lambdas and serialized Map forms)
fn evaluate_lazy(vm: &mut crate::lang::runtime::vm::VirtualMachine, val: &Val) -> Val {
    match val {
        Val::Box(b) => {
            if let Some(lambda_info) = b
                .as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
            {
                if lambda_info.parameters.is_empty() {
                    let saved_suppress = if lambda_info.is_lazy_param {
                        let current = vm.get_suppress_result_checking();
                        vm.set_suppress_result_checking(true);
                        Some(current)
                    } else {
                        None
                    };

                    let result = match vm.execute_lambda(val, &[]) {
                        Ok(result) => result,
                        Err(e) => {
                            if let Some(saved) = saved_suppress {
                                vm.set_suppress_result_checking(saved);
                            }
                            tracing::warn!("Failed to evaluate lazy argument: {}", e);
                            return val.clone();
                        }
                    };

                    if let Some(saved) = saved_suppress {
                        vm.set_suppress_result_checking(saved);
                    }

                    result
                } else {
                    val.clone()
                }
            } else {
                val.clone()
            }
        }
        Val::Map(m) => {
            if m.contains_key(&Val::from("parameters"))
                && m.contains_key(&Val::from("instructions"))
                && m.contains_key(&Val::from("register_count"))
            {
                match serde_json::to_value(m) {
                    Ok(json_val) => {
                        match serde_json::from_value::<crate::lang::bytecode::LambdaInfo>(json_val)
                        {
                            Ok(lambda_info) => {
                                if lambda_info.parameters.is_empty() {
                                    let boxed_lambda = Val::Box(Box::new(lambda_info));
                                    match vm.execute_lambda(&boxed_lambda, &[]) {
                                        Ok(result) => result,
                                        Err(e) => {
                                            tracing::warn!(
                                                "Failed to evaluate reconstructed lazy argument: {}",
                                                e
                                            );
                                            val.clone()
                                        }
                                    }
                                } else {
                                    val.clone()
                                }
                            }
                            Err(e) => {
                                tracing::warn!("Failed to deserialize lambda from map: {}", e);
                                val.clone()
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!(
                            "Failed to convert map to JSON for lambda reconstruction: {}",
                            e
                        );
                        val.clone()
                    }
                }
            } else {
                val.clone()
            }
        }
        _ => val.clone(),
    }
}

/// Context-aware fail: produces `::hot::run/Failure` or `::hot::task/Failure`
pub fn fail(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    let (msg, err) = match args.len() {
        1 => {
            let err_data = evaluate_lazy(vm, &args[0]);
            let msg: String = match &err_data {
                Val::Str(s) => (**s).to_owned(),
                Val::Map(m) => m
                    .get(&Val::from("message"))
                    .or_else(|| m.get(&Val::from("msg")))
                    .or_else(|| m.get(&Val::from("$msg")))
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
            let err_data = evaluate_lazy(vm, &args[1]);
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
    let failure = crate::val!({
        "$type": type_name,
        "$val": {
            "$msg": msg.clone(),
            "$err": err.clone()
        }
    });

    let is_first_failure = vm.set_failure(msg.clone(), failure.clone());

    if is_first_failure
        && let (Some(emitter), Some(execution_context)) =
            (vm.get_emitter().as_ref(), vm.get_execution_context())
    {
        let event = crate::lang::emitter::EngineEvent::run_fail(execution_context, failure.clone());
        emitter.emit(event);
        tracing::debug!("VM: Emitted run:fail event with {} type", type_name);
    }

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
            let data = evaluate_lazy(vm, &args[1]);
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
    let cancellation = crate::val!({
        "$type": type_name,
        "$val": {
            "$msg": msg.clone(),
            "$data": data.clone()
        }
    });

    let is_first_cancellation = vm.set_cancellation(msg.clone(), cancellation.clone());

    if is_first_cancellation
        && let (Some(emitter), Some(execution_context)) =
            (vm.get_emitter().as_ref(), vm.get_execution_context())
    {
        let event =
            crate::lang::emitter::EngineEvent::run_cancel(execution_context, cancellation.clone());
        emitter.emit(event);
        tracing::debug!("VM: Emitted run:cancel event with {} type", type_name);
    }

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
