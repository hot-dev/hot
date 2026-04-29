//! Allocator wrapper that emits an OOM diagnostic before aborting.
//!
//! Why this exists
//! ---------------
//! When the allocator fails (`alloc` / `realloc` returns null) the standard
//! library calls `std::alloc::handle_alloc_error`, which calls the unstable
//! `set_alloc_error_hook` (if one is set) and then `process::abort()`.
//! On stable Rust we can't install that hook, so by default an OOM produces
//! a bare `memory allocation of N bytes failed` line on stderr with no
//! context about the thread or what was being allocated for.
//!
//! By wrapping the system allocator, we can log structured info — process
//! pid, thread name, requested layout — *before* returning the null pointer
//! that triggers the abort. The log goes through `libc::write(STDERR, ...)`
//! rather than `tracing` because `tracing` itself allocates (and we are, by
//! definition, in an allocation-failure path).
//!
//! Activating the wrapper
//! ----------------------
//! Each binary that wants this opts in by declaring:
//!
//! ```ignore
//! #[global_allocator]
//! static A: hot::lang::runtime::oom_logger::LoggingAllocator = hot::lang::runtime::oom_logger::LoggingAllocator;
//! ```
//!
//! Without that opt-in the wrapper is dormant code (no impact).
//!
//! Caveats
//! -------
//! * This does NOT prevent the abort. The process still dies; we just leave
//!   a better breadcrumb in process logs.
//! * Stack overflow (SIGSEGV from running off the end of the stack) bypasses
//!   this entirely — it doesn't go through the allocator. The recursion-depth
//!   cap in `vm.rs` handles that case.
//! * Inside the OOM logger we can't call any function that allocates. Stick
//!   to `core::fmt::Write` into stack-allocated buffers and direct syscalls.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicBool, Ordering};

/// Re-entrancy guard so a panic / log inside the logger doesn't recurse.
static IN_LOGGER: AtomicBool = AtomicBool::new(false);

/// A drop-in replacement for `System` that logs a structured line to stderr
/// when an allocation fails before the process aborts.
pub struct LoggingAllocator;

unsafe impl GlobalAlloc for LoggingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc(layout) };
        if ptr.is_null() {
            log_oom("alloc", layout, 0);
        }
        ptr
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let ptr = unsafe { System.alloc_zeroed(layout) };
        if ptr.is_null() {
            log_oom("alloc_zeroed", layout, 0);
        }
        ptr
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let new_ptr = unsafe { System.realloc(ptr, layout, new_size) };
        if new_ptr.is_null() {
            log_oom("realloc", layout, new_size);
        }
        new_ptr
    }
}

/// Write a single OOM diagnostic line to stderr without allocating.
///
/// Format (one line):
/// ```text
/// [hot oom] op=alloc size=<requested_bytes> align=<align> tid=<thread_id> pid=<pid>
/// ```
fn log_oom(op: &str, layout: Layout, new_size: usize) {
    // Re-entrancy guard: if logging itself causes an alloc failure, just
    // bail rather than spinning.
    if IN_LOGGER.swap(true, Ordering::Relaxed) {
        return;
    }

    // Build the message in a fixed stack buffer so we don't allocate.
    use core::fmt::Write;
    let mut buf = StackBuf::<256>::new();

    // Best-effort write; if the buffer fills, the truncation is acceptable
    // — we still get a recognizable prefix in process logs.
    let _ = writeln!(
        &mut buf,
        "[hot oom] op={} size={} align={} new_size={} pid={}",
        op,
        layout.size(),
        layout.align(),
        new_size,
        std::process::id(),
    );

    // Write directly to stderr fd (2). std::io::Write would allocate.
    let bytes = buf.as_bytes();
    #[cfg(unix)]
    unsafe {
        let _ = libc::write(2, bytes.as_ptr() as *const libc::c_void, bytes.len());
    }
    #[cfg(not(unix))]
    {
        // On non-unix targets, just drop the diagnostic (no clean syscall path
        // that doesn't allocate via std::io). The default abort message will
        // still appear.
        let _ = bytes;
    }

    IN_LOGGER.store(false, Ordering::Relaxed);
}

/// Tiny `core::fmt::Write` sink backed by a stack-allocated byte buffer.
struct StackBuf<const N: usize> {
    buf: [u8; N],
    len: usize,
}

impl<const N: usize> StackBuf<N> {
    fn new() -> Self {
        Self {
            buf: [0u8; N],
            len: 0,
        }
    }
    fn as_bytes(&self) -> &[u8] {
        &self.buf[..self.len]
    }
}

impl<const N: usize> core::fmt::Write for StackBuf<N> {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let space = N.saturating_sub(self.len);
        let take = bytes.len().min(space);
        self.buf[self.len..self.len + take].copy_from_slice(&bytes[..take]);
        self.len += take;
        // We deliberately succeed even on truncation so the rest of the
        // formatting machinery doesn't bail out partway through.
        Ok(())
    }
}
