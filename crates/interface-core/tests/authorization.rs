use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use chrono::{Duration, Utc};
use interface_core::{
    Authority, AuthorityStore, AuthorizationGuard, SessionOwnershipAuthority,
    SessionOwnershipRecordError, SessionOwnershipRegistry,
};
use sha2::{Digest, Sha256};
use types::{
    AttemptId, Capability, CommandEnvelope, CommandId, CommandOutcome, InterfaceErrorCode,
    InterfaceOperation, PrincipalId, RequestContext, RuntimeCommand, SessionId, WorkflowId,
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
        command: RuntimeCommand::Primitive(types::PrimitiveCommand::ListPages(
            types::ListPagesCommand,
        )),
    }
}

#[tokio::test]
async fn expired_hash_enrollment_is_rejected_without_consuming_capacity() {
    let store = AuthorityStore::with_capacity(1);
    let bearer = "expired-external-authority-bearer";
    let token_hash: [u8; 32] = Sha256::digest(bearer.as_bytes()).into();

    assert!(store
        .enroll_hash(
            token_hash,
            principal(),
            [Capability::SessionRead],
            Utc::now() - Duration::seconds(1),
        )
        .await
        .is_err());
    assert!(store
        .enroll_hash(token_hash, principal(), [Capability::SessionRead], expiry())
        .await
        .is_ok());
}

#[tokio::test]
async fn externally_enrolled_hash_and_handle_share_expiry_and_revocation_state() {
    let store = AuthorityStore::with_capacity(1);
    let bearer = "external-authority-bearer-0000001";
    let token_hash: [u8; 32] = Sha256::digest(bearer.as_bytes()).into();
    let handle = store
        .enroll_hash(token_hash, principal(), [Capability::SessionRead], expiry())
        .await
        .unwrap();

    assert!(store.authenticate(bearer, Utc::now()).await.is_ok());
    let wrong = store
        .authenticate("different-authority-bearer-00001", Utc::now())
        .await
        .unwrap_err();
    let malformed = store
        .authenticate("not a bearer", Utc::now())
        .await
        .unwrap_err();
    assert_eq!(wrong.code, InterfaceErrorCode::AuthenticationFailed);
    assert_eq!(wrong.message, malformed.message);

    let context = handle.context(expiry(), None);
    store.revoke(&principal()).await.unwrap();
    assert_eq!(
        store
            .authenticate(bearer, Utc::now())
            .await
            .unwrap_err()
            .code,
        InterfaceErrorCode::AuthenticationFailed
    );
    assert_eq!(
        AuthorizationGuard::new(handle)
            .authorize(&context, InterfaceOperation::RuntimeInfo)
            .unwrap_err()
            .code,
        InterfaceErrorCode::AuthenticationFailed
    );

    let expiring = AuthorityStore::with_capacity(1);
    let expires_at = Utc::now() + Duration::minutes(1);
    let expired_handle = expiring
        .enroll_hash(
            token_hash,
            principal(),
            [Capability::SessionRead],
            expires_at,
        )
        .await
        .unwrap();
    assert_eq!(
        expiring
            .authenticate(bearer, expires_at + Duration::seconds(1))
            .await
            .unwrap_err()
            .code,
        InterfaceErrorCode::AuthenticationFailed
    );
    assert!(!expired_handle.is_valid_at(expires_at + Duration::seconds(1)));
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
            now + Duration::seconds(1),
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
    let validation_time = now + Duration::seconds(2);
    let errors = [unknown, malformed, expired.as_str(), revoked.as_str()]
        .into_iter()
        .map(|token| async {
            store
                .authenticate(token, validation_time)
                .await
                .unwrap_err()
        });
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
        store
            .authenticate(&live, validation_time)
            .await
            .unwrap_err()
            .message,
        errors[0].message
    );
}

#[tokio::test]
async fn authority_capacity_refuses_live_overflow_and_reclaims_invalid_records() {
    let store = AuthorityStore::with_capacity(1);
    let now = Utc::now();
    store
        .issue(
            principal(),
            [Capability::SessionRead],
            now + Duration::minutes(5),
        )
        .await
        .unwrap();
    let full = store
        .issue(
            PrincipalId::from_uuid(uuid!("30000000-0000-0000-0000-000000000003")),
            [Capability::SessionRead],
            now + Duration::minutes(5),
        )
        .await
        .unwrap_err();
    assert_eq!(full.code, InterfaceErrorCode::ResourceExhausted);

    store.revoke(&principal()).await.unwrap();
    assert!(store
        .issue(
            PrincipalId::from_uuid(uuid!("30000000-0000-0000-0000-000000000003")),
            [Capability::SessionRead],
            now + Duration::minutes(5),
        )
        .await
        .is_ok());

    let expired = AuthorityStore::with_capacity(1);
    assert!(expired
        .issue(
            principal(),
            [Capability::SessionRead],
            now - Duration::seconds(1),
        )
        .await
        .is_err());
    assert!(expired
        .issue(
            PrincipalId::from_uuid(uuid!("40000000-0000-0000-0000-000000000004")),
            [Capability::SessionRead],
            now + Duration::minutes(5),
        )
        .await
        .is_ok());
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

    async fn submit(&self, envelope: CommandEnvelope) -> CommandOutcome {
        self.record();
        CommandOutcome::Completed {
            command_id: envelope.command_id,
            evidence: Vec::new(),
        }
    }
}

async fn dispatch_submit(
    guard: &AuthorizationGuard,
    runtime: &RecordingRuntime,
    context: RequestContext,
    envelope: CommandEnvelope,
) -> interface_core::InterfaceResult<CommandOutcome> {
    guard.authorize(&context, InterfaceOperation::SubmitCommand)?;
    Ok(runtime.submit(envelope).await)
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
    let api = AuthorizationGuard::new(authority);

    let error = dispatch_submit(&api, &runtime, context, envelope())
        .await
        .unwrap_err();
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
    let api = AuthorizationGuard::new(authority);

    let error = dispatch_submit(&api, &runtime, forged, envelope())
        .await
        .unwrap_err();
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
    let api = AuthorizationGuard::new(authority);

    let error = dispatch_submit(&api, &runtime, expired, envelope())
        .await
        .unwrap_err();
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
    let api = AuthorizationGuard::new(authority);
    store.revoke(&principal()).await.unwrap();

    let error = dispatch_submit(&api, &runtime, context, envelope())
        .await
        .unwrap_err();
    assert_eq!(error.code, InterfaceErrorCode::AuthenticationFailed);
    assert_eq!(runtime.calls(), 0);
}

#[tokio::test]
async fn require_capability_grants_when_authority_and_context_both_hold_it() {
    let store = AuthorityStore::in_memory();
    let token = store
        .issue(principal(), [Capability::FileUpload], expiry())
        .await
        .unwrap()
        .expose_once();
    let authority = store.verify(&token).await.unwrap();
    let context = authority.context(expiry(), None);
    let api = AuthorizationGuard::new(authority);

    assert!(api
        .require_capability(&context, Capability::FileUpload)
        .is_ok());
}

#[tokio::test]
async fn require_capability_denies_when_authority_lacks_it_even_if_context_claims_it() {
    let store = AuthorityStore::in_memory();
    let token = store
        .issue(principal(), [Capability::BrowserMutate], expiry())
        .await
        .unwrap()
        .expose_once();
    let authority = store.verify(&token).await.unwrap();
    let forged = RequestContext::new_for_test(principal(), [Capability::FileUpload], expiry());
    let api = AuthorizationGuard::new(authority);

    let error = api
        .require_capability(&forged, Capability::FileUpload)
        .unwrap_err();
    assert_eq!(error.code, InterfaceErrorCode::MissingCapability);
    assert_eq!(error.required_capability, Some(Capability::FileUpload));
}

#[tokio::test]
async fn require_capability_denies_when_context_lacks_it_even_if_authority_holds_it() {
    let store = AuthorityStore::in_memory();
    let token = store
        .issue(
            principal(),
            [Capability::BrowserMutate, Capability::FileDownload],
            expiry(),
        )
        .await
        .unwrap()
        .expose_once();
    let authority = store.verify(&token).await.unwrap();
    let narrow = RequestContext::new_for_test(principal(), [Capability::BrowserMutate], expiry());
    let api = AuthorizationGuard::new(authority);

    let error = api
        .require_capability(&narrow, Capability::FileDownload)
        .unwrap_err();
    assert_eq!(error.code, InterfaceErrorCode::MissingCapability);
    assert_eq!(error.required_capability, Some(Capability::FileDownload));
}

#[test]
fn trusted_session_ownership_recorder_is_bounded_and_cannot_rebind() {
    let (registry, recorder) = SessionOwnershipRegistry::bounded(1);
    let session_id = SessionId::new();
    let owner = principal();
    let attacker = PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000002"));

    let reservation = recorder.reserve(owner.clone()).unwrap();
    assert!(matches!(
        recorder.reserve(attacker.clone()),
        Err(SessionOwnershipRecordError::CapacityExhausted)
    ));
    drop(reservation);

    let reservation = recorder.reserve(owner.clone()).unwrap();
    reservation.finalize(session_id.clone()).unwrap();

    assert!(registry.owns_session(&owner, &session_id));
    assert_eq!(
        recorder.record_authenticated_session(attacker, session_id.clone()),
        Err(SessionOwnershipRecordError::OwnershipConflict)
    );
    assert_eq!(
        recorder.record_authenticated_session(owner.clone(), SessionId::new()),
        Err(SessionOwnershipRecordError::CapacityExhausted)
    );
    assert!(registry.owns_session(&owner, &session_id));
}
