use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use checkpoint_store::CheckpointStore;
use chrono::{Duration, Utc};
use interface_core::{
    command_identity_sha256, Authority, AuthorityStore, CapabilityHandle, IdempotencyPermit,
    IdempotencyReservation, IdempotencyStore, RuntimeInterface, SessionOwnershipAuthority,
    SessionOwnershipRecorder, SessionOwnershipRegistry,
};
use page_runtime::{PageRuntime, RecoveryCoordinator};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use session_manager::SessionManager;
use types::{
    AttemptId, Capability, CheckpointId, ClickCommand, CommandClass, CommandEnvelope, CommandError,
    CommandId, CommandOutcome, CompleteFormField, CompleteFormIntent, ControlAction,
    CreateSessionRequest, Evidence, ExecutionPolicy, FillIntent, IdempotencyKey, InspectCommand,
    IntentCommand, IntentHints, InterfaceErrorCode, LocateIntent, NavigateCommand, OpenPageRequest,
    PageId, PrincipalId, RequestContext, RuntimeCommand, SessionId, TargetSpec, TypeTextCommand,
    WorkerId, WorkflowCheckpoint, WorkflowId,
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

    async fn collect_candidates(
        &self,
        _: &PageId,
        _: &TargetSpec,
    ) -> Result<Vec<dom_engine::Candidate>, CommandError> {
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

struct BlockingNavigateWorker {
    id: WorkerId,
    profile: PathBuf,
    started: Arc<tokio::sync::Semaphore>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl BrowserWorker for BlockingNavigateWorker {
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
        self.started.add_permits(1);
        self.release.notified().await;
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

    async fn collect_candidates(
        &self,
        _: &PageId,
        _: &TargetSpec,
    ) -> Result<Vec<dom_engine::Candidate>, CommandError> {
        Ok(Vec::new())
    }

    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }
}

struct BlockingNavigateFactory {
    started: Arc<tokio::sync::Semaphore>,
    release: Arc<tokio::sync::Notify>,
}

#[async_trait]
impl WorkerFactory for BlockingNavigateFactory {
    async fn launch(&self, session_id: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(Arc::new(BlockingNavigateWorker {
            id: WorkerId::new(),
            profile: PathBuf::from(format!("/profiles/{}", session_id.0)),
            started: Arc::clone(&self.started),
            release: Arc::clone(&self.release),
        }))
    }
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

/// Same shape as `authenticated`, with an explicit capability set so per-primitive
/// gating can be exercised independent of the coarse `browser:mutate` grant.
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
        command: RuntimeCommand::Primitive(types::PrimitiveCommand::ListPages(
            types::ListPagesCommand,
        )),
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
            command_identity_sha256(
                request.schema_version,
                &request.session_id,
                &request.page_id,
                &request.command,
                false,
            )
            .unwrap(),
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
                command_identity_sha256(
                    request.schema_version,
                    &request.session_id,
                    &request.page_id,
                    &request.command,
                    false,
                )
                .unwrap(),
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
    // A runtime built without vision wiring must say so: the repair for
    // `visionAssistFailed` diverges on exactly this signal.
    assert!(!info.capabilities.contains(&"vision-assist".to_owned()));
    assert!(!info.capabilities.contains(&"vision-provider".to_owned()));
    let metrics = info
        .operational_metrics
        .expect("current runtimes expose process-local operational metrics");
    assert_eq!(metrics.vision.attempted, 0);
    assert_eq!(metrics.workflow_calls.composite_workflow, 0);
    assert!(api.list_sessions(read_context).await.unwrap().is_empty());
}

#[tokio::test]
async fn fingerprint_and_humanize_policies_require_their_capabilities() {
    let base = [
        Capability::SessionRead,
        Capability::SessionWrite,
        Capability::PageWrite,
        Capability::BrowserMutate,
    ];
    let (api, handle) = authenticated_with(RuntimeService::default(), base).await;
    for policy in [
        ExecutionPolicy {
            fingerprint: true,
            ..ExecutionPolicy::default()
        },
        ExecutionPolicy {
            humanize: true,
            ..ExecutionPolicy::default()
        },
    ] {
        let error = api
            .create_session(
                handle.context(expiry(), None),
                CreateSessionRequest {
                    profile: "policy-gated".into(),
                    proxy: None,
                    execution_policy: policy,
                    zigzagzig: false,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, InterfaceErrorCode::MissingCapability);
    }
    assert_eq!(api.create_session_dispatch_count(), 0);

    let (api, handle) = authenticated_with(
        RuntimeService::default(),
        base.into_iter()
            .chain([Capability::BrowserFingerprint, Capability::BrowserHumanize]),
    )
    .await;
    api.create_session(
        handle.context(expiry(), None),
        CreateSessionRequest {
            profile: "policy-gated".into(),
            proxy: None,
            execution_policy: ExecutionPolicy {
                fingerprint: true,
                humanize: true,
                ..ExecutionPolicy::default()
            },
            zigzagzig: false,
        },
    )
    .await
    .unwrap();
}

#[tokio::test]
async fn zigzagzig_sessions_require_the_fingerprint_and_humanize_capabilities() {
    // Godmode forces fingerprint + humanize server-side, so it stands in for
    // both grants at the creation gate: a principal missing either
    // capability gets no flagged session at all.
    let base = [
        Capability::SessionRead,
        Capability::SessionWrite,
        Capability::PageWrite,
        Capability::BrowserMutate,
    ];
    for extra in [
        vec![],
        vec![Capability::BrowserFingerprint],
        vec![Capability::BrowserHumanize],
    ] {
        let (api, handle) =
            authenticated_with(RuntimeService::default(), base.into_iter().chain(extra)).await;
        let error = api
            .create_session(
                handle.context(expiry(), None),
                CreateSessionRequest {
                    profile: "godmode-denied".into(),
                    proxy: None,
                    execution_policy: Default::default(),
                    zigzagzig: true,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, InterfaceErrorCode::MissingCapability);
    }

    let (api, handle) = authenticated_with(
        RuntimeService::default(),
        base.into_iter()
            .chain([Capability::BrowserFingerprint, Capability::BrowserHumanize]),
    )
    .await;
    let session = api
        .create_session(
            handle.context(expiry(), None),
            CreateSessionRequest {
                profile: "godmode".into(),
                proxy: None,
                execution_policy: Default::default(),
                zigzagzig: true,
            },
        )
        .await
        .unwrap();
    assert!(session.zigzagzig);
    assert!(session.execution_policy.fingerprint);
    assert!(session.execution_policy.humanize);
    assert!(session.execution_policy.vision_assist);
    assert!(session.execution_policy.javascript_evaluation);
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
                zigzagzig: false,
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
                command: RuntimeCommand::Primitive(types::PrimitiveCommand::ListPages(
                    types::ListPagesCommand,
                )),
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
                zigzagzig: false,
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
                zigzagzig: false,
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
async fn checkpoint_rejects_another_principals_session_before_persistence() {
    let authority = AuthorityStore::in_memory();
    let owner_principal = PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000010"));
    let other_principal = PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000011"));
    let owner_token = authority
        .issue(
            owner_principal.clone(),
            [Capability::RecoveryWrite],
            expiry(),
        )
        .await
        .unwrap()
        .expose_once();
    let other_token = authority
        .issue(other_principal, [Capability::RecoveryWrite], expiry())
        .await
        .unwrap()
        .expose_once();
    authority.verify(&owner_token).await.unwrap();
    let other_handle = authority.verify(&other_token).await.unwrap();
    let (ownership, recorder) = SessionOwnershipRegistry::bounded(4);
    let session_id = SessionId::new();
    recorder
        .record_authenticated_session(owner_principal, session_id.clone())
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let runtime = RuntimeService::with_recovery(
        Default::default(),
        Default::default(),
        RecoveryCoordinator::new(store.clone()),
    );
    let other =
        AuthenticatedRuntime::with_session_ownership(runtime, other_handle.clone(), recorder);
    let checkpoint = WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: CheckpointId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id,
        page_id: PageId::new(),
        restart_url: "https://example.test".into(),
        current_url: "https://example.test".into(),
        cursor: None,
        boundary_command_id: None,
        recovery_class: CommandClass::Replayable,
        invariants: Vec::new(),
        replayable_inputs: Vec::new(),
        evidence: Vec::new(),
        recovery_history: Vec::new(),
        recovery_receipts: Vec::new(),
        created_at: Utc::now(),
    };

    let denial = other
        .checkpoint(
            other_handle.context(expiry(), None),
            checkpoint.clone(),
            Vec::new(),
        )
        .await
        .unwrap_err();

    assert_eq!(denial.code, InterfaceErrorCode::NotFound);
    assert_eq!(other.checkpoint_dispatch_count(), 0);
    assert!(store.load(&checkpoint.workflow_id).await.is_err());
    assert!(ownership.owns_session(
        &PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000010")),
        &checkpoint.session_id
    ));
}

#[tokio::test]
async fn recovery_rejects_another_principals_checkpoint_before_browser_dispatch() {
    let authority = AuthorityStore::in_memory();
    let owner_principal = PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000010"));
    let other_principal = PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000011"));
    let other_token = authority
        .issue(other_principal, [Capability::RecoveryWrite], expiry())
        .await
        .unwrap()
        .expose_once();
    let other_handle = authority.verify(&other_token).await.unwrap();
    let (_ownership, recorder) = SessionOwnershipRegistry::bounded(4);
    let session_id = SessionId::new();
    recorder
        .record_authenticated_session(owner_principal, session_id.clone())
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let checkpoint = WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: CheckpointId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id,
        page_id: PageId::new(),
        restart_url: "https://example.test".into(),
        current_url: "https://example.test".into(),
        cursor: None,
        boundary_command_id: None,
        recovery_class: CommandClass::Replayable,
        invariants: Vec::new(),
        replayable_inputs: Vec::new(),
        evidence: Vec::new(),
        recovery_history: Vec::new(),
        recovery_receipts: Vec::new(),
        created_at: Utc::now(),
    };
    store.save(&checkpoint).await.unwrap();
    let runtime = RuntimeService::with_recovery(
        Default::default(),
        Default::default(),
        RecoveryCoordinator::new(store),
    );
    let other =
        AuthenticatedRuntime::with_session_ownership(runtime, other_handle.clone(), recorder);

    let denial = other
        .recover(other_handle.context(expiry(), None), checkpoint.workflow_id)
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
                zigzagzig: false,
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
                zigzagzig: false,
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
                zigzagzig: false,
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
                zigzagzig: false,
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
        command: RuntimeCommand::Primitive(types::PrimitiveCommand::ListPages(
            types::ListPagesCommand,
        )),
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
                command: RuntimeCommand::Primitive(types::PrimitiveCommand::ListPages(
                    types::ListPagesCommand,
                )),
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
        command: RuntimeCommand::Primitive(types::PrimitiveCommand::UploadFiles(
            types::UploadFilesCommand {
                selector: "input[type=file]".into(),
                target: None,
                paths: vec!["/tmp/example.txt".into()],
            },
        )),
        ..submit_request()
    }
}

fn solve_challenge_envelope() -> CommandEnvelope {
    CommandEnvelope {
        command: RuntimeCommand::Intent(types::IntentCommand::SolveChallenge(
            types::SolveChallengeIntent {
                purpose: "clear the recaptcha blocking the form".into(),
                hints: types::SolveChallengeHints::default(),
            },
        )),
        ..submit_request()
    }
}

fn detect_challenge_envelope() -> CommandEnvelope {
    CommandEnvelope {
        command: RuntimeCommand::Intent(types::IntentCommand::DetectChallenge(
            types::DetectChallengeIntent {
                purpose: "check what blocks this page".into(),
                hints: types::DetectChallengeHints::default(),
            },
        )),
        ..submit_request()
    }
}

fn download_url_envelope() -> CommandEnvelope {
    CommandEnvelope {
        command: RuntimeCommand::Primitive(types::PrimitiveCommand::DownloadUrl(
            types::DownloadUrlCommand {
                url: "https://example.com/file.bin".into(),
                expected_content_type: None,
                max_bytes: 1024,
                save_as: None,
            },
        )),
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

    // No idempotency key, so the early return is infallible: any `Err` here could only
    // have come from the capability gate.
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
async fn challenge_intents_require_vision_assist_beyond_intent_execute() {
    for (name, envelope) in [
        ("solve", solve_challenge_envelope()),
        ("detect", detect_challenge_envelope()),
    ] {
        let (api, handle) = authenticated_with(
            RuntimeService::default(),
            [
                Capability::SessionWrite,
                Capability::PageWrite,
                Capability::BrowserMutate,
                Capability::IntentExecute,
            ],
        )
        .await;
        let context = handle.context(expiry(), None);

        let error = api.submit(context, envelope).await.unwrap_err();

        assert_eq!(
            error.code,
            InterfaceErrorCode::MissingCapability,
            "{name} must gate on vision:assist before dispatch"
        );
        assert_eq!(error.required_capability, Some(Capability::VisionAssist));
        assert_eq!(api.submit_dispatch_count(), 0, "{name} never dispatched");
    }
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

// RuntimeService's per-session ExecutionPolicy gate for EvaluateJavaScript, and its
// composition with AuthenticatedRuntime's token capability gate. Both fire before browser
// dispatch, so `RuntimeService::default()` (no worker pool) proves them: `pages.execute`
// on an unconfigured `PageRuntime` can only produce `Failed`, never `PolicyDenied`, so a
// `PolicyDenied` outcome can only have come from the gate.

fn evaluate_javascript_envelope(session_id: SessionId) -> CommandEnvelope {
    CommandEnvelope {
        session_id,
        command: RuntimeCommand::Primitive(types::PrimitiveCommand::EvaluateJavaScript(
            types::EvaluateJavaScriptCommand {
                expression: "1 + 1".into(),
                timeout_ms: 1_000,
                await_promise: false,
            },
        )),
        ..submit_request()
    }
}

#[tokio::test]
async fn evaluate_javascript_is_policy_denied_for_a_session_that_was_never_created() {
    // No `create_session` happened, so `self.sessions.get` returns `Err(NotFound)`. The
    // gate must deny, not skip the check: a missing or evicted session is never an
    // implicit allow.
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
            zigzagzig: false,
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
                ..types::ExecutionPolicy::default()
            },
            zigzagzig: false,
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
                ..types::ExecutionPolicy::default()
            },
            zigzagzig: false,
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
            zigzagzig: false,
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

fn locate_intent_envelope(session_id: SessionId) -> CommandEnvelope {
    CommandEnvelope {
        session_id,
        command: RuntimeCommand::Intent(IntentCommand::Locate(LocateIntent {
            purpose: "Continue".into(),
            hints: IntentHints::default(),
        })),
        ..submit_request()
    }
}

fn locate_intent_on_page(session_id: SessionId, page_id: PageId) -> CommandEnvelope {
    CommandEnvelope {
        session_id,
        page_id: Some(page_id),
        command: RuntimeCommand::Intent(IntentCommand::Locate(LocateIntent {
            purpose: "Continue".into(),
            hints: IntentHints::default(),
        })),
        ..submit_request()
    }
}

fn assert_failed_with(outcome: &CommandOutcome, expected: types::ErrorCode) {
    match outcome {
        CommandOutcome::Failed { error, .. } => assert_eq!(error.code, expected),
        other => panic!("expected Failed({expected:?}), got {other:?}"),
    }
}

#[tokio::test]
async fn locate_intent_without_intent_execute_capability_is_denied_before_dispatch() {
    let runtime = RuntimeService::default();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "default".into(),
            proxy: None,
            execution_policy: Default::default(),
            zigzagzig: false,
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
        .submit(context, locate_intent_envelope(session.id))
        .await
        .unwrap_err();

    assert_eq!(error.code, InterfaceErrorCode::MissingCapability);
    assert_eq!(error.required_capability, Some(Capability::IntentExecute));
    assert_eq!(api.submit_dispatch_count(), 0);
}

fn fill_files_intent_envelope(session_id: SessionId) -> CommandEnvelope {
    CommandEnvelope {
        session_id,
        command: RuntimeCommand::Intent(IntentCommand::Fill(FillIntent {
            purpose: "Resume".into(),
            hints: IntentHints::default(),
            value: ControlAction::SetFiles {
                paths: vec!["./data/uploads/cv.pdf".into()],
            },
        })),
        ..submit_request()
    }
}

fn complete_form_files_intent_envelope(session_id: SessionId) -> CommandEnvelope {
    CommandEnvelope {
        session_id,
        command: RuntimeCommand::Intent(IntentCommand::CompleteForm(CompleteFormIntent {
            purpose: "Complete application".into(),
            fields: vec![CompleteFormField {
                name: "resume".into(),
                purpose: "Resume".into(),
                hints: IntentHints::default(),
                value: ControlAction::SetFiles {
                    paths: vec!["./data/uploads/cv.pdf".into()],
                },
            }],
        })),
        ..submit_request()
    }
}

#[tokio::test]
async fn fill_files_intent_without_file_upload_capability_is_denied_before_dispatch() {
    let runtime = RuntimeService::default();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "default".into(),
            proxy: None,
            execution_policy: Default::default(),
            zigzagzig: false,
        })
        .await
        .unwrap();

    let (api, handle) = authenticated_with(
        runtime,
        [
            Capability::SessionWrite,
            Capability::PageWrite,
            Capability::BrowserMutate,
            Capability::IntentExecute,
        ],
    )
    .await;
    let context = handle.context(expiry(), None);

    let error = api
        .submit(context, fill_files_intent_envelope(session.id))
        .await
        .unwrap_err();

    assert_eq!(error.code, InterfaceErrorCode::MissingCapability);
    assert_eq!(error.required_capability, Some(Capability::FileUpload));
    assert_eq!(api.submit_dispatch_count(), 0);
}

#[tokio::test]
async fn complete_form_files_without_file_upload_capability_is_denied_before_dispatch() {
    let runtime = RuntimeService::default();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "default".into(),
            proxy: None,
            execution_policy: Default::default(),
            zigzagzig: false,
        })
        .await
        .unwrap();

    let (api, handle) = authenticated_with(
        runtime,
        [
            Capability::SessionWrite,
            Capability::PageWrite,
            Capability::BrowserMutate,
            Capability::IntentExecute,
        ],
    )
    .await;
    let context = handle.context(expiry(), None);

    let error = api
        .submit(context, complete_form_files_intent_envelope(session.id))
        .await
        .unwrap_err();

    assert_eq!(error.code, InterfaceErrorCode::MissingCapability);
    assert_eq!(error.required_capability, Some(Capability::FileUpload));
    assert_eq!(api.submit_dispatch_count(), 0);
}

#[tokio::test]
async fn locate_intent_does_not_require_vision_assist_upfront() {
    // Vision is double-gated at escalation time inside IntentEngine. Holding
    // intent:execute without vision:assist must clear the AuthenticatedRuntime
    // capability gate so the command can reach PageRuntime.
    let runtime = RuntimeService::default();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "default".into(),
            proxy: None,
            execution_policy: types::ExecutionPolicy {
                javascript_evaluation: false,
                vision_assist: false,
                ..types::ExecutionPolicy::default()
            },
            zigzagzig: false,
        })
        .await
        .unwrap();
    assert!(!session.execution_policy.vision_assist);

    let (api, handle) = authenticated_with(
        runtime,
        [
            Capability::SessionWrite,
            Capability::PageWrite,
            Capability::BrowserMutate,
            Capability::IntentExecute,
        ],
    )
    .await;
    let context = handle.context(expiry(), None);

    let outcome = api
        .submit(context, locate_intent_envelope(session.id))
        .await
        .unwrap();

    assert!(
        !matches!(outcome, CommandOutcome::PolicyDenied { .. }),
        "vision:assist must not be required upfront for intents; got {outcome:?}"
    );
    // MissingCapability would surface as Err; Ok proves the auth gate cleared.
}

#[tokio::test]
async fn cancelling_a_dispatched_command_releases_the_in_flight_count() {
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(
        workflow_journal::JsonlJournal::open(root.path().join("journal.jsonl"))
            .await
            .unwrap(),
    );
    let started = Arc::new(tokio::sync::Semaphore::new(0));
    let release = Arc::new(tokio::sync::Notify::new());
    let workers = Arc::new(WorkerPool::new(
        1,
        Arc::new(BlockingNavigateFactory {
            started: Arc::clone(&started),
            release,
        }),
    ));
    let service = RuntimeService::new(
        SessionManager::new(Arc::clone(&workers)),
        PageRuntime::new(journal, workers),
    );
    let session = service
        .create_session(CreateSessionRequest {
            profile: "cancelled-dispatch".into(),
            proxy: None,
            execution_policy: ExecutionPolicy::default(),
            zigzagzig: false,
        })
        .await
        .unwrap();
    let page = service
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();
    let submit_service = service.clone();
    let submit = tokio::spawn(async move {
        submit_service
            .submit(CommandEnvelope {
                session_id: session.id,
                page_id: Some(page.id),
                command: RuntimeCommand::Primitive(types::PrimitiveCommand::Navigate(
                    NavigateCommand {
                        url: "https://example.test".into(),
                        wait_until: types::WaitUntil::Interactive,
                        timeout_ms: 30_000,
                    },
                )),
                ..submit_request()
            })
            .await
    });

    started.acquire().await.unwrap().forget();
    assert_eq!(service.runtime_info().await.queued_jobs, 1);
    submit.abort();
    assert!(submit.await.unwrap_err().is_cancelled());

    assert_eq!(
        service.runtime_info().await.queued_jobs,
        0,
        "dropping a dispatched command must release runtime capacity"
    );
}

#[tokio::test]
async fn one_shot_vision_consent_applies_to_exactly_one_command() {
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(
        workflow_journal::JsonlJournal::open(root.path().join("journal.jsonl"))
            .await
            .unwrap(),
    );
    let closes = Arc::new(AtomicUsize::new(0));
    let workers = Arc::new(WorkerPool::new(
        1,
        Arc::new(LifecycleFactory {
            attempts: AtomicUsize::new(0),
            fail_first: false,
            closes,
        }),
    ));
    let service = RuntimeService::new(
        SessionManager::new(workers.clone()),
        PageRuntime::new(journal, workers),
    );
    let session = service
        .create_session(CreateSessionRequest {
            profile: "one-shot".into(),
            proxy: None,
            execution_policy: ExecutionPolicy::default(),
            zigzagzig: false,
        })
        .await
        .unwrap();
    let page = service
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();
    let (api, handle) = authenticated_with(
        service.clone(),
        [
            Capability::SessionWrite,
            Capability::PageWrite,
            Capability::BrowserMutate,
            Capability::IntentExecute,
            Capability::VisionAssist,
        ],
    )
    .await;

    let granted = api
        .submit_with_one_shot_vision_consent(
            handle.context(expiry(), None),
            locate_intent_on_page(session.id.clone(), page.id.clone()),
        )
        .await
        .expect("held vision capability authorizes one-shot consent");
    assert_failed_with(&granted, types::ErrorCode::VisionAssistFailed);

    let stored = service.sessions.get(&session.id).await.unwrap();
    assert!(!stored.execution_policy.vision_assist);

    let ordinary = api
        .submit(
            handle.context(expiry(), None),
            locate_intent_on_page(session.id, page.id),
        )
        .await
        .unwrap();
    assert_failed_with(&ordinary, types::ErrorCode::VisionAssistDenied);
}

#[tokio::test]
async fn one_shot_vision_consent_requires_the_principal_capability() {
    let runtime = RuntimeService::default();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "one-shot-denied".into(),
            proxy: None,
            execution_policy: ExecutionPolicy::default(),
            zigzagzig: false,
        })
        .await
        .unwrap();
    let (api, handle) = authenticated_with(
        runtime,
        [
            Capability::SessionWrite,
            Capability::PageWrite,
            Capability::BrowserMutate,
            Capability::IntentExecute,
        ],
    )
    .await;

    let error = api
        .submit_with_one_shot_vision_consent(
            handle.context(expiry(), None),
            locate_intent_envelope(session.id),
        )
        .await
        .unwrap_err();

    assert_eq!(error.code, InterfaceErrorCode::MissingCapability);
    assert_eq!(error.required_capability, Some(Capability::VisionAssist));
    assert_eq!(api.submit_dispatch_count(), 0);
}

#[tokio::test]
async fn one_shot_consent_is_part_of_the_idempotency_identity() {
    let runtime = RuntimeService::default();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "one-shot-idempotency".into(),
            proxy: None,
            execution_policy: ExecutionPolicy::default(),
            zigzagzig: false,
        })
        .await
        .unwrap();
    let (api, handle) = authenticated_with(
        runtime,
        [
            Capability::SessionWrite,
            Capability::PageWrite,
            Capability::BrowserMutate,
            Capability::IntentExecute,
            Capability::VisionAssist,
        ],
    )
    .await;
    let key = IdempotencyKey::try_from("one-shot-consent-mode").unwrap();
    let request = locate_intent_envelope(session.id);

    api.submit_with_one_shot_vision_consent(
        handle.context(expiry(), Some(key.clone())),
        request.clone(),
    )
    .await
    .expect("one-shot outcome is retained");
    api.submit_with_one_shot_vision_consent(
        handle.context(expiry(), Some(key.clone())),
        request.clone(),
    )
    .await
    .expect("the same one-shot submission replays");
    let conflict = api
        .submit(handle.context(expiry(), Some(key)), request)
        .await
        .expect_err("an ordinary submission must not replay a one-shot grant");

    assert_eq!(conflict.code, InterfaceErrorCode::IdempotencyConflict);
    assert_eq!(api.submit_dispatch_count(), 1);
}

#[tokio::test]
async fn create_session_replays_retained_session_and_conflicts_before_dispatch() {
    let (runtime, _, _) = runtime_with_workers(false);
    let (api, handle) = authenticated(runtime).await;
    let key = IdempotencyKey::try_from("sdk-retained-create-session").unwrap();

    let first = api
        .create_session(
            handle.context(expiry(), Some(key.clone())),
            request_profile("retained"),
        )
        .await
        .unwrap();
    let replayed = api
        .create_session(
            handle.context(expiry(), Some(key.clone())),
            request_profile("retained"),
        )
        .await
        .unwrap();
    assert_eq!(replayed.id, first.id);
    assert_eq!(api.create_session_dispatch_count(), 1);

    let conflict = api
        .create_session(
            handle.context(expiry(), Some(key)),
            request_profile("different"),
        )
        .await
        .unwrap_err();
    assert_eq!(conflict.code, InterfaceErrorCode::IdempotencyConflict);
    assert_eq!(api.create_session_dispatch_count(), 1);
}

fn request_profile(profile: &str) -> CreateSessionRequest {
    CreateSessionRequest {
        profile: profile.into(),
        proxy: None,
        execution_policy: Default::default(),
        zigzagzig: false,
    }
}

#[tokio::test]
async fn vision_enabled_session_is_rejected_when_runtime_has_no_provider() {
    let runtime = RuntimeService::default();
    let error = runtime
        .create_session(CreateSessionRequest {
            profile: "vision-without-provider".into(),
            proxy: None,
            execution_policy: ExecutionPolicy {
                vision_assist: true,
                ..ExecutionPolicy::default()
            },
            zigzagzig: false,
        })
        .await
        .unwrap_err();
    assert!(matches!(error, types::RuntimeError::InvalidRequest(_)));
    assert!(error.to_string().contains("vision provider"), "{error}");
}

#[tokio::test]
async fn create_session_failure_abandons_the_reservation_so_retry_dispatches() {
    let (runtime, _, _) = runtime_with_workers(true);
    let (api, handle) = authenticated(runtime).await;
    let key = IdempotencyKey::try_from("sdk-abandoned-create-session").unwrap();

    assert!(api
        .create_session(
            handle.context(expiry(), Some(key.clone())),
            request_profile("fails-once"),
        )
        .await
        .is_err());
    let session = api
        .create_session(
            handle.context(expiry(), Some(key)),
            request_profile("fails-once"),
        )
        .await
        .unwrap();

    assert_eq!(api.create_session_dispatch_count(), 2);
    assert!(api
        .list_sessions(handle.context(expiry(), None))
        .await
        .unwrap()
        .iter()
        .any(|listed| listed.id == session.id));
}

#[tokio::test]
async fn checkpoint_replays_retained_checkpoint_and_conflicts_before_dispatch() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let runtime = RuntimeService::with_recovery(
        Default::default(),
        Default::default(),
        RecoveryCoordinator::new(store),
    );
    let (api, handle) = authenticated_with(runtime, [Capability::RecoveryWrite]).await;
    let key = IdempotencyKey::try_from("sdk-retained-checkpoint").unwrap();
    let checkpoint = WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: CheckpointId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: PageId::new(),
        restart_url: "https://example.test".into(),
        current_url: "https://example.test".into(),
        cursor: None,
        boundary_command_id: None,
        recovery_class: CommandClass::Replayable,
        invariants: Vec::new(),
        replayable_inputs: Vec::new(),
        evidence: Vec::new(),
        recovery_history: Vec::new(),
        recovery_receipts: Vec::new(),
        created_at: Utc::now(),
    };

    let first = api
        .checkpoint(
            handle.context(expiry(), Some(key.clone())),
            checkpoint.clone(),
            Vec::new(),
        )
        .await
        .unwrap();
    let replayed = api
        .checkpoint(
            handle.context(expiry(), Some(key.clone())),
            checkpoint.clone(),
            Vec::new(),
        )
        .await
        .unwrap();
    assert_eq!(replayed.checkpoint_id, first.checkpoint_id);
    assert_eq!(api.checkpoint_dispatch_count(), 1);

    let mut changed = checkpoint.clone();
    changed.restart_url = "https://other.test".into();
    let conflict = api
        .checkpoint(handle.context(expiry(), Some(key)), changed, Vec::new())
        .await
        .unwrap_err();
    assert_eq!(conflict.code, InterfaceErrorCode::IdempotencyConflict);
    assert_eq!(api.checkpoint_dispatch_count(), 1);
}

#[tokio::test]
async fn delete_session_releases_the_session_worker_and_ownership() {
    let (runtime, pool, closes) = runtime_with_workers(false);
    let (api, context, ownership, _) = session_owned_runtime(runtime.clone(), 4).await;
    let session = api
        .create_session(context.clone(), request_profile("delete-me"))
        .await
        .unwrap();
    assert_eq!(pool.active_workers().await, 1);

    api.delete_session(context.clone(), session.id.clone())
        .await
        .unwrap();
    assert!(runtime.list_sessions().await.is_empty());
    assert_eq!(closes.load(Ordering::SeqCst), 1);
    assert!(!ownership.owns_session(&context.principal_id, &session.id));

    let denied = api.delete_session(context, session.id).await.unwrap_err();
    assert_eq!(denied.code, InterfaceErrorCode::NotFound);
}

#[tokio::test]
async fn delete_session_reclaims_page_runtime_entries_missing_from_session_state() {
    let (runtime, _, _) = runtime_with_workers(false);
    let (api, context, _, _) = session_owned_runtime(runtime.clone(), 4).await;
    let session = api
        .create_session(context.clone(), request_profile("delete-stale-page"))
        .await
        .unwrap();

    // Register directly in PageRuntime so SessionState.page_ids is
    // intentionally stale. Authenticated deletion must use the page registry
    // itself as the reclamation authority.
    let stale_page = runtime
        .pages
        .open(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await;
    assert!(runtime
        .sessions
        .get(&session.id)
        .await
        .unwrap()
        .page_ids
        .is_empty());
    assert!(runtime.pages.get(&stale_page.id).await.is_ok());

    api.delete_session(context, session.id).await.unwrap();

    assert!(matches!(
        runtime.pages.get(&stale_page.id).await,
        Err(types::RuntimeError::NotFound(_))
    ));
}

#[tokio::test]
async fn delete_session_rejects_another_principals_session_as_not_found() {
    let (runtime, _, _) = runtime_with_workers(false);
    let (api, context, _, recorder) = session_owned_runtime(runtime.clone(), 4).await;
    let session = api
        .create_session(context.clone(), request_profile("owned"))
        .await
        .unwrap();
    let authority = AuthorityStore::in_memory();
    let other_token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000077")),
            [Capability::SessionWrite],
            expiry(),
        )
        .await
        .unwrap()
        .expose_once();
    let other_handle = authority.verify(&other_token).await.unwrap();
    let other = AuthenticatedRuntime::with_session_ownership(
        runtime.clone(),
        other_handle.clone(),
        recorder,
    );

    let denied = other
        .delete_session(other_handle.context(expiry(), None), session.id.clone())
        .await
        .unwrap_err();
    assert_eq!(denied.code, InterfaceErrorCode::NotFound);
    let _ = &context;
    assert!(runtime
        .list_sessions()
        .await
        .iter()
        .any(|listed| listed.id == session.id));
}

#[tokio::test]
async fn recovery_status_returns_the_checkpoint_and_requires_ownership() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let checkpoint = WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: CheckpointId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: PageId::new(),
        restart_url: "https://example.test".into(),
        current_url: "https://example.test".into(),
        cursor: None,
        boundary_command_id: None,
        recovery_class: CommandClass::Replayable,
        invariants: Vec::new(),
        replayable_inputs: Vec::new(),
        evidence: Vec::new(),
        recovery_history: Vec::new(),
        recovery_receipts: Vec::new(),
        created_at: Utc::now(),
    };
    store.save(&checkpoint).await.unwrap();
    let runtime = RuntimeService::with_recovery(
        Default::default(),
        Default::default(),
        RecoveryCoordinator::new(store),
    );
    let authority = AuthorityStore::in_memory();
    let principal = PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000041"));
    let token = authority
        .issue(principal.clone(), [Capability::RecoveryRead], expiry())
        .await
        .unwrap()
        .expose_once();
    let handle = authority.verify(&token).await.unwrap();
    let (_ownership, recorder) = SessionOwnershipRegistry::bounded(4);
    recorder
        .record_authenticated_session(principal, checkpoint.session_id.clone())
        .unwrap();
    let api = AuthenticatedRuntime::with_session_ownership(runtime, handle.clone(), recorder);

    let status = api
        .recovery_status(
            handle.context(expiry(), None),
            checkpoint.workflow_id.clone(),
        )
        .await
        .unwrap();
    assert_eq!(status.checkpoint.checkpoint_id, checkpoint.checkpoint_id);
    assert!(status.receipts.is_empty());

    let missing = api
        .recovery_status(handle.context(expiry(), None), WorkflowId::new())
        .await;
    assert!(missing.is_err());
}
