use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use page_runtime::ContextGraph;
use types::{AccessibilityNode, AccessibilityTarget, PageId};

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
fn context_queries_do_not_allocate_query_buffers_per_candidate() {
    let graph = ContextGraph::new();
    let page = PageId::new();
    let candidates = 2048;
    graph.record(
        &page,
        (0..candidates)
            .map(|ordinal| AccessibilityNode {
                name: Some("irrelevant label".into()),
                target: Some(AccessibilityTarget {
                    role: "button".into(),
                    accessible_name: "irrelevant label".into(),
                    ordinal: Some(ordinal),
                    frame_path: Vec::new(),
                }),
                ..AccessibilityNode::default()
            })
            .collect(),
    );
    ALLOCATIONS.with(|count| count.set(Some(0)));
    let answer = graph.ask(&page, "missing control");
    let allocations = ALLOCATIONS.with(|count| count.replace(None).unwrap());
    assert!(answer.is_none());
    eprintln!("Context query allocations for {candidates} candidates: {allocations}");
    assert!(
        allocations <= 4 * candidates + 8,
        "query buffers are allocated for each candidate: {allocations}"
    );
}
