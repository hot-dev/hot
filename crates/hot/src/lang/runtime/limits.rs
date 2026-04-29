//! Runtime limits enforced on user-controllable allocations.
//!
//! These guards exist to keep a single hostile or buggy Hot expression from
//! exhausting host memory. An allocator OOM in Rust calls `abort()` which
//! bypasses `catch_unwind`, so we cannot recover from it after the fact —
//! the only safe move is to refuse the allocation up front.
//!
//! Each limit is overridable via an environment variable for operators that
//! genuinely need bigger budgets (data-engineering workloads, large response
//! payloads, etc.). Defaults are conservative enough to keep a worker process
//! comfortably within a few hundred MB of resident memory under typical web /
//! background-task loads.
//!
//! # Common pattern
//!
//! ```ignore
//! use crate::lang::runtime::limits;
//!
//! let n = user_supplied_count;
//! limits::check_collection_size("range", n)?;
//! let mut out = Vec::with_capacity(n);
//! ```
//!
//! All `check_*` helpers return `Err(Val)` shaped like a Hot error string so
//! they can be returned directly from a `HotResult` builtin.

use std::sync::OnceLock;

use crate::val::Val;

// ---------------------------------------------------------------------------
// Defaults (override with env vars)
// ---------------------------------------------------------------------------

/// Maximum number of elements in a single Vec/Map produced by a builtin.
/// Default: 16 million. A 16M-element `Vec<Val>` is ~512 MB; this is the
/// right order of magnitude for "very large but not memory-bombing".
pub const DEFAULT_MAX_COLLECTION_SIZE: usize = 16 * 1024 * 1024;

/// Maximum byte length of a single string produced by a builtin (`repeat`,
/// `concat`, template rendering, etc.). Default: 256 MB.
pub const DEFAULT_MAX_STRING_BYTES: usize = 256 * 1024 * 1024;

/// Maximum byte length of input accepted by a parser (`from-json`,
/// `from-xml`). Default: 64 MB. This limits how big a single
/// user-controlled blob we will attempt to parse, since some parsers can
/// allocate significantly more than the input size during parsing.
pub const DEFAULT_MAX_PARSE_INPUT_BYTES: usize = 64 * 1024 * 1024;

/// Maximum byte length of a single allocation request expressed in bytes
/// (e.g. `random-bytes(n)`). Default: 256 MB.
pub const DEFAULT_MAX_ALLOC_BYTES: usize = 256 * 1024 * 1024;

/// Maximum source-text length of a single regex pattern. Patterns are
/// user-supplied strings; long pathological patterns can blow up memory
/// during compilation even when they reject quickly. Default: 64 KB.
pub const DEFAULT_MAX_REGEX_PATTERN_BYTES: usize = 64 * 1024;

/// Compile-time NFA/DFA memory budget for a single regex (passed to
/// `RegexBuilder::size_limit`). Default: 10 MB, the regex crate's own
/// default — explicit so operators can lower it.
pub const DEFAULT_MAX_REGEX_COMPILE_BYTES: usize = 10 * 1024 * 1024;

/// Maximum byte length of input we'll let a regex builtin scan. Matching is
/// linear in input size in the `regex` crate, but a hostile caller can still
/// occupy a thread for a long time on huge inputs. Default: 64 MB.
pub const DEFAULT_MAX_REGEX_INPUT_BYTES: usize = 64 * 1024 * 1024;

// ---------------------------------------------------------------------------
// Cached env lookups
// ---------------------------------------------------------------------------

fn cached_env(var: &str, default: usize) -> usize {
    // We use a separate OnceLock per env var. The cache key is the var name,
    // but since the set of vars is small and known at compile time, we just
    // pattern-match into a single shared static here.
    static MAX_COLLECTION: OnceLock<usize> = OnceLock::new();
    static MAX_STRING: OnceLock<usize> = OnceLock::new();
    static MAX_PARSE: OnceLock<usize> = OnceLock::new();
    static MAX_ALLOC: OnceLock<usize> = OnceLock::new();
    static MAX_REGEX_PATTERN: OnceLock<usize> = OnceLock::new();
    static MAX_REGEX_COMPILE: OnceLock<usize> = OnceLock::new();
    static MAX_REGEX_INPUT: OnceLock<usize> = OnceLock::new();

    let cell = match var {
        "HOT_MAX_COLLECTION_SIZE" => &MAX_COLLECTION,
        "HOT_MAX_STRING_BYTES" => &MAX_STRING,
        "HOT_MAX_PARSE_INPUT_BYTES" => &MAX_PARSE,
        "HOT_MAX_ALLOC_BYTES" => &MAX_ALLOC,
        "HOT_MAX_REGEX_PATTERN_BYTES" => &MAX_REGEX_PATTERN,
        "HOT_MAX_REGEX_COMPILE_BYTES" => &MAX_REGEX_COMPILE,
        "HOT_MAX_REGEX_INPUT_BYTES" => &MAX_REGEX_INPUT,
        _ => return default,
    };
    *cell.get_or_init(|| {
        std::env::var(var)
            .ok()
            .and_then(|s| s.parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(default)
    })
}

/// Active maximum collection element count.
pub fn max_collection_size() -> usize {
    cached_env("HOT_MAX_COLLECTION_SIZE", DEFAULT_MAX_COLLECTION_SIZE)
}

/// Active maximum produced-string byte length.
pub fn max_string_bytes() -> usize {
    cached_env("HOT_MAX_STRING_BYTES", DEFAULT_MAX_STRING_BYTES)
}

/// Active maximum parser input byte length.
pub fn max_parse_input_bytes() -> usize {
    cached_env("HOT_MAX_PARSE_INPUT_BYTES", DEFAULT_MAX_PARSE_INPUT_BYTES)
}

/// Active maximum bytes-allocation request size.
pub fn max_alloc_bytes() -> usize {
    cached_env("HOT_MAX_ALLOC_BYTES", DEFAULT_MAX_ALLOC_BYTES)
}

/// Active maximum regex pattern source length.
pub fn max_regex_pattern_bytes() -> usize {
    cached_env(
        "HOT_MAX_REGEX_PATTERN_BYTES",
        DEFAULT_MAX_REGEX_PATTERN_BYTES,
    )
}

/// Active per-regex compile-time memory budget (NFA/DFA size).
pub fn max_regex_compile_bytes() -> usize {
    cached_env(
        "HOT_MAX_REGEX_COMPILE_BYTES",
        DEFAULT_MAX_REGEX_COMPILE_BYTES,
    )
}

/// Active maximum haystack length passed to a regex builtin.
pub fn max_regex_input_bytes() -> usize {
    cached_env("HOT_MAX_REGEX_INPUT_BYTES", DEFAULT_MAX_REGEX_INPUT_BYTES)
}

// ---------------------------------------------------------------------------
// Guard helpers (return Hot-friendly errors)
// ---------------------------------------------------------------------------

fn err_msg(s: String) -> Val {
    Val::from(s)
}

/// Reject if `requested` exceeds the configured collection size cap.
/// `op` is the builtin's name (used in the error message).
pub fn check_collection_size(op: &str, requested: usize) -> Result<(), Val> {
    let cap = max_collection_size();
    if requested > cap {
        return Err(err_msg(format!(
            "{}: refusing to allocate collection of {} elements (limit {}). \
             Raise HOT_MAX_COLLECTION_SIZE if this is intentional.",
            op, requested, cap,
        )));
    }
    Ok(())
}

/// Reject if `requested` exceeds the configured produced-string size cap.
pub fn check_string_bytes(op: &str, requested: usize) -> Result<(), Val> {
    let cap = max_string_bytes();
    if requested > cap {
        return Err(err_msg(format!(
            "{}: refusing to produce string of {} bytes (limit {}). \
             Raise HOT_MAX_STRING_BYTES if this is intentional.",
            op, requested, cap,
        )));
    }
    Ok(())
}

/// Reject if `input_len` exceeds the configured parser-input size cap.
pub fn check_parse_input(op: &str, input_len: usize) -> Result<(), Val> {
    let cap = max_parse_input_bytes();
    if input_len > cap {
        return Err(err_msg(format!(
            "{}: refusing to parse input of {} bytes (limit {}). \
             Raise HOT_MAX_PARSE_INPUT_BYTES if this is intentional.",
            op, input_len, cap,
        )));
    }
    Ok(())
}

/// Reject if `requested` exceeds the configured byte-allocation cap.
pub fn check_alloc_bytes(op: &str, requested: usize) -> Result<(), Val> {
    let cap = max_alloc_bytes();
    if requested > cap {
        return Err(err_msg(format!(
            "{}: refusing to allocate {} bytes (limit {}). \
             Raise HOT_MAX_ALLOC_BYTES if this is intentional.",
            op, requested, cap,
        )));
    }
    Ok(())
}

/// Reject regex patterns that are too long to be safe to compile.
pub fn check_regex_pattern(op: &str, pattern_len: usize) -> Result<(), Val> {
    let cap = max_regex_pattern_bytes();
    if pattern_len > cap {
        return Err(err_msg(format!(
            "{}: refusing to compile regex of {} bytes (limit {}). \
             Raise HOT_MAX_REGEX_PATTERN_BYTES if this is intentional.",
            op, pattern_len, cap,
        )));
    }
    Ok(())
}

/// Reject haystacks longer than the configured regex-input cap.
pub fn check_regex_input(op: &str, input_len: usize) -> Result<(), Val> {
    let cap = max_regex_input_bytes();
    if input_len > cap {
        return Err(err_msg(format!(
            "{}: refusing to scan input of {} bytes (limit {}). \
             Raise HOT_MAX_REGEX_INPUT_BYTES if this is intentional.",
            op, input_len, cap,
        )));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Fallible allocation helpers
// ---------------------------------------------------------------------------
//
// `Vec::try_reserve_exact` returns an error instead of aborting on allocation
// failure. We wrap it to produce a Hot-shaped error so builtins can return a
// recoverable runtime error rather than calling `handle_alloc_error` and
// taking down the worker.
//
// NOTE: a true alloc-error hook (`std::alloc::set_alloc_error_hook`) is still
// nightly-only as of Rust 1.95. Until it is stabilized, the cleanest defense
// is to use `try_reserve` at any site whose capacity is user-controlled.

/// Try to reserve capacity, returning a Hot-friendly error on failure.
/// Use this at any allocation site whose size depends on user input that
/// has already passed the size guards but might still exceed available memory.
pub fn try_reserve_vec<T>(op: &str, vec: &mut Vec<T>, additional: usize) -> Result<(), Val> {
    vec.try_reserve_exact(additional).map_err(|e| {
        err_msg(format!(
            "{}: failed to reserve {} elements: {}",
            op, additional, e
        ))
    })
}

/// Try to reserve capacity in a String.
pub fn try_reserve_string(op: &str, s: &mut String, additional: usize) -> Result<(), Val> {
    s.try_reserve_exact(additional).map_err(|e| {
        err_msg(format!(
            "{}: failed to reserve {} bytes: {}",
            op, additional, e
        ))
    })
}

/// Compute the number of elements a `range(start, end, step)` would produce,
/// or `None` on overflow. Used by `range` to gate before allocating.
pub fn range_element_count(start: i64, end: i64, step: i64) -> Option<usize> {
    if step == 0 {
        return None;
    }
    let span = (end as i128) - (start as i128);
    if (step > 0 && span <= 0) || (step < 0 && span >= 0) {
        return Some(0);
    }
    let abs_span = span.unsigned_abs();
    let abs_step = (step as i128).unsigned_abs();
    // ceil(abs_span / abs_step)
    let count = abs_span.div_ceil(abs_step);
    usize::try_from(count).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_count_basics() {
        assert_eq!(range_element_count(0, 10, 1), Some(10));
        assert_eq!(range_element_count(0, 10, 3), Some(4)); // 0,3,6,9
        assert_eq!(range_element_count(10, 0, -1), Some(10));
        assert_eq!(range_element_count(0, 0, 1), Some(0));
        assert_eq!(range_element_count(0, -5, 1), Some(0));
        assert_eq!(range_element_count(0, 10, 0), None);
    }

    #[test]
    fn range_count_handles_huge_span() {
        // i64::MAX span shouldn't panic; should return None or huge number.
        let n = range_element_count(i64::MIN, i64::MAX, 1);
        // On 64-bit systems usize::MAX > i64::MAX, so this fits.
        assert!(n.is_some(), "expected Some, got {:?}", n);
    }

    #[test]
    fn collection_guard_rejects_oversized() {
        let too_big = max_collection_size() + 1;
        assert!(check_collection_size("range", too_big).is_err());
    }

    #[test]
    fn collection_guard_accepts_within_limit() {
        assert!(check_collection_size("range", 1024).is_ok());
    }
}
