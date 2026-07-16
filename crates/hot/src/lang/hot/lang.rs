use crate::lang::hot::r#type::HotResult;
use crate::val::Val;
use crate::{validate_args, validate_no_args};

use super::coll::{OnErrDisposition, apply_onerr_disposition, parse_onerr_disposition};

// Function invocation utilities (moved from function.rs)

/// Extract arguments from a Vec value
fn extract_args_vec(args_val: &Val) -> Result<Vec<Val>, Val> {
    match args_val {
        Val::Vec(vec) => Ok(vec.clone()),
        _ => Err(Val::from(format!(
            "Arguments must be a Vec, got {:?}",
            args_val
        ))),
    }
}

/// Resolve a function name using Hot's scoping rules
fn resolve_function_with_scoping(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    function_name: &str,
) -> Option<Val> {
    // Use the VM's built-in resolution logic - this ensures we use the same
    // resolution order as the VM itself and don't duplicate hard-coded logic
    vm.resolve_function_name(function_name).map(Val::from)
}

fn parse_call_onerr_disposition(args: &[Val]) -> Result<OnErrDisposition, Val> {
    match args.len() {
        1 => Ok(OnErrDisposition::Force),
        2 => match parse_onerr_disposition("::hot::lang/call", args, 1) {
            Ok(disposition) => Ok(disposition),
            Err(_) => Ok(OnErrDisposition::Force),
        },
        3 => match parse_onerr_disposition("::hot::lang/call", args, 2) {
            Ok(disposition) => Ok(disposition),
            Err(err) => {
                if matches!(&args[2], Val::Vec(v) if v.is_empty()) {
                    Ok(OnErrDisposition::Force)
                } else {
                    Err(err)
                }
            }
        },
        _ => Err(Val::from(format!(
            "::hot::lang/call expects 1, 2, or 3 arguments, got {}",
            args.len()
        ))),
    }
}

fn call_args_value(args: &[Val]) -> Option<&Val> {
    match args.len() {
        1 => None,
        2 => match parse_onerr_disposition("::hot::lang/call", args, 1) {
            Ok(_) => None,
            Err(_) => Some(&args[1]),
        },
        3 => Some(&args[1]),
        _ => None,
    }
}

/// The user-facing message of a VmError for embedding in Err payloads and
/// failure state. Display prepends "Runtime error: ", and callers up the
/// stack wrap the string into another RuntimeError whose Display prepends
/// it again — producing "Runtime error: Runtime error: ..." chains. Errors
/// with a source location keep Display so the file:line context survives.
fn vm_error_message(err: &crate::lang::runtime::vm::VmError) -> String {
    match err {
        crate::lang::runtime::vm::VmError::RuntimeError(re) if re.location.is_none() => {
            re.message.clone()
        }
        other => other.to_string(),
    }
}

fn execute_lambda_for_call(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    function_val: &Val,
    args: &[Val],
    disposition: OnErrDisposition,
) -> Result<Val, String> {
    let saved_suppress = if matches!(disposition, OnErrDisposition::Preserve) {
        let current = vm.get_suppress_result_checking();
        vm.set_suppress_result_checking(true);
        Some(current)
    } else {
        None
    };

    let exec = vm.execute_lambda(function_val, args);

    if let Some(saved) = saved_suppress {
        vm.set_suppress_result_checking(saved);
    }

    match exec {
        Ok(value) => apply_onerr_disposition(vm, value, disposition)
            .map(|value| wrap_preserved_call_result(value, disposition)),
        Err(err) => handle_call_execution_error(vm, vm_error_message(&err), disposition),
    }
}

fn execute_user_function_for_call(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    function_id: crate::lang::bytecode::FunctionId,
    args: &[Val],
    disposition: OnErrDisposition,
) -> Result<Val, String> {
    let saved_suppress = if matches!(disposition, OnErrDisposition::Preserve) {
        let current = vm.get_suppress_result_checking();
        vm.set_suppress_result_checking(true);
        Some(current)
    } else {
        None
    };

    let exec = vm.execute_compiled_user_function(function_id, args);

    if let Some(saved) = saved_suppress {
        vm.set_suppress_result_checking(saved);
    }

    match exec {
        Ok(value) => apply_onerr_disposition(vm, value, disposition)
            .map(|value| wrap_preserved_call_result(value, disposition)),
        Err(err) => handle_call_execution_error(vm, vm_error_message(&err), disposition),
    }
}

fn handle_call_execution_error(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    msg: String,
    disposition: OnErrDisposition,
) -> Result<Val, String> {
    if matches!(disposition, OnErrDisposition::Force) && !vm.has_failed() && !vm.has_cancelled() {
        vm.set_failure(msg.clone(), Val::from(msg.clone()));
    }

    Err(msg)
}

fn wrap_preserved_call_result(val: Val, disposition: OnErrDisposition) -> Val {
    if matches!(disposition, OnErrDisposition::Preserve) && val.is_err() {
        let mut raw = indexmap::IndexMap::new();
        raw.insert(Val::from("$type"), Val::from("Raw"));
        raw.insert(Val::from("$val"), val);
        Val::Map(Box::new(raw))
    } else {
        val
    }
}

/// List all namespaces known to the program
pub fn namespaces(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    validate_no_args!("::hot::lang/namespaces", args);

    let ns_paths = vm.get_namespace_registry().get_namespace_paths();
    // Return typed Namespace values for compatibility with hot-std
    let vals: Vec<Val> = ns_paths
        .into_iter()
        .map(|ns| {
            // ::hot::type/Namespace(ns)
            match crate::lang::hot::r#type::namespace_constructor(&[Val::from(ns)]) {
                crate::lang::hot::r#type::HotResult::Ok(v) => v,
                crate::lang::hot::r#type::HotResult::Err(e) => e, // fall back to error value
            }
        })
        .collect();
    HotResult::Ok(Val::Vec(vals))
}

/// List functions in a given namespace
pub fn functions_in_namespace(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    validate_args!("::hot::lang/functions-in-namespace", args, 1);

    // Accept Namespace (typed), fully qualified namespace string, or a FunctionRef
    let ns_path: String = match &args[0] {
        Val::Str(s) => (**s).to_owned(),
        Val::Map(m) => {
            // Typed Namespace object {"$type": "::hot::type/Namespace", "$val": "::ns"}
            if let Some(Val::Str(tn)) = m.get(&Val::from("$type"))
                && &**tn == "::hot::type/Namespace"
            {
                if let Some(Val::Str(val)) = m.get(&Val::from("$val")) {
                    (**val).to_owned()
                } else {
                    return HotResult::Err(Val::from(
                        "Namespace value missing $val string".to_string(),
                    ));
                }
            } else {
                return HotResult::Err(Val::from(
                    "functions-in-namespace expects a Namespace or namespace string".to_string(),
                ));
            }
        }
        Val::Box(b) => {
            // Try NamespaceRef; if FunctionRef, extract namespace
            if let Some(nsref) = b.as_any().downcast_ref::<crate::lang::refs::NamespaceRef>() {
                nsref.path().to_string()
            } else if let Some(fnref) = b
                .as_any()
                .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>()
            {
                let full = fnref.name();
                if let Some((ns, _func)) = full.rsplit_once('/') {
                    ns.to_string()
                } else {
                    full.to_string()
                }
            } else {
                return HotResult::Err(Val::from(
                    "functions-in-namespace expects a Namespace or namespace string".to_string(),
                ));
            }
        }
        _ => {
            return HotResult::Err(Val::from(
                "functions-in-namespace expects a Namespace or namespace string".to_string(),
            ));
        }
    };

    let funcs = vm
        .get_namespace_registry()
        .get_functions_in_namespace(&ns_path);

    // Return typed Fn values via ::hot::type/Fn
    let names: Vec<Val> = funcs
        .into_iter()
        .map(|vi| {
            let fq = format!("{}/{}", ns_path, vi.name);
            match crate::lang::hot::r#type::fn_constructor(&[Val::from(fq)]) {
                crate::lang::hot::r#type::HotResult::Ok(v) => v,
                crate::lang::hot::r#type::HotResult::Err(e) => e,
            }
        })
        .collect();

    HotResult::Ok(Val::Vec(names))
}

// Dynamic function calling (moved from function.rs)

/// Legacy call-lib function - delegates to the universal call function
/// This maintains compatibility with existing Hot code that uses call-lib
pub fn call_lib(args: &[Val]) -> HotResult<Val> {
    // call-lib cannot access VM, so it can only call hotlib functions
    // For user-defined functions, use call_vm_aware through call-lib built-in
    validate_args!("::hot::lang/call-lib", args, 2);

    let (function_val, args_val) = (&args[0], &args[1]);

    // Extract function name
    let function_name: String = match function_val {
        Val::Str(name) => (**name).to_owned(),
        Val::Box(boxed) => boxed.to_string(),
        _ => {
            return HotResult::Err(Val::from(format!(
                "call-lib: first argument must be a function reference, got {:?}",
                function_val
            )));
        }
    };

    // Extract arguments
    let arg_vals = match extract_args_vec(args_val) {
        Ok(av) => av,
        Err(err) => return HotResult::Err(err),
    };

    // Try hotlib functions only (call-lib cannot access VM for user functions)
    let hotlib_map = super::get_hotlib_map();
    if let Some(lib_fn) = hotlib_map.get(&function_name) {
        match lib_fn {
            super::HotLibFn::LibFn(func) => {
                return func(&arg_vals);
            }
            super::HotLibFn::VmAwareFn(_) | super::HotLibFn::VmAwareJitFn(_, _) => {
                return HotResult::Err(Val::from(format!(
                    "Function '{}' requires VM access and cannot be called from call-lib context",
                    function_name
                )));
            }
        }
    }

    HotResult::Err(Val::from(format!(
        "Function '{}' not found in hotlib functions (call-lib can only call hotlib functions)",
        function_name
    )))
}

/// VM-aware universal function dispatcher - the core function calling mechanism
/// Takes a function reference and arguments, dispatches to hotlib OR user-defined functions
pub fn call(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    // Support 1-arg (function only), 2-arg (function, args or OnErr), and
    // 3-arg (function, args, OnErr) forms.
    if args.is_empty() || args.len() > 3 {
        return HotResult::Err(Val::from(format!(
            "::hot::lang/call expects 1, 2, or 3 arguments, got {}",
            args.len()
        )));
    }

    let disposition = match parse_call_onerr_disposition(args) {
        Ok(disposition) => disposition,
        Err(err) => return HotResult::Err(err),
    };

    // Check for call function recursion to prevent infinite loops
    if let Err(err) = vm.increment_call_depth() {
        return HotResult::Err(Val::from(vm_error_message(&err)));
    }

    let function_val = &args[0];
    // Prepare arguments vector
    let arg_vals: Vec<Val> = match call_args_value(args) {
        None => Vec::new(),
        Some(args_val) => match extract_args_vec(args_val) {
            Ok(av) => av,
            Err(err) => {
                vm.decrement_call_depth();
                return HotResult::Err(err);
            }
        },
    };

    // Debug: Check what type of value is being passed as the function
    tracing::debug!("call_vm_aware called with function_val: {:?}", function_val);
    tracing::debug!(
        "function_val type discriminant: {:?}",
        std::mem::discriminant(function_val)
    );

    // Fast-path: typed Fn wrapper
    if let Val::Map(m) = function_val
        && let Some(Val::Str(tn)) = m.get(&Val::from("$type"))
        && &**tn == "::hot::type/Fn"
        && let Some(inner) = m.get(&Val::from("$val"))
    {
        // Lambda inside typed Fn
        if let Val::Box(b) = inner {
            if b.as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                .is_some()
            {
                let exec = execute_lambda_for_call(vm, inner, &arg_vals, disposition);
                vm.decrement_call_depth();
                return match exec {
                    Ok(v) => HotResult::Ok(v),
                    Err(e) => HotResult::Err(Val::from(e)),
                };
            }
            if let Some(_fr) = b
                .as_any()
                .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>()
            {
                // Fall through to generic path; boxed FunctionRef handled there
            }
        }
        // If inner is a string, continue to generic path below
    }

    // Extract function name/reference - UNIFIED APPROACH
    let function_name: String = match function_val {
        // Direct string function name (LAZY QUALIFIED REFERENCE)
        // This is the unified approach: ::namespace/function as a string
        Val::Str(name) => {
            if name.starts_with("::") && name.contains('/') {
                (**name).to_owned()
            } else {
                // Unqualified names should be resolved through namespace scope/imports
                // For now, pass through as-is and let the VM handle core function resolution
                tracing::debug!(
                    "Unqualified function name '{}' - passing to VM for resolution",
                    name
                );
                (**name).to_owned()
            }
        }

        // Function object (Fn type) - extract the fully qualified name from $val if it's a string
        Val::Map(map) => {
            if let Some(Val::Str(type_name)) = map.get(&Val::from("$type"))
                && &**type_name == "::hot::type/Fn"
            {
                if let Some(Val::Str(qualified_name)) = map.get(&Val::from("$val")) {
                    (**qualified_name).to_owned()
                } else {
                    vm.decrement_call_depth();
                    return HotResult::Err(Val::from(
                        "Fn value missing string $val for named function".to_string(),
                    ));
                }
            } else {
                vm.decrement_call_depth();
                return HotResult::Err(Val::from(
                    "Expected function reference (Fn), got Map without proper $type".to_string(),
                ));
            }
        }

        // Legacy: Null reference (from & syntax compilation issues)
        Val::Null => {
            vm.decrement_call_depth();
            return HotResult::Err(Val::from(
                "Function reference is null - use qualified name instead of & syntax".to_string(),
            ));
        }

        // Legacy: Boxed function reference (from & syntax) or boxed FunctionRef/LambdaInfo
        Val::Box(boxed_val) => {
            // If it's a boxed LambdaInfo, execute directly
            if boxed_val
                .as_any()
                .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                .is_some()
            {
                let exec = execute_lambda_for_call(
                    vm,
                    &Val::Box(boxed_val.clone_box()),
                    &arg_vals,
                    disposition,
                );
                vm.decrement_call_depth();
                return match exec {
                    Ok(v) => HotResult::Ok(v),
                    Err(e) => HotResult::Err(Val::from(e)),
                };
            }

            if let Some(fr) = boxed_val
                .as_any()
                .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>()
            {
                fr.name().to_string()
            } else {
                vm.decrement_call_depth();
                return HotResult::Err(Val::from(
                    "Invalid boxed function reference (expected FunctionRef or Lambda)".to_string(),
                ));
            }
        }

        // Other types - show helpful error
        _ => {
            let debug_info = match function_val {
                Val::Int(i) => format!("Int({})", i),
                Val::Bool(b) => format!("Bool({})", b),
                Val::Vec(v) => format!("Vec(len={})", v.len()),
                _ => "Other".to_string(),
            };
            vm.decrement_call_depth();
            return HotResult::Err(Val::from(format!(
                "Invalid function reference type: {} (use qualified name like ::namespace/function)",
                debug_info
            )));
        }
    };

    // Debug: Log the extracted function name
    tracing::debug!(
        "call_vm_aware: extracted function_name = '{}'",
        function_name
    );

    // CALL FUNCTION: Use a special VM method that bypasses the unified lookup to avoid recursion
    // The issue is that unified_function_lookup calls hotlib functions, including call itself

    // Resolve function name with proper scoping
    let resolved_function_name = if function_name.starts_with("::") {
        // Already qualified, use as-is
        function_name.clone()
    } else {
        // Unqualified name - use proper scoping to resolve it
        match resolve_function_with_scoping(vm, &function_name) {
            Some(Val::Str(resolved_name)) => (*resolved_name).to_string(),
            _ => {
                // If scoping resolution fails, fall back to original name
                function_name.clone()
            }
        }
    };

    tracing::debug!("Final resolved function name: '{}'", resolved_function_name);

    // IMPORTANT: `call` must ONLY invoke compiled Hot functions (user-defined).
    // Do NOT dispatch to hotlib here; `call-lib` handles that. This prevents
    // recursion where hot test runner code calls back into `call` via hotlib.
    if let Some(function_id) =
        vm.find_best_user_function_overload(&resolved_function_name, &arg_vals)
    {
        let exec = execute_user_function_for_call(vm, function_id, &arg_vals, disposition);
        vm.decrement_call_depth();
        return match exec {
            Ok(result) => HotResult::Ok(result),
            Err(err) => HotResult::Err(Val::from(format!(
                "Error executing function '{}': {}",
                resolved_function_name, err
            ))),
        };
    }

    let hotlib_map = super::get_hotlib_map();
    if hotlib_map.contains_key(&resolved_function_name) {
        let exec = vm
            .execute_call_lib(&resolved_function_name, &arg_vals)
            .map_err(|err| vm_error_message(&err))
            .and_then(|value| apply_onerr_disposition(vm, value, disposition))
            .map(|value| wrap_preserved_call_result(value, disposition));
        vm.decrement_call_depth();
        return match exec {
            Ok(result) => HotResult::Ok(result),
            Err(err) => HotResult::Err(Val::from(format!(
                "Error executing function '{}': {}",
                resolved_function_name, err
            ))),
        };
    }

    // Not found among compiled functions: instruct caller to use call-lib for hotlib
    vm.decrement_call_depth();
    HotResult::Err(Val::from(format!(
        "Function '{}' not found in compiled Hot functions. Use `call-lib` for hotlib functions.",
        resolved_function_name
    )))
}

fn contain_success(value: Val) -> HotResult<Val> {
    // Idempotent on Results: a callee that already returned a Result
    // (e.g. a native that errors by value) passes through unchanged.
    // Wrapping an Err in Ok would hide it from is-err/if-err.
    if value.is_ok() || value.is_err() {
        HotResult::Ok(value)
    } else {
        HotResult::Ok(Val::ok(value))
    }
}

fn contained_error_payload(
    vm: &crate::lang::runtime::vm::VirtualMachine,
    err: impl ToString,
) -> Val {
    if let Some(failure) = vm.get_failure() {
        return failure.data;
    }
    if let Some(cancellation) = vm.get_cancellation() {
        return cancellation.data;
    }

    let msg = err.to_string();
    crate::val!({
        "msg": msg.clone(),
        "err": {"error": msg},
    })
}

fn contain_error(
    vm: &crate::lang::runtime::vm::VirtualMachine,
    err: impl ToString,
) -> HotResult<Val> {
    HotResult::Ok(Val::err(contained_error_payload(vm, err)))
}

fn reset_contained_state(vm: &crate::lang::runtime::vm::VirtualMachine) {
    vm.reset_failure_state();
    vm.reset_cancellation_state();
}

/// Resolve a contained call's outcome at the halt boundary. A halt raised
/// inside a JIT-compiled frame surfaces as an Ok-wrapped Err value while the
/// VM failure/cancellation state is still set (the run boundary handles the
/// same Ok-but-failed case when emitting events). Treat that state as the
/// halt it represents: return the structured halt payload and reset so code
/// outside the boundary runs normally.
fn contain_outcome(
    vm: &crate::lang::runtime::vm::VirtualMachine,
    exec: Result<Val, impl ToString>,
) -> HotResult<Val> {
    match exec {
        Ok(value) => {
            if vm.has_failed() || vm.has_cancelled() {
                let result = contain_error(vm, "halted");
                reset_contained_state(vm);
                result
            } else {
                contain_success(value)
            }
        }
        Err(err) => {
            let result = contain_error(vm, err);
            reset_contained_state(vm);
            result
        }
    }
}

/// Runtime-internal halt containment (::hot::internal::exec/contain).
/// Calls a function and contains halts (fail()/cancel()) and hard runtime
/// errors as a Result.Err; a callee that returns a Result passes through
/// unchanged. This backs the engine's own supervision boundaries (AI tool
/// dispatch, lifecycle fan-out). Application code uses the error model
/// instead; the public ::hot::lang/try and try-call were removed in 2.6.0.
pub fn contain(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    // Support 1-arg (function only) and 2-arg (function, args) forms
    if args.len() != 1 && args.len() != 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::internal::exec/contain expects 1 or 2 arguments, got {}",
            args.len(),
        )));
    }

    // Check for call function recursion to prevent infinite loops
    if let Err(err) = vm.increment_call_depth() {
        return contain_error(vm, err);
    }

    let function_val = &args[0];
    let arg_vals: Vec<Val> = if args.len() == 1 {
        Vec::new()
    } else {
        match extract_args_vec(&args[1]) {
            Ok(av) => av,
            Err(err) => {
                vm.decrement_call_depth();
                return contain_error(vm, err);
            }
        }
    };

    // Fast-path: typed Fn wrapping an inline lambda. Mirrors `call`'s
    // lambda fast-path so `contain(some-lambda, [...])` works when the
    // callee is an inline lambda value rather than a qualified function
    // name. Without this, the generic name-extraction path below errors
    // because $val is a boxed LambdaInfo rather than a string.
    if let Val::Map(m) = function_val
        && let Some(Val::Str(tn)) = m.get(&Val::from("$type"))
        && &**tn == "::hot::type/Fn"
        && let Some(inner) = m.get(&Val::from("$val"))
        && let Val::Box(b) = inner
        && b.as_any()
            .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
            .is_some()
    {
        let exec = vm.execute_lambda(inner, &arg_vals);
        vm.decrement_call_depth();
        return contain_outcome(vm, exec);
    }

    // Bare boxed LambdaInfo (less common; mirrors `call`'s Val::Box arm).
    if let Val::Box(boxed_val) = function_val
        && boxed_val
            .as_any()
            .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
            .is_some()
    {
        let exec = vm.execute_lambda(&Val::Box(boxed_val.clone_box()), &arg_vals);
        vm.decrement_call_depth();
        return contain_outcome(vm, exec);
    }

    // Extract function name (same logic as call)
    let function_name: String = match function_val {
        Val::Str(name) => (**name).to_owned(),
        Val::Map(map) => {
            if let Some(Val::Str(type_name)) = map.get(&Val::from("$type"))
                && &**type_name == "::hot::type/Fn"
            {
                if let Some(Val::Str(qualified_name)) = map.get(&Val::from("$val")) {
                    (**qualified_name).to_owned()
                } else {
                    vm.decrement_call_depth();
                    return contain_error(vm, "Fn value missing string $val");
                }
            } else {
                vm.decrement_call_depth();
                return contain_error(vm, "Expected function reference (Fn)");
            }
        }
        Val::Box(boxed_val) => {
            if let Some(fr) = boxed_val
                .as_any()
                .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>()
            {
                fr.name().to_string()
            } else {
                vm.decrement_call_depth();
                return contain_error(vm, "Invalid function reference");
            }
        }
        _ => {
            vm.decrement_call_depth();
            return contain_error(
                vm,
                format!("Invalid function reference type: {:?}", function_val),
            );
        }
    };

    // Resolve function name
    let resolved_function_name = if function_name.starts_with("::") {
        function_name.clone()
    } else {
        match resolve_function_with_scoping(vm, &function_name) {
            Some(Val::Str(resolved_name)) => (*resolved_name).to_string(),
            _ => function_name.clone(),
        }
    };

    // Call the function and catch ANY error
    if let Some(function_id) =
        vm.find_best_user_function_overload(&resolved_function_name, &arg_vals)
    {
        let exec = vm.execute_compiled_user_function(function_id, &arg_vals);
        vm.decrement_call_depth();
        return contain_outcome(vm, exec);
    }

    vm.decrement_call_depth();
    contain_error(
        vm,
        format!(
            "Function '{}' not found in compiled Hot functions",
            resolved_function_name
        ),
    )
}

/// VM-aware resolve function with proper namespace scoping (implements ::hot::lang/resolve)
pub fn resolve(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from(format!(
            "::hot::lang/resolve expects 1 argument, got {}",
            args.len()
        )));
    }

    // Extract function name from various input types
    let function_name: String = match &args[0] {
        // Direct string function name
        Val::Str(name) => (**name).to_owned(),

        // Already a Fn type - extract the function name from $val
        Val::Map(map) => {
            if let Some(Val::Str(type_name)) = map.get(&Val::from("$type"))
                && &**type_name == "::hot::type/Fn"
            {
                if let Some(Val::Str(_qualified_name)) = map.get(&Val::from("$val")) {
                    // Already a Fn type, return as-is
                    return HotResult::Ok(Val::Map(map.clone()));
                } else {
                    return HotResult::Err(Val::from(
                        "Fn value missing string $val for named function".to_string(),
                    ));
                }
            } else {
                return HotResult::Err(Val::from(format!(
                    "resolve expects a string function name or Fn type, got Map: {:?}",
                    map
                )));
            }
        }

        // Boxed function reference
        Val::Box(boxed) => {
            if let Some(function_ref) = boxed
                .as_any()
                .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>(
            ) {
                function_ref.name().to_string()
            } else {
                return HotResult::Err(Val::from(format!(
                    "resolve expects a string function name, Fn type, or FunctionRef, got boxed: {:?}",
                    boxed
                )));
            }
        }

        other => {
            return HotResult::Err(Val::from(format!(
                "resolve expects a string function name, Fn type, or FunctionRef, got: {:?}",
                other
            )));
        }
    };

    // Resolve the function name to a fully qualified name
    if let Some(resolved_name_val) = resolve_function_with_scoping(vm, &function_name) {
        match resolved_name_val {
            Val::Str(resolved_name) => {
                // Create a proper Fn type object using the type constructor
                // This creates {"$type": "::hot::type/Fn", "$val": "resolved_name"}
                super::r#type::user_type_constructor("::hot::type/Fn", &[Val::Str(resolved_name)])
            }
            other => HotResult::Ok(other), // Return as-is if not a string
        }
    } else {
        HotResult::Err(Val::from(format!("Function '{}' not found", function_name)))
    }
}
