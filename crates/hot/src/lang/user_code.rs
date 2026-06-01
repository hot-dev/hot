//! User-code execution boundary: panic capture and resilience.
//!
//! Why this module exists
//! ----------------------
//! Hot is a language runtime that executes arbitrary user code in the same
//! process as the host (worker, task worker, dev server, REPL, etc.).
//! A panic anywhere inside user-code execution must NOT take down the host —
//! one user's bad expression should at worst fail their request, not crash
//! the whole worker and force a restart.
//!
//! Two pieces work together:
//!
//! 1. [`install_panic_hook`] — call once at process startup. Installs a
//!    global panic hook that captures structured info (location, message,
//!    backtrace, thread name) into a thread-local cell, and routes the panic
//!    notice to `tracing::error!` instead of dumping to stderr.
//!
//! 2. [`run_user_code`] / [`run_user_code_async`] — boundary helpers that
//!    wrap a user-code closure / future in `catch_unwind` and, on panic,
//!    return a `Result::Err(UserCodePanic)` containing the captured info.
//!    The host can then surface a structured error to the user without
//!    losing detail and without dying.
//!
//! Important semantics:
//!
//! * `catch_unwind` only catches Rust unwinding panics. Things that bypass
//!   it (stack overflow → SIGSEGV, allocator OOM, abort from C code) are
//!   handled separately (recursion-depth cap in the VM, alloc-error handler,
//!   process supervisor). See `notes/panic-resilience.md` if it exists.
//! * `parking_lot::Mutex` is used everywhere shared state is held across
//!   user-code execution to avoid the silent poison-on-panic failure mode.
//! * The captured backtrace respects `RUST_BACKTRACE` like the default
//!   hook would, so debug ergonomics are preserved.

use std::cell::RefCell;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Once;

use serde::{Deserialize, Serialize};

/// Structured information about a caught panic from user code.
///
/// All fields are `String` so the value is cheap to clone, send across
/// threads, and serialize into a Hot-level error or JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserCodePanic {
    /// The panic message (`format!`-style payload).
    pub message: String,
    /// Source location of the `panic!`/`unwrap`/etc. that triggered it,
    /// formatted as `"file:line:col"`. `None` if the panic payload didn't
    /// carry a location (rare).
    pub location: Option<String>,
    /// Captured backtrace, when one was available. Whether a backtrace is
    /// captured at all is governed by `RUST_BACKTRACE` / `RUST_LIB_BACKTRACE`
    /// the same way the default hook decides.
    pub backtrace: Option<String>,
    /// Name of the thread the panic occurred on.
    pub thread: String,
}

impl UserCodePanic {
    /// Format a one-line summary suitable for logs / error messages.
    pub fn summary(&self) -> String {
        match &self.location {
            Some(loc) => format!("panic at {}: {}", loc, self.message),
            None => format!("panic: {}", self.message),
        }
    }
}

impl std::fmt::Display for UserCodePanic {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.summary())
    }
}

impl std::error::Error for UserCodePanic {}

// ---------------------------------------------------------------------------
// Thread-local panic capture
// ---------------------------------------------------------------------------

thread_local! {
    /// Last panic captured by the global hook on this thread. Cleared by
    /// `run_user_code` after it observes a panic. Outside a `catch_unwind`,
    /// a panic still populates this cell — that's fine; whoever catches
    /// next will see fresh data because we always overwrite.
    static LAST_PANIC: RefCell<Option<UserCodePanic>> = const { RefCell::new(None) };
}

/// Read and clear the last panic captured on this thread.
fn take_thread_panic() -> Option<UserCodePanic> {
    LAST_PANIC.with(|cell| cell.borrow_mut().take())
}

// ---------------------------------------------------------------------------
// Global panic hook
// ---------------------------------------------------------------------------

static INSTALL_HOOK: Once = Once::new();

/// Install the global panic hook. Idempotent; safe to call from every
/// binary's `main`. Subsequent calls are no-ops.
///
/// The hook:
/// 1. Builds a [`UserCodePanic`] and stashes it in the thread-local for
///    a downstream [`run_user_code`] to pick up.
/// 2. Logs a structured `tracing::error!` so the panic is visible in
///    observability without touching stderr.
/// 3. Suppresses the default hook's stderr dump (which would otherwise
///    print a confusing standalone backtrace not associated with the
///    request that triggered it).
///
/// If the env var `HOT_PANIC_STDERR=1` is set, the previous (default)
/// hook is also invoked so backtraces still appear on stderr — useful
/// for local debugging.
pub fn install_panic_hook() {
    INSTALL_HOOK.call_once(|| {
        let prev = std::panic::take_hook();
        let also_stderr = std::env::var_os("HOT_PANIC_STDERR").is_some();

        std::panic::set_hook(Box::new(move |info| {
            let location = info
                .location()
                .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()));

            // Extract message from the panic payload. Most `panic!()` calls
            // produce a `&str` or `String` payload; everything else is opaque.
            let payload = info.payload();
            let message = if let Some(s) = payload.downcast_ref::<&'static str>() {
                (*s).to_string()
            } else if let Some(s) = payload.downcast_ref::<String>() {
                s.clone()
            } else {
                "<non-string panic payload>".to_string()
            };

            let thread = std::thread::current()
                .name()
                .unwrap_or("unnamed")
                .to_string();

            let backtrace = capture_backtrace_if_enabled();

            let captured = UserCodePanic {
                message: message.clone(),
                location: location.clone(),
                backtrace: backtrace.clone(),
                thread: thread.clone(),
            };

            // Stash for run_user_code to pick up.
            LAST_PANIC.with(|cell| *cell.borrow_mut() = Some(captured));

            // Structured log so the panic is visible without stderr noise.
            // `target = "hot::panic"` lets operators filter / route these.
            tracing::error!(
                target: "hot::panic",
                thread = %thread,
                location = location.as_deref().unwrap_or("<unknown>"),
                "panic in user-code execution: {}",
                message,
            );

            if let Some(bt) = backtrace.as_deref()
                && !bt.is_empty()
            {
                tracing::debug!(target: "hot::panic", "backtrace:\n{}", bt);
            }

            if also_stderr {
                prev(info);
            }
        }));
    });
}

/// Capture a backtrace if the runtime is configured to do so.
///
/// Mirrors the behavior of `std::panic`'s default hook: respects
/// `RUST_BACKTRACE` (and `RUST_LIB_BACKTRACE` for libraries) so we don't
/// pay for a backtrace in production unless someone asked.
fn capture_backtrace_if_enabled() -> Option<String> {
    // Cheap env check; avoid pulling in std::backtrace::BacktraceStatus matching.
    let on = std::env::var_os("RUST_BACKTRACE")
        .or_else(|| std::env::var_os("RUST_LIB_BACKTRACE"))
        .map(|v| v != "0")
        .unwrap_or(false);
    if !on {
        return None;
    }
    let bt = std::backtrace::Backtrace::force_capture();
    Some(bt.to_string())
}

// ---------------------------------------------------------------------------
// Synchronous boundary
// ---------------------------------------------------------------------------

/// Run a closure that may execute user-supplied Hot code, catching any
/// panic and returning a structured [`UserCodePanic`] instead of letting
/// it propagate.
///
/// `label` is included in log messages to help correlate caught panics
/// with the request / handler / task that triggered them.
///
/// # Safety / unwind-safety
///
/// We use [`AssertUnwindSafe`] internally because the `UnwindSafe` trait
/// is overly conservative for the patterns the VM uses (interior mutability
/// in `RefCell`, etc.). This is the standard practice for runtime code that
/// owns its own state and treats a caught panic as "throw away the in-flight
/// work, the host is still consistent". Shared state held across user code
/// must use `parking_lot` (no poisoning) — see the module docs.
pub fn run_user_code<F, T>(label: &str, f: F) -> Result<T, UserCodePanic>
where
    F: FnOnce() -> T,
{
    // Clear any stale panic info captured before this call (defensive).
    let _ = take_thread_panic();

    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(value) => Ok(value),
        Err(payload) => Err(take_thread_panic().unwrap_or_else(|| {
            // Hook didn't fire (e.g., not installed) — synthesize a minimal
            // panic record from the payload so callers always get *something*.
            fallback_panic_from_payload(payload, label)
        })),
    }
}

/// Like [`run_user_code`] but for an `async` closure / future. Uses
/// `futures::FutureExt::catch_unwind` to wrap each `poll` call.
pub async fn run_user_code_async<F, T>(label: &str, fut: F) -> Result<T, UserCodePanic>
where
    F: std::future::Future<Output = T>,
{
    use futures::FutureExt;
    let _ = take_thread_panic();
    match AssertUnwindSafe(fut).catch_unwind().await {
        Ok(value) => Ok(value),
        Err(payload) => {
            Err(take_thread_panic().unwrap_or_else(|| fallback_panic_from_payload(payload, label)))
        }
    }
}

fn fallback_panic_from_payload(
    payload: Box<dyn std::any::Any + Send>,
    label: &str,
) -> UserCodePanic {
    let message = if let Some(s) = payload.downcast_ref::<&'static str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        format!("<non-string panic payload in {}>", label)
    };
    UserCodePanic {
        message,
        location: None,
        backtrace: None,
        thread: std::thread::current()
            .name()
            .unwrap_or("unnamed")
            .to_string(),
    }
}

// ---------------------------------------------------------------------------
// Conversion to a Hot Failure value
// ---------------------------------------------------------------------------

impl UserCodePanic {
    /// Build a `Result.Err(::hot::task/Failure(...))`-shaped Hot value so a
    /// caught panic flows through the normal Hot error-handling paths. The
    /// shape matches what `fail()` produces wrapped in `err()`, with an extra
    /// `panic: true` flag plus location / thread / backtrace for debugging.
    ///
    /// Layout:
    /// ```text
    /// {
    ///   $type: "::hot::type/Result.Err",
    ///   $val: {
    ///     $type: "::hot::task/Failure",
    ///     $val: {
    ///       msg: "panicked: ...",
    ///       err: { panic: true, location, thread, backtrace? }
    ///     }
    ///   }
    /// }
    /// ```
    pub fn to_failure_val(&self) -> crate::val::Val {
        use crate::val::Val;
        use indexmap::IndexMap;

        // Inner err map carrying structured panic info.
        let mut err: IndexMap<Val, Val> = IndexMap::new();
        err.insert(Val::from("panic"), Val::from(true));
        if let Some(loc) = &self.location {
            err.insert(Val::from("location"), Val::from(loc.clone()));
        }
        err.insert(Val::from("thread"), Val::from(self.thread.clone()));
        if let Some(bt) = &self.backtrace
            && !bt.is_empty()
        {
            err.insert(Val::from("backtrace"), Val::from(bt.clone()));
        }

        // ::hot::task/Failure payload.
        let mut failure_val: IndexMap<Val, Val> = IndexMap::new();
        failure_val.insert(
            Val::from("msg"),
            Val::from(format!("panicked: {}", self.message)),
        );
        failure_val.insert(Val::from("err"), Val::from(err));

        let mut failure: IndexMap<Val, Val> = IndexMap::new();
        failure.insert(Val::from("$type"), Val::from("::hot::task/Failure"));
        failure.insert(Val::from("$val"), Val::from(failure_val));

        // Wrap in Result.Err so call sites that check `Val::is_err()` route
        // it through the existing failure path.
        let mut result: IndexMap<Val, Val> = IndexMap::new();
        result.insert(Val::from("$type"), Val::from("::hot::type/Result.Err"));
        result.insert(Val::from("$val"), Val::from(failure));
        Val::from(result)
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn captures_str_panic() {
        install_panic_hook();
        let result: Result<i32, _> = run_user_code("test", || panic!("boom"));
        let err = result.expect_err("should have panicked");
        assert!(err.message.contains("boom"), "got: {}", err.message);
        assert!(err.location.is_some());
    }

    #[test]
    fn captures_string_panic() {
        install_panic_hook();
        let label = "demo".to_string();
        let result: Result<i32, _> = run_user_code("test", || {
            panic!("dynamic: {}", label);
        });
        let err = result.expect_err("should have panicked");
        assert!(err.message.contains("dynamic: demo"));
    }

    #[test]
    fn returns_value_when_no_panic() {
        install_panic_hook();
        let result: Result<i32, _> = run_user_code("test", || 42);
        assert_eq!(result.unwrap(), 42);
    }

    #[tokio::test]
    async fn async_captures_panic() {
        install_panic_hook();
        let result: Result<i32, _> =
            run_user_code_async("test", async { panic!("async boom") }).await;
        let err = result.expect_err("should have panicked");
        assert!(err.message.contains("async boom"));
    }

    #[tokio::test]
    async fn async_returns_value() {
        install_panic_hook();
        let result: Result<i32, _> = run_user_code_async("test", async { 7 }).await;
        assert_eq!(result.unwrap(), 7);
    }
}
