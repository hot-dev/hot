// Collection functions for bytecode engine

use crate::lang::hot::r#type::HotResult;
use crate::lang::runtime::limits;
use crate::lang::runtime::vm::{CancellationState, FailureState};
use crate::val::Val;
use ahash::AHashSet;
use indexmap::IndexMap;
// Removed placeholder sanitizer; placeholders are no longer emitted.
use crate::validate_args;

#[inline(always)]
pub fn fast_is_empty_vec(v: &[Val]) -> Val {
    Val::Bool(v.is_empty())
}

#[inline(always)]
pub fn fast_is_empty_str(s: &str) -> Val {
    Val::Bool(s.is_empty())
}

#[inline(always)]
pub fn fast_is_empty_map(m: &IndexMap<Val, Val>) -> Val {
    Val::Bool(m.is_empty())
}

#[inline(always)]
pub fn fast_length_vec(v: &[Val]) -> Val {
    Val::Int(v.len() as i64)
}

#[inline(always)]
pub fn fast_length_str(s: &str) -> Val {
    Val::Int(s.len() as i64)
}

#[inline(always)]
pub fn fast_length_map(m: &IndexMap<Val, Val>) -> Val {
    Val::Int(m.len() as i64)
}

#[inline(always)]
pub fn fast_first_vec(v: &[Val]) -> Val {
    if v.is_empty() {
        Val::Null
    } else {
        v[0].clone()
    }
}

#[inline(always)]
pub fn fast_rest_vec(v: &[Val]) -> Val {
    if v.is_empty() {
        Val::Vec(Vec::new())
    } else {
        Val::Vec(v[1..].to_vec())
    }
}

#[inline(always)]
pub fn fast_get_vec_int(v: &[Val], i: i64) -> Val {
    if i < 0 {
        return Val::Null;
    }
    v.get(i as usize).cloned().unwrap_or(Val::Null)
}

#[inline(always)]
pub fn fast_get_map(m: &IndexMap<Val, Val>, key: &Val) -> Val {
    m.get(key).cloned().unwrap_or(Val::Null)
}

#[inline(always)]
pub fn fast_concat_str(a: &str, b: &str) -> Val {
    let mut s = String::with_capacity(a.len() + b.len());
    s.push_str(a);
    s.push_str(b);
    Val::from(s)
}

#[inline(always)]
pub fn fast_concat_vec(a: &[Val], b: &[Val]) -> Val {
    let mut result = Vec::with_capacity(a.len() + b.len());
    result.extend_from_slice(a);
    result.extend_from_slice(b);
    Val::Vec(result)
}

/// Build an Int range [0..end) — the most common range call. Returns `None` if
/// the element count cannot be represented (so the caller falls back to the
/// hotlib `range` for exact semantics rather than silently yielding an empty
/// `Vec`).
#[inline(always)]
pub fn fast_range_1_int(end: i64) -> Option<Val> {
    build_int_range(0, end, 1).map(Val::Vec)
}

/// Build an Int range [start..end) with step 1. Returns `None` on an
/// unrepresentable element count (see [`fast_range_1_int`]).
#[inline(always)]
pub fn fast_range_2_int(start: i64, end: i64) -> Option<Val> {
    build_int_range(start, end, 1).map(Val::Vec)
}

/// Build an Int range [start..end) with explicit step.
#[inline(always)]
pub fn fast_range_3_int(start: i64, end: i64, step: i64) -> Option<Val> {
    build_int_range(start, end, step).map(Val::Vec)
}

fn build_int_range(start: i64, end: i64, step: i64) -> Option<Vec<Val>> {
    let count = limits::range_element_count(start, end, step)?;
    let mut result = Vec::with_capacity(count);
    let mut current = start;
    if step > 0 {
        while current < end {
            result.push(Val::Int(current));
            // Use checked_add so a step that would overflow `i64` terminates
            // the loop instead of panicking in debug or wrapping in release.
            match current.checked_add(step) {
                Some(next) => current = next,
                None => break,
            }
        }
    } else {
        while current > end {
            result.push(Val::Int(current));
            match current.checked_add(step) {
                Some(next) => current = next,
                None => break,
            }
        }
    }
    Some(result)
}

enum PreparedFunction<'a> {
    Named(&'a str),
    CallableVal(&'a Val),
    Invalid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OnErrDisposition {
    Force,
    Preserve,
}

pub(crate) fn parse_onerr_disposition(
    function_name: &str,
    args: &[Val],
    base_arity: usize,
) -> Result<OnErrDisposition, Val> {
    match args.len() {
        n if n == base_arity => Ok(OnErrDisposition::Force),
        n if n == base_arity + 1 => match &args[base_arity] {
            val if is_onerr_variant(val, "Force") => Ok(OnErrDisposition::Force),
            val if is_onerr_variant(val, "Preserve") => Ok(OnErrDisposition::Preserve),
            other => Err(Val::from(format!(
                "{} optional trailing argument must be OnErr.Force or OnErr.Preserve, got {:?}",
                function_name, other
            ))),
        },
        n => Err(Val::from(format!(
            "{} expects {} or {} arguments, got {}",
            function_name,
            base_arity,
            base_arity + 1,
            n
        ))),
    }
}

fn is_onerr_variant(val: &Val, variant: &str) -> bool {
    let expected_full = format!("::hot::type/OnErr.{}", variant);
    let expected_short = format!("OnErr.{}", variant);

    match val {
        Val::Map(map) => {
            if let Some(Val::Str(type_str)) = map.get(&Val::from("$type")) {
                **type_str == expected_full || **type_str == expected_short
            } else {
                false
            }
        }
        Val::Str(s) => **s == expected_full || **s == expected_short || &**s == variant,
        _ => false,
    }
}

fn onerr_variant_val(variant: &str) -> Val {
    let mut map = IndexMap::new();
    map.insert(
        Val::from("$type"),
        Val::from(format!("::hot::type/OnErr.{}", variant)),
    );
    Val::Map(Box::new(map))
}

fn unwrap_ok_preserve_err(val: Val) -> Val {
    if let Val::Map(map) = &val
        && let Some(Val::Str(type_str)) = map.get(&Val::from("$type"))
        && &**type_str == "::hot::type/Result.Ok"
    {
        return map.get(&Val::from("$val")).cloned().unwrap_or(Val::Null);
    }

    val
}

pub(crate) fn apply_onerr_disposition(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    val: Val,
    disposition: OnErrDisposition,
) -> Result<Val, String> {
    match disposition {
        OnErrDisposition::Force => {
            // Force is the HOF's explicit contract decision on a fully
            // returned callback result. Ambient result-check suppression can
            // still be active here (a lazy branch bracket in the callback's
            // tail position spans the lambda return) and must not disable it.
            let prev = vm.get_suppress_result_checking();
            vm.set_suppress_result_checking(false);
            let out = vm.unwrap_result_if_ok(&val).map_err(|err| err.to_string());
            vm.set_suppress_result_checking(prev);
            out
        }
        OnErrDisposition::Preserve => Ok(unwrap_ok_preserve_err(val)),
    }
}

fn prepare_function(function_val: &Val) -> PreparedFunction<'_> {
    let mut cur = function_val;
    loop {
        if let Val::Map(m) = cur
            && let Some(Val::Str(tn)) = m.get(&Val::from("$type"))
        {
            if &**tn == "::hot::type/Fn"
                && let Some(inner) = m.get(&Val::from("$val"))
            {
                cur = inner;
                continue;
            }
            if &**tn == "::hot::type/FunctionAlias"
                && let Some(Val::Str(target)) = m.get(&Val::from("$target"))
            {
                return PreparedFunction::Named(target);
            }
        }

        return match cur {
            Val::Str(name) => PreparedFunction::Named(name),
            Val::Box(boxed)
                if boxed
                    .as_any()
                    .downcast_ref::<crate::lang::bytecode::LambdaInfo>()
                    .is_some() =>
            {
                PreparedFunction::CallableVal(cur)
            }
            Val::Box(boxed) => {
                if let Some(fr) = boxed
                    .as_any()
                    .downcast_ref::<crate::lang::runtime::function_ref::FunctionRef>()
                {
                    PreparedFunction::Named(fr.name())
                } else {
                    PreparedFunction::Invalid
                }
            }
            _ => PreparedFunction::Invalid,
        };
    }
}

fn call_prepared_with_vm(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    function: &PreparedFunction<'_>,
    arg: &Val,
) -> Result<Val, String> {
    call_prepared_with_vm_multi_args(vm, function, std::slice::from_ref(arg))
}

fn call_prepared_with_vm_multi_args(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    function: &PreparedFunction<'_>,
    args: &[Val],
) -> Result<Val, String> {
    match function {
        PreparedFunction::Named(name) => {
            if let Some(function_id) = vm.find_best_user_function_overload(name, args) {
                match vm.execute_compiled_user_function(function_id, args) {
                    Ok(result) => return Ok(result),
                    Err(vm_error) => {
                        return Err(format!("Compiled function call failed: {:?}", vm_error));
                    }
                }
            }

            match vm.execute_function_call_by_name(name, args) {
                Ok(result) => Ok(result),
                Err(vm_error) => Err(format!("VM function call failed: {:?}", vm_error)),
            }
        }
        PreparedFunction::CallableVal(function_val) => {
            if let Ok(Some(result)) = vm.try_jit_lambda_call(function_val, args) {
                return Ok(result);
            }

            match vm.execute_lambda(function_val, args) {
                Ok(result) => Ok(result),
                Err(vm_error) => Err(format!("Lambda execution failed: {:?}", vm_error)),
            }
        }
        PreparedFunction::Invalid => Err(
            "Invalid function type. Expected string function name, function reference, or lambda."
                .to_string(),
        ),
    }
}

/// Helper function to call a function with VM context
pub fn call_function_with_vm(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    function_val: &Val,
    arg: &Val,
) -> Result<Val, String> {
    let function = prepare_function(function_val);
    call_prepared_with_vm(vm, &function, arg)
}

/// Count elements in a collection
pub fn count(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::coll/count", args, 1);

    match &args[0] {
        Val::Vec(v) => HotResult::Ok(Val::Int(v.len() as i64)),
        Val::Map(m) => HotResult::Ok(Val::Int(m.len() as i64)),
        Val::Str(s) => HotResult::Ok(Val::Int(s.chars().count() as i64)),
        _ => HotResult::Err(Val::from("count requires a collection")),
    }
}

/// Get first element of a collection
pub fn first(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::coll/first", args, 1);

    match &args[0] {
        Val::Vec(v) => {
            if v.is_empty() {
                HotResult::Ok(Val::Null)
            } else {
                HotResult::Ok(v[0].clone())
            }
        }
        Val::Str(s) => match s.chars().next() {
            Some(c) => HotResult::Ok(Val::from(c.to_string())),
            None => HotResult::Ok(Val::Null),
        },
        _ => HotResult::Err(Val::from("first requires a collection")),
    }
}

/// Get last element of a collection
pub fn last(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::coll/last", args, 1);

    match &args[0] {
        Val::Vec(v) => {
            if v.is_empty() {
                HotResult::Ok(Val::Null)
            } else {
                HotResult::Ok(v[v.len() - 1].clone())
            }
        }
        Val::Str(s) => match s.chars().last() {
            Some(c) => HotResult::Ok(Val::from(c.to_string())),
            None => HotResult::Ok(Val::Null),
        },
        _ => HotResult::Err(Val::from("last requires a collection")),
    }
}

/// Get nth element of a collection (0-indexed)
pub fn nth(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::coll/nth", args, 2);

    let index = match &args[1] {
        Val::Int(i) => *i as usize,
        _ => return HotResult::Err(Val::from("nth requires an integer index")),
    };

    match &args[0] {
        Val::Vec(v) => {
            if index < v.len() {
                HotResult::Ok(v[index].clone())
            } else {
                HotResult::Ok(Val::Null)
            }
        }
        Val::Str(s) => {
            let chars: Vec<char> = s.chars().collect();
            if index < chars.len() {
                HotResult::Ok(Val::from(chars[index].to_string()))
            } else {
                HotResult::Ok(Val::Null)
            }
        }
        _ => HotResult::Err(Val::from("nth requires a collection")),
    }
}

/// Map a function over a collection and concatenate the results
pub fn mapcat(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    let disposition = match parse_onerr_disposition("::hot::coll/mapcat", args, 2) {
        Ok(disposition) => disposition,
        Err(err) => return HotResult::Err(err),
    };

    let collection = &args[0];
    let function_val = &args[1];

    tracing::debug!(
        "VM: mapcat_vm_aware called with collection: {:?}, function: {:?}",
        collection,
        function_val
    );

    // Null collections map to an empty vector.
    if matches!(collection, Val::Null) {
        tracing::debug!("VM: mapcat_vm_aware - collection is null, returning empty vector");
        return HotResult::Ok(Val::Vec(vec![]));
    }

    match collection {
        Val::Vec(items) => {
            let function = prepare_function(function_val);
            let mut result = Vec::with_capacity(items.len());

            // Apply function to each item and concatenate results
            for item in items {
                // Call the function with the item using VM context
                let function_result = match call_prepared_with_vm(vm, &function, item)
                    .and_then(|val| apply_onerr_disposition(vm, val, disposition))
                {
                    Ok(val) => val,
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Mapcat function call failed: {}",
                            err
                        )));
                    }
                };

                // Concatenate the result (should be a vector)
                match function_result {
                    Val::Vec(mut sub_items) => {
                        result.append(&mut sub_items);
                    }
                    single_item => {
                        result.push(single_item);
                    }
                }
            }

            HotResult::Ok(Val::Vec(result))
        }
        Val::Map(map) => {
            let function = prepare_function(function_val);
            let mut result = Vec::with_capacity(map.len());

            // Apply function to each key-value pair and concatenate results
            for (key, value) in map.iter() {
                let pair = Val::Vec(vec![key.clone(), value.clone()]);

                // Call the function with the key-value pair using VM context
                let function_result = match call_prepared_with_vm(vm, &function, &pair)
                    .and_then(|val| apply_onerr_disposition(vm, val, disposition))
                {
                    Ok(val) => val,
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Mapcat function call failed: {}",
                            err
                        )));
                    }
                };

                // Concatenate the result (should be a vector)
                match function_result {
                    Val::Vec(mut sub_items) => {
                        result.append(&mut sub_items);
                    }
                    single_item => {
                        result.push(single_item);
                    }
                }
            }

            HotResult::Ok(Val::Vec(result))
        }
        _ => HotResult::Err(Val::from("mapcat requires a collection")),
    }
}

/// Helper function to call a function with VM context using multiple arguments
pub fn call_function_with_vm_multi_args(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    function_val: &Val,
    args: &[Val],
) -> Result<Val, String> {
    let function = prepare_function(function_val);
    call_prepared_with_vm_multi_args(vm, &function, args)
}

/// Filter a collection using a predicate function (VM-aware version)
pub fn filter(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/filter expects 2 arguments, got {}",
            args.len()
        )));
    }

    // Enforce coll-first ordering: (coll, predicate)
    let collection = &args[0];
    let predicate_val = &args[1];

    tracing::debug!(
        "VM: filter called with collection: {:?}, predicate: {:?}",
        collection,
        predicate_val
    );

    // Null collections map to an empty vector.
    if matches!(collection, Val::Null) {
        tracing::debug!("VM: filter - collection is null, returning empty vector");
        return HotResult::Ok(Val::Vec(vec![]));
    }

    match collection {
        Val::Vec(items) => {
            let predicate = prepare_function(predicate_val);
            let mut result = Vec::with_capacity(items.len());

            // Apply predicate to each item and keep those that return truthy
            for item in items {
                // Call the predicate function with the item using VM context
                let predicate_result = match call_prepared_with_vm(vm, &predicate, item)
                    .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(val) => val,
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Filter predicate call failed: {}",
                            err
                        )));
                    }
                };

                // Check if result is truthy using the standardized is_truthy method
                let is_truthy = predicate_result.is_truthy();

                // Debug logging for test functions
                if let Val::Str(item_str) = item
                    && item_str.starts_with("test-")
                {
                    tracing::debug!(
                        "VM: filter predicate result for '{}': {:?} (truthy: {})",
                        item_str,
                        predicate_result,
                        is_truthy
                    );
                }

                if is_truthy {
                    result.push(item.clone());
                }
            }

            HotResult::Ok(Val::Vec(result))
        }
        Val::Map(map) => {
            let predicate = prepare_function(predicate_val);
            let mut result = Vec::with_capacity(map.len());

            // Apply predicate to each key-value pair and keep those that return truthy
            for (key, value) in map.iter() {
                // Call the predicate function with key and value using VM context
                let predicate_result = match call_prepared_with_vm_multi_args(
                    vm,
                    &predicate,
                    &[key.clone(), value.clone()],
                )
                .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(val) => val,
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Filter predicate call failed: {}",
                            err
                        )));
                    }
                };

                // Check if result is truthy using the standardized is_truthy method
                let is_truthy = predicate_result.is_truthy();

                if is_truthy {
                    // Include the key-value pair as a vector [key, value]
                    result.push(Val::Vec(vec![key.clone(), value.clone()]));
                }
            }

            HotResult::Ok(Val::Vec(result))
        }
        Val::Str(s) => {
            let predicate = prepare_function(predicate_val);
            let mut result = Vec::new();

            // Apply predicate to each character and keep those that return truthy
            for ch in s.chars() {
                let char_val = Val::from(ch.to_string());

                // Call the predicate function with the character using VM context
                let predicate_result = match call_prepared_with_vm(vm, &predicate, &char_val)
                    .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(val) => val,
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Filter predicate call failed: {}",
                            err
                        )));
                    }
                };

                // Check if result is truthy using the standardized is_truthy method
                let is_truthy = predicate_result.is_truthy();

                if is_truthy {
                    result.push(char_val);
                }
            }

            HotResult::Ok(Val::Vec(result))
        }
        Val::Box(boxed) => {
            // Check if it's an iterator
            if let Some(iter_box) = boxed
                .as_any()
                .downcast_ref::<crate::lang::hot::iter::IteratorBox>()
            {
                let predicate = prepare_function(predicate_val);
                let mut result = Vec::new();

                // Iterate over all values from the iterator
                loop {
                    let (value, done) = {
                        let mut guard = match iter_box.inner.lock() {
                            Ok(g) => g,
                            Err(e) => {
                                return HotResult::Err(Val::from(format!(
                                    "Filter: failed to lock iterator: {}",
                                    e
                                )));
                            }
                        };
                        match guard.next() {
                            Ok(r) => r,
                            Err(e) => {
                                return HotResult::Err(Val::from(format!(
                                    "Filter: iterator error: {}",
                                    e
                                )));
                            }
                        }
                    };

                    if done {
                        break;
                    }

                    // Apply the predicate to each value
                    let predicate_result = match call_prepared_with_vm(vm, &predicate, &value)
                        .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                    {
                        Ok(val) => val,
                        Err(err) => {
                            return HotResult::Err(Val::from(format!(
                                "Filter predicate call failed: {}",
                                err
                            )));
                        }
                    };

                    if predicate_result.is_truthy() {
                        result.push(value);
                    }
                }

                HotResult::Ok(Val::Vec(result))
            } else {
                HotResult::Err(Val::from(
                    "Filter function expects a collection (Vec, Map, Str, or Iter)".to_string(),
                ))
            }
        }
        _ => HotResult::Err(Val::from(
            "Filter function expects a collection (Vec, Map, Str, or Iter)".to_string(),
        )),
    }
}

/// Map a function over a collection
/// This is a simplified version that works with basic collections
/// Map function over a collection (VM-aware version)
pub fn map(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    let disposition = match parse_onerr_disposition("::hot::coll/map", args, 2) {
        Ok(disposition) => disposition,
        Err(err) => return HotResult::Err(err),
    };

    // Enforce coll-first ordering: (coll, func)
    let collection = &args[0];
    let function_val = &args[1];

    tracing::debug!(
        "VM: map_vm_aware called with collection: {:?}, function: {:?}",
        collection,
        function_val
    );

    // Null collections map to an empty vector.
    if matches!(collection, Val::Null) {
        tracing::debug!("VM: map_vm_aware - collection is null, returning empty vector");
        return HotResult::Ok(Val::Vec(vec![]));
    }

    match collection {
        Val::Vec(items) => {
            let function = prepare_function(function_val);
            let mut result = Vec::with_capacity(items.len());

            // Apply function to each item
            for item in items {
                // Call the function with the item using VM context
                let function_result = match call_prepared_with_vm(vm, &function, item)
                    .and_then(|val| apply_onerr_disposition(vm, val, disposition))
                {
                    Ok(val) => val,
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Map function call failed: {}",
                            err
                        )));
                    }
                };

                result.push(function_result);
            }

            HotResult::Ok(Val::Vec(result))
        }
        Val::Map(map) => {
            let function = prepare_function(function_val);
            let mut result = Vec::with_capacity(map.len());

            // Apply function to each key-value pair
            for (key, value) in map.iter() {
                // Call the function with key and value using VM context
                let function_result = match call_prepared_with_vm_multi_args(
                    vm,
                    &function,
                    &[key.clone(), value.clone()],
                )
                .and_then(|val| apply_onerr_disposition(vm, val, disposition))
                {
                    Ok(val) => val,
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Map function call failed: {}",
                            err
                        )));
                    }
                };

                result.push(function_result);
            }

            HotResult::Ok(Val::Vec(result))
        }
        Val::Str(s) => {
            let function = prepare_function(function_val);
            let mut result = Vec::new();

            // Apply function to each character
            for ch in s.chars() {
                let char_val = Val::from(ch.to_string());

                // Call the function with the character using VM context
                let function_result = match call_prepared_with_vm(vm, &function, &char_val)
                    .and_then(|val| apply_onerr_disposition(vm, val, disposition))
                {
                    Ok(val) => val,
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Map function call failed: {}",
                            err
                        )));
                    }
                };

                result.push(function_result);
            }

            HotResult::Ok(Val::Vec(result))
        }
        Val::Box(boxed) => {
            // Check if it's an iterator
            if let Some(iter_box) = boxed
                .as_any()
                .downcast_ref::<crate::lang::hot::iter::IteratorBox>()
            {
                let function = prepare_function(function_val);
                let mut result = Vec::new();

                // Iterate over all values from the iterator
                loop {
                    let (value, done) = {
                        let mut guard = match iter_box.inner.lock() {
                            Ok(g) => g,
                            Err(e) => {
                                return HotResult::Err(Val::from(format!(
                                    "Map: failed to lock iterator: {}",
                                    e
                                )));
                            }
                        };
                        match guard.next() {
                            Ok(r) => r,
                            Err(e) => {
                                return HotResult::Err(Val::from(format!(
                                    "Map: iterator error: {}",
                                    e
                                )));
                            }
                        }
                    };

                    if done {
                        break;
                    }

                    // Apply the function to each value
                    let function_result = match call_prepared_with_vm(vm, &function, &value)
                        .and_then(|val| apply_onerr_disposition(vm, val, disposition))
                    {
                        Ok(val) => val,
                        Err(err) => {
                            return HotResult::Err(Val::from(format!(
                                "Map function call failed: {}",
                                err
                            )));
                        }
                    };

                    result.push(function_result);
                }

                HotResult::Ok(Val::Vec(result))
            } else {
                HotResult::Err(Val::from(
                    "Map function expects a collection (Vec, Map, Str, or Iter)".to_string(),
                ))
            }
        }
        _ => HotResult::Err(Val::from(
            "Map function expects a collection (Vec, Map, Str, or Iter)".to_string(),
        )),
    }
}

/// Get the length of a collection
pub fn length(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::coll/length", args, 1);

    let len = match &args[0] {
        Val::Vec(v) => v.len(),
        Val::Map(m) => m.len(),
        Val::Str(s) => s.len(),
        Val::Bytes(b) => b.len(),
        _ => {
            return HotResult::Err(Val::from(
                "length expects collection (Vec, Map, Str, or Bytes)".to_string(),
            ));
        }
    };
    HotResult::Ok(Val::Int(len as i64))
}

/// Get a value from a collection by key/index
/// Works on Map (by key), Vec (by index), Str (by index), and Bytes (by index)
/// 2-arity: get(coll, key) - returns null if not found
/// 3-arity: get(coll, key, not-found) - returns not-found value if not found
pub fn get(args: &[Val]) -> HotResult<Val> {
    if args.len() < 2 || args.len() > 3 {
        return HotResult::Err(Val::from(
            "::hot::coll/get expects 2 or 3 arguments (coll, key, [not-found])".to_string(),
        ));
    }

    let coll = &args[0];
    let key = &args[1];
    let not_found = if args.len() == 3 {
        args[2].clone()
    } else {
        Val::Null
    };

    match coll {
        Val::Map(m) => {
            // Map lookup by key
            match m.get(key) {
                Some(v) => HotResult::Ok(v.clone()),
                None => HotResult::Ok(not_found),
            }
        }
        Val::Vec(v) => {
            // Vec lookup by integer index
            let index = match key {
                Val::Int(i) => {
                    if *i < 0 {
                        return HotResult::Ok(not_found);
                    }
                    *i as usize
                }
                _ => {
                    return HotResult::Err(Val::from(
                        "::hot::coll/get: Vec index must be an Int".to_string(),
                    ));
                }
            };
            match v.get(index) {
                Some(val) => HotResult::Ok(val.clone()),
                None => HotResult::Ok(not_found),
            }
        }
        Val::Str(s) => {
            // Str lookup by integer index, returns single character string
            let index = match key {
                Val::Int(i) => {
                    if *i < 0 {
                        return HotResult::Ok(not_found);
                    }
                    *i as usize
                }
                _ => {
                    return HotResult::Err(Val::from(
                        "::hot::coll/get: Str index must be an Int".to_string(),
                    ));
                }
            };
            match s.chars().nth(index) {
                Some(c) => HotResult::Ok(Val::from(c.to_string())),
                None => HotResult::Ok(not_found),
            }
        }
        Val::Bytes(b) => {
            // Bytes lookup by integer index, returns single byte as Int
            let index = match key {
                Val::Int(i) => {
                    if *i < 0 {
                        return HotResult::Ok(not_found);
                    }
                    *i as usize
                }
                _ => {
                    return HotResult::Err(Val::from(
                        "::hot::coll/get: Bytes index must be an Int".to_string(),
                    ));
                }
            };
            match b.get(index) {
                Some(byte) => HotResult::Ok(Val::Int(*byte as i64)),
                None => HotResult::Ok(not_found),
            }
        }
        _ => HotResult::Err(Val::from(
            "::hot::coll/get expects collection (Map, Vec, Str, or Bytes)".to_string(),
        )),
    }
}

/// Associate a key-value pair with a map, returning a new map
/// Works only on Map. Creates a new map with the key-value pair added.
pub fn assoc(args: &[Val]) -> HotResult<Val> {
    if args.len() != 3 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/assoc expects 3 arguments (map, key, value), got {}",
            args.len()
        )));
    }

    let map = &args[0];
    let key = &args[1];
    let value = &args[2];

    match map {
        Val::Map(m) => {
            let mut new_map = (**m).clone(); // Clone the inner IndexMap
            new_map.insert(key.clone(), value.clone());
            HotResult::Ok(Val::Map(Box::new(new_map)))
        }
        _ => HotResult::Err(Val::from(
            "::hot::coll/assoc expects a Map as the first argument".to_string(),
        )),
    }
}

/// Concatenate collections
pub fn concat(args: &[Val]) -> HotResult<Val> {
    if args.len() < 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/concat expects at least 2 arguments, got {}",
            args.len()
        )));
    }

    tracing::debug!(
        "VM: concat called with {} arguments: {:?}",
        args.len(),
        args
    );

    // Determine concat mode: any string -> string mode; all bytes -> bytes mode; all vec -> vec mode; else error
    let any_string = args.iter().any(|a| matches!(a, Val::Str(_)));
    let all_bytes = args
        .iter()
        .all(|a| matches!(a, Val::Bytes(_)) || matches!(a, Val::Null));
    let all_vec = args
        .iter()
        .all(|a| matches!(a, Val::Vec(_)) || matches!(a, Val::Null));
    let mode = if any_string {
        "Str"
    } else if all_bytes {
        "Bytes"
    } else if all_vec {
        "Vec"
    } else {
        "Other"
    };

    if mode == "Other" {
        tracing::error!("VM: concat unsupported argument mix: {:?}", args);
        return HotResult::Err(Val::from(
            "concat expects all arguments to be strings, vectors, or bytes".to_string(),
        ));
    }

    // Filter out null arguments and collect
    let mut valid_args: Vec<&Val> = Vec::new();
    for (i, arg) in args.iter().enumerate() {
        match arg {
            Val::Null => {
                tracing::debug!("VM: concat ignoring null argument at position {}", i + 1);
                continue; // Skip null arguments
            }
            _ => valid_args.push(arg),
        }
    }

    match mode {
        "Bytes" => {
            let mut result: Vec<u8> = Vec::new();
            for arg in valid_args {
                if let Val::Bytes(bytes) = arg {
                    result.extend(bytes.iter());
                }
            }
            HotResult::Ok(Val::Bytes(result))
        }
        "Vec" => {
            let mut result = Vec::new();
            for arg in valid_args {
                if let Val::Vec(vec) = arg {
                    result.extend(vec.iter().cloned());
                }
            }
            HotResult::Ok(Val::Vec(result))
        }
        "Str" => {
            let mut result = String::new();
            for arg in valid_args {
                match arg {
                    Val::Str(s) => result.push_str(s),
                    Val::Int(i) => result.push_str(&i.to_string()),
                    Val::Dec(d) => result.push_str(&d.to_string()),
                    Val::Bool(b) => result.push_str(&b.to_string()),
                    Val::Null => {}
                    Val::Byte(b) => result.push_str(&format!("{}", b)),
                    Val::Bytes(bytes) => result.push_str(&format!("{:?}", bytes)),
                    Val::Vec(v) => result.push_str(&format!("{:?}", v)),
                    Val::Map(m) => result.push_str(&format!("{:?}", m)),
                    Val::Box(b) => result.push_str(&b.to_string()),
                }
            }
            HotResult::Ok(Val::from(result))
        }
        // The branches above cover every value `mode` can hold, but we use a
        // structured error rather than `unreachable!()` so a future refactor
        // can't take the worker down.
        other => HotResult::Err(Val::from(format!(
            "concat: internal error — unsupported mode {:?}",
            other
        ))),
    }
}

// first and last functions are already defined above

/// Merge two maps
pub fn merge(args: &[Val]) -> HotResult<Val> {
    validate_args!("::hot::coll/merge", args, 2);

    let first_map = &args[0];
    let second_map = &args[1];

    let unwrapped_first = first_map.clone();
    let unwrapped_second = second_map.clone();

    match (&unwrapped_first, &unwrapped_second) {
        (Val::Map(_), Val::Map(_)) => {
            // Delegate to Val::merge for canonical deep merge semantics
            let result = unwrapped_first.merge(&unwrapped_second);
            HotResult::Ok(result)
        }
        _ => HotResult::Err(Val::from("merge expects two maps")),
    }
}

pub fn reduce(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.len() != 3 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/reduce expects 3 arguments, got {}",
            args.len()
        )));
    }

    let collection = &args[0];
    let function_val = &args[1];
    let init_val = &args[2];

    tracing::debug!(
        "VM: reduce_vm_aware called with collection: {:?}, function: {:?}, init: {:?}",
        collection,
        function_val,
        init_val
    );

    // Null collections reduce to the initial value.
    if matches!(collection, Val::Null) {
        tracing::debug!("VM: reduce_vm_aware - collection is null, returning init value");
        return HotResult::Ok(init_val.clone());
    }

    let function = prepare_function(function_val);
    match collection {
        Val::Vec(items) => {
            let mut accumulator = init_val.clone();

            for item in items {
                let function_result = match call_prepared_with_vm_multi_args(
                    vm,
                    &function,
                    &[accumulator.clone(), item.clone()],
                )
                .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(val) => val,
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Reduce function call failed: {}",
                            err
                        )));
                    }
                };

                if is_reduced(&function_result) {
                    return HotResult::Ok(unwrap_reduced(function_result));
                }

                accumulator = function_result;
            }

            HotResult::Ok(accumulator)
        }
        Val::Map(map) => {
            let mut accumulator = init_val.clone();

            for (key, value) in map.iter() {
                let pair = Val::Vec(vec![key.clone(), value.clone()]);

                let function_result = match call_prepared_with_vm_multi_args(
                    vm,
                    &function,
                    &[accumulator.clone(), pair],
                )
                .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(val) => val,
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Reduce function call failed: {}",
                            err
                        )));
                    }
                };

                if is_reduced(&function_result) {
                    return HotResult::Ok(unwrap_reduced(function_result));
                }

                accumulator = function_result;
            }

            HotResult::Ok(accumulator)
        }
        Val::Box(boxed) => {
            // Check if it's an iterator
            if let Some(iter_box) = boxed
                .as_any()
                .downcast_ref::<crate::lang::hot::iter::IteratorBox>()
            {
                let mut accumulator = init_val.clone();

                // Iterate over all values from the iterator
                loop {
                    let (value, done) = {
                        let mut guard = match iter_box.inner.lock() {
                            Ok(g) => g,
                            Err(e) => {
                                return HotResult::Err(Val::from(format!(
                                    "Reduce: failed to lock iterator: {}",
                                    e
                                )));
                            }
                        };
                        match guard.next() {
                            Ok(r) => r,
                            Err(e) => {
                                return HotResult::Err(Val::from(format!(
                                    "Reduce: iterator error: {}",
                                    e
                                )));
                            }
                        }
                    };

                    if done {
                        break;
                    }

                    let function_result = match call_prepared_with_vm_multi_args(
                        vm,
                        &function,
                        &[accumulator.clone(), value],
                    )
                    .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                    {
                        Ok(val) => val,
                        Err(err) => {
                            return HotResult::Err(Val::from(format!(
                                "Reduce function call failed: {}",
                                err
                            )));
                        }
                    };

                    if is_reduced(&function_result) {
                        return HotResult::Ok(unwrap_reduced(function_result));
                    }

                    accumulator = function_result;
                }

                HotResult::Ok(accumulator)
            } else {
                HotResult::Err(Val::from(
                    "Reduce requires a collection (Vec, Map, or Iter)".to_string(),
                ))
            }
        }
        _ => HotResult::Err(Val::from(
            "Reduce requires a collection (Vec, Map, or Iter)".to_string(),
        )),
    }
}

/// Check if any element in the collection satisfies the predicate (VM-aware version)
pub fn some(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/some expects 2 arguments, got {}",
            args.len()
        )));
    }

    let collection = &args[0];
    let predicate_val = &args[1];

    tracing::debug!(
        "VM: some_vm_aware called with collection: {:?}, predicate: {:?}",
        collection,
        predicate_val
    );

    // Special debug for calls from is-test
    if matches!(collection, Val::Null) {
        tracing::debug!(
            "VM: some_vm_aware received NULL collection - this suggests a variable scoping issue"
        );
    }

    // Handle null collection - return false
    if matches!(collection, Val::Null) {
        tracing::debug!("VM: some_vm_aware - collection is null, returning false");
        return HotResult::Ok(Val::Bool(false));
    }

    let predicate = prepare_function(predicate_val);
    match collection {
        Val::Vec(items) => {
            // Apply predicate to each item and return true if any is truthy
            for item in items {
                // Call the predicate with the item using VM context
                let predicate_result = match call_prepared_with_vm(vm, &predicate, item)
                    .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(val) => val,
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Some predicate call failed: {}",
                            err
                        )));
                    }
                };

                // Check if the result is truthy
                if is_truthy(&predicate_result) {
                    return HotResult::Ok(Val::Bool(true));
                }
            }

            HotResult::Ok(Val::Bool(false))
        }
        Val::Map(map) => {
            // Apply predicate to each key-value pair and return true if any is truthy
            for (key, value) in map.iter() {
                let pair = Val::Vec(vec![key.clone(), value.clone()]);

                // Call the predicate with the key-value pair using VM context
                let predicate_result = match call_prepared_with_vm(vm, &predicate, &pair)
                    .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(val) => val,
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Some predicate call failed: {}",
                            err
                        )));
                    }
                };

                // Check if the result is truthy
                if is_truthy(&predicate_result) {
                    return HotResult::Ok(Val::Bool(true));
                }
            }

            HotResult::Ok(Val::Bool(false))
        }
        Val::Box(boxed) => {
            if let Some(iter_box) = boxed
                .as_any()
                .downcast_ref::<crate::lang::hot::iter::IteratorBox>()
            {
                loop {
                    let (value, done) = {
                        let mut guard = match iter_box.inner.lock() {
                            Ok(g) => g,
                            Err(e) => {
                                return HotResult::Err(Val::from(format!(
                                    "some: failed to lock iterator: {}",
                                    e
                                )));
                            }
                        };
                        match guard.next() {
                            Ok(r) => r,
                            Err(e) => {
                                return HotResult::Err(Val::from(format!(
                                    "some: iterator error: {}",
                                    e
                                )));
                            }
                        }
                    };

                    if done {
                        break;
                    }

                    let predicate_result = match call_prepared_with_vm(vm, &predicate, &value)
                        .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                    {
                        Ok(val) => val,
                        Err(err) => {
                            return HotResult::Err(Val::from(format!(
                                "Some predicate call failed: {}",
                                err
                            )));
                        }
                    };

                    if is_truthy(&predicate_result) {
                        return HotResult::Ok(Val::Bool(true));
                    }
                }

                HotResult::Ok(Val::Bool(false))
            } else {
                HotResult::Err(Val::from("some requires a collection (Vec, Map, or Iter)"))
            }
        }
        _ => HotResult::Err(Val::from("some requires a collection (Vec, Map, or Iter)")),
    }
}

/// Helper function to check if a value is truthy (the language-wide
/// Val::is_truthy semantics: only false, null, and Err are falsy).
fn is_truthy(val: &Val) -> bool {
    val.is_truthy()
}

/// Check if a value is a Reduced wrapper (short-circuit signal for reduce).
fn is_reduced(val: &Val) -> bool {
    if let Val::Map(m) = val
        && let Some(Val::Str(tn)) = m.get(&Val::from("$type"))
    {
        return &**tn == "::hot::coll/Reduced";
    }
    false
}

/// Unwrap a Reduced value, extracting the inner value from $val.value.
fn unwrap_reduced(val: Val) -> Val {
    if let Val::Map(m) = &val
        && let Some(Val::Map(inner_map)) = m.get(&Val::from("$val"))
        && let Some(inner) = inner_map.get(&Val::from("value"))
    {
        return inner.clone();
    }
    val
}

/// Find the first element in a collection that satisfies a predicate (short-circuits).
pub fn find_first(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/find_first expects 2 arguments, got {}",
            args.len()
        )));
    }

    let collection = &args[0];
    let predicate_val = &args[1];

    if matches!(collection, Val::Null) {
        return HotResult::Ok(Val::Null);
    }

    match collection {
        Val::Vec(items) => {
            for item in items {
                let predicate_result = match call_function_with_vm(vm, predicate_val, item)
                    .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(val) => val,
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "find-first predicate call failed: {}",
                            err
                        )));
                    }
                };

                if is_truthy(&predicate_result) {
                    return HotResult::Ok(item.clone());
                }
            }

            HotResult::Ok(Val::Null)
        }
        Val::Map(map) => {
            for (key, value) in map.iter() {
                let pair = Val::Vec(vec![key.clone(), value.clone()]);

                let predicate_result = match call_function_with_vm(vm, predicate_val, &pair)
                    .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(val) => val,
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "find-first predicate call failed: {}",
                            err
                        )));
                    }
                };

                if is_truthy(&predicate_result) {
                    return HotResult::Ok(pair);
                }
            }

            HotResult::Ok(Val::Null)
        }
        Val::Box(boxed) => {
            if let Some(iter_box) = boxed
                .as_any()
                .downcast_ref::<crate::lang::hot::iter::IteratorBox>()
            {
                loop {
                    let (value, done) = {
                        let mut guard = match iter_box.inner.lock() {
                            Ok(g) => g,
                            Err(e) => {
                                return HotResult::Err(Val::from(format!(
                                    "find-first: failed to lock iterator: {}",
                                    e
                                )));
                            }
                        };
                        match guard.next() {
                            Ok(r) => r,
                            Err(e) => {
                                return HotResult::Err(Val::from(format!(
                                    "find-first: iterator error: {}",
                                    e
                                )));
                            }
                        }
                    };

                    if done {
                        break;
                    }

                    let predicate_result = match call_function_with_vm(vm, predicate_val, &value)
                        .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                    {
                        Ok(val) => val,
                        Err(err) => {
                            return HotResult::Err(Val::from(format!(
                                "find-first predicate call failed: {}",
                                err
                            )));
                        }
                    };

                    if is_truthy(&predicate_result) {
                        return HotResult::Ok(value);
                    }
                }

                HotResult::Ok(Val::Null)
            } else {
                HotResult::Err(Val::from(
                    "find-first requires a collection (Vec, Map, or Iter)",
                ))
            }
        }
        _ => HotResult::Err(Val::from(
            "find-first requires a collection (Vec, Map, or Iter)",
        )),
    }
}

/// Apply a function to each element for its side effects; results are
/// forced through the strict-argument law (a callback Err halts, matching
/// `map`'s default) and then discarded. Accepts Vec, Map, Str, or Iter —
/// iterators are pulled lazily, so infinite sources work with `take`-style
/// callbacks that stop externally.
pub fn for_each(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::iter/for-each expects 2 arguments, got {}",
            args.len()
        )));
    }

    let collection = &args[0];
    let function_val = &args[1];

    if matches!(collection, Val::Null) {
        return HotResult::Ok(Val::Null);
    }

    let run = |vm: &mut crate::lang::runtime::vm::VirtualMachine, item: &Val| -> Result<(), Val> {
        match call_function_with_vm(vm, function_val, item)
            .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
        {
            Ok(_) => Ok(()),
            Err(err) => Err(Val::from(format!("for-each function call failed: {}", err))),
        }
    };

    match collection {
        Val::Vec(items) => {
            for item in items {
                if let Err(e) = run(vm, item) {
                    return HotResult::Err(e);
                }
            }
            HotResult::Ok(Val::Null)
        }
        Val::Map(map) => {
            for (key, value) in map.iter() {
                let pair = Val::Vec(vec![key.clone(), value.clone()]);
                if let Err(e) = run(vm, &pair) {
                    return HotResult::Err(e);
                }
            }
            HotResult::Ok(Val::Null)
        }
        Val::Str(s) => {
            for ch in s.chars() {
                let char_val = Val::from(ch.to_string());
                if let Err(e) = run(vm, &char_val) {
                    return HotResult::Err(e);
                }
            }
            HotResult::Ok(Val::Null)
        }
        Val::Box(boxed) => {
            if let Some(iter_box) = boxed
                .as_any()
                .downcast_ref::<crate::lang::hot::iter::IteratorBox>()
            {
                loop {
                    let (value, done) = {
                        let mut guard = match iter_box.inner.lock() {
                            Ok(g) => g,
                            Err(e) => {
                                return HotResult::Err(Val::from(format!(
                                    "for-each: failed to lock iterator: {}",
                                    e
                                )));
                            }
                        };
                        match guard.next() {
                            Ok(r) => r,
                            Err(e) => {
                                return HotResult::Err(Val::from(format!(
                                    "for-each: iterator error: {}",
                                    e
                                )));
                            }
                        }
                    };

                    if done {
                        break;
                    }

                    if let Err(e) = run(vm, &value) {
                        return HotResult::Err(e);
                    }
                }

                HotResult::Ok(Val::Null)
            } else {
                HotResult::Err(Val::from(
                    "for-each requires a collection (Vec, Map, Str, or Iter)",
                ))
            }
        }
        _ => HotResult::Err(Val::from(
            "for-each requires a collection (Vec, Map, Str, or Iter)",
        )),
    }
}

/// Parallel map function with TRUE parallel execution using tokio
pub fn pmap(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    let disposition = match parse_onerr_disposition("::hot::coll/pmap", args, 2) {
        Ok(disposition) => disposition,
        Err(err) => return HotResult::Err(err),
    };

    let collection = &args[0];
    let function_val = &args[1];
    let thread_count = vm.get_thread_count();

    tracing::debug!(
        "VM: pmap called with thread_count = {}, executing in parallel",
        thread_count
    );

    // Check if we're in a tokio runtime
    let has_runtime = tokio::runtime::Handle::try_current().is_ok();

    // pmap shares `map`'s failure contract: unhandled `fail()` / `cancel()`
    // in a per-item call propagates. The only difference is that pmap
    // first lets in-flight parallel work complete before propagating,
    // and always propagates the lowest-indexed halt so the observed
    // failure is deterministic regardless of scheduling.
    if !has_runtime {
        tracing::debug!("pmap: No tokio runtime available, running sequentially");
        return match collection {
            Val::Vec(items) => pmap_vec(vm, function_val, items, thread_count, disposition),
            Val::Map(map_val) => pmap_map(vm, function_val, map_val, thread_count, disposition),
            Val::Str(s) => pmap_str(vm, function_val, s, thread_count, disposition),
            _ => HotResult::Err(Val::from(format!(
                "::hot::coll/pmap expects collection (Vec, Map, or Str), got {:?}",
                collection
            ))),
        };
    }

    match collection {
        Val::Vec(items) => pmap_vec(vm, function_val, items, thread_count, disposition),
        Val::Map(map_val) => pmap_map(vm, function_val, map_val, thread_count, disposition),
        Val::Str(s) => pmap_str(vm, function_val, s, thread_count, disposition),
        _ => HotResult::Err(Val::from(format!(
            "::hot::coll/pmap expects collection (Vec, Map, or Str), got {:?}",
            collection
        ))),
    }
}

#[derive(Clone)]
enum PmapHaltKind {
    Fail,
    Cancel,
}

#[derive(Clone)]
struct PmapHalt {
    msg: String,
    data: Val,
    kind: PmapHaltKind,
}

/// Parallel map over vector
fn pmap_vec(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    function_val: &Val,
    items: &[Val],
    thread_count: usize,
    disposition: OnErrDisposition,
) -> HotResult<Val> {
    if items.is_empty() {
        return HotResult::Ok(Val::Vec(vec![]));
    }

    // For small collections, just use sequential map (halt-propagating).
    if items.len() < thread_count * 2 {
        let onerr = match disposition {
            OnErrDisposition::Force => onerr_variant_val("Force"),
            OnErrDisposition::Preserve => onerr_variant_val("Preserve"),
        };
        return map(vm, &[Val::Vec(items.to_vec()), function_val.clone(), onerr]);
    }

    // Calculate chunk size
    let chunk_size = items.len().div_ceil(thread_count);

    // Capture VM state needed for parallel execution
    let program = vm.program.clone();
    let hot_ast = vm.get_hot_ast_arc();
    let function_mapping = vm.get_function_mapping_arc();
    let core_functions = vm.get_core_functions_arc();
    let type_implementations = vm.get_type_implementations_arc();
    let core_variables = vm.get_core_variables_arc();
    let conf = vm.get_conf().clone();
    let function_val_clone = function_val.clone();

    // Worker VMs need the same realized namespace state as the parent because
    // callbacks can call package functions that switch namespaces and read
    // package-level constants.
    let namespace_snapshot = vm.namespace_variables_snapshot();
    let current_namespace = vm.get_current_namespace().to_string();

    // pmap mirrors `map`'s failure contract with a batch-completion twist:
    //   * Each worker runs its chunk; if a per-item call halts via `fail()`
    //     or `cancel()`, the worker stops that chunk early and records the
    //     halt (kind + msg + data + local index).
    //   * Other in-flight workers continue processing their own chunks so
    //     parallel work is not silently discarded.
    //   * After all workers join, the lowest-input-index halt (chunk
    //     order, then within-chunk order) is propagated on the parent VM
    //     via `set_failure` / `set_cancellation`, and we return
    //     HotResult::Err. The user observes the same halt they would
    //     have observed from a sequential `map`.
    //   * If no halt occurred, results are flattened in input order and
    //     returned as a Vec. Per-item `Result.Err` *values* (e.g. the
    //     callback explicitly returned a `Result.Err` via `if-err`)
    //     survive in the output just like `map`.
    //
    // To recover per item, the callback should use `if-err` (or
    // `try-call`) internally — same idiom as `map`.

    // Spawn parallel threads (using std::thread for true CPU parallelism).
    // We use parking_lot::Mutex so a panic inside a worker thread can't poison
    // the shared error/results slots and force the parent into a failure path.
    use parking_lot::Mutex;
    use std::sync::Arc as StdArc;
    use std::thread;

    // Capture Tokio runtime handle so spawned threads can use async functions
    let tokio_handle = tokio::runtime::Handle::try_current().ok();
    let host_context = vm.host_context_snapshot();

    // Store results with chunk index to preserve order. Each entry is
    // (chunk_idx, values_produced_before_halt, optional halt).
    type ChunkResult = (usize, Vec<Val>, Option<PmapHalt>);
    let results_mutex: StdArc<Mutex<Vec<ChunkResult>>> =
        StdArc::new(Mutex::new(Vec::with_capacity(thread_count)));
    let error_mutex: StdArc<Mutex<Option<String>>> = StdArc::new(Mutex::new(None));

    let mut join_handles = Vec::new();
    for (chunk_idx, chunk) in items.chunks(chunk_size).enumerate() {
        let chunk_vec = chunk.to_vec();
        let program_clone = program.clone();
        let hot_ast_clone = hot_ast.clone();
        let function_mapping_clone = function_mapping.clone();
        let core_functions_clone = core_functions.clone();
        let type_implementations_clone = type_implementations.clone();
        let core_variables_clone = core_variables.clone();
        let conf_clone = Some(conf.clone());
        let function_clone = function_val_clone.clone();
        let namespace_snapshot_clone = namespace_snapshot.clone();
        let namespace_clone = current_namespace.clone();
        let results_clone = results_mutex.clone();
        let error_clone = error_mutex.clone();
        let tokio_handle_clone = tokio_handle.clone();
        let host_context_clone = host_context.clone();

        // These workers run the VM, which may execute JIT-compiled code. The
        // JIT stack-overflow guard is calibrated for a large stack (see
        // `VM_THREAD_STACK_SIZE`); the default `thread::spawn` stack (~2 MiB) is
        // far smaller than the guard budget, which would let recursive JIT code
        // overrun the real stack and segfault. Spawn with an explicit large
        // stack so the guard is effective.
        let worker_body = move || {
            // Enter Tokio runtime context so async functions work on spawned threads
            let _tokio_guard = tokio_handle_clone.as_ref().map(|h| h.enter());

            // Create a new VM for this thread with its own private failure
            // state. Halts in this worker do not affect the parent or
            // sibling workers until we surface them in the join phase.
            let mut task_vm = crate::lang::runtime::vm::VirtualMachine::new(
                program_clone,
                hot_ast_clone,
                function_mapping_clone,
                core_functions_clone,
                type_implementations_clone,
                core_variables_clone,
                conf_clone,
            );
            task_vm.inherit_host_context(&host_context_clone);
            task_vm.inherit_namespace_variables(&namespace_snapshot_clone);
            task_vm.set_current_namespace(namespace_clone);

            let mut chunk_results: Vec<Val> = Vec::with_capacity(chunk_vec.len());
            let mut chunk_halt: Option<PmapHalt> = None;
            for item in chunk_vec.iter() {
                match call_function_with_vm(&mut task_vm, &function_clone, item)
                    .and_then(|val| apply_onerr_disposition(&mut task_vm, val, disposition))
                {
                    Ok(val) => chunk_results.push(val),
                    Err(err) => {
                        if task_vm.has_failed() {
                            let f = task_vm.get_failure().unwrap_or(FailureState {
                                msg: "pmap item halted".to_string(),
                                data: Val::Null,
                            });
                            task_vm.reset_failure_state();
                            chunk_halt = Some(PmapHalt {
                                msg: f.msg,
                                data: f.data,
                                kind: PmapHaltKind::Fail,
                            });
                            break;
                        } else if task_vm.has_cancelled() {
                            let c = task_vm.get_cancellation().unwrap_or(CancellationState {
                                msg: "pmap item cancelled".to_string(),
                                data: Val::Null,
                            });
                            task_vm.reset_cancellation_state();
                            chunk_halt = Some(PmapHalt {
                                msg: c.msg,
                                data: c.data,
                                kind: PmapHaltKind::Cancel,
                            });
                            break;
                        } else {
                            // Non-halt error (e.g. argument mismatch). Record
                            // once; other workers continue but this will be
                            // surfaced over results in the join phase.
                            let mut error = error_clone.lock();
                            if error.is_none() {
                                *error = Some(format!("pmap function call failed: {}", err));
                            }
                            return;
                        }
                    }
                }
            }

            let mut results = results_clone.lock();
            results.push((chunk_idx, chunk_results, chunk_halt));
        };

        let handle = match thread::Builder::new()
            .name(format!("pmap-w{}", chunk_idx))
            .stack_size(crate::lang::runtime::jit::VM_THREAD_STACK_SIZE)
            .spawn(worker_body)
        {
            Ok(handle) => handle,
            Err(e) => {
                return HotResult::Err(Val::from(format!(
                    "pmap: failed to spawn worker thread: {}",
                    e
                )));
            }
        };

        join_handles.push(handle);
    }

    // Wait for all threads to complete (batch completion).
    for handle in join_handles {
        handle.join().ok();
    }

    // Non-halt errors take precedence over halts and over results.
    if let Some(error_msg) = error_mutex.lock().as_ref() {
        return HotResult::Err(Val::from(error_msg.clone()));
    }

    // Sort chunks by input order, then walk in order to find the
    // lowest-index halt (if any). Propagate that halt on the parent VM.
    let mut all_results = std::mem::take(&mut *results_mutex.lock());
    all_results.sort_by_key(|(idx, _, _)| *idx);

    for (_, _values, halt) in &all_results {
        if let Some(h) = halt {
            match h.kind {
                PmapHaltKind::Fail => {
                    vm.set_failure(h.msg.clone(), h.data.clone());
                }
                PmapHaltKind::Cancel => {
                    vm.set_cancellation(h.msg.clone(), h.data.clone());
                }
            }
            return HotResult::Err(Val::from(h.msg.clone()));
        }
    }

    let flattened: Vec<Val> = all_results
        .into_iter()
        .flat_map(|(_, chunk, _)| chunk)
        .collect();
    HotResult::Ok(Val::Vec(flattened))
}

/// Parallel map over map (for now, delegate to sequential)
fn pmap_map(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    function_val: &Val,
    map_val: &IndexMap<Val, Val>,
    _thread_count: usize,
    disposition: OnErrDisposition,
) -> HotResult<Val> {
    // Map over maps is more complex, use sequential for now
    let mut results = Vec::new();
    for (key, value) in map_val.iter() {
        match call_function_with_vm_multi_args(vm, function_val, &[key.clone(), value.clone()])
            .and_then(|val| apply_onerr_disposition(vm, val, disposition))
        {
            Ok(val) => results.push(val),
            Err(err) => {
                return HotResult::Err(Val::from(format!("pmap function call failed: {}", err)));
            }
        }
    }
    HotResult::Ok(Val::Vec(results))
}

/// Parallel map over string (for now, delegate to sequential)
fn pmap_str(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    function_val: &Val,
    s: &str,
    _thread_count: usize,
    disposition: OnErrDisposition,
) -> HotResult<Val> {
    // String mapping is less common, use sequential
    let mut results = Vec::new();
    for ch in s.chars() {
        let ch_val = Val::from(ch.to_string());
        match call_function_with_vm(vm, function_val, &ch_val)
            .and_then(|val| apply_onerr_disposition(vm, val, disposition))
        {
            Ok(val) => results.push(val),
            Err(err) => {
                return HotResult::Err(Val::from(format!("pmap function call failed: {}", err)));
            }
        }
    }
    HotResult::Ok(Val::Vec(results))
}

/// Map with index - applies function to each element with its index
pub fn map_indexed(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    let disposition = match parse_onerr_disposition("::hot::coll/map-indexed", args, 2) {
        Ok(disposition) => disposition,
        Err(err) => return HotResult::Err(err),
    };

    // Enforce coll-first ordering: (coll, func)
    let collection = &args[0];
    let function_val = &args[1];

    tracing::debug!(
        "VM: map_indexed called with collection: {:?}, function: {:?}",
        collection,
        function_val
    );

    match collection {
        Val::Vec(items) => {
            let mut result = Vec::new();
            for (index, item) in items.iter().enumerate() {
                let index_val = Val::Int(index as i64);
                match call_function_with_vm_multi_args(vm, function_val, &[index_val, item.clone()])
                    .and_then(|val| apply_onerr_disposition(vm, val, disposition))
                {
                    Ok(val) => result.push(val),
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Map-indexed function call failed: {}",
                            err
                        )));
                    }
                }
            }
            HotResult::Ok(Val::Vec(result))
        }
        Val::Map(map) => {
            let mut result_vec = Vec::new();
            for (index, (key, value)) in map.iter().enumerate() {
                let index_val = Val::Int(index as i64);
                match call_function_with_vm_multi_args(
                    vm,
                    function_val,
                    &[index_val, key.clone(), value.clone()],
                )
                .and_then(|val| apply_onerr_disposition(vm, val, disposition))
                {
                    Ok(val) => {
                        result_vec.push(val);
                    }
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Map-indexed function call failed: {}",
                            err
                        )));
                    }
                }
            }
            HotResult::Ok(Val::Vec(result_vec))
        }
        Val::Str(s) => {
            let mut result = Vec::new();
            for (index, ch) in s.chars().enumerate() {
                let index_val = Val::Int(index as i64);
                let char_val = Val::from(ch.to_string());
                match call_function_with_vm_multi_args(vm, function_val, &[index_val, char_val])
                    .and_then(|val| apply_onerr_disposition(vm, val, disposition))
                {
                    Ok(val) => result.push(val),
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Map-indexed function call failed: {}",
                            err
                        )));
                    }
                }
            }
            HotResult::Ok(Val::Vec(result))
        }
        _ => HotResult::Err(Val::from("map-indexed requires a collection")),
    }
}

/// Remove elements from a collection based on a predicate
pub fn remove(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/remove expects 2 arguments, got {}",
            args.len()
        )));
    }

    // Enforce coll-first ordering: (coll, predicate)
    let collection = &args[0];
    let predicate_val = &args[1];

    tracing::debug!(
        "VM: remove_vm_aware called with predicate: {:?}, collection: {:?}",
        predicate_val,
        collection
    );

    match collection {
        Val::Vec(items) => {
            let mut result = Vec::new();
            for item in items {
                match call_function_with_vm(vm, predicate_val, item)
                    .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(predicate_result) => {
                        // Remove means keep items where predicate is false
                        if !is_truthy(&predicate_result) {
                            result.push(item.clone());
                        }
                    }
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Remove predicate call failed: {}",
                            err
                        )));
                    }
                }
            }
            HotResult::Ok(Val::Vec(result))
        }
        Val::Map(map) => {
            let mut result_map = indexmap::IndexMap::new();
            for (key, value) in map.iter() {
                match call_function_with_vm_multi_args(
                    vm,
                    predicate_val,
                    &[key.clone(), value.clone()],
                )
                .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(predicate_result) => {
                        // Remove means keep items where predicate is false
                        if !is_truthy(&predicate_result) {
                            result_map.insert(key.clone(), value.clone());
                        }
                    }
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Remove predicate call failed: {}",
                            err
                        )));
                    }
                }
            }
            HotResult::Ok(Val::Map(Box::new(result_map)))
        }
        Val::Str(s) => {
            let mut out = String::new();
            for ch in s.chars() {
                let ch_val = Val::from(ch.to_string());
                match call_function_with_vm(vm, predicate_val, &ch_val)
                    .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(predicate_result) => {
                        if !is_truthy(&predicate_result) {
                            out.push(ch);
                        }
                    }
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Remove predicate call failed: {}",
                            err
                        )));
                    }
                }
            }
            HotResult::Ok(Val::from(out))
        }
        _ => HotResult::Err(Val::from("remove requires a collection")),
    }
}

/// Delete a key from a map or index from a vector
pub fn delete(args: &[Val]) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/delete expects 2 arguments, got {}",
            args.len()
        )));
    }

    let collection = &args[0];
    let key_or_index = &args[1];

    match (collection, key_or_index) {
        (Val::Str(s), Val::Int(target_index)) => {
            let chars: Vec<char> = s.chars().collect();
            let len = chars.len() as i64;
            let actual_index = if *target_index < 0 {
                len + *target_index
            } else {
                *target_index
            };
            if actual_index < 0 || actual_index >= len {
                HotResult::Ok(Val::from(s.clone()))
            } else {
                let mut result_chars = chars;
                result_chars.remove(actual_index as usize);
                HotResult::Ok(Val::from(result_chars.into_iter().collect::<String>()))
            }
        }
        (Val::Vec(items), Val::Int(target_index)) => {
            let len = items.len() as i64;
            let actual_index = if *target_index < 0 {
                len + *target_index
            } else {
                *target_index
            };
            if actual_index < 0 || actual_index >= len {
                HotResult::Ok(Val::Vec(items.clone()))
            } else {
                let mut result_items = items.clone();
                result_items.remove(actual_index as usize);
                HotResult::Ok(Val::Vec(result_items))
            }
        }
        (Val::Map(map), Val::Str(target_key)) => {
            let mut result_map = (**map).clone(); // Clone the inner IndexMap
            result_map.shift_remove(&Val::from(target_key.clone()));
            HotResult::Ok(Val::Map(Box::new(result_map)))
        }
        (Val::Map(map), _) => HotResult::Ok(Val::Map(map.clone())), // map is Box, clone returns Box
        (Val::Str(s), _) => HotResult::Ok(Val::from(s.clone())),
        (Val::Vec(items), _) => HotResult::Ok(Val::Vec(items.clone())),
        _ => HotResult::Err(Val::from("delete requires a map or vector")),
    }
}

/// Get all elements except the first one (rest)
pub fn rest(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/rest expects 1 argument, got {}",
            args.len()
        )));
    }

    match &args[0] {
        Val::Vec(vec) => {
            if vec.is_empty() {
                HotResult::Ok(Val::Vec(Vec::new()))
            } else {
                HotResult::Ok(Val::Vec(vec[1..].to_vec()))
            }
        }
        Val::Str(s) => {
            if s.is_empty() {
                HotResult::Ok(Val::from(""))
            } else {
                let chars: Vec<char> = s.chars().collect();
                if chars.len() > 1 {
                    HotResult::Ok(Val::from(chars[1..].iter().collect::<String>()))
                } else {
                    HotResult::Ok(Val::from(""))
                }
            }
        }
        Val::Null => HotResult::Ok(Val::Vec(Vec::new())),
        _ => HotResult::Err(Val::from("rest requires a collection")),
    }
}

/// Get all elements except the last one (butlast)
pub fn butlast(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/butlast expects 1 argument, got {}",
            args.len()
        )));
    }

    match &args[0] {
        Val::Vec(vec) => {
            if vec.is_empty() {
                HotResult::Ok(Val::Vec(Vec::new()))
            } else {
                HotResult::Ok(Val::Vec(vec[..vec.len() - 1].to_vec()))
            }
        }
        Val::Str(s) => {
            if s.is_empty() {
                HotResult::Ok(Val::from(""))
            } else {
                let chars: Vec<char> = s.chars().collect();
                if chars.len() > 1 {
                    HotResult::Ok(Val::from(
                        chars[..chars.len() - 1].iter().collect::<String>(),
                    ))
                } else {
                    HotResult::Ok(Val::from(""))
                }
            }
        }
        Val::Null => HotResult::Ok(Val::Vec(Vec::new())),
        _ => HotResult::Err(Val::from("butlast requires a collection")),
    }
}

/// Check if a collection is empty
pub fn is_empty(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/is-empty expects 1 argument, got {}",
            args.len()
        )));
    }

    match &args[0] {
        Val::Vec(vec) => HotResult::Ok(Val::Bool(vec.is_empty())),
        Val::Map(map) => HotResult::Ok(Val::Bool(map.is_empty())),
        Val::Str(s) => HotResult::Ok(Val::Bool(s.is_empty())),
        Val::Null => HotResult::Ok(Val::Bool(true)),
        _ => HotResult::Err(Val::from("is-empty requires a collection")),
    }
}

/// Partition a collection into two parts based on a predicate
pub fn partition(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/partition expects 2 arguments, got {}",
            args.len()
        )));
    }

    let a0 = &args[0];
    let a1 = &args[1];

    tracing::debug!(
        "VM: partition_vm_aware called with args: {:?}, {:?}",
        a0,
        a1
    );

    // Overload 1: numeric size partitioning (chunking) - coll-first only
    if let (Val::Vec(_) | Val::Map(_) | Val::Str(_), Val::Int(n)) = (a0, a1) {
        let collection = a0;
        let chunk_size = (*n) as usize;
        if chunk_size == 0 {
            return HotResult::Err(Val::from("partition size must be > 0"));
        }

        match collection {
            Val::Vec(items) => {
                let mut result: Vec<Val> = Vec::new();
                let mut idx = 0usize;
                while idx < items.len() {
                    let end = std::cmp::min(idx + chunk_size, items.len());
                    let chunk = Val::Vec(items[idx..end].to_vec());
                    result.push(chunk);
                    idx = end;
                }
                return HotResult::Ok(Val::Vec(result));
            }
            Val::Str(s) => {
                let chars: Vec<Val> = s.chars().map(|c| Val::from(c.to_string())).collect();
                let mut result: Vec<Val> = Vec::new();
                let mut idx = 0usize;
                while idx < chars.len() {
                    let end = std::cmp::min(idx + chunk_size, chars.len());
                    let chunk = Val::Vec(chars[idx..end].to_vec());
                    result.push(chunk);
                    idx = end;
                }
                return HotResult::Ok(Val::Vec(result));
            }
            Val::Map(map) => {
                let mut result: Vec<Val> = Vec::new();
                let mut current = indexmap::IndexMap::new();
                for (i, (k, v)) in map.iter().enumerate() {
                    current.insert(k.clone(), v.clone());
                    if (i + 1) % chunk_size == 0 {
                        result.push(Val::Map(Box::new(std::mem::take(&mut current))));
                    }
                }
                if !current.is_empty() {
                    result.push(Val::Map(Box::new(current)));
                }
                return HotResult::Ok(Val::Vec(result));
            }
            _ => {
                return HotResult::Err(Val::from("partition requires a collection"));
            }
        }
    }

    // Overload 2: predicate-based boolean partitioning - coll-first only
    let collection = a0;
    let predicate_val = a1;

    match collection {
        Val::Vec(items) => {
            let mut true_items = Vec::new();
            let mut false_items = Vec::new();

            for item in items {
                match call_function_with_vm(vm, predicate_val, item)
                    .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(predicate_result) => {
                        if is_truthy(&predicate_result) {
                            true_items.push(item.clone());
                        } else {
                            false_items.push(item.clone());
                        }
                    }
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Partition predicate call failed: {}",
                            err
                        )));
                    }
                }
            }

            // Return [true_items, false_items]
            HotResult::Ok(Val::Vec(vec![Val::Vec(true_items), Val::Vec(false_items)]))
        }
        Val::Map(map) => {
            let mut true_map = indexmap::IndexMap::new();
            let mut false_map = indexmap::IndexMap::new();

            for (key, value) in map.iter() {
                let pair = Val::Vec(vec![key.clone(), value.clone()]);
                match call_function_with_vm(vm, predicate_val, &pair)
                    .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(predicate_result) => {
                        if is_truthy(&predicate_result) {
                            true_map.insert(key.clone(), value.clone());
                        } else {
                            false_map.insert(key.clone(), value.clone());
                        }
                    }
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Partition predicate call failed: {}",
                            err
                        )));
                    }
                }
            }

            // Return [true_map, false_map]
            HotResult::Ok(Val::Vec(vec![
                Val::Map(Box::new(true_map)),
                Val::Map(Box::new(false_map)),
            ]))
        }
        _ => HotResult::Err(Val::from("partition requires a collection")),
    }
}

/// Partition a collection by grouping consecutive elements that return the same value from a function
pub fn partition_by(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/partition-by expects 2 arguments, got {}",
            args.len()
        )));
    }

    // Enforce coll-first ordering: (coll, func)
    let collection = &args[0];
    let function_val = &args[1];

    tracing::debug!(
        "VM: partition_by_vm_aware called with function: {:?}, collection: {:?}",
        function_val,
        collection
    );

    match collection {
        Val::Vec(items) => {
            if items.is_empty() {
                return HotResult::Ok(Val::Vec(Vec::new()));
            }

            let mut result = Vec::new();
            let mut current_group = Vec::new();
            let mut current_key: Option<Val> = None;

            for item in items {
                match call_function_with_vm(vm, function_val, item)
                    .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(key) => {
                        if let Some(ref prev_key) = current_key {
                            // Compare keys - if different, start new group
                            if !vals_equal(prev_key, &key) && !current_group.is_empty() {
                                result.push(Val::Vec(current_group.clone()));
                                current_group.clear();
                            }
                        }
                        current_group.push(item.clone());
                        current_key = Some(key);
                    }
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Partition-by function call failed: {}",
                            err
                        )));
                    }
                }
            }

            // Add the last group
            if !current_group.is_empty() {
                result.push(Val::Vec(current_group));
            }

            HotResult::Ok(Val::Vec(result))
        }
        _ => HotResult::Err(Val::from("partition-by requires a vector")),
    }
}

/// Helper function to compare two Val instances for equality
fn vals_equal(a: &Val, b: &Val) -> bool {
    match (a, b) {
        (Val::Int(a), Val::Int(b)) => a == b,
        (Val::Str(a), Val::Str(b)) => a == b,
        (Val::Bool(a), Val::Bool(b)) => a == b,
        (Val::Dec(a), Val::Dec(b)) => a == b,
        (Val::Null, Val::Null) => true,
        (Val::Byte(a), Val::Byte(b)) => a == b,
        (Val::Vec(a), Val::Vec(b)) => a == b,
        (Val::Map(a), Val::Map(b)) => a == b,
        (Val::Bytes(a), Val::Bytes(b)) => a == b,
        _ => false,
    }
}

/// Get distinct (unique) elements from a collection
pub fn distinct(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/distinct expects 1 argument, got {}",
            args.len()
        )));
    }

    match &args[0] {
        Val::Vec(items) => {
            let mut seen = AHashSet::new();
            let mut result = Vec::new();

            for item in items {
                // Create a simple hash key for the item
                let key = format!("{:?}", item);
                if seen.insert(key) {
                    result.push(item.clone());
                }
            }

            HotResult::Ok(Val::Vec(result))
        }
        Val::Str(s) => {
            let mut seen = AHashSet::new();
            let mut result = String::new();

            for ch in s.chars() {
                if seen.insert(ch) {
                    result.push(ch);
                }
            }

            HotResult::Ok(Val::from(result))
        }
        _ => HotResult::Err(Val::from("distinct requires a collection")),
    }
}

/// Reverse a collection
pub fn reverse(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/reverse expects 1 argument, got {}",
            args.len()
        )));
    }

    match &args[0] {
        Val::Vec(items) => {
            let mut reversed = items.clone();
            reversed.reverse();
            HotResult::Ok(Val::Vec(reversed))
        }
        Val::Str(s) => {
            let reversed: String = s.chars().rev().collect();
            HotResult::Ok(Val::from(reversed))
        }
        _ => HotResult::Err(Val::from("reverse requires a collection")),
    }
}

/// Sort a collection
pub fn sort(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/sort expects 1 argument, got {}",
            args.len()
        )));
    }

    match &args[0] {
        Val::Vec(items) => {
            let mut sorted = items.clone();
            sorted.sort_by(compare_vals);
            HotResult::Ok(Val::Vec(sorted))
        }
        Val::Str(s) => {
            let mut chars: Vec<char> = s.chars().collect();
            chars.sort();
            let sorted: String = chars.into_iter().collect();
            HotResult::Ok(Val::from(sorted))
        }
        _ => HotResult::Err(Val::from("sort requires a collection")),
    }
}

/// Helper function to compare two Val instances for sorting
fn compare_vals(a: &Val, b: &Val) -> std::cmp::Ordering {
    use std::cmp::Ordering;

    match (a, b) {
        (Val::Int(a), Val::Int(b)) => a.cmp(b),
        (Val::Str(a), Val::Str(b)) => a.cmp(b),
        (Val::Dec(a), Val::Dec(b)) => a.partial_cmp(b).unwrap_or(Ordering::Equal),
        (Val::Bool(a), Val::Bool(b)) => a.cmp(b),
        (Val::Byte(a), Val::Byte(b)) => a.cmp(b),
        // Mixed type comparisons - convert to string for comparison
        _ => format!("{:?}", a).cmp(&format!("{:?}", b)),
    }
}

/// Sort a collection by applying a function to each element
pub fn sort_by(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/sort-by expects 2 arguments, got {}",
            args.len()
        )));
    }

    // Enforce coll-first ordering: (coll, key-fn)
    let collection = &args[0];
    let key_fn = &args[1];

    tracing::debug!(
        "VM: sort_by_vm_aware called with key_fn: {:?}, collection: {:?}",
        key_fn,
        collection
    );

    match collection {
        Val::Vec(items) => {
            // Create pairs of (key, original_item) for sorting
            let mut keyed_items = Vec::new();

            for item in items {
                match call_function_with_vm(vm, key_fn, item)
                    .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(key) => keyed_items.push((key, item.clone())),
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "Sort-by key function call failed: {}",
                            err
                        )));
                    }
                }
            }

            // Sort by the keys
            keyed_items.sort_by(|(key_a, _), (key_b, _)| compare_vals(key_a, key_b));

            // Extract the sorted items
            let sorted_items: Vec<Val> = keyed_items.into_iter().map(|(_, item)| item).collect();

            HotResult::Ok(Val::Vec(sorted_items))
        }
        _ => HotResult::Err(Val::from("sort-by requires a vector")),
    }
}

/// Shuffle a collection randomly
pub fn shuffle(args: &[Val]) -> HotResult<Val> {
    use rand::seq::SliceRandom;
    use rand::thread_rng;

    if args.len() != 1 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/shuffle expects 1 argument, got {}",
            args.len()
        )));
    }

    match &args[0] {
        Val::Vec(items) => {
            let mut shuffled = items.clone();
            let mut rng = thread_rng();
            shuffled.shuffle(&mut rng);
            HotResult::Ok(Val::Vec(shuffled))
        }
        Val::Str(s) => {
            let mut chars: Vec<char> = s.chars().collect();
            let mut rng = thread_rng();
            chars.shuffle(&mut rng);
            let shuffled: String = chars.into_iter().collect();
            HotResult::Ok(Val::from(shuffled))
        }
        _ => HotResult::Err(Val::from("shuffle requires a collection")),
    }
}

/// Interleave multiple collections
pub fn interleave(args: &[Val]) -> HotResult<Val> {
    if args.is_empty() {
        return HotResult::Ok(Val::Vec(Vec::new()));
    }

    // Convert all arguments to vectors for processing
    let mut collections: Vec<Vec<Val>> = Vec::new();

    for arg in args {
        match arg {
            Val::Vec(items) => collections.push(items.clone()),
            Val::Str(s) => {
                let chars: Vec<Val> = s.chars().map(|c| Val::from(c.to_string())).collect();
                collections.push(chars);
            }
            _ => return HotResult::Err(Val::from("interleave requires collections")),
        }
    }

    let mut result = Vec::new();
    let max_len = collections.iter().map(|c| c.len()).max().unwrap_or(0);

    // Interleave by taking one element from each collection in turn
    for i in 0..max_len {
        for collection in &collections {
            if i < collection.len() {
                result.push(collection[i].clone());
            }
        }
    }

    HotResult::Ok(Val::Vec(result))
}

/// Insert a separator between elements of a collection
pub fn interpose(args: &[Val]) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/interpose expects 2 arguments, got {}",
            args.len()
        )));
    }

    // Enforce coll-first ordering: (coll, separator)
    let collection = &args[0];
    let separator = &args[1];

    match collection {
        Val::Vec(items) => {
            if items.is_empty() {
                return HotResult::Ok(Val::Vec(Vec::new()));
            }

            let mut result = Vec::new();
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    result.push(separator.clone());
                }
                result.push(item.clone());
            }
            HotResult::Ok(Val::Vec(result))
        }
        Val::Str(s) => {
            if s.is_empty() {
                return HotResult::Ok(Val::from(""));
            }

            let sep_str: String = match separator {
                Val::Str(sep) => (**sep).to_owned(),
                _ => format!("{:?}", separator),
            };

            let chars: Vec<String> = s.chars().map(|c| c.to_string()).collect();
            let result = chars.join(&sep_str);
            HotResult::Ok(Val::from(result))
        }
        _ => HotResult::Err(Val::from("interpose requires a collection")),
    }
}

/// Flatten nested collections by one level
pub fn flatten(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/flatten expects 1 argument, got {}",
            args.len()
        )));
    }

    match &args[0] {
        Val::Vec(items) => {
            fn flatten_recursive(val: &Val, out: &mut Vec<Val>) {
                match val {
                    Val::Vec(nested) => {
                        for elem in nested {
                            flatten_recursive(elem, out);
                        }
                    }
                    Val::Str(s) => {
                        for ch in s.chars() {
                            out.push(Val::from(ch.to_string()));
                        }
                    }
                    other => out.push(other.clone()),
                }
            }

            let mut result = Vec::new();
            for item in items {
                flatten_recursive(item, &mut result);
            }
            HotResult::Ok(Val::Vec(result))
        }
        _ => HotResult::Err(Val::from("flatten requires a vector")),
    }
}

/// Create a map from two collections (keys and values)
pub fn zipmap(args: &[Val]) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/zipmap expects 2 arguments, got {}",
            args.len()
        )));
    }

    let keys_collection = &args[0];
    let values_collection = &args[1];

    // Convert collections to vectors
    let keys = match keys_collection {
        Val::Vec(items) => items.clone(),
        Val::Str(s) => s.chars().map(|c| Val::from(c.to_string())).collect(),
        _ => return HotResult::Err(Val::from("zipmap keys must be a collection")),
    };

    let values = match values_collection {
        Val::Vec(items) => items.clone(),
        Val::Str(s) => s.chars().map(|c| Val::from(c.to_string())).collect(),
        _ => return HotResult::Err(Val::from("zipmap values must be a collection")),
    };

    let mut result_map = indexmap::IndexMap::new();

    // Zip keys and values together
    for (key, value) in keys.into_iter().zip(values) {
        result_map.insert(key, value);
    }

    HotResult::Ok(Val::Map(Box::new(result_map)))
}

/// Check if all elements in a collection satisfy a predicate
pub fn all(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/all expects 2 arguments, got {}",
            args.len()
        )));
    }

    // Enforce coll-first ordering: (coll, predicate)
    let collection = &args[0];
    let predicate_val = &args[1];

    tracing::debug!(
        "VM: all_vm_aware called with predicate: {:?}, collection: {:?}",
        predicate_val,
        collection
    );

    let predicate = prepare_function(predicate_val);
    match collection {
        Val::Vec(items) => {
            for item in items {
                match call_prepared_with_vm(vm, &predicate, item)
                    .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(predicate_result) => {
                        if !is_truthy(&predicate_result) {
                            return HotResult::Ok(Val::Bool(false));
                        }
                    }
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "All predicate call failed: {}",
                            err
                        )));
                    }
                }
            }
            HotResult::Ok(Val::Bool(true))
        }
        Val::Map(map) => {
            for (key, value) in map.iter() {
                let pair = Val::Vec(vec![key.clone(), value.clone()]);
                match call_prepared_with_vm(vm, &predicate, &pair)
                    .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(predicate_result) => {
                        if !is_truthy(&predicate_result) {
                            return HotResult::Ok(Val::Bool(false));
                        }
                    }
                    Err(err) => {
                        return HotResult::Err(Val::from(format!(
                            "All predicate call failed: {}",
                            err
                        )));
                    }
                }
            }
            HotResult::Ok(Val::Bool(true))
        }
        Val::Box(boxed) => {
            if let Some(iter_box) = boxed
                .as_any()
                .downcast_ref::<crate::lang::hot::iter::IteratorBox>()
            {
                loop {
                    let (value, done) = {
                        let mut guard = match iter_box.inner.lock() {
                            Ok(g) => g,
                            Err(e) => {
                                return HotResult::Err(Val::from(format!(
                                    "all: failed to lock iterator: {}",
                                    e
                                )));
                            }
                        };
                        match guard.next() {
                            Ok(r) => r,
                            Err(e) => {
                                return HotResult::Err(Val::from(format!(
                                    "all: iterator error: {}",
                                    e
                                )));
                            }
                        }
                    };

                    if done {
                        break;
                    }

                    let predicate_result = match call_prepared_with_vm(vm, &predicate, &value)
                        .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                    {
                        Ok(val) => val,
                        Err(err) => {
                            return HotResult::Err(Val::from(format!(
                                "All predicate call failed: {}",
                                err
                            )));
                        }
                    };

                    if !is_truthy(&predicate_result) {
                        return HotResult::Ok(Val::Bool(false));
                    }
                }

                HotResult::Ok(Val::Bool(true))
            } else {
                HotResult::Err(Val::from("all requires a collection (Vec, Map, or Iter)"))
            }
        }
        _ => HotResult::Err(Val::from("all requires a collection (Vec, Map, or Iter)")),
    }
}

/// Extract a slice from a collection
pub fn slice(args: &[Val]) -> HotResult<Val> {
    if args.len() < 2 || args.len() > 3 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/slice expects 2 or 3 arguments, got {}",
            args.len()
        )));
    }

    let collection = &args[0];
    let start = match &args[1] {
        Val::Int(n) => *n as usize,
        _ => return HotResult::Err(Val::from("slice start index must be an integer")),
    };

    let end = if args.len() == 3 {
        match &args[2] {
            Val::Int(n) => Some(*n as usize),
            _ => return HotResult::Err(Val::from("slice end index must be an integer")),
        }
    } else {
        None
    };

    match collection {
        Val::Vec(items) => {
            let len = items.len();
            let start_idx = start.min(len);
            let end_idx = end.unwrap_or(len).min(len);

            if start_idx <= end_idx {
                let sliced = items[start_idx..end_idx].to_vec();
                HotResult::Ok(Val::Vec(sliced))
            } else {
                HotResult::Ok(Val::Vec(Vec::new()))
            }
        }
        Val::Str(s) => {
            let chars: Vec<char> = s.chars().collect();
            let len = chars.len();
            let start_idx = start.min(len);
            let end_idx = end.unwrap_or(len).min(len);

            if start_idx <= end_idx {
                let sliced: String = chars[start_idx..end_idx].iter().collect();
                HotResult::Ok(Val::from(sliced))
            } else {
                HotResult::Ok(Val::from(""))
            }
        }
        Val::Map(map) => {
            let len = map.len();
            let start_idx = start.min(len);
            let end_idx = end.unwrap_or(len).min(len);

            if start_idx <= end_idx {
                let mut result_map = indexmap::IndexMap::new();
                for (i, (k, v)) in map.iter().enumerate() {
                    if i >= start_idx && i < end_idx {
                        result_map.insert(k.clone(), v.clone());
                    }
                }
                HotResult::Ok(Val::Map(Box::new(result_map)))
            } else {
                HotResult::Ok(Val::map_empty())
            }
        }
        Val::Bytes(bytes) => {
            let len = bytes.len();
            let start_idx = start.min(len);
            let end_idx = end.unwrap_or(len).min(len);

            if start_idx <= end_idx {
                let sliced = bytes[start_idx..end_idx].to_vec();
                HotResult::Ok(Val::Bytes(sliced))
            } else {
                HotResult::Ok(Val::Bytes(Vec::new()))
            }
        }
        _ => HotResult::Err(Val::from(
            "slice requires a collection (Vec, Str, Map, or Bytes)".to_string(),
        )),
    }
}

/// Get keys from a map
pub fn keys(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/keys expects 1 argument, got {}",
            args.len()
        )));
    }

    match &args[0] {
        Val::Map(map) => {
            let keys: Vec<Val> = map.keys().cloned().collect();
            HotResult::Ok(Val::Vec(keys))
        }
        Val::Vec(items) => {
            let idxs: Vec<Val> = (0..items.len()).map(|i| Val::Int(i as i64)).collect();
            HotResult::Ok(Val::Vec(idxs))
        }
        Val::Str(s) => {
            let idxs: Vec<Val> = (0..s.chars().count()).map(|i| Val::Int(i as i64)).collect();
            HotResult::Ok(Val::Vec(idxs))
        }
        _ => HotResult::Err(Val::from("keys requires a collection")),
    }
}

/// Get values from a map
pub fn values(args: &[Val]) -> HotResult<Val> {
    if args.len() != 1 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/values expects 1 argument, got {}",
            args.len()
        )));
    }

    match &args[0] {
        Val::Map(map) => {
            let vals: Vec<Val> = map.values().cloned().collect();
            HotResult::Ok(Val::Vec(vals))
        }
        Val::Vec(items) => HotResult::Ok(Val::Vec(items.clone())),
        Val::Str(s) => {
            let chars: Vec<Val> = s.chars().map(|c| Val::from(c.to_string())).collect();
            HotResult::Ok(Val::Vec(chars))
        }
        _ => HotResult::Err(Val::from("values requires a collection")),
    }
}

/// Create a range of numbers
pub fn range(args: &[Val]) -> HotResult<Val> {
    use crate::lang::runtime::limits;
    match args.len() {
        1 => {
            // range(end) -> [0, 1, 2, ..., end-1]
            let end = &args[0];
            match end {
                Val::Int(end_int) => {
                    // Guard: refuse to allocate gigantic vectors. range(2^31)
                    // would otherwise OOM the worker before catch_unwind sees
                    // anything.
                    let n = limits::range_element_count(0, *end_int, 1).unwrap_or(0);
                    if let Err(e) = limits::check_collection_size("range", n) {
                        return HotResult::Err(e);
                    }
                    let mut result = Vec::with_capacity(n);
                    result.extend((0..*end_int).map(Val::Int));
                    HotResult::Ok(Val::Vec(result))
                }
                Val::Dec(end_dec) => {
                    let end_num = end_dec.to_string().parse::<f64>().unwrap_or(0.0);
                    let mut result = Vec::new();
                    let mut current = 0.0;
                    while current < end_num {
                        let current_str = current.to_string();
                        if let Ok(dec_val) = fastnum::D256::from_str(
                            &current_str,
                            fastnum::decimal::Context::default(),
                        ) {
                            result.push(Val::Dec(dec_val));
                        } else {
                            result.push(Val::Dec(fastnum::D256::ZERO));
                        }
                        current += 1.0;
                    }
                    HotResult::Ok(Val::Vec(result))
                }
                _ => HotResult::Err(Val::from("range end must be a number")),
            }
        }
        2 | 3 => {
            // Determine start, end, step
            let start = args[0].clone();
            let end = args[1].clone();
            let step = if args.len() == 3 {
                args[2].clone()
            } else {
                Val::Int(1)
            };

            // Fast path: all Ints
            if let (Val::Int(start_int), Val::Int(end_int), Val::Int(step_int)) =
                (&start, &end, &step)
            {
                if *step_int == 0 {
                    return HotResult::Err(Val::from("range step cannot be zero"));
                }
                // Guard against memory-bombing range arguments before we start
                // allocating. Saturated arithmetic in range_element_count avoids
                // a panic on extreme i64 values.
                let n = limits::range_element_count(*start_int, *end_int, *step_int).unwrap_or(0);
                if let Err(e) = limits::check_collection_size("range", n) {
                    return HotResult::Err(e);
                }
                let mut result = Vec::with_capacity(n);
                let mut current = *start_int;
                if *step_int > 0 {
                    while current < *end_int {
                        result.push(Val::Int(current));
                        current += *step_int;
                    }
                } else {
                    while current > *end_int {
                        result.push(Val::Int(current));
                        current += *step_int;
                    }
                }
                return HotResult::Ok(Val::Vec(result));
            }

            // Decimal mode using f64 stepping and D256 storage
            let start_num = match &start {
                Val::Int(i) => *i as f64,
                Val::Dec(d) => d.to_string().parse::<f64>().unwrap_or(0.0),
                _ => return HotResult::Err(Val::from("range start must be a number")),
            };
            let end_num = match &end {
                Val::Int(i) => *i as f64,
                Val::Dec(d) => d.to_string().parse::<f64>().unwrap_or(0.0),
                _ => return HotResult::Err(Val::from("range end must be a number")),
            };
            let step_num = match &step {
                Val::Int(i) => *i as f64,
                Val::Dec(d) => d.to_string().parse::<f64>().unwrap_or(0.0),
                _ => return HotResult::Err(Val::from("range step must be a number")),
            };
            if step_num == 0.0 {
                return HotResult::Err(Val::from("range step cannot be zero"));
            }

            let mut result = Vec::new();
            let mut count = 0usize;
            if step_num > 0.0 {
                loop {
                    let current = start_num + (count as f64 * step_num);
                    if current >= end_num {
                        break;
                    }
                    let current_str = current.to_string();
                    if let Ok(dec_val) =
                        fastnum::D256::from_str(&current_str, fastnum::decimal::Context::default())
                    {
                        result.push(Val::Dec(dec_val));
                    } else {
                        result.push(Val::Dec(fastnum::D256::ZERO));
                    }
                    count += 1;
                }
            } else {
                loop {
                    let current = start_num + (count as f64 * step_num);
                    if current <= end_num {
                        break;
                    }
                    let current_str = current.to_string();
                    if let Ok(dec_val) =
                        fastnum::D256::from_str(&current_str, fastnum::decimal::Context::default())
                    {
                        result.push(Val::Dec(dec_val));
                    } else {
                        result.push(Val::Dec(fastnum::D256::ZERO));
                    }
                    count += 1;
                }
            }
            HotResult::Ok(Val::Vec(result))
        }
        _ => HotResult::Err(Val::from(format!(
            "::hot::coll/range expects 1, 2, or 3 arguments, got {}",
            args.len()
        ))),
    }
}

/// Walk one level of a data structure: apply `inner` to each child, rebuild
/// the collection, then apply `outer` to the rebuilt value.
pub fn walk(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.len() != 3 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/walk expects 3 arguments, got {}",
            args.len()
        )));
    }

    let inner = &args[0];
    let outer = &args[1];
    let form = &args[2];

    let walked = match form {
        Val::Vec(items) => {
            let mut result = Vec::with_capacity(items.len());
            for item in items {
                match call_function_with_vm(vm, inner, item)
                    .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(value) => result.push(value),
                    Err(err) => return HotResult::Err(Val::from(err)),
                }
            }
            Val::Vec(result)
        }
        Val::Map(map) => {
            let mut result = IndexMap::with_capacity(map.len());
            for (key, value) in map.iter() {
                match call_function_with_vm(vm, inner, value)
                    .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
                {
                    Ok(walked_value) => {
                        result.insert(key.clone(), walked_value);
                    }
                    Err(err) => return HotResult::Err(Val::from(err)),
                }
            }
            Val::Map(Box::new(result))
        }
        _ => form.clone(),
    };

    match call_function_with_vm(vm, outer, &walked)
        .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
    {
        Ok(value) => HotResult::Ok(value),
        Err(err) => HotResult::Err(Val::from(err)),
    }
}

/// Pre-order walk a nested data structure, applying a function to each element
pub fn prewalk(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/prewalk expects 2 arguments, got {}",
            args.len()
        )));
    }

    let function_val = &args[0];
    let data = &args[1];

    fn prewalk_recursive(
        vm: &mut crate::lang::runtime::vm::VirtualMachine,
        function_val: &Val,
        data: &Val,
    ) -> HotResult<Val> {
        // Apply function first (pre-order)
        let transformed = match call_function_with_vm(vm, function_val, data)
            .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
        {
            Ok(val) => val,
            Err(err) => return HotResult::Err(Val::from(err)),
        };

        // Then recurse into children
        match &transformed {
            Val::Vec(items) => {
                let mut result = Vec::new();
                for item in items {
                    match prewalk_recursive(vm, function_val, item) {
                        HotResult::Ok(walked_item) => result.push(walked_item),
                        HotResult::Err(err) => return HotResult::Err(err),
                    }
                }
                HotResult::Ok(Val::Vec(result))
            }
            Val::Map(map) => {
                let mut result_map = indexmap::IndexMap::new();
                for (key, value) in map.iter() {
                    match prewalk_recursive(vm, function_val, value) {
                        HotResult::Ok(walked_value) => {
                            result_map.insert(key.clone(), walked_value);
                        }
                        HotResult::Err(err) => return HotResult::Err(err),
                    }
                }
                HotResult::Ok(Val::Map(Box::new(result_map)))
            }
            _ => HotResult::Ok(transformed),
        }
    }

    prewalk_recursive(vm, function_val, data)
}

/// Post-order walk a nested data structure, applying a function to each element
pub fn postwalk(vm: &mut crate::lang::runtime::vm::VirtualMachine, args: &[Val]) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/postwalk expects 2 arguments, got {}",
            args.len()
        )));
    }

    let function_val = &args[0];
    let data = &args[1];

    fn postwalk_recursive(
        vm: &mut crate::lang::runtime::vm::VirtualMachine,
        function_val: &Val,
        data: &Val,
    ) -> HotResult<Val> {
        // First recurse into children
        let walked = match data {
            Val::Vec(items) => {
                let mut result = Vec::new();
                for item in items {
                    match postwalk_recursive(vm, function_val, item) {
                        HotResult::Ok(walked_item) => result.push(walked_item),
                        HotResult::Err(err) => return HotResult::Err(err),
                    }
                }
                Val::Vec(result)
            }
            Val::Map(map) => {
                let mut result_map = indexmap::IndexMap::new();
                for (key, value) in map.iter() {
                    match postwalk_recursive(vm, function_val, value) {
                        HotResult::Ok(walked_value) => {
                            result_map.insert(key.clone(), walked_value);
                        }
                        HotResult::Err(err) => return HotResult::Err(err),
                    }
                }
                Val::Map(Box::new(result_map))
            }
            _ => data.clone(),
        };

        // Then apply function (post-order)
        match call_function_with_vm(vm, function_val, &walked)
            .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
        {
            Ok(val) => HotResult::Ok(val),
            Err(err) => HotResult::Err(Val::from(err)),
        }
    }

    postwalk_recursive(vm, function_val, data)
}

/// Post-order walk replacing values found in a replacement map
/// using `{old-value: new-value}` substitution
pub fn postwalk_replace(
    _vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    if args.len() != 2 {
        return HotResult::Err(Val::from(format!(
            "::hot::coll/postwalk-replace expects 2 arguments, got {}",
            args.len()
        )));
    }

    let replacement_map = match &args[0] {
        Val::Map(m) => m,
        _ => {
            return HotResult::Err(Val::from(
                "::hot::coll/postwalk-replace: first argument must be a map of {old: new} values"
                    .to_string(),
            ));
        }
    };
    let data = &args[1];

    fn postwalk_replace_recursive(
        replacement_map: &indexmap::IndexMap<Val, Val>,
        data: &Val,
    ) -> Val {
        // First recurse into children (post-order)
        let walked = match data {
            Val::Vec(items) => Val::Vec(
                items
                    .iter()
                    .map(|item| postwalk_replace_recursive(replacement_map, item))
                    .collect(),
            ),
            Val::Map(map) => {
                let mut result_map = indexmap::IndexMap::new();
                for (key, value) in map.iter() {
                    result_map.insert(
                        postwalk_replace_recursive(replacement_map, key),
                        postwalk_replace_recursive(replacement_map, value),
                    );
                }
                Val::Map(Box::new(result_map))
            }
            _ => data.clone(),
        };

        // Then replace this element if it matches a key in the map
        match replacement_map.get(&walked) {
            Some(replacement) => replacement.clone(),
            None => walked,
        }
    }

    HotResult::Ok(postwalk_replace_recursive(replacement_map, data))
}

// --- Deep path operations ---

/// Helper: look up a single key in a Map or Vec
fn get_one(coll: &Val, key: &Val) -> Val {
    match coll {
        Val::Map(m) => m.get(key).cloned().unwrap_or(Val::Null),
        Val::Vec(v) => {
            if let Val::Int(i) = key {
                if *i >= 0 {
                    v.get(*i as usize).cloned().unwrap_or(Val::Null)
                } else {
                    Val::Null
                }
            } else {
                Val::Null
            }
        }
        _ => Val::Null,
    }
}

/// get-in: walk a path of keys through nested Maps/Vecs.
/// 2-arity: get-in(coll, path) — returns null if any key is missing
/// 3-arity: get-in(coll, path, default) — returns default if any key is missing
pub fn get_in(args: &[Val]) -> HotResult<Val> {
    if args.len() < 2 || args.len() > 3 {
        return HotResult::Err(Val::from(
            "get-in expects 2 or 3 arguments (coll, path, [default])".to_string(),
        ));
    }

    let path = match &args[1] {
        Val::Vec(v) => v,
        _ => {
            return HotResult::Err(Val::from("get-in: path must be a Vec".to_string()));
        }
    };

    let not_found = if args.len() == 3 {
        args[2].clone()
    } else {
        Val::Null
    };

    let mut current = args[0].clone();
    for key in path.iter() {
        let next = get_one(&current, key);
        if matches!(next, Val::Null) && !matches!(current, Val::Map(_) | Val::Vec(_)) {
            return HotResult::Ok(not_found);
        }
        current = next;
    }

    if matches!(current, Val::Null) && args.len() == 3 {
        HotResult::Ok(not_found)
    } else {
        HotResult::Ok(current)
    }
}

/// assoc-in: immutably set a value at a nested path, creating intermediate maps as needed.
/// assoc-in(coll, path, value)
pub fn assoc_in(args: &[Val]) -> HotResult<Val> {
    if args.len() != 3 {
        return HotResult::Err(Val::from(
            "assoc-in expects 3 arguments (coll, path, value)".to_string(),
        ));
    }

    let path = match &args[1] {
        Val::Vec(v) => v.clone(),
        _ => {
            return HotResult::Err(Val::from("assoc-in: path must be a Vec".to_string()));
        }
    };

    if path.is_empty() {
        return HotResult::Ok(args[2].clone());
    }

    fn assoc_in_recursive(coll: &Val, path: &[Val], value: &Val) -> Result<Val, Val> {
        if path.is_empty() {
            return Ok(value.clone());
        }

        let key = &path[0];
        let rest = &path[1..];

        match coll {
            Val::Map(m) => {
                let mut new_map = (**m).clone();
                let child = m
                    .get(key)
                    .cloned()
                    .unwrap_or(Val::Map(Box::new(IndexMap::new())));
                let new_child = assoc_in_recursive(&child, rest, value)?;
                new_map.insert(key.clone(), new_child);
                Ok(Val::Map(Box::new(new_map)))
            }
            Val::Vec(v) => {
                if let Val::Int(i) = key {
                    if *i < 0 {
                        return Err(Val::from(format!(
                            "assoc-in: vector index cannot be negative, got {}",
                            i
                        )));
                    }
                    let idx = usize::try_from(*i).map_err(|_| {
                        Val::from(format!(
                            "assoc-in: vector index {} is too large to address",
                            i
                        ))
                    })?;
                    let mut new_vec = v.clone();
                    if idx >= new_vec.len() {
                        // Growing to `idx + 1` is a user-controlled allocation;
                        // bound it explicitly so a hostile caller can't push the
                        // worker into an OOM abort.
                        let new_len = idx.checked_add(1).ok_or_else(|| {
                            Val::from(format!(
                                "assoc-in: vector index {} would overflow length",
                                i
                            ))
                        })?;
                        crate::lang::runtime::limits::check_collection_size("assoc-in", new_len)?;
                        let additional = new_len - new_vec.len();
                        crate::lang::runtime::limits::try_reserve_vec(
                            "assoc-in",
                            &mut new_vec,
                            additional,
                        )?;
                        new_vec.resize(new_len, Val::Null);
                    }
                    let child = new_vec
                        .get(idx)
                        .cloned()
                        .unwrap_or(Val::Map(Box::new(IndexMap::new())));
                    new_vec[idx] = assoc_in_recursive(&child, rest, value)?;
                    Ok(Val::Vec(new_vec))
                } else {
                    Ok(coll.clone())
                }
            }
            _ => {
                // Non-collection at intermediate path: create a new map
                let mut new_map = IndexMap::new();
                let child = Val::Map(Box::new(IndexMap::new()));
                let new_child = assoc_in_recursive(&child, rest, value)?;
                new_map.insert(key.clone(), new_child);
                Ok(Val::Map(Box::new(new_map)))
            }
        }
    }

    match assoc_in_recursive(&args[0], &path, &args[2]) {
        Ok(v) => HotResult::Ok(v),
        Err(e) => HotResult::Err(e),
    }
}

/// update-in: apply a function to the value at a nested path.
/// update-in(coll, path, func) — VM-aware because it needs to call the function
pub fn update_in(
    vm: &mut crate::lang::runtime::vm::VirtualMachine,
    args: &[Val],
) -> HotResult<Val> {
    if args.len() != 3 {
        return HotResult::Err(Val::from(
            "update-in expects 3 arguments (coll, path, func)".to_string(),
        ));
    }

    let path = match &args[1] {
        Val::Vec(v) => v.clone(),
        _ => {
            return HotResult::Err(Val::from("update-in: path must be a Vec".to_string()));
        }
    };

    if path.is_empty() {
        return match call_function_with_vm(vm, &args[2], &args[0])
            .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
        {
            Ok(result) => HotResult::Ok(result),
            Err(e) => HotResult::Err(Val::from(e)),
        };
    }

    // Get the current value at the path
    let current = {
        let mut c = args[0].clone();
        for key in path.iter() {
            c = get_one(&c, key);
        }
        c
    };

    // Apply the function to the current value
    let new_value = match call_function_with_vm(vm, &args[2], &current)
        .and_then(|val| apply_onerr_disposition(vm, val, OnErrDisposition::Force))
    {
        Ok(result) => result,
        Err(e) => return HotResult::Err(Val::from(e)),
    };

    // Use assoc_in to set the new value
    assoc_in(&[args[0].clone(), Val::Vec(path), new_value])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_merge_preserves_left_key_order() {
        let mut a = IndexMap::new();
        a.insert(Val::from("a"), Val::Int(1));
        a.insert(Val::from("b"), Val::Int(2));
        let mut b = IndexMap::new();
        b.insert(Val::from("b"), Val::Int(3));
        b.insert(Val::from("c"), Val::Int(4));

        let result = merge(&[Val::Map(Box::new(a)), Val::Map(Box::new(b))]);
        match result {
            HotResult::Ok(Val::Map(m)) => {
                let keys: Vec<Val> = m.keys().cloned().collect();
                assert_eq!(
                    keys,
                    vec![Val::from("a"), Val::from("b"), Val::from("c")],
                    "left map's key order wins"
                );
                assert_eq!(
                    m.get(&Val::from("b")),
                    Some(&Val::Int(3)),
                    "right value wins"
                );
            }
            other => panic!("Expected map, got {:?}", other),
        }
    }

    #[test]
    fn test_get_map_found() {
        let mut map = IndexMap::new();
        map.insert(Val::from("a"), Val::Int(1));
        map.insert(Val::from("b"), Val::Int(2));

        // 2-arity: get(map, key) - found
        let result = get(&[Val::Map(Box::new(map.clone())), Val::from("a")]);
        assert_eq!(result, HotResult::Ok(Val::Int(1)));
    }

    #[test]
    fn test_get_map_not_found() {
        let mut map = IndexMap::new();
        map.insert(Val::from("a"), Val::Int(1));

        // 2-arity: get(map, key) - not found, returns null
        let result = get(&[Val::Map(Box::new(map.clone())), Val::from("missing")]);
        assert_eq!(result, HotResult::Ok(Val::Null));
    }

    #[test]
    fn test_get_map_with_default() {
        let mut map = IndexMap::new();
        map.insert(Val::from("a"), Val::Int(1));

        // 3-arity: get(map, key, default) - not found, returns default
        let result = get(&[
            Val::Map(Box::new(map)),
            Val::from("missing"),
            Val::from("default"),
        ]);
        assert_eq!(result, HotResult::Ok(Val::from("default")));
    }

    #[test]
    fn test_get_vec_found() {
        let vec = vec![Val::Int(10), Val::Int(20), Val::Int(30)];

        let result = get(&[Val::Vec(vec.clone()), Val::Int(1)]);
        assert_eq!(result, HotResult::Ok(Val::Int(20)));
    }

    #[test]
    fn test_get_vec_out_of_bounds() {
        let vec = vec![Val::Int(10), Val::Int(20)];

        // Out of bounds returns null
        let result = get(&[Val::Vec(vec.clone()), Val::Int(10)]);
        assert_eq!(result, HotResult::Ok(Val::Null));
    }

    #[test]
    fn test_get_vec_with_default() {
        let vec = vec![Val::Int(10), Val::Int(20)];

        // Out of bounds returns default
        let result = get(&[Val::Vec(vec), Val::Int(10), Val::Int(-1)]);
        assert_eq!(result, HotResult::Ok(Val::Int(-1)));
    }

    #[test]
    fn test_get_vec_negative_index() {
        let vec = vec![Val::Int(10), Val::Int(20)];

        // Negative index returns null (or default)
        let result = get(&[Val::Vec(vec), Val::Int(-1)]);
        assert_eq!(result, HotResult::Ok(Val::Null));
    }

    #[test]
    fn test_get_str_found() {
        // Get character at index
        let result = get(&[Val::from("hello"), Val::Int(0)]);
        assert_eq!(result, HotResult::Ok(Val::from("h")));

        let result = get(&[Val::from("hello"), Val::Int(4)]);
        assert_eq!(result, HotResult::Ok(Val::from("o")));
    }

    #[test]
    fn test_get_str_out_of_bounds() {
        let result = get(&[Val::from("hello"), Val::Int(100)]);
        assert_eq!(result, HotResult::Ok(Val::Null));
    }

    #[test]
    fn test_get_bytes_found() {
        let bytes = vec![65u8, 66, 67]; // ABC

        let result = get(&[Val::Bytes(bytes.clone()), Val::Int(0)]);
        assert_eq!(result, HotResult::Ok(Val::Int(65)));

        let result = get(&[Val::Bytes(bytes), Val::Int(2)]);
        assert_eq!(result, HotResult::Ok(Val::Int(67)));
    }

    #[test]
    fn test_get_bytes_out_of_bounds() {
        let bytes = vec![65u8, 66, 67];

        let result = get(&[Val::Bytes(bytes), Val::Int(10)]);
        assert_eq!(result, HotResult::Ok(Val::Null));
    }

    #[test]
    fn test_get_bytes_with_default() {
        let bytes = vec![65u8, 66, 67];

        let result = get(&[Val::Bytes(bytes), Val::Int(10), Val::Int(-1)]);
        assert_eq!(result, HotResult::Ok(Val::Int(-1)));
    }

    #[test]
    fn test_get_wrong_arity() {
        // Too few args
        let result = get(&[Val::Vec(vec![Val::Int(1)])]);
        assert!(matches!(result, HotResult::Err(_)));

        // Too many args
        let result = get(&[
            Val::Vec(vec![Val::Int(1)]),
            Val::Int(0),
            Val::Int(0),
            Val::Int(0),
        ]);
        assert!(matches!(result, HotResult::Err(_)));
    }

    #[test]
    fn test_get_vec_wrong_index_type() {
        let vec = vec![Val::Int(10), Val::Int(20)];

        // String index on Vec should error
        let result = get(&[Val::Vec(vec), Val::from("a")]);
        assert!(matches!(result, HotResult::Err(_)));
    }

    // --- get_in tests ---

    #[test]
    fn test_get_in_nested_map() {
        let inner = indexmap::indexmap! {
            Val::from("c") => Val::Int(42),
        };
        let mid = indexmap::indexmap! {
            Val::from("b") => Val::Map(Box::new(inner)),
        };
        let outer = indexmap::indexmap! {
            Val::from("a") => Val::Map(Box::new(mid)),
        };
        let path = vec![Val::from("a"), Val::from("b"), Val::from("c")];
        let result = get_in(&[Val::Map(Box::new(outer)), Val::Vec(path)]);
        assert_eq!(result, HotResult::Ok(Val::Int(42)));
    }

    #[test]
    fn test_get_in_mixed_map_vec() {
        let user = indexmap::indexmap! {
            Val::from("name") => Val::from("Alice"),
        };
        let users_vec = vec![Val::Map(Box::new(user))];
        let outer = indexmap::indexmap! {
            Val::from("users") => Val::Vec(users_vec),
        };
        let path = vec![Val::from("users"), Val::Int(0), Val::from("name")];
        let result = get_in(&[Val::Map(Box::new(outer)), Val::Vec(path)]);
        assert_eq!(result, HotResult::Ok(Val::from("Alice")));
    }

    #[test]
    fn test_get_in_missing_key() {
        let m = indexmap::indexmap! { Val::from("a") => Val::Int(1) };
        let path = vec![Val::from("b"), Val::from("c")];
        let result = get_in(&[Val::Map(Box::new(m)), Val::Vec(path)]);
        assert_eq!(result, HotResult::Ok(Val::Null));
    }

    #[test]
    fn test_get_in_with_default() {
        let m = indexmap::indexmap! { Val::from("a") => Val::Int(1) };
        let path = vec![Val::from("b")];
        let result = get_in(&[Val::Map(Box::new(m)), Val::Vec(path), Val::from("fallback")]);
        assert_eq!(result, HotResult::Ok(Val::from("fallback")));
    }

    #[test]
    fn test_get_in_empty_path() {
        let m = indexmap::indexmap! { Val::from("a") => Val::Int(1) };
        let result = get_in(&[Val::Map(Box::new(m.clone())), Val::Vec(vec![])]);
        assert_eq!(result, HotResult::Ok(Val::Map(Box::new(m))));
    }

    // --- assoc_in tests ---

    #[test]
    fn test_assoc_in_nested() {
        let inner = indexmap::indexmap! { Val::from("b") => Val::Int(1) };
        let outer = indexmap::indexmap! { Val::from("a") => Val::Map(Box::new(inner)) };
        let path = vec![Val::from("a"), Val::from("b")];
        let result = assoc_in(&[Val::Map(Box::new(outer)), Val::Vec(path), Val::Int(99)]);
        if let HotResult::Ok(Val::Map(m)) = &result
            && let Some(Val::Map(inner)) = m.get(&Val::from("a"))
        {
            assert_eq!(inner.get(&Val::from("b")), Some(&Val::Int(99)));
            return;
        }
        panic!("assoc_in nested failed: {:?}", result);
    }

    #[test]
    fn test_assoc_in_creates_intermediates() {
        let m = indexmap::indexmap! {};
        let path = vec![Val::from("a"), Val::from("b")];
        let result = assoc_in(&[Val::Map(Box::new(m)), Val::Vec(path), Val::Int(1)]);
        if let HotResult::Ok(Val::Map(outer)) = &result
            && let Some(Val::Map(inner)) = outer.get(&Val::from("a"))
        {
            assert_eq!(inner.get(&Val::from("b")), Some(&Val::Int(1)));
            return;
        }
        panic!("assoc_in create intermediates failed: {:?}", result);
    }

    #[test]
    fn test_assoc_in_preserves_siblings() {
        let inner = indexmap::indexmap! {
            Val::from("b") => Val::Int(1),
            Val::from("c") => Val::Int(2),
        };
        let outer = indexmap::indexmap! { Val::from("a") => Val::Map(Box::new(inner)) };
        let path = vec![Val::from("a"), Val::from("b")];
        let result = assoc_in(&[Val::Map(Box::new(outer)), Val::Vec(path), Val::Int(99)]);
        if let HotResult::Ok(Val::Map(outer)) = &result
            && let Some(Val::Map(inner)) = outer.get(&Val::from("a"))
        {
            assert_eq!(inner.get(&Val::from("b")), Some(&Val::Int(99)));
            assert_eq!(inner.get(&Val::from("c")), Some(&Val::Int(2)));
            return;
        }
        panic!("assoc_in preserve siblings failed: {:?}", result);
    }
}
