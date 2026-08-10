use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use context_store::{
    ContextStore, ControlContext, FormContext, IntentStats, PageContext, SiteContext,
};
use intent_engine::{VisionAssist, VisionProposal, VisionProposeRequest};
use sdk_core::RuntimeService;
use types::{
    CommandError, CreateSessionRequest, Evidence, InspectCommand, NavigateCommand, PageId,
    SessionId, WorkerId,
};
use worker_pool::{BrowserWorker, WorkerFactory};

struct CountingFactory(Arc<AtomicUsize>);
struct Worker(WorkerId);
struct InjectedVision;

#[async_trait]
impl VisionAssist for InjectedVision {
    async fn propose(
        &self,
        _request: VisionProposeRequest,
    ) -> Result<VisionProposal, CommandError> {
        unreachable!("session creation must not invoke the provider")
    }
}

#[async_trait]
impl WorkerFactory for CountingFactory {
    async fn launch(
        &self,
        _session_id: &SessionId,
    ) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(Worker(WorkerId::new())))
    }
}

#[async_trait]
impl BrowserWorker for Worker {
    fn worker_id(&self) -> WorkerId {
        self.0.clone()
    }
    fn profile_dir(&self) -> &Path {
        Path::new("injected")
    }
    async fn open_page(&self, _page_id: PageId) -> Result<(), CommandError> {
        Ok(())
    }
    async fn navigate(
        &self,
        _page_id: &PageId,
        _command: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![])
    }
    async fn inspect(
        &self,
        _page_id: &PageId,
        _command: &InspectCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![])
    }
    async fn click(
        &self,
        _page_id: &PageId,
        _command: &types::ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![])
    }
    async fn type_text(
        &self,
        _page_id: &PageId,
        _command: &types::TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![])
    }
    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }
}

#[tokio::test]
async fn build_with_worker_factory_consumes_the_injected_factory() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config::AppConfig::default();
    config.storage.journal_path = root.path().join("commands.jsonl");
    config.storage.checkpoints_dir = root.path().join("checkpoints");
    config.browser.artifacts_dir = root.path().join("artifacts");
    let launches = Arc::new(AtomicUsize::new(0));

    let runtime = RuntimeService::build_with_worker_factory(
        &config,
        Arc::new(CountingFactory(launches.clone())),
    )
    .await
    .unwrap();
    runtime
        .sessions
        .create(CreateSessionRequest {
            profile: "injected".into(),
            proxy: None,
            execution_policy: Default::default(),
        })
        .await
        .unwrap();

    assert_eq!(launches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn injected_vision_assist_counts_as_a_configured_provider() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config::AppConfig::default();
    config.storage.journal_path = root.path().join("commands.jsonl");
    config.storage.checkpoints_dir = root.path().join("checkpoints");
    config.browser.artifacts_dir = root.path().join("artifacts");

    let runtime = RuntimeService::build_with_worker_factory_and_vision_assist(
        &config,
        Arc::new(CountingFactory(Arc::new(AtomicUsize::new(0)))),
        Arc::new(InjectedVision),
    )
    .await
    .unwrap();

    runtime
        .create_session(CreateSessionRequest {
            profile: "injected-vision".into(),
            proxy: None,
            execution_policy: types::ExecutionPolicy {
                vision_assist: true,
                ..Default::default()
            },
        })
        .await
        .expect("an injected vision provider satisfies the explicit opt-in");
}

#[tokio::test]
async fn context_ttl_is_applied_during_runtime_build() {
    let root = tempfile::tempdir().unwrap();
    let context_root = root.path().join("context");
    let (store, _) = ContextStore::open(&context_root, "profile-a")
        .await
        .unwrap();
    let mut intents = std::collections::BTreeMap::new();
    intents.insert(
        "fill".to_string(),
        IntentStats {
            success_count: 1,
            last_verified_day: Some(1),
            ..IntentStats::default()
        },
    );
    let site = SiteContext {
        pages: std::collections::BTreeMap::from([(
            "/login".to_string(),
            PageContext {
                forms: std::collections::BTreeMap::from([(
                    "page".to_string(),
                    FormContext {
                        controls: vec![ControlContext {
                            role: "textbox".to_string(),
                            accessible_name: "Email".to_string(),
                            ordinal: None,
                            form_membership: "page".to_string(),
                            intents,
                        }],
                    },
                )]),
            },
        )]),
    };
    store.upsert_site("https://example.com", site).await;
    assert!(store.flush().await.is_empty());
    drop(store);

    let mut config = config::AppConfig::default();
    config.storage.journal_path = root.path().join("commands.jsonl");
    config.storage.checkpoints_dir = root.path().join("checkpoints");
    config.browser.artifacts_dir = root.path().join("artifacts");
    config.context.dir = Some(context_root);
    config.context.ttl_days = 90;
    let runtime = RuntimeService::build_with_context_promotion(
        &config,
        Arc::new(CountingFactory(Arc::new(AtomicUsize::new(0)))),
        "profile-a",
    )
    .await
    .unwrap();

    let promotion = runtime.pages.context_promotion().unwrap();
    assert!(promotion
        .store()
        .site("https://example.com")
        .await
        .is_none());
}
