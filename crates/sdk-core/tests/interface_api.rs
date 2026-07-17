use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use interface_core::{
    canonical_sha256, Authority, AuthorityStore, CapabilityHandle, IdempotencyPermit,
    IdempotencyReservation, IdempotencyStore, RuntimeInterface, SessionOwnershipAuthority,
    SessionOwnershipRecorder, SessionOwnershipRegistry,
};
use page_runtime::PageRuntime;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use session_manager::SessionManager;
use types::{
    AttemptId, Capability, ClickCommand, CommandEnvelope, CommandError, CommandId, CommandOutcome,
    CreateSessionRequest, Evidence, IdempotencyKey, InspectCommand, InterfaceErrorCode,
    NavigateCommand, OpenPageRequest, PageId, PrincipalId, RequestContext, SessionId,
    TypeTextCommand, WorkerId, WorkflowId,
};
use uuid::uuid;
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};

fn assert_runtime_interface<T: RuntimeInterface>() {}

fn expiry() -> chrono::DateTime<Utc> {
    Utc::now() + Duration::minutes(5)
}

struct LifecycleWorker {
    id: WorkerId,
    profile: PathBuf,
    closes: Arc<AtomicUsize>,
}

#[async_trait]
impl BrowserWorker for LifecycleWorker {
    fn worker_id(&self) -> WorkerId {
        self.id.clone()
    }

    fn profile_dir(&self) -> &Path {
        &self.profile
    }

    async fn open_page(&self, _: PageId) -> Result<(), CommandError> {
        Ok(())
    }

    async fn navigate(
        &self,
        _: &PageId,
        _: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(Vec::new())
    }

    async fn inspect(&self, _: &PageId, _: &InspectCommand) -> Result<Vec<Evidence>, CommandError> {
        Ok(Vec::new())
    }

    async fn click(&self, _: &PageId, _: &ClickCommand) -> Result<Vec<Evidence>, CommandError> {
        Ok(Vec::new())
    }

    async fn type_text(
        &self,
        _: &PageId,
        _: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(Vec::new())
    }

    async fn close(&self) -> Result<(), CommandError> {
        self.closes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

struct LifecycleFactory {
    attempts: AtomicUsize,
    fail_first: bool,
    closes: Arc<AtomicUsize>,
}

#[async_trait]
impl WorkerFactory for LifecycleFactory {
    async fn launch(&self, session_id: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        if self.fail_first && self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(CommandError {
                code: types::ErrorCode::BrowserLaunchFailed,
                message: "injected session creation failure".into(),
                layer: types::ErrorLayer::Driver,
                retryable: true,
            });
        }
        Ok(Arc::new(LifecycleWorker {
            id: WorkerId::new(),
            profile: PathBuf::from(format!("/profiles/{}", session_id.0)),
            closes: self.closes.clone(),
        }))
    }
}

fn runtime_with_workers(fail_first: bool) -> (RuntimeService, Arc<WorkerPool>, Arc<AtomicUsize>) {
    let closes = Arc::new(AtomicUsize::new(0));
    let pool = Arc::new(WorkerPool::new(
        2,
        Arc::new(LifecycleFactory {
            attempts: AtomicUsize::new(0),
            fail_first,
            closes: closes.clone(),
        }),
    ));
    let runtime = RuntimeService::new(SessionManager::new(pool.clone()), PageRuntime::default());
    (runtime, pool, closes)
}

async fn session_owned_runtime(
    runtime: RuntimeService,
    capacity: usize,
) -> (
    AuthenticatedRuntime,
    RequestContext,
    Arc<SessionOwnershipRegistry>,
    SessionOwnershipRecorder,
) {
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
    let (ownership, recorder) = SessionOwnershipRegistry::bounded(capacity);
    let api = AuthenticatedRuntime::with_session_ownership(runtime, handle, recorder.clone());
    (api, context, ownership, recorder)
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
    let runtime = RuntimeService::default();
    let runtime_probe = runtime.clone();
    let api = AuthenticatedRuntime::with_session_ownership(runtime, handle, recorder);

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
    assert_eq!(api.create_session_dispatch_count(), 1);
    let live = runtime_probe.list_sessions().await;
    assert_eq!(live.len(), 1);
    assert_eq!(live[0].id, session.id);
}

#[tokio::test]
async fn full_ownership_registry_refuses_before_runtime_session_dispatch() {
    let runtime = RuntimeService::default();
    let runtime_probe = runtime.clone();
    let (api, context, _ownership, recorder) = session_owned_runtime(runtime, 1).await;
    recorder
        .record_authenticated_session(context.principal_id.clone(), SessionId::new())
        .unwrap();

    let error = api
        .create_session(
            context,
            CreateSessionRequest {
                profile: "must-not-dispatch".into(),
                proxy: None,
            },
        )
        .await
        .unwrap_err();

    assert_eq!(error.code, InterfaceErrorCode::ResourceExhausted);
    assert_eq!(api.create_session_dispatch_count(), 0);
    assert!(runtime_probe.list_sessions().await.is_empty());
}

#[tokio::test]
async fn runtime_session_failure_releases_the_ownership_reservation() {
    let (runtime, pool, _) = runtime_with_workers(true);
    let (api, context, ownership, _) = session_owned_runtime(runtime, 1).await;

    assert!(api
        .create_session(
            context.clone(),
            CreateSessionRequest {
                profile: "fails-once".into(),
                proxy: None,
            },
        )
        .await
        .is_err());
    let session = api
        .create_session(
            context.clone(),
            CreateSessionRequest {
                profile: "reservation-reused".into(),
                proxy: None,
            },
        )
        .await
        .unwrap();

    assert_eq!(api.create_session_dispatch_count(), 2);
    assert!(ownership.owns_session(&context.principal_id, &session.id));
    assert_eq!(pool.active_workers().await, 1);
}

#[tokio::test]
async fn forced_finalize_failure_rolls_back_the_live_session_and_worker() {
    let (runtime, pool, closes) = runtime_with_workers(false);
    let runtime_probe = runtime.clone();
    let (api, context, _ownership, recorder) = session_owned_runtime(runtime, 1).await;
    recorder.inject_finalize_failure_once_for_test();

    let error = api
        .create_session(
            context,
            CreateSessionRequest {
                profile: "rollback-finalize".into(),
                proxy: None,
            },
        )
        .await
        .unwrap_err();

    assert_eq!(error.code, InterfaceErrorCode::ResourceExhausted);
    assert_eq!(api.create_session_dispatch_count(), 1);
    assert!(runtime_probe.list_sessions().await.is_empty());
    assert_eq!(pool.active_workers().await, 0);
    assert_eq!(closes.load(Ordering::SeqCst), 1);
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
