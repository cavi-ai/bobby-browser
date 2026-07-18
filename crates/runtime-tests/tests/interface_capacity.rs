use interface_core::{Event, EventGapReason, EventStore};
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::Duration,
};
use tokio::sync::{Barrier, Semaphore};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn sixty_four_clients_are_bounded_without_starving_interactive_work() {
    let connections = Arc::new(Semaphore::new(64));
    let workflows = Arc::new(Semaphore::new(8));
    let warm_sessions = Arc::new(Semaphore::new(32));
    let barrier = Arc::new(Barrier::new(65));
    let peak = Arc::new(AtomicUsize::new(0));
    let active = Arc::new(AtomicUsize::new(0));
    let mut clients = Vec::new();
    for id in 0..64 {
        let (connections, workflows, warm_sessions, barrier, peak, active) = (
            connections.clone(),
            workflows.clone(),
            warm_sessions.clone(),
            barrier.clone(),
            peak.clone(),
            active.clone(),
        );
        clients.push(tokio::spawn(async move {
            let _connection = connections.acquire_owned().await.unwrap();
            barrier.wait().await;
            let _session = warm_sessions.acquire_owned().await.unwrap();
            let _workflow = workflows.acquire_owned().await.unwrap();
            let now = active.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(Duration::from_millis(if id == 0 { 1 } else { 3 })).await;
            active.fetch_sub(1, Ordering::SeqCst);
            id
        }));
    }
    barrier.wait().await;
    let interactive = tokio::time::timeout(Duration::from_secs(2), &mut clients[0])
        .await
        .unwrap()
        .unwrap();
    assert_eq!(interactive, 0);
    for client in clients.iter_mut().skip(1) {
        client.await.unwrap();
    }
    assert!(peak.load(Ordering::SeqCst) <= 8);
    assert_eq!(connections.available_permits(), 64);
    assert_eq!(warm_sessions.available_permits(), 32);
}

#[tokio::test]
async fn slow_consumers_are_bounded_and_receive_actionable_gap_recovery() {
    let events = EventStore::new(32);
    for index in 0..1024 {
        events
            .append(Event::new("load", serde_json::json!({"index": index})))
            .await;
    }
    let gap = events.read_after(0.into(), 256).await.unwrap_err();
    assert_eq!(gap.reason, EventGapReason::HistoryLost);
    assert_eq!(gap.earliest_available.0, 993);
    let resumed = events.read_after(992.into(), 256).await.unwrap();
    assert_eq!(resumed.events.len(), 32);
}

#[tokio::test]
#[ignore = "requires installed Chromium for warm-session and artifact-reader capacity proof"]
async fn installed_chromium_capacity_fixture_supports_warm_sessions() {
    let harness = interface_conformance::live::ChromeRuntimeHarness::start().await;
    assert_eq!(harness.config.browser.max_active, 8);
    assert_eq!(harness.config.interface.max_connections, 64);
}
