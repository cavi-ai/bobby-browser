use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use chrono::{Duration, Utc};
use interface_core::{canonical_sha256, IdempotencyReservation, IdempotencyStore};
use tokio::sync::Barrier;
use types::{
    AttemptId, CommandError, CommandId, CommandOutcome, CorrelationId, ErrorCode, ErrorLayer,
    IdempotencyKey, InterfaceError, InterfaceErrorCode, InterfaceOperation, PrincipalId,
};

fn principal(value: &str) -> PrincipalId {
    PrincipalId::from_uuid(uuid::Uuid::parse_str(value).unwrap())
}

fn key(value: &str) -> IdempotencyKey {
    IdempotencyKey::try_from(value).unwrap()
}

fn completed(command_id: CommandId) -> CommandOutcome {
    CommandOutcome::Completed {
        command_id,
        evidence: Vec::new(),
    }
}

fn command_error(retryable: bool) -> CommandError {
    CommandError {
        code: ErrorCode::Internal,
        message: "test failure".into(),
        layer: ErrorLayer::Page,
        retryable,
    }
}

fn reconciliation(command_id: CommandId) -> CommandOutcome {
    CommandOutcome::NeedsReconciliation {
        command_id,
        error: command_error(false),
        evidence: Vec::new(),
    }
}

async fn reserve(
    store: &IdempotencyStore,
    principal: PrincipalId,
    key: IdempotencyKey,
    digest: [u8; 32],
    correlation_id: CorrelationId,
) -> Result<IdempotencyReservation, InterfaceError> {
    let now = Utc::now();
    store
        .reserve(
            principal,
            key,
            InterfaceOperation::SubmitCommand,
            digest,
            now,
            now + Duration::seconds(5),
            correlation_id,
        )
        .await
}

struct DispatchCase {
    store: IdempotencyStore,
    principal: PrincipalId,
    key: IdempotencyKey,
    digest: [u8; 32],
    correlation_id: CorrelationId,
}

async fn counted_dispatch(
    case: DispatchCase,
    calls: Arc<AtomicUsize>,
    barrier: Arc<Barrier>,
    outcome: CommandOutcome,
) -> Result<CommandOutcome, InterfaceError> {
    barrier.wait().await;
    match reserve(
        &case.store,
        case.principal,
        case.key,
        case.digest,
        case.correlation_id,
    )
    .await?
    {
        IdempotencyReservation::Replay(outcome) => Ok(outcome),
        IdempotencyReservation::Acquired(permit) => {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            case.store
                .finish(permit, outcome.clone(), Utc::now())
                .await?;
            Ok(outcome)
        }
    }
}

#[tokio::test]
async fn committed_success_replays_and_conflicts_preserve_request_correlation() {
    let store = IdempotencyStore::with_global_capacity(8, 16, Duration::minutes(5));
    let principal = principal("10000000-0000-0000-0000-000000000001");
    let key = key("same-request");
    let digest = canonical_sha256(&serde_json::json!({"value": 1})).unwrap();
    let command_id = CommandId::new();
    let permit = match reserve(
        &store,
        principal.clone(),
        key.clone(),
        digest,
        CorrelationId::new(),
    )
    .await
    .unwrap()
    {
        IdempotencyReservation::Acquired(permit) => permit,
        IdempotencyReservation::Replay(_) => panic!("first request must reserve"),
    };
    store
        .finish(permit, completed(command_id.clone()), Utc::now())
        .await
        .unwrap();

    assert!(matches!(
        reserve(
            &store,
            principal.clone(),
            key.clone(),
            digest,
            CorrelationId::new(),
        )
        .await
        .unwrap(),
        IdempotencyReservation::Replay(CommandOutcome::Completed { command_id: actual, .. })
            if actual == command_id
    ));

    let correlation_id = CorrelationId::new();
    let mismatch = reserve(
        &store,
        principal,
        key,
        canonical_sha256(&serde_json::json!({"value": 2})).unwrap(),
        correlation_id.clone(),
    )
    .await
    .unwrap_err();
    assert_eq!(mismatch.code, InterfaceErrorCode::IdempotencyConflict);
    assert_eq!(mismatch.correlation_id, correlation_id);
}

#[tokio::test]
async fn uncertain_outcome_is_a_tombstone_while_explicitly_retryable_outcome_releases() {
    let store = IdempotencyStore::with_global_capacity(4, 8, Duration::minutes(5));
    let principal = principal("10000000-0000-0000-0000-000000000001");
    let uncertain_key = key("uncertain");
    let uncertain_digest = canonical_sha256(&"uncertain").unwrap();
    let command_id = CommandId::new();
    let permit = match reserve(
        &store,
        principal.clone(),
        uncertain_key.clone(),
        uncertain_digest,
        CorrelationId::new(),
    )
    .await
    .unwrap()
    {
        IdempotencyReservation::Acquired(permit) => permit,
        IdempotencyReservation::Replay(_) => unreachable!(),
    };
    store
        .finish(permit, reconciliation(command_id.clone()), Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        reserve(
            &store,
            principal.clone(),
            uncertain_key,
            uncertain_digest,
            CorrelationId::new(),
        )
        .await
        .unwrap(),
        IdempotencyReservation::Replay(CommandOutcome::NeedsReconciliation {
            command_id: actual,
            ..
        }) if actual == command_id
    ));

    let retryable_key = key("retryable");
    let retryable_digest = canonical_sha256(&"retryable").unwrap();
    let permit = match reserve(
        &store,
        principal.clone(),
        retryable_key.clone(),
        retryable_digest,
        CorrelationId::new(),
    )
    .await
    .unwrap()
    {
        IdempotencyReservation::Acquired(permit) => permit,
        IdempotencyReservation::Replay(_) => unreachable!(),
    };
    store
        .finish(
            permit,
            CommandOutcome::RetryableFailure {
                command_id: CommandId::new(),
                error: command_error(true),
            },
            Utc::now(),
        )
        .await
        .unwrap();
    assert!(matches!(
        reserve(
            &store,
            principal,
            retryable_key,
            retryable_digest,
            CorrelationId::new(),
        )
        .await
        .unwrap(),
        IdempotencyReservation::Acquired(_)
    ));
}

#[tokio::test]
async fn same_key_and_digest_concurrent_callers_dispatch_once() {
    let store = IdempotencyStore::with_global_capacity(4, 8, Duration::minutes(5));
    let principal = principal("10000000-0000-0000-0000-000000000001");
    let key = key("concurrent-same");
    let digest = canonical_sha256(&"same").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));
    let command_id = CommandId::new();
    let first = tokio::spawn(counted_dispatch(
        DispatchCase {
            store: store.clone(),
            principal: principal.clone(),
            key: key.clone(),
            digest,
            correlation_id: CorrelationId::new(),
        },
        calls.clone(),
        barrier.clone(),
        completed(command_id.clone()),
    ));
    let second = tokio::spawn(counted_dispatch(
        DispatchCase {
            store,
            principal,
            key,
            digest,
            correlation_id: CorrelationId::new(),
        },
        calls.clone(),
        barrier,
        completed(command_id),
    ));
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn different_digest_concurrent_callers_conflict_before_second_dispatch() {
    let store = IdempotencyStore::with_global_capacity(4, 8, Duration::minutes(5));
    let principal = principal("10000000-0000-0000-0000-000000000001");
    let key = key("concurrent-conflict");
    let calls = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));
    let first = tokio::spawn(counted_dispatch(
        DispatchCase {
            store: store.clone(),
            principal: principal.clone(),
            key: key.clone(),
            digest: canonical_sha256(&"first").unwrap(),
            correlation_id: CorrelationId::new(),
        },
        calls.clone(),
        barrier.clone(),
        completed(CommandId::new()),
    ));
    let second = tokio::spawn(counted_dispatch(
        DispatchCase {
            store,
            principal,
            key,
            digest: canonical_sha256(&"second").unwrap(),
            correlation_id: CorrelationId::new(),
        },
        calls.clone(),
        barrier,
        completed(CommandId::new()),
    ));
    let results = [first.await.unwrap(), second.await.unwrap()];
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(error) if error.code == InterfaceErrorCode::IdempotencyConflict))
            .count(),
        1
    );
}

#[tokio::test]
async fn global_bound_refuses_safety_relevant_entries_even_after_ttl() {
    let store = IdempotencyStore::with_global_capacity(2, 2, Duration::milliseconds(20));
    let first_principal = principal("10000000-0000-0000-0000-000000000001");
    let second_principal = principal("20000000-0000-0000-0000-000000000002");
    for (principal, key_value) in [(first_principal, "first"), (second_principal, "second")] {
        let digest = canonical_sha256(&key_value).unwrap();
        let permit = match reserve(
            &store,
            principal,
            key(key_value),
            digest,
            CorrelationId::new(),
        )
        .await
        .unwrap()
        {
            IdempotencyReservation::Acquired(permit) => permit,
            IdempotencyReservation::Replay(_) => unreachable!(),
        };
        store
            .finish(permit, reconciliation(CommandId::new()), Utc::now())
            .await
            .unwrap();
    }

    let correlation_id = CorrelationId::new();
    let full = reserve(
        &store,
        principal("30000000-0000-0000-0000-000000000003"),
        key("third"),
        canonical_sha256(&"third").unwrap(),
        correlation_id.clone(),
    )
    .await
    .unwrap_err();
    assert_eq!(full.code, InterfaceErrorCode::ResourceExhausted);
    assert_eq!(full.correlation_id, correlation_id);

    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    let still_full = reserve(
        &store,
        principal("30000000-0000-0000-0000-000000000003"),
        key("third"),
        canonical_sha256(&"third").unwrap(),
        CorrelationId::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(still_full.code, InterfaceErrorCode::ResourceExhausted);
}

#[tokio::test]
async fn safety_tombstone_survives_time_advance_until_explicit_resolution() {
    let store = IdempotencyStore::with_global_capacity(1, 1, Duration::milliseconds(1));
    let principal = principal("10000000-0000-0000-0000-000000000001");
    let idempotency_key = key("durable-uncertain");
    let digest = canonical_sha256(&"durable-uncertain").unwrap();
    let command_id = CommandId::new();
    let started_at = Utc::now();
    let permit = match store
        .reserve(
            principal.clone(),
            idempotency_key.clone(),
            InterfaceOperation::SubmitCommand,
            digest,
            started_at,
            started_at + Duration::hours(2),
            CorrelationId::new(),
        )
        .await
        .unwrap()
    {
        IdempotencyReservation::Acquired(permit) => permit,
        IdempotencyReservation::Replay(_) => unreachable!(),
    };
    store
        .finish(permit, reconciliation(command_id.clone()), started_at)
        .await
        .unwrap();

    let much_later = started_at + Duration::hours(1);
    assert!(matches!(
        store
            .reserve(
                principal.clone(),
                idempotency_key.clone(),
                InterfaceOperation::SubmitCommand,
                digest,
                much_later,
                much_later + Duration::minutes(1),
                CorrelationId::new(),
            )
            .await
            .unwrap(),
        IdempotencyReservation::Replay(CommandOutcome::NeedsReconciliation {
            command_id: actual,
            ..
        }) if actual == command_id
    ));

    let dispatches = Arc::new(AtomicUsize::new(0));
    let replay = counted_dispatch(
        DispatchCase {
            store: store.clone(),
            principal: principal.clone(),
            key: idempotency_key.clone(),
            digest,
            correlation_id: CorrelationId::new(),
        },
        dispatches.clone(),
        Arc::new(Barrier::new(1)),
        completed(CommandId::new()),
    )
    .await
    .unwrap();
    assert!(matches!(
        replay,
        CommandOutcome::NeedsReconciliation {
            command_id: actual,
            ..
        } if actual == command_id
    ));
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);

    let full = store
        .reserve(
            principal.clone(),
            key("blocked-by-safety"),
            InterfaceOperation::SubmitCommand,
            canonical_sha256(&"blocked-by-safety").unwrap(),
            much_later,
            much_later + Duration::minutes(1),
            CorrelationId::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(full.code, InterfaceErrorCode::ResourceExhausted);

    let resolved = store
        .resolve_safety_tombstone(
            &principal,
            &idempotency_key,
            InterfaceOperation::SubmitCommand,
            digest,
            CorrelationId::new(),
        )
        .await
        .unwrap();
    assert!(matches!(
        resolved,
        CommandOutcome::NeedsReconciliation {
            command_id: actual,
            ..
        } if actual == command_id
    ));
    assert!(matches!(
        store
            .reserve(
                principal,
                idempotency_key,
                InterfaceOperation::SubmitCommand,
                digest,
                much_later,
                much_later + Duration::minutes(1),
                CorrelationId::new(),
            )
            .await
            .unwrap(),
        IdempotencyReservation::Acquired(_)
    ));
}

#[tokio::test]
async fn per_principal_bound_refuses_safety_state_without_blocking_another_principal() {
    let store = IdempotencyStore::with_global_capacity(1, 2, Duration::minutes(5));
    let first_principal = principal("10000000-0000-0000-0000-000000000001");
    let digest = canonical_sha256(&"first").unwrap();
    let permit = match reserve(
        &store,
        first_principal.clone(),
        key("first"),
        digest,
        CorrelationId::new(),
    )
    .await
    .unwrap()
    {
        IdempotencyReservation::Acquired(permit) => permit,
        IdempotencyReservation::Replay(_) => unreachable!(),
    };
    store
        .finish(permit, reconciliation(CommandId::new()), Utc::now())
        .await
        .unwrap();

    let same_principal = reserve(
        &store,
        first_principal,
        key("second"),
        canonical_sha256(&"second").unwrap(),
        CorrelationId::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(same_principal.code, InterfaceErrorCode::ResourceExhausted);
    assert!(matches!(
        reserve(
            &store,
            principal("20000000-0000-0000-0000-000000000002"),
            key("other-principal"),
            canonical_sha256(&"other").unwrap(),
            CorrelationId::new(),
        )
        .await
        .unwrap(),
        IdempotencyReservation::Acquired(_)
    ));
}

#[test]
fn retryable_failed_helper_remains_explicit_for_future_outcomes() {
    let outcome = CommandOutcome::Failed {
        command_id: CommandId::new(),
        error: command_error(true),
        evidence: vec![],
    };
    assert!(matches!(outcome, CommandOutcome::Failed { error, .. } if error.retryable));

    let restarted = CommandOutcome::Restarted {
        command_id: CommandId::new(),
        prior_attempt_id: AttemptId::new(),
        attempt_id: AttemptId::new(),
        reason: "restart".into(),
    };
    assert!(matches!(restarted, CommandOutcome::Restarted { .. }));
}
