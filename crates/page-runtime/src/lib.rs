mod executor;
mod recovery;

use std::collections::HashMap;
use std::sync::Arc;

use checkpoint_store::CheckpointStore;
use tokio::sync::RwLock;
use types::{OpenPageRequest, PageId, PageMode, PageState, RuntimeError, SessionId};
use worker_pool::WorkerPool;
use workflow_journal::CommandJournal;

pub use executor::ExecutorError;
pub use recovery::{evaluate_invariants, InvariantEvaluation, RecoveryCoordinator, RecoveryError};

#[derive(Clone, Default)]
pub struct PageRuntime {
    inner: Arc<RwLock<HashMap<PageId, PageState>>>,
    journal: Option<Arc<dyn CommandJournal>>,
    workers: Option<Arc<WorkerPool>>,
    checkpoints: Option<CheckpointStore>,
}

impl PageRuntime {
    pub fn new(journal: Arc<dyn CommandJournal>, workers: Arc<WorkerPool>) -> Self {
        Self {
            inner: Arc::default(),
            journal: Some(journal),
            workers: Some(workers),
            checkpoints: None,
        }
    }

    pub fn new_with_checkpoints(
        journal: Arc<dyn CommandJournal>,
        workers: Arc<WorkerPool>,
        checkpoints: CheckpointStore,
    ) -> Self {
        Self {
            inner: Arc::default(),
            journal: Some(journal),
            workers: Some(workers),
            checkpoints: Some(checkpoints),
        }
    }

    pub async fn open(&self, req: OpenPageRequest) -> PageState {
        self.register_page(req.session_id).await
    }

    pub async fn open_browser(&self, session_id: SessionId) -> Result<PageState, RuntimeError> {
        let workers = self
            .workers
            .as_ref()
            .ok_or_else(|| RuntimeError::Internal("browser workers are not configured".into()))?;
        let lease = workers
            .lease(session_id.clone())
            .await
            .map_err(|error| RuntimeError::Internal(error.message))?;
        let page = self.register_page(session_id).await;
        if let Err(error) = lease.worker().open_page(page.id.clone()).await {
            self.inner.write().await.remove(&page.id);
            return Err(RuntimeError::Internal(error.message));
        }
        Ok(page)
    }

    async fn register_page(&self, session_id: SessionId) -> PageState {
        let page = PageState {
            id: PageId::default(),
            session_id,
            url: None,
            mode: PageMode::Document,
            ready_state: "created".to_string(),
            pending_requests: 0,
        };
        self.inner
            .write()
            .await
            .insert(page.id.clone(), page.clone());
        page
    }

    pub async fn get(&self, id: &PageId) -> Result<PageState, RuntimeError> {
        self.inner
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| RuntimeError::NotFound("page".to_string()))
    }

    pub async fn set_url(
        &self,
        id: &PageId,
        url: String,
        ready_state: &str,
    ) -> Result<PageState, RuntimeError> {
        let mut guard = self.inner.write().await;
        let page = guard
            .get_mut(id)
            .ok_or_else(|| RuntimeError::NotFound("page".to_string()))?;
        page.url = Some(url);
        page.ready_state = ready_state.to_string();
        Ok(page.clone())
    }
}
