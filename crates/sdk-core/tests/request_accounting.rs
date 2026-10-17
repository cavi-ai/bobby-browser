use chrono::{Duration, Utc};
use interface_core::{
    command_identity_sha256, Authority, AuthorityStore, CapabilityHandle, IdempotencyReservation,
    IdempotencyStore, RuntimeInterface,
};
use observability::test_support::CaptureSink;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use std::task::{Context, Poll, Waker};
use types::{
    AttemptId, Capability, CapabilitySet, CommandEnvelope, CommandId, IdempotencyKey,
    InterfaceOperation, NavigateCommand, PrimitiveCommand, PrincipalId, RuntimeCommand, SessionId,
    WaitUntil, WorkflowId,
};

fn deadline() -> chrono::DateTime<Utc> {
    Utc::now() + Duration::minutes(5)
}

async fn setup(
    store: IdempotencyStore,
) -> (AuthorityStore, AuthenticatedRuntime, CapabilityHandle) {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [
                Capability::BrowserMutate,
                Capability::VisionAssist,
                Capability::RecoveryWrite,
            ],
            deadline(),
        )
        .await
        .unwrap()
        .expose_once();
    let handle = authority.verify(&token).await.unwrap();
    let api =
        AuthenticatedRuntime::with_idempotency(RuntimeService::default(), handle.clone(), store);
    (authority, api, handle)
}

fn envelope() -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: None,
        deadline: deadline(),
        command: RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
            url: "https://example.test/PRIVATE_COMMAND_PAYLOAD?token=SECRET".into(),
            wait_until: WaitUntil::Interactive,
            timeout_ms: 1000,
        })),
    }
}

#[tokio::test]
async fn accounting_reconciles_replay_rejection_and_runtime_calls_without_payloads() {
    let sink = CaptureSink::install();
    let (authority, api, handle) = setup(IdempotencyStore::default()).await;
    let request = envelope();
    let context = handle.context(
        deadline(),
        Some(IdempotencyKey::try_from("PRIVATE_IDEMPOTENCY_KEY").unwrap()),
    );
    let first = api.submit(context.clone(), request.clone()).await.unwrap();
    let mut retried = request.clone();
    retried.command_id = CommandId::new();
    retried.attempt_id = AttemptId::new();
    let replay = api.submit(context.clone(), retried.clone()).await.unwrap();
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(replay).unwrap()
    );
    let (checkpoint_replay, checkpoint) = api
        .submit_with_auto_checkpoint(context.clone(), request.clone())
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(checkpoint_replay).unwrap()
    );
    assert!(checkpoint.is_none());
    api.submit_with_one_shot_vision_consent(handle.context(deadline(), None), request.clone())
        .await
        .unwrap();
    assert!(api
        .submit_with_auto_checkpoint(handle.context(deadline(), None), request.clone())
        .await
        .is_err());
    let mut denied = context.clone();
    denied.capabilities = CapabilitySet::default();
    assert!(api.submit(denied, request.clone()).await.is_err());
    authority.revoke(&context.principal_id).await.unwrap();
    assert!(api.submit(context, request.clone()).await.is_err());

    let events = sink.events();
    let requests: Vec<_> = events
        .iter()
        .filter(|e| e["fields"]["message"] == "command.request.finished")
        .collect();
    assert_eq!(requests.len(), 7);
    for (disposition, count) in [("runtimeInvoked", 3), ("replayed", 2), ("rejected", 2)] {
        assert_eq!(
            requests
                .iter()
                .filter(|e| e["fields"]["disposition"] == disposition)
                .count(),
            count
        );
    }
    assert_eq!(api.submit_dispatch_count(), 3);
    assert_eq!(
        events
            .iter()
            .filter(|e| e["fields"]["message"] == "command.finished")
            .count(),
        2
    );
    assert_eq!(
        requests[1]["span"]["command_id"],
        retried.command_id.0.to_string()
    );
    assert_eq!(
        requests[1]["fields"]["outcome_command_id"],
        request.command_id.0.to_string()
    );
    for (mode, count) in [("submit", 4), ("autoCheckpoint", 2), ("oneShotVision", 1)] {
        assert_eq!(
            requests
                .iter()
                .filter(|e| e["span"]["request_mode"] == mode)
                .count(),
            count
        );
    }
    for event in requests {
        assert_eq!(
            event["span"]["workflow_id"],
            request.workflow_id.0.to_string()
        );
        assert!(event["fields"]["elapsed_ms"].is_u64());
        assert_eq!(event["fields"]["interrupted"], false);
    }
    assert!(!serde_json::to_string(&events).unwrap().contains("PRIVATE_"));
}

#[tokio::test]
async fn cancelled_waiter_reports_interruption_without_an_execution_or_retry_claim() {
    let sink = CaptureSink::install();
    let store = IdempotencyStore::default();
    let (_, api, handle) = setup(store.clone()).await;
    let request = envelope();
    let context = handle.context(
        deadline(),
        Some(IdempotencyKey::try_from("held-reservation").unwrap()),
    );
    let digest = command_identity_sha256(
        request.schema_version,
        &request.session_id,
        &request.page_id,
        &request.command,
        false,
    )
    .unwrap();
    let reservation = store
        .reserve(
            context.principal_id.clone(),
            context.idempotency_key.clone().unwrap(),
            InterfaceOperation::SubmitCommand,
            digest,
            Utc::now(),
            context.deadline,
            context.correlation_id.clone(),
        )
        .await
        .unwrap();
    let IdempotencyReservation::Acquired(permit) = reservation else {
        panic!("fixture must own reservation")
    };
    let mut pending = api.submit(context, request);
    assert!(matches!(
        pending
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop())),
        Poll::Pending
    ));
    drop(pending);
    let events = sink.events();
    let terminal: Vec<_> = events
        .iter()
        .filter(|e| e["fields"]["message"] == "command.request.finished")
        .collect();
    assert_eq!(terminal.len(), 1);
    assert_eq!(terminal[0]["fields"]["interrupted"], true);
    assert_eq!(terminal[0]["fields"]["runtime_invoked"], false);
    assert_eq!(api.submit_dispatch_count(), 0);
    assert!(!events
        .iter()
        .any(|e| e["fields"]["message"] == "command.finished"));
    drop(permit);
}
