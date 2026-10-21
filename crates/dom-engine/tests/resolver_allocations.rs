use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

use dom_engine::{rank_candidates, Candidate, CandidateState, ResolutionPolicy};
use types::TargetSpec;

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
fn css_misses_do_not_allocate_per_candidate() {
    let candidates: Vec<_> = (0..2048)
        .map(|index| Candidate {
            id: index.to_string(),
            tag: Some("button".into()),
            css: None,
            test_id: None,
            role: None,
            name: None,
            label: None,
            text: String::new(),
            attributes: Default::default(),
            frame_path: Vec::new(),
            state: CandidateState {
                attached: true,
                visible: true,
                enabled: true,
            },
        })
        .collect();
    let target = TargetSpec {
        css: Some("input[data-kind='checkout']".into()),
        ..TargetSpec::default()
    };
    let policy = ResolutionPolicy::default();
    let allocations_for = |candidates: &[Candidate]| {
        ALLOCATIONS.with(|count| count.set(Some(0)));
        let ranked = rank_candidates(&target, candidates, &policy).unwrap();
        let count = ALLOCATIONS.with(|count| count.replace(None).unwrap());
        assert!(ranked.is_empty());
        count
    };

    let small = allocations_for(&candidates[..32]);
    let large = allocations_for(&candidates);
    eprintln!("CSS miss allocations: 32={small}, 2048={large}");
    assert!(
        large <= small + 8,
        "CSS misses allocate with candidate count: 32={small}, 2048={large}"
    );
}
