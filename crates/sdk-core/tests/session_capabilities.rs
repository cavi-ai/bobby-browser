use async_trait::async_trait;
use chrono::{Duration, Utc};
use interface_core::{Authority, AuthorityStore, RuntimeInterface, SessionOwnershipRegistry};
use sdk_core::AuthenticatedRuntime;
use std::{
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
};
use types::{
    Capability, CapabilitySet, CommandError, CreateSessionRequest, ExecutionPolicy,
    InterfaceErrorCode, PrincipalId, SessionConnectionState, SessionFeatureUnavailableReason,
    SessionId, WorkerId,
};
use worker_pool::{
    BrowserWorker, EngineFeatureSupport, WorkerFactory, WorkerFeatureObservation, WorkerPool,
};

struct Worker {
    id: WorkerId,
    profile: PathBuf,
    connected: Arc<AtomicBool>,
}
#[async_trait]
impl BrowserWorker for Worker {
    fn worker_id(&self) -> WorkerId {
        self.id.clone()
    }
    fn profile_dir(&self) -> &Path {
        &self.profile
    }
    async fn close(&self) -> Result<(), CommandError> {
        self.connected.store(false, Ordering::SeqCst);
        Ok(())
    }
    async fn feature_observation(&self) -> WorkerFeatureObservation {
        WorkerFeatureObservation {
            support: Some(
                EngineFeatureSupport::IMPLEMENTATIONS
                    .iter()
                    .find(|s| s.engine_name() == "firefox")
                    .unwrap()
                    .clone(),
            ),
            connection: if self.connected.load(Ordering::SeqCst) {
                SessionConnectionState::Connected
            } else {
                SessionConnectionState::Disconnected
            },
        }
    }
}
struct Factory {
    launches: Arc<AtomicUsize>,
    worker: Arc<Worker>,
}
#[async_trait]
impl WorkerFactory for Factory {
    async fn launch(&self, _: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        Ok(self.worker.clone())
    }
    fn engine_feature_support(&self) -> Vec<EngineFeatureSupport> {
        EngineFeatureSupport::IMPLEMENTATIONS.to_vec()
    }
}

#[tokio::test]
async fn session_availability_uses_existing_worker_policy_live_grants_and_connection() {
    let root = tempfile::tempdir().unwrap();
    let connected = Arc::new(AtomicBool::new(true));
    let launches = Arc::new(AtomicUsize::new(0));
    let workers = Arc::new(WorkerPool::new(
        1,
        Arc::new(Factory {
            launches: launches.clone(),
            worker: Arc::new(Worker {
                id: WorkerId::new(),
                profile: root.path().into(),
                connected: connected.clone(),
            }),
        }),
    ));
    let journal = Arc::new(
        workflow_journal::JsonlJournal::open(root.path().join("commands.jsonl"))
            .await
            .unwrap(),
    );
    let service = sdk_core::RuntimeService::new(
        session_manager::SessionManager::new(workers.clone()),
        page_runtime::PageRuntime::new(journal, workers.clone()),
    );
    let authority = AuthorityStore::in_memory();
    let expires = Utc::now() + Duration::minutes(5);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [
                Capability::SessionRead,
                Capability::SessionWrite,
                Capability::BrowserMutate,
                Capability::JavascriptEvaluate,
            ],
            expires,
        )
        .await
        .unwrap()
        .expose_once();
    let handle = authority.verify(&token).await.unwrap();
    let (_, recorder) = SessionOwnershipRegistry::bounded(8);
    let api = AuthenticatedRuntime::with_session_ownership(
        service.clone(),
        handle.clone(),
        recorder.clone(),
    );
    let context = handle.context(expires, None);
    let session = api
        .create_session(
            context.clone(),
            CreateSessionRequest {
                profile: "capabilities".into(),
                proxy: None,
                execution_policy: ExecutionPolicy {
                    javascript_evaluation: true,
                    ..Default::default()
                },
                zigzagzig: false,
            },
        )
        .await
        .unwrap();
    let policy_off = api
        .create_session(
            context.clone(),
            CreateSessionRequest {
                profile: "policy-off".into(),
                proxy: None,
                execution_policy: ExecutionPolicy::default(),
                zigzagzig: false,
            },
        )
        .await
        .unwrap();
    let off = api
        .session_capabilities(context.clone(), policy_off.id)
        .await
        .unwrap();
    assert_eq!(
        off.features
            .iter()
            .find(|f| f.name == "evaluateJavaScript")
            .unwrap()
            .reason,
        Some(SessionFeatureUnavailableReason::PolicyDisabled)
    );
    // An inspection must not acquire another execution lease or start a browser.
    let held = workers.lease(session.id.clone()).await.unwrap();
    let report = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        api.session_capabilities(context.clone(), session.id.clone()),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(launches.load(Ordering::SeqCst), 2);
    assert_eq!(report.engine.as_deref(), Some("firefox"));
    assert_eq!(report.connection, SessionConnectionState::Connected);
    let viewport = report
        .features
        .iter()
        .find(|f| f.name == "captureScreenshot.viewport")
        .unwrap();
    assert!(viewport.available);
    let full_page = report
        .features
        .iter()
        .find(|f| f.name == "captureScreenshot.fullPage")
        .unwrap();
    assert_eq!(full_page.supported, Some(false));
    assert_eq!(
        full_page.reason,
        Some(SessionFeatureUnavailableReason::UnsupportedByEngine)
    );
    assert!(
        report
            .features
            .iter()
            .find(|f| f.name == "evaluateJavaScript")
            .unwrap()
            .available
    );
    let mut restricted = context.clone();
    restricted.capabilities =
        CapabilitySet::new([Capability::SessionRead, Capability::BrowserMutate]);
    let report = api
        .session_capabilities(restricted, session.id.clone())
        .await
        .unwrap();
    assert_eq!(
        report
            .features
            .iter()
            .find(|f| f.name == "evaluateJavaScript")
            .unwrap()
            .reason,
        Some(SessionFeatureUnavailableReason::MissingCapability)
    );
    connected.store(false, Ordering::SeqCst);
    let report = api
        .session_capabilities(context.clone(), session.id.clone())
        .await
        .unwrap();
    assert_eq!(report.connection, SessionConnectionState::Disconnected);
    assert!(!report.features.iter().any(|f| f.available));
    assert_eq!(
        report
            .features
            .iter()
            .find(|f| f.name == "captureScreenshot.viewport")
            .unwrap()
            .reason,
        Some(SessionFeatureUnavailableReason::Disconnected)
    );
    assert_eq!(launches.load(Ordering::SeqCst), 2);
    assert_eq!(
        api.session_capabilities(context.clone(), SessionId::new())
            .await
            .unwrap_err()
            .code,
        InterfaceErrorCode::NotFound
    );
    let other_token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [Capability::SessionRead],
            expires,
        )
        .await
        .unwrap()
        .expose_once();
    let other_handle = authority.verify(&other_token).await.unwrap();
    let other_api =
        AuthenticatedRuntime::with_session_ownership(service, other_handle.clone(), recorder);
    assert_eq!(
        other_api
            .session_capabilities(other_handle.context(expires, None), session.id.clone())
            .await
            .unwrap_err()
            .code,
        InterfaceErrorCode::NotFound
    );
    authority.revoke(&context.principal_id).await.unwrap();
    assert_eq!(
        api.session_capabilities(context, session.id)
            .await
            .unwrap_err()
            .code,
        InterfaceErrorCode::AuthenticationFailed
    );
    drop(held);
}
