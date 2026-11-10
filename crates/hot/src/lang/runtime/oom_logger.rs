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

use std::alloc::{GlobalAlloc, Layout};

/// The allocator under the logger: the system's.
#[cfg(not(all(feature = "mimalloc", unix)))]
use std::alloc::System;

/// The allocator under the logger: mimalloc, or the system's when
/// `HOT_ALLOCATOR=system` is set as the process starts. On Windows it is
/// always the system's: mimalloc isn't built there.
#[cfg(all(feature = "mimalloc", unix))]
#[allow(non_upper_case_globals)]
static System: chosen::Chosen = chosen::Chosen;

/// The allocator this process runs on: `"mimalloc"` or `"system"`.
pub fn allocator_name() -> &'static str {
    #[cfg(all(feature = "mimalloc", unix))]
    if !chosen::is_system() {
        return "mimalloc";
    }
    "system"
}

/// mimalloc, with the system allocator as a choice made once, at the
/// process's first allocation.
///
/// mimalloc is faster for Hot's many small allocations and holds more
/// memory. The system allocator stays available without a rebuild, for a
/// deployment that has to give that memory back, and for glibc's heap
/// checks (`MALLOC_CHECK_`), which only see what glibc allocates.
#[cfg(all(feature = "mimalloc", unix))]
mod chosen {
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::sync::atomic::{AtomicU8, Ordering};

    const UNDECIDED: u8 = 0;
    const MIMALLOC: u8 = 1;
    const SYSTEM: u8 = 2;

    /// Decided once and never changed after: a block goes back to the
    /// allocator it came from.
    static CHOICE: AtomicU8 = AtomicU8::new(UNDECIDED);
    static MI: mimalloc::MiMalloc = mimalloc::MiMalloc;

    #[inline(always)]
    pub(super) fn is_system() -> bool {
        match CHOICE.load(Ordering::Relaxed) {
            MIMALLOC => false,
            SYSTEM => true,
            _ => decide(),
        }
    }

    /// Threads that race here read the same environment and store the same
    /// answer.
    #[cold]
    fn decide() -> bool {
        let system = wants_system();
        CHOICE.store(if system { SYSTEM } else { MIMALLOC }, Ordering::Relaxed);
        system
    }

    fn wants_system() -> bool {
        // SAFETY: `getenv` reads the environment and allocates nothing. This
        // runs at the first allocation, before anything can have changed it.
        let value = unsafe { libc::getenv(c"HOT_ALLOCATOR".as_ptr()) };
        // SAFETY: a non-null result is a NUL-terminated string.
        !value.is_null() && unsafe { std::ffi::CStr::from_ptr(value) }.to_bytes() == b"system"
    }

    pub(super) struct Chosen;

    unsafe impl GlobalAlloc for Chosen {
        #[inline]
        unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
            if is_system() {
                unsafe { System.alloc(layout) }
            } else {
                unsafe { MI.alloc(layout) }
            }
        }

        #[inline]
        unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
            if is_system() {
                unsafe { System.dealloc(ptr, layout) }
            } else {
                unsafe { MI.dealloc(ptr, layout) }
            }
        }

        #[inline]
        unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
            if is_system() {
                unsafe { System.alloc_zeroed(layout) }
            } else {
                unsafe { MI.alloc_zeroed(layout) }
            }
        }

        #[inline]
        unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
            if is_system() {
                unsafe { System.realloc(ptr, layout, new_size) }
            } else {
                unsafe { MI.realloc(ptr, layout, new_size) }
            }
        }
    }
}
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

/// Arenas `limit_malloc_arenas` allows when `MALLOC_ARENA_MAX` is unset.
pub const DEFAULT_MALLOC_ARENAS: i32 = 4;

/// Cap glibc's malloc arenas at `max`, unless `MALLOC_ARENA_MAX` sets the cap.
/// It matters only when the process runs on the system allocator (see
/// `allocator_name`): mimalloc has no arenas of this kind.
/// glibc gives threads that contend for the heap arenas of their own, up to
/// eight per core, and keeps the memory they once held: a worker whose
/// threads ran a burst of large runs stays near its peak resident size. Call
/// it before the threads that do the allocating start. A no-op off glibc.
pub fn limit_malloc_arenas(max: i32) {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    if std::env::var_os("MALLOC_ARENA_MAX").is_none() {
        // SAFETY: mallopt only adjusts allocator parameters.
        unsafe {
            libc::mallopt(libc::M_ARENA_MAX, max);
        }
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    let _ = max;
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
