use std::alloc::{GlobalAlloc, Layout, System};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use context_store::{
    ContextStore, ControlContext, FormContext, IntentStats, PageContext, SiteContext,
};

// One isolated test includes Tokio's filesystem workers. The odd string size
// distinguishes structural payload copies from serialization buffer growth.
const PAYLOAD_LEN: usize = 10_003;
const CONTROLS: usize = 64;
static TRACKING: AtomicBool = AtomicBool::new(false);
static PAYLOAD_ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
struct AllocationTracker;

fn record(size: usize) {
    if TRACKING.load(Ordering::Relaxed) && size == PAYLOAD_LEN {
        PAYLOAD_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
    }
}

unsafe impl GlobalAlloc for AllocationTracker {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: forward the unchanged layout to the system allocator.
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        record(layout.size());
        // SAFETY: forward the unchanged layout to the system allocator.
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: every allocation above belongs to the system allocator.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        record(size);
        // SAFETY: forward the original allocation and requested size unchanged.
        unsafe { System.realloc(ptr, layout, size) }
    }
}

#[global_allocator]
static ALLOCATOR: AllocationTracker = AllocationTracker;

fn fixture() -> SiteContext {
    let controls = (0..CONTROLS)
        .map(|index| ControlContext {
            role: "textbox".into(),
            accessible_name: format!("{index:03}{}", "x".repeat(PAYLOAD_LEN - 3)),
            ordinal: Some(index as u32),
            form_membership: "form".into(),
            intents: BTreeMap::from([(
                "fill".into(),
                IntentStats {
                    success_count: 1,
                    last_verified_day: Some(if index < CONTROLS / 2 { 10 } else { 100 }),
                    ..Default::default()
                },
            )]),
        })
        .collect();
    SiteContext {
        pages: BTreeMap::from([(
            "/form".into(),
            PageContext {
                forms: BTreeMap::from([("form".into(), FormContext { controls })]),
            },
        )]),
        ..Default::default()
    }
}

fn expected_bytes(site: &SiteContext) -> Vec<u8> {
    let mut bytes = br#"{"schema":1,"site_key":"site","site":"#.to_vec();
    bytes.extend(serde_json::to_vec(site).unwrap());
    bytes.push(b'}');
    bytes
}

#[test]
fn listing_flush_and_retention_avoid_redundant_site_copies() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (store, _) = runtime
        .block_on(ContextStore::open(dir.path(), "profile"))
        .unwrap();
    let mut expected = fixture();
    runtime.block_on(store.upsert_site("site", expected.clone()));
    let path = store.root().join("73697465.json");

    TRACKING.store(true, Ordering::Relaxed);
    let listed = runtime.block_on(store.list_sites());
    TRACKING.store(false, Ordering::Relaxed);
    let listing_allocations = PAYLOAD_ALLOCATIONS.swap(0, Ordering::Relaxed);
    assert_eq!(listed, vec!["site"]);

    TRACKING.store(true, Ordering::Relaxed);
    let failed = runtime.block_on(store.flush());
    TRACKING.store(false, Ordering::Relaxed);
    let flush_allocations = PAYLOAD_ALLOCATIONS.swap(0, Ordering::Relaxed);
    assert!(failed.is_empty());
    assert_eq!(std::fs::read(&path).unwrap(), expected_bytes(&expected));
    assert_eq!(runtime.block_on(store.usage()).pending_changes, 0);

    TRACKING.store(true, Ordering::Relaxed);
    let removed = runtime.block_on(store.sweep(30, 100)).unwrap();
    TRACKING.store(false, Ordering::Relaxed);
    let sweep_allocations = PAYLOAD_ALLOCATIONS.load(Ordering::Relaxed);
    assert_eq!(removed, (CONTROLS / 2) as u64);
    expected
        .pages
        .get_mut("/form")
        .unwrap()
        .forms
        .get_mut("form")
        .unwrap()
        .controls
        .drain(..CONTROLS / 2);
    assert_eq!(std::fs::read(&path).unwrap(), expected_bytes(&expected));
    drop(store);
    let (store, report) = runtime
        .block_on(ContextStore::open(dir.path(), "profile"))
        .unwrap();
    assert_eq!(report.sites_loaded, 1);
    assert!(report.skipped.is_empty());
    assert_eq!(runtime.block_on(store.site("site")), Some(expected));

    eprintln!("payload allocations: listing={listing_allocations}, flush={flush_allocations}, retention={sweep_allocations}");
    assert_eq!(listing_allocations, 0, "listing copied resident context");
    assert!(
        flush_allocations < CONTROLS * 2,
        "flush copied the serialization payload"
    );
    assert!(
        sweep_allocations < CONTROLS * 2,
        "retention copied the serialization payload"
    );
}
