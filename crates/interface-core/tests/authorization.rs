use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use interface_core::{AuthenticatedRuntime, Authority, AuthorityStore, RuntimeInterface};
use types::{
    AttemptId, Capability, CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest,
    Evidence, IdempotencyKey, InterfaceErrorCode, OpenPageRequest, PageState, PrincipalId,
    RecoveryDecision, RequestContext, RuntimeInfo, SessionId, SessionState, WorkflowCheckpoint,
    WorkflowId,
};
use uuid::uuid;

fn principal() -> PrincipalId {
    PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000001"))
}

fn expiry() -> chrono::DateTime<Utc> {
    Utc::now() + Duration::minutes(5)
}

fn envelope() -> CommandEnvelope {
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

#[tokio::test]
async fn token_verification_is_scoped_and_raw_token_is_never_retained() {
    let store = AuthorityStore::in_memory();
    let issued = store
        .issue(principal(), [Capability::SessionRead], expiry())
        .await
        .unwrap();
    let issued_debug = format!("{issued:?}");
    let token = issued.expose_once().to_owned();

    let handle = store.verify(&token).await.unwrap();
    let context = handle.context(expiry(), None);
    assert_eq!(context.principal_id, principal());
    assert!(context.capabilities.contains(Capability::SessionRead));
    assert_eq!(
        store.verify("wrong").await.unwrap_err().code,
        InterfaceErrorCode::AuthenticationFailed
    );
    assert!(!issued_debug.contains(&token));
    assert!(!format!("{store:?}").contains(&token));
    assert!(!format!("{handle:?}").contains(&token));
}

#[tokio::test]
async fn unknown_expired_revoked_and_malformed_tokens_are_indistinguishable() {
    let store = AuthorityStore::in_memory();
    let now = Utc::now();
    let live_principal = principal();
    let live = store
        .issue(
            live_principal.clone(),
            [Capability::SessionRead],
            now + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once()
        .to_owned();
    let expired = store
        .issue(
            PrincipalId::from_uuid(uuid!("20000000-0000-0000-0000-000000000002")),
            [Capability::SessionRead],
            now - Duration::seconds(1),
        )
        .await
        .unwrap()
        .expose_once()
        .to_owned();
    let revoked = store
        .issue(
            live_principal.clone(),
            [Capability::SessionRead],
            now + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once()
        .to_owned();
    store.revoke(&live_principal).await.unwrap();

    let unknown = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
    let malformed = "not+a+url-safe-token";
    let errors = [unknown, malformed, expired.as_str(), revoked.as_str()]
        .into_iter()
        .map(|token| async { store.authenticate(token, now).await.unwrap_err() });
    let errors = futures::future::join_all(errors).await;
    for error in &errors {
        assert_eq!(error.code, InterfaceErrorCode::AuthenticationFailed);
        assert!(!error.retryable);
        assert!(!error.reconciliation_required);
        assert!(error.required_capability.is_none());
    }
    for error in errors.iter().skip(1) {
        assert_eq!(error.code, errors[0].code);
        assert_eq!(error.message, errors[0].message);
    }

    assert_eq!(
        store.authenticate(&live, now).await.unwrap_err().message,
        errors[0].message
    );
}

#[derive(Clone, Default)]
struct RecordingRuntime {
    calls: Arc<AtomicUsize>,
}

impl RecordingRuntime {
    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }

    fn record(&self) {
        self.calls.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl RuntimeInterface for RecordingRuntime {
    async fn runtime_info(
        &self,
        _ctx: RequestContext,
    ) -> interface_core::InterfaceResult<RuntimeInfo> {
        self.record();
        unreachable!("authorization tests must not dispatch")
    }

    async fn list_sessions(
        &self,
        _ctx: RequestContext,
    ) -> interface_core::InterfaceResult<Vec<SessionState>> {
        self.record();
        unreachable!("authorization tests must not dispatch")
    }

    async fn create_session(
        &self,
        _ctx: RequestContext,
        _req: CreateSessionRequest,
    ) -> interface_core::InterfaceResult<SessionState> {
        self.record();
        unreachable!("authorization tests must not dispatch")
    }

    async fn open_page(
        &self,
        _ctx: RequestContext,
        _req: OpenPageRequest,
    ) -> interface_core::InterfaceResult<PageState> {
        self.record();
        unreachable!("authorization tests must not dispatch")
    }

    async fn submit(
        &self,
        _ctx: RequestContext,
        envelope: CommandEnvelope,
    ) -> interface_core::InterfaceResult<CommandOutcome> {
        self.record();
        Ok(CommandOutcome::Completed {
            command_id: envelope.command_id,
            evidence: Vec::new(),
        })
    }

    async fn checkpoint(
        &self,
        _ctx: RequestContext,
        _checkpoint: WorkflowCheckpoint,
        _evidence: Vec<Evidence>,
    ) -> interface_core::InterfaceResult<WorkflowCheckpoint> {
        self.record();
        unreachable!("authorization tests must not dispatch")
    }

    async fn recover(
        &self,
        _ctx: RequestContext,
        _workflow: WorkflowId,
    ) -> interface_core::InterfaceResult<RecoveryDecision> {
        self.record();
        unreachable!("authorization tests must not dispatch")
    }
}

#[tokio::test]
async fn authorization_denies_missing_capability_before_runtime_dispatch() {
    let store = AuthorityStore::in_memory();
    let token = store
        .issue(principal(), [Capability::PageRead], expiry())
        .await
        .unwrap()
        .expose_once()
        .to_owned();
    let authority = store.verify(&token).await.unwrap();
    let context = authority.context(expiry(), None);
    let runtime = RecordingRuntime::default();
    let api = AuthenticatedRuntime::new(runtime.clone(), authority);

    let error = api.submit(context, envelope()).await.unwrap_err();
    assert_eq!(error.code, InterfaceErrorCode::MissingCapability);
    assert_eq!(error.required_capability, Some(Capability::BrowserMutate));
    assert_eq!(runtime.calls(), 0);
}

#[tokio::test]
async fn handle_authority_cannot_be_expanded_by_a_forged_context() {
    let store = AuthorityStore::in_memory();
    let token = store
        .issue(principal(), [Capability::PageRead], expiry())
        .await
        .unwrap()
        .expose_once()
        .to_owned();
    let authority = store.verify(&token).await.unwrap();
    let forged = RequestContext::new_for_test(principal(), [Capability::BrowserMutate], expiry());
    let runtime = RecordingRuntime::default();
    let api = AuthenticatedRuntime::new(runtime.clone(), authority);

    let error = api.submit(forged, envelope()).await.unwrap_err();
    assert_eq!(error.code, InterfaceErrorCode::MissingCapability);
    assert_eq!(runtime.calls(), 0);
}

#[tokio::test]
async fn deadline_is_rejected_before_capability_and_dispatch() {
    let store = AuthorityStore::in_memory();
    let token = store
        .issue(principal(), [Capability::PageRead], expiry())
        .await
        .unwrap()
        .expose_once()
        .to_owned();
    let authority = store.verify(&token).await.unwrap();
    let expired = authority.context(Utc::now() - Duration::seconds(1), None);
    let runtime = RecordingRuntime::default();
    let api = AuthenticatedRuntime::new(runtime.clone(), authority);

    let error = api.submit(expired, envelope()).await.unwrap_err();
    assert_eq!(error.code, InterfaceErrorCode::DeadlineExceeded);
    assert_eq!(runtime.calls(), 0);
}

#[tokio::test]
async fn revocation_invalidates_an_already_issued_handle_before_dispatch() {
    let store = AuthorityStore::in_memory();
    let token = store
        .issue(principal(), [Capability::BrowserMutate], expiry())
        .await
        .unwrap()
        .expose_once();
    let authority = store.verify(&token).await.unwrap();
    let context = authority.context(expiry(), None);
    let runtime = RecordingRuntime::default();
    let api = AuthenticatedRuntime::new(runtime.clone(), authority);
    store.revoke(&principal()).await.unwrap();

    let error = api.submit(context, envelope()).await.unwrap_err();
    assert_eq!(error.code, InterfaceErrorCode::AuthenticationFailed);
    assert_eq!(runtime.calls(), 0);
}

#[tokio::test]
async fn matching_idempotent_submit_reuses_only_the_committed_outcome() {
    let store = AuthorityStore::in_memory();
    let token = store
        .issue(principal(), [Capability::BrowserMutate], expiry())
        .await
        .unwrap()
        .expose_once()
        .to_owned();
    let authority = store.verify(&token).await.unwrap();
    let key = IdempotencyKey::try_from("same-submit").unwrap();
    let context = authority.context(expiry(), Some(key.clone()));
    let runtime = RecordingRuntime::default();
    let api = AuthenticatedRuntime::new(runtime.clone(), authority);
    let request = envelope();

    api.submit(context.clone(), request.clone()).await.unwrap();
    api.submit(context.clone(), request).await.unwrap();
    assert_eq!(runtime.calls(), 1);

    let error = api
        .submit(context, envelope())
        .await
        .expect_err("same key with different request must fail closed");
    assert_eq!(error.code, InterfaceErrorCode::IdempotencyConflict);
    assert_eq!(runtime.calls(), 1);
}
