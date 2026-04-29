// This file contains utility functions for the hot library functions

use crate::lang::hot::r#type::HotResult;
use crate::val::Val;

/// Validate that function has exactly the expected number of arguments
pub fn validate_args(fn_name: &str, args: &[Val], expected_args: usize) -> HotResult<Val> {
    if args.len() != expected_args {
        return HotResult::Err(Val::from(format!(
            "{} expects {} argument{}, got {}",
            fn_name,
            expected_args,
            if expected_args == 1 { "" } else { "s" },
            args.len()
        )));
    }
    HotResult::Ok(Val::Bool(true))
}

/// Validate that function has at least the expected number of arguments
pub fn validate_args_at_least(fn_name: &str, args: &[Val], expected_args: usize) -> HotResult<Val> {
    if args.len() < expected_args {
        return HotResult::Err(Val::from(format!(
            "{} expects at least {} argument{}, got {}",
            fn_name,
            expected_args,
            if expected_args == 1 { "" } else { "s" },
            args.len()
        )));
    }
    HotResult::Ok(Val::Bool(true))
}

/// Validate that function has no arguments (zero arguments)
pub fn validate_no_args(fn_name: &str, args: &[Val]) -> HotResult<Val> {
    if !args.is_empty() {
        return HotResult::Err(Val::from(format!(
            "{} expects 0 arguments, got {}",
            fn_name,
            args.len()
        )));
    }
    HotResult::Ok(Val::Bool(true))
}

/// Validate that function has between min and max arguments (inclusive)
pub fn validate_args_range(
    fn_name: &str,
    args: &[Val],
    min_args: usize,
    max_args: usize,
) -> HotResult<Val> {
    if args.len() < min_args || args.len() > max_args {
        return HotResult::Err(Val::from(format!(
            "{} expects {} to {} arguments, got {}",
            fn_name,
            min_args,
            max_args,
            args.len()
        )));
    }
    HotResult::Ok(Val::Bool(true))
}

/// Convenience macro to make argument validation more succinct
/// Usage: validate_args!(fn_name, args, 2);
#[macro_export]
macro_rules! validate_args {
    ($fn_name:expr, $args:expr, $expected:expr) => {
        if let HotResult::Err(err) =
            $crate::lang::hot::lib_util::validate_args($fn_name, $args, $expected)
        {
            return HotResult::Err(err);
        }
    };
}

/// Convenience macro for validating no arguments
/// Usage: validate_no_args!(fn_name, args);
#[macro_export]
macro_rules! validate_no_args {
    ($fn_name:expr, $args:expr) => {
        if let HotResult::Err(err) = $crate::lang::hot::lib_util::validate_no_args($fn_name, $args)
        {
            return HotResult::Err(err);
        }
    };
}

/// Convenience macro for validating at least N arguments
/// Usage: validate_args_at_least!(fn_name, args, 2);
#[macro_export]
macro_rules! validate_args_at_least {
    ($fn_name:expr, $args:expr, $expected:expr) => {
        if let HotResult::Err(err) =
            $crate::lang::hot::lib_util::validate_args_at_least($fn_name, $args, $expected)
        {
            return HotResult::Err(err);
        }
    };
}

/// Convenience macro for validating argument range
/// Usage: validate_args_range!(fn_name, args, 2, 3);
#[macro_export]
macro_rules! validate_args_range {
    ($fn_name:expr, $args:expr, $min:expr, $max:expr) => {
        if let HotResult::Err(err) =
            $crate::lang::hot::lib_util::validate_args_range($fn_name, $args, $min, $max)
        {
            return HotResult::Err(err);
        }
    };
}
