use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use worker_pool::{har_document, HarEntry};

struct CountingAllocator;

thread_local! {
    static ALLOCATIONS: Cell<Option<usize>> = const { Cell::new(None) };
}

fn count_allocation() {
    let _ = ALLOCATIONS.try_with(|count| {
        if let Some(current) = count.get() {
            count.set(Some(current + 1));
        }
    });
}

unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count_allocation();
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        count_allocation();
        unsafe { System.realloc(ptr, layout, size) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

#[test]
fn har_generation_does_not_copy_the_completed_entry_tree() {
    let entries: Vec<_> = (0..512)
        .map(|index| HarEntry {
            url: format!("https://example.test/{index}"),
            method: "GET".into(),
            status: Some(200),
            status_text: Some("OK".into()),
            redirect_url: Some("https://example.test/next".into()),
            started_unix_ms: 1_700_000_000_123.0,
            elapsed_ms: Some(12.0),
            transfer_bytes: Some(3),
            mime_type: Some("text/plain".into()),
            error_text: None,
        })
        .collect();
    ALLOCATIONS.with(|count| count.set(Some(0)));
    let document = har_document(&entries, "https://example.test/");
    let allocations = ALLOCATIONS.with(|count| count.replace(None).unwrap());
    eprintln!(
        "HAR allocations for {} entries: {allocations}",
        entries.len()
    );
    assert_eq!(
        document["log"]["entries"].as_array().unwrap().len(),
        entries.len()
    );
    assert_eq!(
        document["log"]["entries"][511]["request"]["url"],
        entries[511].url
    );
    assert!(
        allocations <= 60 * entries.len(),
        "HAR generation allocates a second entry tree: {allocations}"
    );
}
