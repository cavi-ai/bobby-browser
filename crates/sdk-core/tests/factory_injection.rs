use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use sdk_core::RuntimeService;
use types::{
    CommandError, CreateSessionRequest, Evidence, InspectCommand, NavigateCommand, PageId,
    SessionId, WorkerId,
};
use worker_pool::{BrowserWorker, WorkerFactory};

struct CountingFactory(Arc<AtomicUsize>);
struct Worker(WorkerId);

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
