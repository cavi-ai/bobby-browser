use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use task_scheduler::{Job, JobEvent, JobPriority, JobStore, JournalJobStore, JournalRecord};

// This binary contains one test, so allocation measurements cannot overlap
// another test. Track Tokio's filesystem workers too, not just the test thread.
struct AllocationTracker;
static TRACKING: AtomicBool = AtomicBool::new(false);
static LARGEST_ALLOCATION: AtomicUsize = AtomicUsize::new(0);

fn record_allocation(size: usize) {
    if TRACKING.load(Ordering::Relaxed) {
        LARGEST_ALLOCATION.fetch_max(size, Ordering::Relaxed);
    }
}

unsafe impl GlobalAlloc for AllocationTracker {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        // SAFETY: forward the unchanged layout to the system allocator.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record_allocation(layout.size());
        // SAFETY: forward the unchanged layout to the system allocator.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: all allocations above are owned by the system allocator.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        record_allocation(new_size);
        // SAFETY: forward the original allocation and requested size unchanged.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static ALLOCATOR: AllocationTracker = AllocationTracker;

fn measure<T>(operation: impl FnOnce() -> T) -> (T, usize) {
    LARGEST_ALLOCATION.store(0, Ordering::Relaxed);
    TRACKING.store(true, Ordering::Relaxed);
    let result = operation();
    TRACKING.store(false, Ordering::Relaxed);
    (result, LARGEST_ALLOCATION.load(Ordering::Relaxed))
}

#[test]
fn inspection_and_recovery_do_not_allocate_a_whole_journal_buffer() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("jobs.jsonl");
    let mut job = Job::new("first".into(), serde_json::json!({}), JobPriority::Normal);
    let mut file = std::fs::File::create(&path).unwrap();
    for sequence in 0..2048 {
        if sequence == 2047 {
            job.name = "latest".into();
        }
        serde_json::to_writer(
            &mut file,
            &JournalRecord {
                schema_version: 1,
                sequence,
                recorded_at: chrono::Utc::now(),
                event: JobEvent::Submitted,
                job: job.clone(),
            },
        )
        .unwrap();
        file.write_all(b"\n").unwrap();
    }
    drop(file);
    let bytes = std::fs::metadata(&path).unwrap().len();
    assert!(bytes > 512 * 1024);

    let (health, inspect_allocation) =
        measure(|| runtime.block_on(JournalJobStore::inspect(&path)).unwrap());
    assert_eq!(health.records, 2048);
    assert_eq!(health.bytes, bytes);
    assert_eq!(health.incompatible_records, 0);
    assert!(!health.torn_tail);

    let (store, open_allocation) =
        measure(|| runtime.block_on(JournalJobStore::open(&path)).unwrap());
    let restored = runtime.block_on(store.load_all()).unwrap();
    assert_eq!(restored.len(), 1);
    assert_eq!(restored[0].name, "latest");
    assert_eq!(std::fs::metadata(&path).unwrap().len(), bytes);

    // Generous allowance for I/O buffers and the single retained job. Neither
    // operation should allocate storage proportional to this small-record history.
    assert!(
        inspect_allocation < 128 * 1024,
        "inspection allocated {inspect_allocation} bytes at once"
    );
    assert!(
        open_allocation < 128 * 1024,
        "recovery allocated {open_allocation} bytes at once"
    );
}
