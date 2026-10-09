use std::alloc::{GlobalAlloc, Layout, System};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use chrono::{Duration, Utc};
use interface_core::{canonical_sha256, IdempotencyReservation, IdempotencyStore, RetainedOutcome};
use serde::{Deserialize, Serialize};
use types::{CorrelationId, IdempotencyKey, InterfaceOperation, PrincipalId};

// A single test isolates allocations across Tokio's filesystem workers. The
// odd payload size distinguishes string materialization from I/O buffer growth.
const PAYLOAD_LEN: usize = 20_003;
const ENTRIES: usize = 128;
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

#[derive(Clone, Serialize, Deserialize)]
struct Outcome(String);

impl RetainedOutcome for Outcome {
    fn releases_reservation(&self) -> bool {
        false
    }
    fn safety_relevant(&self) -> bool {
        false
    }
}

async fn open(path: &Path) -> IdempotencyStore<Outcome> {
    IdempotencyStore::open_durable(path, |value| async move {
        serde_json::from_value(value)
            .map(Some)
            .map_err(std::io::Error::other)
    })
    .await
    .unwrap()
}

async fn reserve(
    store: &IdempotencyStore<Outcome>,
    owner: &PrincipalId,
    key: &str,
) -> IdempotencyReservation<Outcome> {
    let now = Utc::now();
    store
        .reserve(
            owner.clone(),
            IdempotencyKey::try_from(key).unwrap(),
            InterfaceOperation::SubmitCommand,
            [0; 32],
            now,
            now + Duration::seconds(2),
            CorrelationId::new(),
        )
        .await
        .unwrap()
}

#[test]
fn ledger_checkpoints_and_loading_do_not_clone_entries_for_checksums() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.json");
    let owner = PrincipalId::from_uuid(uuid::Uuid::nil());
    let payloads: Vec<_> = (0..ENTRIES)
        .map(|index| format!("{index:03}{}", "x".repeat(PAYLOAD_LEN - 3)))
        .collect();
    let entries: Vec<_> = payloads
        .iter()
        .enumerate()
        .map(|(index, payload)| {
            serde_json::json!({
                "principalId": owner, "key": format!("old-{index}"),
                "operation": InterfaceOperation::SubmitCommand, "canonicalSha256": vec![0; 32],
                "expiresAt": Utc::now() + Duration::hours(1), "lastUsed": index + 1,
                "state": {"kind": "retained", "value": payload}
            })
        })
        .collect();
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 1, "entries": entries
        }))
        .unwrap(),
    )
    .unwrap();
    let store = runtime.block_on(open(&path));

    TRACKING.store(true, Ordering::Relaxed);
    let reservation = runtime.block_on(reserve(&store, &owner, "fresh"));
    TRACKING.store(false, Ordering::Relaxed);
    let allocations = PAYLOAD_ALLOCATIONS.load(Ordering::Relaxed);
    eprintln!("checkpoint creation payload allocations: {allocations}");
    assert!(
        allocations < ENTRIES * 3,
        "checkpoint creation materialized retained payloads {allocations} times"
    );

    let bytes = std::fs::read(&path).unwrap();
    let header: serde_json::Value =
        serde_json::from_slice(bytes.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(header["schemaVersion"], 2);
    let expected = canonical_sha256(&serde_json::json!({
        "schemaVersion": 2, "entries": header["entries"]
    }))
    .unwrap();
    assert_eq!(header["sha256"], serde_json::json!(expected));
    let IdempotencyReservation::Acquired(permit) = reservation else {
        panic!("fresh reservation")
    };
    runtime
        .block_on(store.finish(permit, Outcome("fresh".into()), Utc::now()))
        .unwrap();
    drop(store);
    let original = std::fs::read(&path).unwrap();

    PAYLOAD_ALLOCATIONS.store(0, Ordering::Relaxed);
    TRACKING.store(true, Ordering::Relaxed);
    let store = runtime.block_on(open(&path));
    TRACKING.store(false, Ordering::Relaxed);
    let allocations = PAYLOAD_ALLOCATIONS.load(Ordering::Relaxed);
    eprintln!("ledger loading payload allocations: {allocations}");
    assert!(
        allocations < ENTRIES * 3,
        "ledger loading materialized retained payloads {allocations} times"
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
    assert!(store.integrity_issue().is_none());
    for (index, payload) in payloads.iter().enumerate() {
        let IdempotencyReservation::Replay(outcome) =
            runtime.block_on(reserve(&store, &owner, &format!("old-{index}")))
        else {
            panic!("retained key must replay")
        };
        assert_eq!(&outcome.0, payload);
    }
    let IdempotencyReservation::Replay(outcome) =
        runtime.block_on(reserve(&store, &owner, "fresh"))
    else {
        panic!("fresh outcome must survive restart")
    };
    assert_eq!(outcome.0, "fresh");
}
