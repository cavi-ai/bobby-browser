use std::alloc::{GlobalAlloc, Layout, System};
use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use types::{AttemptId, CommandId, CommandPhase};
use workflow_journal::{CommandJournal, JournalRecord, JsonlJournal, PreparedResult};

// One test in this binary isolates allocations across Tokio's filesystem workers.
// The unescaped JSON string allocates its exact length when decoded into Value.
// An odd length separates that allocation from power-of-two I/O buffer growth.
const PAYLOAD_LEN: usize = 200_003;
static TRACKING: AtomicBool = AtomicBool::new(false);
static PAYLOAD_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
struct AllocationTracker;

fn record_allocation(size: usize) {
    if TRACKING.load(Ordering::Relaxed) && size == PAYLOAD_LEN {
        PAYLOAD_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
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
fn opening_validates_and_indexes_without_decoding_the_payload_twice() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("commands.jsonl");
    let id = CommandId::new();
    let mut file = std::fs::File::create(&path).unwrap();
    let record = JournalRecord {
        sequence: 7,
        recorded_at: chrono::Utc::now(),
        command_id: id.clone(),
        phase: CommandPhase::Prepared,
        envelope: None,
        outcome: None,
        prepared_result: Some(PreparedResult {
            command_id: id.clone(),
            attempt_id: AttemptId::new(),
            state_version: 3,
            state_delta: serde_json::json!({"value": "x".repeat(PAYLOAD_LEN)}),
            evidence: Vec::new(),
            artifact_id: None,
            artifact_sha256: None,
            artifact_bytes: None,
            artifact_staging_id: None,
            download: None,
        }),
    };
    serde_json::to_writer(&mut file, &record).unwrap();
    file.write_all(b"\n").unwrap();
    file.sync_all().unwrap();
    drop(file);
    let original = std::fs::read(&path).unwrap();

    TRACKING.store(true, Ordering::Relaxed);
    let journal = runtime.block_on(JsonlJournal::open(&path)).unwrap();
    TRACKING.store(false, Ordering::Relaxed);
    let allocations = PAYLOAD_ALLOCATIONS.load(Ordering::Relaxed);
    assert_eq!(
        allocations, 1,
        "opening materialized the payload {allocations} times"
    );

    let scan = runtime.block_on(journal.history(id.clone())).unwrap();
    assert_eq!(scan.records.len(), 1);
    assert_eq!(scan.records[0].sequence, 7);
    assert_eq!(scan.records[0].phase, CommandPhase::Prepared);
    assert_eq!(
        scan.records[0]
            .prepared_result
            .as_ref()
            .unwrap()
            .state_delta["value"],
        "x".repeat(PAYLOAD_LEN)
    );
    assert_eq!(scan.incompatible_records, 0);
    assert!(runtime.block_on(journal.archives()).is_empty());
    assert_eq!(std::fs::read(&path).unwrap(), original);

    let mut next = record;
    next.phase = CommandPhase::Executing;
    next.prepared_result = None;
    runtime.block_on(journal.append(next)).unwrap();
    drop(journal);
    let reopened = runtime.block_on(JsonlJournal::open(&path)).unwrap();
    let scan = runtime.block_on(reopened.history(id)).unwrap();
    assert_eq!(scan.records.len(), 2);
    assert_eq!(scan.records[1].sequence, 8);
    assert_eq!(scan.records[1].phase, CommandPhase::Executing);

    // The same payload in a preserved archive must also be decoded only once.
    let archive_root = tempfile::tempdir().unwrap();
    let archive_path = archive_root.path().join("commands.jsonl");
    let archive = archive_root.path().join("commands.jsonl.archive-fixture");
    std::fs::write(&archive, &original).unwrap();
    PAYLOAD_ALLOCATIONS.store(0, Ordering::Relaxed);
    TRACKING.store(true, Ordering::Relaxed);
    let journal = runtime.block_on(JsonlJournal::open(&archive_path)).unwrap();
    TRACKING.store(false, Ordering::Relaxed);
    let allocations = PAYLOAD_ALLOCATIONS.load(Ordering::Relaxed);
    assert_eq!(
        allocations, 1,
        "archive loading materialized the payload {allocations} times"
    );
    assert_eq!(runtime.block_on(journal.archives()), vec![archive.clone()]);
    assert_eq!(std::fs::read(&archive).unwrap(), original);
    assert!(std::fs::read(&archive_path).unwrap().is_empty());
    let archived = runtime
        .block_on(journal.history(scan.records[0].command_id.clone()))
        .unwrap();
    assert_eq!(archived.records.len(), 1);
    assert_eq!(archived.records[0].sequence, 7);
    assert_eq!(archived.records[0].phase, CommandPhase::Prepared);
    assert_eq!(
        archived.records[0]
            .prepared_result
            .as_ref()
            .unwrap()
            .state_delta["value"],
        "x".repeat(PAYLOAD_LEN)
    );
    assert!(archived.incompatible_records > 0);
    assert!(runtime
        .block_on(journal.append(archived.records[0].clone()))
        .is_err());
}
