use chrono::{Duration, Utc};
use interface_core::{
    canonical_sha256, Authority, AuthorityStore, CapabilityHandle, IdempotencyPermit,
    IdempotencyReservation, IdempotencyStore, RuntimeInterface, SessionOwnershipAuthority,
    SessionOwnershipRegistry,
};
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

async fn authenticated_with_store(
    runtime: RuntimeService,
    authority: &AuthorityStore,
    handle_expiry: chrono::DateTime<Utc>,
    idempotency: IdempotencyStore,
) -> (AuthenticatedRuntime, CapabilityHandle) {
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000001")),
            [Capability::BrowserMutate],
            handle_expiry,
        )
        .await
        .unwrap()
        .expose_once();
    let handle = authority.verify(&token).await.unwrap();
    (
        AuthenticatedRuntime::with_idempotency(runtime, handle.clone(), idempotency),
        handle,
    )
}

fn submit_request() -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: None,
        deadline: expiry(),
        command: types::PrimitiveCommand::ListPages(types::ListPagesCommand),
    }
}

async fn hold_reservation(
    store: &IdempotencyStore,
    context: &types::RequestContext,
    request: &CommandEnvelope,
) -> IdempotencyPermit {
    match store
        .reserve(
            context.principal_id.clone(),
            context.idempotency_key.clone().unwrap(),
            types::InterfaceOperation::SubmitCommand,
            canonical_sha256(request).unwrap(),
            Utc::now(),
            context.deadline,
            context.correlation_id.clone(),
        )
        .await
        .unwrap()
    {
        IdempotencyReservation::Acquired(permit) => permit,
        IdempotencyReservation::Replay(_) => unreachable!(),
    }
}

fn retryable_release() -> CommandOutcome {
    CommandOutcome::RetryableFailure {
        command_id: CommandId::new(),
        error: types::CommandError {
            code: types::ErrorCode::Internal,
            message: "retry".into(),
            layer: types::ErrorLayer::Page,
            retryable: true,
        },
    }
}

async fn assert_reservation_released(
    store: &IdempotencyStore,
    context: &types::RequestContext,
    request: &CommandEnvelope,
) {
    assert!(matches!(
        store
            .reserve(
                context.principal_id.clone(),
                context.idempotency_key.clone().unwrap(),
                types::InterfaceOperation::SubmitCommand,
                canonical_sha256(request).unwrap(),
                Utc::now(),
                Utc::now() + Duration::seconds(1),
                context.correlation_id.clone(),
            )
            .await
            .unwrap(),
        IdempotencyReservation::Acquired(_)
    ));
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
async fn authenticated_session_creation_populates_the_trusted_ownership_authority() {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000009")),
            [Capability::SessionWrite],
            expiry(),
        )
        .await
        .unwrap()
        .expose_once();
    let handle = authority.verify(&token).await.unwrap();
    let context = handle.context(expiry(), None);
    let principal = context.principal_id.clone();
    let (ownership, recorder) = SessionOwnershipRegistry::bounded(8);
    let api =
        AuthenticatedRuntime::with_session_ownership(RuntimeService::default(), handle, recorder);

    let session = api
        .create_session(
            context,
            CreateSessionRequest {
                profile: "owned-session".into(),
                proxy: None,
            },
        )
        .await
        .unwrap();
    assert!(ownership.owns_session(&principal, &session.id));
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

#[tokio::test]
async fn revoked_waiter_revalidates_and_abandons_without_runtime_dispatch() {
    let authority = AuthorityStore::in_memory();
    let store = IdempotencyStore::with_global_capacity(4, 8, Duration::minutes(5));
    let (api, handle) = authenticated_with_store(
        RuntimeService::default(),
        &authority,
        expiry(),
        store.clone(),
    )
    .await;
    let request = submit_request();
    let context = handle.context(
        Utc::now() + Duration::seconds(2),
        Some(IdempotencyKey::try_from("revoked-waiter").unwrap()),
    );
    let permit = hold_reservation(&store, &context, &request).await;
    let waiter_api = api.clone();
    let waiter_context = context.clone();
    let waiter_request = request.clone();
    let waiter =
        tokio::spawn(async move { waiter_api.submit(waiter_context, waiter_request).await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(
        !waiter.is_finished(),
        "request must be waiting on the reservation"
    );
    authority.revoke(&context.principal_id).await.unwrap();
    store
        .finish(permit, retryable_release(), Utc::now())
        .await
        .unwrap();

    let error = waiter.await.unwrap().unwrap_err();
    assert_eq!(error.code, InterfaceErrorCode::AuthenticationFailed);
    assert_eq!(api.submit_dispatch_count(), 0);
    assert_reservation_released(&store, &context, &request).await;
}

#[tokio::test]
async fn expired_handle_waiter_revalidates_and_abandons_without_runtime_dispatch() {
    let authority = AuthorityStore::in_memory();
    let store = IdempotencyStore::with_global_capacity(4, 8, Duration::minutes(5));
    let handle_expiry = Utc::now() + Duration::milliseconds(80);
    let (api, handle) = authenticated_with_store(
        RuntimeService::default(),
        &authority,
        handle_expiry,
        store.clone(),
    )
    .await;
    let request = submit_request();
    let context = handle.context(
        Utc::now() + Duration::seconds(2),
        Some(IdempotencyKey::try_from("expired-waiter").unwrap()),
    );
    let permit = hold_reservation(&store, &context, &request).await;
    let waiter_api = api.clone();
    let waiter_context = context.clone();
    let waiter_request = request.clone();
    let waiter =
        tokio::spawn(async move { waiter_api.submit(waiter_context, waiter_request).await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(
        !waiter.is_finished(),
        "request must be waiting on the reservation"
    );
    tokio::time::sleep(std::time::Duration::from_millis(80)).await;
    store
        .finish(permit, retryable_release(), Utc::now())
        .await
        .unwrap();

    let error = waiter.await.unwrap().unwrap_err();
    assert_eq!(error.code, InterfaceErrorCode::AuthenticationFailed);
    assert_eq!(api.submit_dispatch_count(), 0);
    assert_reservation_released(&store, &context, &request).await;
}

#[tokio::test]
async fn elapsed_deadline_waiter_never_dispatches_and_reservation_can_be_abandoned() {
    let authority = AuthorityStore::in_memory();
    let store = IdempotencyStore::with_global_capacity(4, 8, Duration::minutes(5));
    let (api, handle) = authenticated_with_store(
        RuntimeService::default(),
        &authority,
        expiry(),
        store.clone(),
    )
    .await;
    let request = submit_request();
    let context = handle.context(
        Utc::now() + Duration::milliseconds(50),
        Some(IdempotencyKey::try_from("deadline-waiter").unwrap()),
    );
    let permit = hold_reservation(&store, &context, &request).await;
    let waiter_api = api.clone();
    let waiter_context = context.clone();
    let waiter_request = request.clone();
    let waiter =
        tokio::spawn(async move { waiter_api.submit(waiter_context, waiter_request).await });
    tokio::time::sleep(std::time::Duration::from_millis(20)).await;
    assert!(
        !waiter.is_finished(),
        "request must be waiting on the reservation"
    );
    tokio::time::sleep(std::time::Duration::from_millis(60)).await;

    let error = waiter.await.unwrap().unwrap_err();
    assert_eq!(error.code, InterfaceErrorCode::DeadlineExceeded);
    assert_eq!(api.submit_dispatch_count(), 0);
    store.abandon(permit).await;
    assert_reservation_released(&store, &context, &request).await;
}
