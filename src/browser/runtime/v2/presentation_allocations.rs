//! Test-only allocation upper bound for the synchronous constructor. Counting
//! every successful allocation, without subtracting frees, is conservative for
//! its additional peak; all unrelated test threads leave the counter disabled.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

thread_local! {
    static TRACKING: Cell<(bool, usize)> = const { Cell::new((false, 0)) };
}
struct AllocationProbe;

fn allocated(bytes: usize) {
    let _ = TRACKING.try_with(|state| {
        let (active, total) = state.get();
        if active {
            state.set((true, total.saturating_add(bytes)));
        }
    });
}

// SAFETY: The wrapper forwards each allocation operation, layout and pointer
// unchanged to System. The counter uses allocation-free, const-initialized TLS.
unsafe impl GlobalAlloc for AllocationProbe {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { System.alloc_zeroed(layout) };
        if !pointer.is_null() {
            allocated(layout.size());
        }
        pointer
    }
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { System.dealloc(pointer, layout) };
    }
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        let result = unsafe { System.realloc(pointer, layout, size) };
        if !result.is_null() {
            allocated(size);
        }
        result
    }
}

#[global_allocator]
static PROBE: AllocationProbe = AllocationProbe;

pub(super) fn measure<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            TRACKING.with(|state| state.set((false, 0)));
        }
    }
    TRACKING.with(|state| {
        assert!(!state.get().0);
        state.set((true, 0));
    });
    let guard = Reset;
    let result = operation();
    let allocated = TRACKING.with(|state| state.get().1);
    drop(guard);
    (result, allocated)
}
