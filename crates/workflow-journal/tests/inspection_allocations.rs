use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use types::{CommandId, CommandPhase};
use workflow_journal::{CommandJournal, JournalRecord, JsonlJournal};

// One test in this binary isolates measurements from other tests, including
// allocations made by Tokio's filesystem workers.
struct AllocationTracker;
static TRACKING: AtomicBool = AtomicBool::new(false);
static LARGEST_ALLOCATION: AtomicUsize = AtomicUsize::new(0);
static PATH_ALLOCATION_SIZE: AtomicUsize = AtomicUsize::new(0);
static PATH_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

fn record_allocation(size: usize) {
    if TRACKING.load(Ordering::Relaxed) {
        LARGEST_ALLOCATION.fetch_max(size, Ordering::Relaxed);
        if size == PATH_ALLOCATION_SIZE.load(Ordering::Relaxed) {
            PATH_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        }
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

#[test]
fn inspection_and_archived_reads_avoid_unrelated_history_allocations() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("commands.jsonl");
    let command_id = CommandId::new();
    let mut file = std::fs::File::create(&path).unwrap();
    for sequence in 0..2048 {
        serde_json::to_writer(
            &mut file,
            &JournalRecord {
                sequence,
                recorded_at: chrono::Utc::now(),
                command_id: command_id.clone(),
                phase: CommandPhase::Accepted,
                envelope: None,
                outcome: None,
                prepared_result: None,
            },
        )
        .unwrap();
        file.write_all(b"\n").unwrap();
    }
    drop(file);
    let bytes = std::fs::metadata(&path).unwrap().len();

    TRACKING.store(true, Ordering::Relaxed);
    let health = runtime.block_on(JsonlJournal::inspect(&path)).unwrap();
    TRACKING.store(false, Ordering::Relaxed);
    let largest = LARGEST_ALLOCATION.load(Ordering::Relaxed);

    assert_eq!(health.records, 2048);
    assert_eq!(health.bytes, bytes);
    assert_eq!(health.incompatible_records, 0);
    assert!(!health.torn_tail);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), bytes);
    assert!(
        largest < 128 * 1024,
        "inspection allocated {largest} bytes at once"
    );

    let archive_path = root.path().join("archived.jsonl");
    let archive = root.path().join("archived.jsonl.archive-fixture");
    let mut original = serde_json::to_vec(&JournalRecord {
        sequence: 0,
        recorded_at: chrono::Utc::now(),
        command_id: command_id.clone(),
        phase: CommandPhase::Accepted,
        envelope: None,
        outcome: None,
        prepared_result: None,
    })
    .unwrap();
    original.push(b'\n');
    std::fs::write(&archive, &original).unwrap();
    let journal = runtime.block_on(JsonlJournal::open(&archive_path)).unwrap();

    // A missing delimiter must not let this indexed read grow with newly
    // appended archive bytes. Those bytes were never part of the indexed line.
    let mut changed = original;
    *changed.last_mut().unwrap() = b' ';
    changed.resize(changed.len() + 1024 * 1024, b' ');
    std::fs::write(&archive, &changed).unwrap();
    LARGEST_ALLOCATION.store(0, Ordering::Relaxed);
    TRACKING.store(true, Ordering::Relaxed);
    let scan = runtime
        .block_on(journal.history(command_id.clone()))
        .unwrap();
    TRACKING.store(false, Ordering::Relaxed);
    let largest = LARGEST_ALLOCATION.load(Ordering::Relaxed);
    assert!(
        largest < 128 * 1024,
        "archived history allocated {largest} bytes at once"
    );
    assert_eq!(scan.records.len(), 1);
    assert_eq!(scan.records[0].command_id, command_id);
    assert!(scan.incompatible_records > 0);
    assert!(scan.torn_tail);
    assert_eq!(std::fs::read(&archive).unwrap(), changed);

    let indexed_path = root.path().join("indexed.jsonl");
    let mut archive_name = format!("indexed.jsonl.archive-{}", "x".repeat(61));
    let mut indexed_archive = root.path().join(&archive_name);
    // An odd path length separates PathBuf clones from power-of-two buffers.
    if indexed_archive
        .as_os_str()
        .as_encoded_bytes()
        .len()
        .is_multiple_of(2)
    {
        archive_name.push('x');
        indexed_archive = root.path().join(archive_name);
    }
    std::fs::copy(&path, &indexed_archive).unwrap();
    PATH_ALLOCATION_SIZE.store(
        indexed_archive.as_os_str().as_encoded_bytes().len(),
        Ordering::Relaxed,
    );
    PATH_ALLOCATIONS.store(0, Ordering::Relaxed);
    TRACKING.store(true, Ordering::Relaxed);
    let indexed = runtime.block_on(JsonlJournal::open(&indexed_path)).unwrap();
    TRACKING.store(false, Ordering::Relaxed);
    let path_allocations = PATH_ALLOCATIONS.load(Ordering::Relaxed);
    assert!(
        path_allocations < 64,
        "opening one archive with 2048 records made {path_allocations} path-sized allocations"
    );
    assert_eq!(
        runtime.block_on(indexed.archives()),
        vec![indexed_archive.clone()]
    );
    let scan = runtime
        .block_on(indexed.history(command_id.clone()))
        .unwrap();
    assert_eq!(scan.records.len(), 2048);
    assert!(scan.incompatible_records > 0);
    for (sequence, record) in scan.records.iter().enumerate() {
        assert_eq!(record.sequence, sequence as u64);
        assert_eq!(record.command_id, command_id);
    }
    assert_eq!(
        std::fs::read(&indexed_archive).unwrap(),
        std::fs::read(&path).unwrap()
    );
}
