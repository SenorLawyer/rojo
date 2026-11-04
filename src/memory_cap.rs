//! A global allocator that can cap how much memory the process allocates.
//!
//! Only the child process that checks untrusted models turns the cap on. A
//! model can declare sizes that make a decoder allocate gigabytes, and with
//! the cap that allocation fails inside the child instead of exhausting the
//! machine. Until the cap is set, allocation only adds one atomic load.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    ptr,
    sync::atomic::{AtomicUsize, Ordering::Relaxed},
};

pub struct CappedAllocator;

/// The cap in bytes, or 0 for none.
static LIMIT: AtomicUsize = AtomicUsize::new(0);
/// Bytes allocated since the cap was set. Memory allocated earlier is not
/// counted, and freeing it saturates at zero, so the count can only err low.
static USED: AtomicUsize = AtomicUsize::new(0);

/// Fails every allocation that would take the process past `bytes`.
pub fn set_memory_limit(bytes: usize) {
    USED.store(0, Relaxed);
    LIMIT.store(bytes, Relaxed);
}

fn reserve(size: usize) -> bool {
    let limit = LIMIT.load(Relaxed);
    if limit == 0 {
        return true;
    }
    let mut used = USED.load(Relaxed);
    loop {
        let Some(total) = used.checked_add(size).filter(|total| *total <= limit) else {
            return false;
        };
        match USED.compare_exchange_weak(used, total, Relaxed, Relaxed) {
            Ok(_) => return true,
            Err(current) => used = current,
        }
    }
}

fn release(size: usize) {
    if LIMIT.load(Relaxed) == 0 {
        return;
    }
    let mut used = USED.load(Relaxed);
    while let Err(current) =
        USED.compare_exchange_weak(used, used.saturating_sub(size), Relaxed, Relaxed)
    {
        used = current;
    }
}

unsafe impl GlobalAlloc for CappedAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if !reserve(layout.size()) {
            return ptr::null_mut();
        }
        let pointer = System.alloc(layout);
        if pointer.is_null() {
            release(layout.size());
        }
        pointer
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        if !reserve(layout.size()) {
            return ptr::null_mut();
        }
        let pointer = System.alloc_zeroed(layout);
        if pointer.is_null() {
            release(layout.size());
        }
        pointer
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        System.dealloc(pointer, layout);
        release(layout.size());
    }

    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let old_size = layout.size();
        if new_size > old_size && !reserve(new_size - old_size) {
            return ptr::null_mut();
        }
        let new_pointer = System.realloc(pointer, layout, new_size);
        if new_pointer.is_null() {
            if new_size > old_size {
                release(new_size - old_size);
            }
        } else if new_size < old_size {
            release(old_size - new_size);
        }
        new_pointer
    }
}
