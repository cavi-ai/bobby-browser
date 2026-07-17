use chrono::{Duration, Utc};
use interface_core::{canonical_sha256, IdempotencyStore};
use types::{
    AttemptId, CommandError, CommandId, CommandOutcome, CorrelationId, ErrorCode, ErrorLayer,
    IdempotencyKey, InterfaceErrorCode, InterfaceOperation, PrincipalId,
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

#[test]
fn committed_success_is_reused_only_for_matching_operation_and_digest() {
    let store = IdempotencyStore::new(8, Duration::minutes(5));
    let principal = principal("10000000-0000-0000-0000-000000000001");
    let key = key("same-request");
    let digest = canonical_sha256(&serde_json::json!({"value": 1})).unwrap();
    let command_id = CommandId::new();
    let now = Utc::now();
    store
        .record_committed_outcome(
            principal.clone(),
            key.clone(),
            InterfaceOperation::SubmitCommand,
            digest,
            completed(command_id.clone()),
            now,
        )
        .unwrap();

    let reused = store
        .lookup_outcome(
            &principal,
            &key,
            InterfaceOperation::SubmitCommand,
            digest,
            now,
            CorrelationId::new(),
        )
        .unwrap()
        .unwrap();
    assert!(matches!(
        reused,
        CommandOutcome::Completed { command_id: actual, .. } if actual == command_id
    ));

    let mismatched_digest = canonical_sha256(&serde_json::json!({"value": 2})).unwrap();
    let digest_error = store
        .lookup_outcome(
            &principal,
            &key,
            InterfaceOperation::SubmitCommand,
            mismatched_digest,
            now,
            CorrelationId::new(),
        )
        .unwrap_err();
    assert_eq!(digest_error.code, InterfaceErrorCode::IdempotencyConflict);
    assert_eq!(
        digest_error.message,
        "idempotency key conflicts with a committed request"
    );

    let operation_error = store
        .lookup_outcome(
            &principal,
            &key,
            InterfaceOperation::OpenPage,
            digest,
            now,
            CorrelationId::new(),
        )
        .unwrap_err();
    assert_eq!(
        operation_error.code,
        InterfaceErrorCode::IdempotencyConflict
    );

    let replacement_error = store
        .record_committed_outcome(
            principal.clone(),
            key.clone(),
            InterfaceOperation::OpenPage,
            mismatched_digest,
            completed(CommandId::new()),
            now,
        )
        .unwrap_err();
    assert_eq!(
        replacement_error.code,
        InterfaceErrorCode::IdempotencyConflict
    );
    let original = store
        .lookup_outcome(
            &principal,
            &key,
            InterfaceOperation::SubmitCommand,
            digest,
            now,
            CorrelationId::new(),
        )
        .unwrap()
        .unwrap();
    assert!(matches!(
        original,
        CommandOutcome::Completed { command_id: actual, .. } if actual == command_id
    ));
}

#[test]
fn non_success_and_uncertain_outcomes_are_never_cached() {
    let store = IdempotencyStore::new(8, Duration::minutes(5));
    let principal = principal("10000000-0000-0000-0000-000000000001");
    let now = Utc::now();
    let outcomes = vec![
        CommandOutcome::RetryableFailure {
            command_id: CommandId::new(),
            error: command_error(true),
        },
        CommandOutcome::NeedsReconciliation {
            command_id: CommandId::new(),
            error: command_error(false),
            evidence: Vec::new(),
        },
        CommandOutcome::ResourceExhausted {
            command_id: CommandId::new(),
            error: command_error(true),
            retry_after_ms: 100,
        },
        CommandOutcome::Restarted {
            command_id: CommandId::new(),
            prior_attempt_id: AttemptId::new(),
            attempt_id: AttemptId::new(),
            reason: "restart".into(),
        },
        CommandOutcome::Failed {
            command_id: CommandId::new(),
            error: command_error(false),
        },
        CommandOutcome::PolicyDenied {
            command_id: CommandId::new(),
            error: command_error(false),
        },
    ];

    for (index, outcome) in outcomes.into_iter().enumerate() {
        let key = key(&format!("not-committed-{index}"));
        let digest = canonical_sha256(&index).unwrap();
        store
            .record_committed_outcome(
                principal.clone(),
                key.clone(),
                InterfaceOperation::SubmitCommand,
                digest,
                outcome,
                now,
            )
            .unwrap();
        assert!(store
            .lookup_outcome(
                &principal,
                &key,
                InterfaceOperation::SubmitCommand,
                digest,
                now,
                CorrelationId::new(),
            )
            .unwrap()
            .is_none());
    }
}

#[test]
fn storage_is_bounded_per_principal_and_expired_entries_are_not_reused() {
    let store = IdempotencyStore::new(1, Duration::seconds(10));
    let first = principal("10000000-0000-0000-0000-000000000001");
    let second = principal("20000000-0000-0000-0000-000000000002");
    let now = Utc::now();
    let digest = canonical_sha256(&"request").unwrap();

    for (principal, key_value) in [(&first, "first-a"), (&second, "second-a")] {
        store
            .record_committed_outcome(
                principal.clone(),
                key(key_value),
                InterfaceOperation::SubmitCommand,
                digest,
                completed(CommandId::new()),
                now,
            )
            .unwrap();
    }
    store
        .record_committed_outcome(
            first.clone(),
            key("first-b"),
            InterfaceOperation::SubmitCommand,
            digest,
            completed(CommandId::new()),
            now,
        )
        .unwrap();

    assert!(store
        .lookup_outcome(
            &first,
            &key("first-a"),
            InterfaceOperation::SubmitCommand,
            digest,
            now,
            CorrelationId::new(),
        )
        .unwrap()
        .is_none());
    assert!(store
        .lookup_outcome(
            &second,
            &key("second-a"),
            InterfaceOperation::SubmitCommand,
            digest,
            now,
            CorrelationId::new(),
        )
        .unwrap()
        .is_some());
    assert!(store
        .lookup_outcome(
            &first,
            &key("first-b"),
            InterfaceOperation::SubmitCommand,
            digest,
            now + Duration::seconds(10),
            CorrelationId::new(),
        )
        .unwrap()
        .is_none());
}
