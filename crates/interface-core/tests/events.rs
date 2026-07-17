use std::time::Duration;

use interface_core::{Event, EventGapReason, EventStore, MAX_EVENT_PAYLOAD_BYTES};
use serde_json::json;
use tokio::time::timeout;
use types::EventCursor;

fn event(sequence: u64) -> Event {
    Event::new(
        "command.outcome",
        json!({
            "sequence": sequence,
            "authorization": "Bearer super-secret",
            "nested": { "cookie": "session=secret" }
        }),
    )
}

#[tokio::test]
async fn bounded_history_reports_a_gap_instead_of_skipping_events() {
    let events = EventStore::new(2);
    events.append(event(1)).await;
    events.append(event(2)).await;
    events.append(event(3)).await;

    let gap = events
        .read_after(EventCursor::default(), 10)
        .await
        .unwrap_err();
    assert_eq!(gap.reason, EventGapReason::HistoryLost);
    assert_eq!(gap.earliest_available, EventCursor(2));

    let batch = events.read_after(EventCursor(1), 10).await.unwrap();
    assert_eq!(
        batch
            .events
            .iter()
            .map(|event| event.cursor)
            .collect::<Vec<_>>(),
        vec![EventCursor(2), EventCursor(3)]
    );
}

#[tokio::test]
async fn append_is_globally_ordered_hard_bounded_and_sanitized_before_retention() {
    let events = EventStore::new(4);
    for sequence in 1..=6 {
        assert_eq!(events.append(event(sequence)).await, EventCursor(sequence));
    }

    let batch = events.read_after(EventCursor(2), 100).await.unwrap();
    assert_eq!(
        batch.events.len(),
        4,
        "the configured bound also caps a batch"
    );
    assert_eq!(batch.events[0].cursor, EventCursor(3));
    assert_eq!(batch.events[3].cursor, EventCursor(6));
    for retained in batch.events {
        assert_eq!(retained.payload["authorization"], "[REDACTED]");
        assert_eq!(retained.payload["nested"]["cookie"], "[REDACTED]");
    }
}

#[tokio::test]
async fn zero_read_limit_is_rejected_without_waiting() {
    let events = EventStore::new(4);
    let gap = timeout(
        Duration::from_millis(100),
        events.read_after(EventCursor::default(), 0),
    )
    .await
    .expect("invalid reads must not wait")
    .unwrap_err();
    assert_eq!(gap.reason, EventGapReason::InvalidLimit);
}

#[tokio::test]
async fn empty_reads_wait_without_losing_an_append_notification() {
    let events = EventStore::new(4);
    let waiter = tokio::spawn({
        let events = events.clone();
        async move { events.read_after(EventCursor::default(), 1).await }
    });
    tokio::task::yield_now().await;

    assert_eq!(events.append(event(1)).await, EventCursor(1));
    let batch = timeout(Duration::from_secs(1), waiter)
        .await
        .expect("append must wake a waiting reader")
        .unwrap()
        .unwrap();
    assert_eq!(batch.events.len(), 1);
    assert_eq!(batch.events[0].cursor, EventCursor(1));
}

#[tokio::test]
async fn concurrent_appends_receive_one_monotonic_cursor_sequence() {
    let events = EventStore::new(128);
    let mut tasks = Vec::new();
    for sequence in 0..64 {
        let events = events.clone();
        tasks.push(tokio::spawn(
            async move { events.append(event(sequence)).await },
        ));
    }
    let mut cursors = Vec::new();
    for task in tasks {
        cursors.push(task.await.unwrap().0);
    }
    cursors.sort_unstable();
    assert_eq!(cursors, (1..=64).collect::<Vec<_>>());
}

#[tokio::test]
async fn retained_payload_keys_are_bounded_before_entering_the_queue() {
    let events = EventStore::new(1);
    let oversized_key = "k".repeat(16 * 1024);
    events
        .append(Event::new(
            "bounded.keys",
            json!({ oversized_key: "value" }),
        ))
        .await;

    let batch = events.read_after(EventCursor::ZERO, 1).await.unwrap();
    let keys = batch.events[0].payload.as_object().unwrap().keys();
    assert!(keys.into_iter().all(|key| key.len() <= 128));
}

#[tokio::test]
async fn sensitive_keys_are_detected_before_key_truncation() {
    let events = EventStore::new(1);
    let sensitive_key = format!("{}token", "x".repeat(128));
    events
        .append(Event::new(
            "bounded.keys",
            json!({ sensitive_key: "must-not-be-retained" }),
        ))
        .await;

    let batch = events.read_after(EventCursor::ZERO, 1).await.unwrap();
    assert_eq!(
        batch.events[0]
            .payload
            .as_object()
            .unwrap()
            .values()
            .next()
            .unwrap(),
        "[REDACTED]"
    );
}

#[tokio::test]
async fn branching_payload_has_one_exact_aggregate_byte_budget_after_sanitizing() {
    let events = EventStore::new(2);
    let branch = (0..64)
        .map(|index| {
            json!({
                format!("field-{index}"): "x".repeat(4096),
                format!("authorization-{index}"): "Bearer branching-secret"
            })
        })
        .collect::<Vec<_>>();
    events
        .append(Event::new("branching", json!({ "branches": branch })))
        .await;

    let batch = events.read_after(EventCursor::ZERO, 1).await.unwrap();
    let serialized = serde_json::to_vec(&batch.events[0].payload).unwrap();
    assert!(serialized.len() <= MAX_EVENT_PAYLOAD_BYTES);
    let retained = String::from_utf8(serialized).unwrap();
    assert!(!retained.contains("branching-secret"));
    assert!(!retained.contains("Bearer"));
}
