use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use task_scheduler::JobQueue;

struct CountingAllocator;

thread_local! {
    static ALLOCATED_BYTES: Cell<Option<usize>> = const { Cell::new(None) };
}

fn count_bytes(bytes: usize) {
    let _ = ALLOCATED_BYTES.try_with(|count| {
        if let Some(current) = count.get() {
            count.set(Some(current + bytes));
        }
    });
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_bytes(layout.size());
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count_bytes(layout.size());
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count_bytes(size);
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
fn empty_queue_allocation_does_not_scale_with_configured_capacity() {
    let allocated_for = |capacity| {
        ALLOCATED_BYTES.with(|count| count.set(Some(0)));
        let queue = JobQueue::new(capacity);
        let bytes = ALLOCATED_BYTES.with(|count| count.replace(None).unwrap());
        assert!(queue.is_empty());
        bytes
    };
    let small = allocated_for(32);
    let large = allocated_for(4096);
    eprintln!("Empty queue allocation bytes: capacity 32={small}, 4096={large}");
    assert!(
        large <= small + 4096,
        "empty queue allocation scales with its limit: 32={small}, 4096={large}"
    );
}
