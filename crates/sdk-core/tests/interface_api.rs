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
use types::{RuntimeCommand, 
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

/// Same principal/session shape as `authenticated`, but with an explicit capability set so
/// per-primitive capability gating (file upload/download) can be exercised independent of
/// the coarse `browser:mutate` grant.
async fn authenticated_with(
    runtime: RuntimeService,
    capabilities: impl IntoIterator<Item = Capability>,
) -> (AuthenticatedRuntime, CapabilityHandle) {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000001")),
            capabilities,
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
        command: RuntimeCommand::Primitive(types::PrimitiveCommand::ListPages(types::ListPagesCommand)),
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
                execution_policy: Default::default(),
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
                command: RuntimeCommand::Primitive(types::PrimitiveCommand::ListPages(types::ListPagesCommand)),
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
                execution_policy: Default::default(),
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
async fn session_owned_runtime_hides_and_rejects_another_principals_session() {
    let authority = AuthorityStore::in_memory();
    let capabilities = [
        Capability::SessionRead,
        Capability::SessionWrite,
        Capability::PageWrite,
    ];
    let owner_token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000010")),
            capabilities,
            expiry(),
        )
        .await
        .unwrap()
        .expose_once();
    let other_token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000011")),
            capabilities,
            expiry(),
        )
        .await
        .unwrap()
        .expose_once();
    let owner_handle = authority.verify(&owner_token).await.unwrap();
    let other_handle = authority.verify(&other_token).await.unwrap();
    let owner_context = owner_handle.context(expiry(), None);
    let other_context = other_handle.context(expiry(), None);
    let (runtime, _, _) = runtime_with_workers(false);
    let (ownership, recorder) = SessionOwnershipRegistry::bounded(4);
    let owner = AuthenticatedRuntime::with_session_ownership(
        runtime.clone(),
        owner_handle,
        recorder.clone(),
    );
    let other = AuthenticatedRuntime::with_session_ownership(runtime, other_handle, recorder);
    let session = owner
        .create_session(
            owner_context,
            CreateSessionRequest {
                profile: "owner".into(),
                proxy: None,
                execution_policy: Default::default(),
            },
        )
        .await
        .unwrap();
    assert!(ownership.owns_session(
        &PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000010")),
        &session.id
    ));

    assert!(other
        .list_sessions(other_context.clone())
        .await
        .unwrap()
        .is_empty());
    let denial = other
        .open_page(
            other_context,
            OpenPageRequest {
                session_id: session.id,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(denial.code, InterfaceErrorCode::NotFound);
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
                execution_policy: Default::default(),
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
                execution_policy: Default::default(),
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
                execution_policy: Default::default(),
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
                execution_policy: Default::default(),
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
        command: RuntimeCommand::Primitive(types::PrimitiveCommand::ListPages(types::ListPagesCommand)),
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
                command: RuntimeCommand::Primitive(types::PrimitiveCommand::ListPages(types::ListPagesCommand)),
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

fn upload_files_envelope() -> CommandEnvelope {
    CommandEnvelope {
        command: RuntimeCommand::Primitive(types::PrimitiveCommand::UploadFiles(types::UploadFilesCommand {
            selector: "input[type=file]".into(),
            target: None,
            paths: vec!["/tmp/example.txt".into()],
        })),
        ..submit_request()
    }
}

fn download_url_envelope() -> CommandEnvelope {
    CommandEnvelope {
        command: RuntimeCommand::Primitive(types::PrimitiveCommand::DownloadUrl(types::DownloadUrlCommand {
            url: "https://example.com/file.bin".into(),
            expected_content_type: None,
            max_bytes: 1024,
        })),
        ..submit_request()
    }
}

fn click_and_wait_for_download_envelope() -> CommandEnvelope {
    CommandEnvelope {
        command: RuntimeCommand::Primitive(types::PrimitiveCommand::ClickAndWaitForDownload(
            types::ClickAndWaitForDownloadCommand {
                selector: "#download".into(),
                target: None,
                timeout_ms: 1_000,
            },
        )),
        ..submit_request()
    }
}

#[tokio::test]
async fn upload_files_without_file_upload_capability_is_denied_before_dispatch() {
    let (api, handle) = authenticated_with(
        RuntimeService::default(),
        [
            Capability::SessionWrite,
            Capability::PageWrite,
            Capability::BrowserMutate,
        ],
    )
    .await;
    let context = handle.context(
        expiry(),
        Some(IdempotencyKey::try_from("upload-denied").unwrap()),
    );

    let error = api
        .submit(context, upload_files_envelope())
        .await
        .unwrap_err();

    assert_eq!(error.code, InterfaceErrorCode::MissingCapability);
    assert_eq!(error.required_capability, Some(Capability::FileUpload));
    assert_eq!(api.submit_dispatch_count(), 0);
}

#[tokio::test]
async fn upload_files_without_file_upload_capability_is_denied_on_the_no_idempotency_path() {
    let (api, handle) = authenticated_with(
        RuntimeService::default(),
        [
            Capability::SessionWrite,
            Capability::PageWrite,
            Capability::BrowserMutate,
        ],
    )
    .await;
    let context = handle.context(expiry(), None);

    let error = api
        .submit(context, upload_files_envelope())
        .await
        .unwrap_err();

    assert_eq!(error.code, InterfaceErrorCode::MissingCapability);
    assert_eq!(error.required_capability, Some(Capability::FileUpload));
}

#[tokio::test]
async fn upload_files_with_file_upload_capability_clears_the_extra_capability_gate() {
    let (api, handle) = authenticated_with(
        RuntimeService::default(),
        [
            Capability::SessionWrite,
            Capability::PageWrite,
            Capability::BrowserMutate,
            Capability::FileUpload,
        ],
    )
    .await;
    let context = handle.context(expiry(), None);

    // The extra capability gate is cleared, so this falls through to the no-idempotency-key
    // early return (`Ok(self.inner.submit(envelope).await)`), which is infallible: any
    // `Err` here could only have come from the capability gate itself.
    let outcome = api.submit(context, upload_files_envelope()).await;

    assert!(
        outcome.is_ok(),
        "expected the file-upload capability gate to be cleared, got {outcome:?}"
    );
}

#[tokio::test]
async fn download_url_without_file_download_capability_is_denied_before_dispatch() {
    let (api, handle) = authenticated_with(
        RuntimeService::default(),
        [
            Capability::SessionWrite,
            Capability::PageWrite,
            Capability::BrowserMutate,
        ],
    )
    .await;
    let context = handle.context(
        expiry(),
        Some(IdempotencyKey::try_from("download-denied").unwrap()),
    );

    let error = api
        .submit(context, download_url_envelope())
        .await
        .unwrap_err();

    assert_eq!(error.code, InterfaceErrorCode::MissingCapability);
    assert_eq!(error.required_capability, Some(Capability::FileDownload));
    assert_eq!(api.submit_dispatch_count(), 0);
}

#[tokio::test]
async fn click_and_wait_for_download_without_file_download_capability_is_denied_before_dispatch() {
    let (api, handle) = authenticated_with(
        RuntimeService::default(),
        [
            Capability::SessionWrite,
            Capability::PageWrite,
            Capability::BrowserMutate,
        ],
    )
    .await;
    let context = handle.context(
        expiry(),
        Some(IdempotencyKey::try_from("click-download-denied").unwrap()),
    );

    let error = api
        .submit(context, click_and_wait_for_download_envelope())
        .await
        .unwrap_err();

    assert_eq!(error.code, InterfaceErrorCode::MissingCapability);
    assert_eq!(error.required_capability, Some(Capability::FileDownload));
    assert_eq!(api.submit_dispatch_count(), 0);
}

#[tokio::test]
async fn non_privileged_command_needs_only_browser_mutate_to_clear_the_extra_capability_gate() {
    let (api, handle) = authenticated_with(
        RuntimeService::default(),
        [
            Capability::SessionWrite,
            Capability::PageWrite,
            Capability::BrowserMutate,
        ],
    )
    .await;
    let context = handle.context(expiry(), None);

    // No idempotency key, so `Err` here could only come from the capability gate.
    let outcome = api.submit(context, submit_request()).await;

    assert!(
        outcome.is_ok(),
        "expected a non-privileged command to clear the extra capability gate, got {outcome:?}"
    );
}

// F4: RuntimeService's per-session ExecutionPolicy gate for EvaluateJavaScript, and its
// composition with AuthenticatedRuntime's token capability gate. Both gates fire before any
// browser dispatch, so these are provable with `RuntimeService::default()` / no worker pool:
// any outcome other than `PolicyDenied` proves the session gate was cleared, and
// `pages.execute` on an unconfigured `PageRuntime` can only ever produce `Failed`, never
// `PolicyDenied` — so a `PolicyDenied` outcome is unambiguous proof the gate (not some
// downstream failure) produced it.

fn evaluate_javascript_envelope(session_id: SessionId) -> CommandEnvelope {
    CommandEnvelope {
        session_id,
        command: RuntimeCommand::Primitive(types::PrimitiveCommand::EvaluateJavaScript(types::EvaluateJavaScriptCommand {
            expression: "1 + 1".into(),
            timeout_ms: 1_000,
            await_promise: false,
        })),
        ..submit_request()
    }
}

#[tokio::test]
async fn evaluate_javascript_is_policy_denied_for_a_session_that_was_never_created() {
    // Fail-closed proof: no `create_session` call happened, so `self.sessions.get` returns
    // `Err(NotFound)` for this session_id. The gate must treat that as deny, not as
    // "skip the check" — a missing/evicted session must never be treated as an implicit
    // allow just because there's nothing to look up.
    let runtime = RuntimeService::default();

    let outcome = runtime
        .submit(evaluate_javascript_envelope(SessionId::new()))
        .await;

    assert!(
        matches!(outcome, CommandOutcome::PolicyDenied { .. }),
        "expected PolicyDenied (fail-closed) for an unknown session, got {outcome:?}"
    );
}

#[tokio::test]
async fn evaluate_javascript_is_policy_denied_when_session_has_not_opted_in() {
    let runtime = RuntimeService::default();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "default".into(),
            proxy: None,
            execution_policy: Default::default(),
        })
        .await
        .unwrap();
    assert!(!session.execution_policy.javascript_evaluation);

    let outcome = runtime
        .submit(evaluate_javascript_envelope(session.id))
        .await;

    assert!(
        matches!(outcome, CommandOutcome::PolicyDenied { .. }),
        "expected PolicyDenied for a session that has not opted into JS, got {outcome:?}"
    );
}

#[tokio::test]
async fn evaluate_javascript_clears_the_session_policy_gate_when_opted_in() {
    let runtime = RuntimeService::default();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "default".into(),
            proxy: None,
            execution_policy: types::ExecutionPolicy {
                javascript_evaluation: true,
                vision_assist: false,
            },
        })
        .await
        .unwrap();

    let outcome = runtime
        .submit(evaluate_javascript_envelope(session.id))
        .await;

    assert!(
        !matches!(outcome, CommandOutcome::PolicyDenied { .. }),
        "expected the session policy gate to be cleared for an opted-in session, got {outcome:?}"
    );
}

#[tokio::test]
async fn evaluate_javascript_without_capability_is_denied_before_the_session_gate_runs() {
    let runtime = RuntimeService::default();
    // The session explicitly allows JS — proving the capability gate alone is enough to
    // deny, independent of the session's policy.
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "default".into(),
            proxy: None,
            execution_policy: types::ExecutionPolicy {
                javascript_evaluation: true,
                vision_assist: false,
            },
        })
        .await
        .unwrap();

    let (api, handle) = authenticated_with(
        runtime,
        [
            Capability::SessionWrite,
            Capability::PageWrite,
            Capability::BrowserMutate,
        ],
    )
    .await;
    let context = handle.context(expiry(), None);

    let error = api
        .submit(context, evaluate_javascript_envelope(session.id))
        .await
        .unwrap_err();

    assert_eq!(error.code, InterfaceErrorCode::MissingCapability);
    assert_eq!(
        error.required_capability,
        Some(Capability::JavascriptEvaluate)
    );
    assert_eq!(api.submit_dispatch_count(), 0);
}

#[tokio::test]
async fn evaluate_javascript_with_capability_but_js_off_session_is_policy_denied() {
    let runtime = RuntimeService::default();
    // The session explicitly denies JS (default) — proving the session gate independently
    // blocks even a token that holds the capability.
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "default".into(),
            proxy: None,
            execution_policy: Default::default(),
        })
        .await
        .unwrap();

    let (api, handle) = authenticated_with(
        runtime,
        [
            Capability::SessionWrite,
            Capability::PageWrite,
            Capability::BrowserMutate,
            Capability::JavascriptEvaluate,
        ],
    )
    .await;
    let context = handle.context(expiry(), None);

    // No idempotency key, so this returns `Ok(outcome)` straight from `self.inner.submit`.
    let outcome = api
        .submit(context, evaluate_javascript_envelope(session.id))
        .await
        .unwrap();

    assert!(
        matches!(outcome, CommandOutcome::PolicyDenied { .. }),
        "expected PolicyDenied: capability gate cleared, session gate must still block, got {outcome:?}"
    );
}
