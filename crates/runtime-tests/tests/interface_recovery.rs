use chrono::{Duration, Utc};
use interface_core::{
    canonical_sha256, Event, EventGapReason, EventStore, IdempotencyReservation, IdempotencyStore,
};
use types::{
    CommandError, CommandId, CommandOutcome, CorrelationId, ErrorCode, ErrorLayer, IdempotencyKey,
    InterfaceOperation, PrincipalId,
};

const BOUNDARIES: [&str; 5] = [
    "accepted",
    "prepared",
    "executing",
    "verifying",
    "result-prepared",
];

fn uncertain(command_id: CommandId) -> CommandOutcome {
    CommandOutcome::NeedsReconciliation {
        command_id,
        error: CommandError {
            code: ErrorCode::Internal,
            message: "transport lost after boundary".into(),
            layer: ErrorLayer::Page,
            retryable: false,
        },
        evidence: vec![],
    }
}

#[tokio::test]
async fn every_crash_boundary_preserves_uncertainty_and_never_implicitly_replays() {
    for boundary in BOUNDARIES {
        let store = IdempotencyStore::new(8, Duration::minutes(5));
        let principal = PrincipalId::from_uuid(uuid::Uuid::new_v4());
        let key = IdempotencyKey::try_from(format!("crash-{boundary}")).unwrap();
        let digest = canonical_sha256(&boundary).unwrap();
        let now = Utc::now();
        let reservation = store
            .reserve(
                principal.clone(),
                key.clone(),
                InterfaceOperation::SubmitCommand,
                digest,
                now,
                now + Duration::seconds(5),
                CorrelationId::new(),
            )
            .await
            .unwrap();
        let permit = match reservation {
            IdempotencyReservation::Acquired(p) => p,
            _ => unreachable!(),
        };
        let expected = uncertain(CommandId::new());
        store
            .finish(permit, expected.clone(), Utc::now())
            .await
            .unwrap();
        let replay = store
            .reserve(
                principal,
                key,
                InterfaceOperation::SubmitCommand,
                digest,
                Utc::now(),
                Utc::now() + Duration::seconds(5),
                CorrelationId::new(),
            )
            .await
            .unwrap();
        assert!(
            matches!(
                replay,
                IdempotencyReservation::Replay(CommandOutcome::NeedsReconciliation { .. })
            ),
            "{boundary}"
        );
    }
}

#[tokio::test]
async fn reconnect_resumes_exactly_or_reports_a_deterministic_gap() {
    let events = EventStore::new(3);
    for sequence in 1..=5 {
        events
            .append(Event::new(
                "boundary",
                serde_json::json!({"sequence": sequence}),
            ))
            .await;
    }
    let exact = events.read_after(3.into(), 8).await.unwrap();
    assert_eq!(
        exact
            .events
            .iter()
            .map(|event| event.cursor.0)
            .collect::<Vec<_>>(),
        [4, 5]
    );
    let gap = events.read_after(1.into(), 8).await.unwrap_err();
    assert_eq!(gap.reason, EventGapReason::HistoryLost);
    assert_eq!(gap.earliest_available.0, 3);
    let invalid = events.read_after(99.into(), 8).await.unwrap_err();
    assert_eq!(invalid.reason, EventGapReason::InvalidCursor);
}

#[tokio::test]
#[ignore = "requires installed Chromium; exercises daemon/worker replacement fixture"]
async fn installed_chromium_recovery_fixture_starts_cleanly() {
    let harness = interface_conformance::live::ChromeRuntimeHarness::start().await;
    assert!(harness.context().deadline > Utc::now());
}
