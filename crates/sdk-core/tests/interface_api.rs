use chrono::{Duration, Utc};
use interface_core::{AuthorityStore, CapabilityHandle, RuntimeInterface};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use types::{
    AttemptId, Capability, CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest,
    IdempotencyKey, InterfaceErrorCode, OpenPageRequest, PrincipalId, SessionId, WorkflowId,
};
use uuid::uuid;

fn assert_runtime_interface<T: RuntimeInterface>() {}

fn expiry() -> chrono::DateTime<Utc> {
    Utc::now() + Duration::minutes(5)
}

async fn authenticated(runtime: RuntimeService) -> (AuthenticatedRuntime, CapabilityHandle) {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000001")),
            [
                Capability::SessionRead,
                Capability::SessionWrite,
                Capability::PageWrite,
                Capability::BrowserMutate,
            ],
            expiry(),
        )
        .await
        .unwrap()
        .expose_once();
    let handle = authority.verify(&token).await.unwrap();
    (AuthenticatedRuntime::new(runtime, handle.clone()), handle)
}

#[tokio::test]
async fn authenticated_runtime_implements_the_versioned_interface() {
    assert_runtime_interface::<AuthenticatedRuntime>();
    let (api, handle) = authenticated(RuntimeService::default()).await;
    let read_context = handle.context(expiry(), None);

    let info = api.runtime_info(read_context.clone()).await.unwrap();
    assert!(info.capabilities.contains(&"sdk".to_owned()));
    assert!(api.list_sessions(read_context).await.unwrap().is_empty());
}

#[tokio::test]
async fn runtime_errors_are_mapped_without_dispatch_outcome_flattening() {
    let (api, handle) = authenticated(RuntimeService::default()).await;
    let session = api
        .create_session(
            handle.context(expiry(), None),
            CreateSessionRequest {
                profile: "interface-test".into(),
                proxy: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(session.profile, "interface-test");

    let error = api
        .open_page(
            handle.context(expiry(), None),
            OpenPageRequest {
                session_id: SessionId::new(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, InterfaceErrorCode::NotFound);

    let command_id = CommandId::new();
    let outcome = api
        .submit(
            handle.context(expiry(), None),
            CommandEnvelope {
                schema_version: CommandEnvelope::SCHEMA_VERSION,
                command_id: command_id.clone(),
                workflow_id: WorkflowId::new(),
                attempt_id: AttemptId::new(),
                session_id: session.id,
                page_id: None,
                deadline: expiry(),
                command: types::PrimitiveCommand::ListPages(types::ListPagesCommand),
            },
        )
        .await
        .unwrap();
    let actual = match outcome {
        CommandOutcome::Completed { command_id, .. }
        | CommandOutcome::RetryableFailure { command_id, .. }
        | CommandOutcome::NeedsReconciliation { command_id, .. }
        | CommandOutcome::PolicyDenied { command_id, .. }
        | CommandOutcome::ResourceExhausted { command_id, .. }
        | CommandOutcome::Restarted { command_id, .. }
        | CommandOutcome::Failed { command_id, .. } => command_id,
    };
    assert_eq!(actual, command_id);
}

#[tokio::test]
async fn authenticated_submit_replays_retained_outcome_and_conflicts_before_dispatch() {
    let (api, handle) = authenticated(RuntimeService::default()).await;
    let key = IdempotencyKey::try_from("sdk-retained-submit").unwrap();
    let context = handle.context(expiry(), Some(key));
    let request = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: None,
        deadline: expiry(),
        command: types::PrimitiveCommand::ListPages(types::ListPagesCommand),
    };

    let first = api.submit(context.clone(), request.clone()).await.unwrap();
    let replay = api.submit(context.clone(), request).await.unwrap();
    assert_eq!(
        std::mem::discriminant(&first),
        std::mem::discriminant(&replay)
    );

    let correlation_id = context.correlation_id.clone();
    let conflict = api
        .submit(
            context,
            CommandEnvelope {
                schema_version: CommandEnvelope::SCHEMA_VERSION,
                command_id: CommandId::new(),
                workflow_id: WorkflowId::new(),
                attempt_id: AttemptId::new(),
                session_id: SessionId::new(),
                page_id: None,
                deadline: expiry(),
                command: types::PrimitiveCommand::ListPages(types::ListPagesCommand),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(conflict.code, InterfaceErrorCode::IdempotencyConflict);
    assert_eq!(conflict.correlation_id, correlation_id);
}
