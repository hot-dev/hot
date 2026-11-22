//! Bytes are arrays of numbers in JSON, and every recorded call with Bytes in it
//! is read back from JSON to be masked. That has to cost about what the Bytes
//! occupy: through `serde_json::Value` it cost hundreds of times more, so one
//! 31 MB response took 13 GB. Its own binary, for the counting allocator.

use hot::val::Val;
use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct CountingAllocator;

static CURRENT: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

fn grew(size: usize) {
    let current = CURRENT.fetch_add(size, Ordering::Relaxed) + size;
    PEAK.fetch_max(current, Ordering::Relaxed);
}

// SAFETY: every call goes straight to `System`; the counters only observe.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        grew(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        grew(new_size);
        CURRENT.fetch_sub(layout.size(), Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
fn reading_recorded_bytes_costs_about_their_size() {
    let size = 4 * 1024 * 1024;
    let bytes: Vec<u8> = (0..size).map(|index| (index % 251) as u8).collect();
    let json = serde_json::to_string(&Val::bytes(bytes.clone())).unwrap();

    let before = CURRENT.load(Ordering::Relaxed);
    PEAK.store(before, Ordering::Relaxed);
    let value: Val = serde_json::from_str(&json).unwrap();
    let cost = PEAK.load(Ordering::Relaxed) - before;

    assert!(matches!(&value, Val::Bytes(read) if **read == bytes));
    assert!(
        cost < 4 * size,
        "reading {size} bytes held {cost} bytes at its peak"
    );
}
