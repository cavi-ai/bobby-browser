mod adaptive;
mod executor;
mod recovery;
mod skill_recovery;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use checkpoint_store::CheckpointStore;
use tokio::sync::RwLock;
use types::{CommandPhase, OpenPageRequest, PageId, PageMode, PageState, RuntimeError, SessionId};
use worker_pool::WorkerPool;
use workflow_journal::CommandJournal;

pub use adaptive::{AdaptiveExecution, AdaptivePageEngine, VisionGate};
pub use executor::ExecutorError;
pub use intent_engine::VisionAssist;
pub use recovery::{
    evaluate_invariants, InvariantEvaluation, RecoveryCoordinator, RecoveryError,
    VerifiedRecoveryCheckpoint,
};
#[cfg(feature = "test-support")]
pub use skill_recovery::RecoveryPreflightObserver;
pub use skill_recovery::{
    SkillRecoveryCoordinator, SkillRecoveryExecution, SkillTacticEffect, SkillTacticEvidence,
};

#[doc(hidden)]
#[async_trait]
pub trait ExecutionPhaseObserver: Send + Sync {
    async fn durable_phase_reached(&self, phase: CommandPhase);
}

#[derive(Clone, Default)]
pub struct PageRuntime {
    inner: Arc<RwLock<HashMap<PageId, PageState>>>,
    journal: Option<Arc<dyn CommandJournal>>,
    workers: Option<Arc<WorkerPool>>,
    checkpoints: Option<CheckpointStore>,
    adaptive: AdaptivePageEngine,
    phase_observer: Option<Arc<dyn ExecutionPhaseObserver>>,
}

impl PageRuntime {
    pub fn new(journal: Arc<dyn CommandJournal>, workers: Arc<WorkerPool>) -> Self {
        Self {
            inner: Arc::default(),
            journal: Some(journal),
            workers: Some(workers),
            checkpoints: None,
            adaptive: AdaptivePageEngine::browser_only(),
            phase_observer: None,
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
            adaptive: AdaptivePageEngine::browser_only(),
            phase_observer: None,
        }
    }

    pub fn new_adaptive(
        journal: Arc<dyn CommandJournal>,
        workers: Arc<WorkerPool>,
        checkpoints: Option<CheckpointStore>,
        adaptive: AdaptivePageEngine,
    ) -> Self {
        Self {
            inner: Arc::default(),
            journal: Some(journal),
            workers: Some(workers),
            checkpoints,
            adaptive,
            phase_observer: None,
        }
    }

    #[doc(hidden)]
    pub fn with_execution_phase_observer(
        mut self,
        observer: Arc<dyn ExecutionPhaseObserver>,
    ) -> Self {
        self.phase_observer = Some(observer);
        self
    }

    pub(crate) async fn observe_durable_phase(&self, phase: CommandPhase) {
        if let Some(observer) = &self.phase_observer {
            observer.durable_phase_reached(phase).await;
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

    pub async fn form_snapshot(
        &self,
        session_id: &SessionId,
        page_id: &PageId,
    ) -> Result<types::FormSnapshot, RuntimeError> {
        let page = self
            .inner
            .read()
            .await
            .get(page_id)
            .cloned()
            .ok_or_else(|| RuntimeError::NotFound("page".into()))?;
        if &page.session_id != session_id {
            return Err(RuntimeError::NotFound("page".into()));
        }
        let workers = self
            .workers
            .as_ref()
            .ok_or_else(|| RuntimeError::Internal("browser workers are not configured".into()))?;
        let lease = workers
            .lease(session_id.clone())
            .await
            .map_err(|error| RuntimeError::Internal(error.message))?;
        let evidence = lease
            .worker()
            .form_snapshot(page_id)
            .await
            .map_err(|error| RuntimeError::Internal(error.message))?;
        evidence
            .into_iter()
            .find_map(|item| match item {
                types::Evidence::FormSnapshot { snapshot } => Some(snapshot),
                _ => None,
            })
            .ok_or_else(|| RuntimeError::Internal("worker omitted form snapshot evidence".into()))
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

    pub(crate) async fn register_page_id(
        &self,
        session_id: SessionId,
        page_id: PageId,
        url: String,
    ) {
        self.inner.write().await.insert(
            page_id.clone(),
            PageState {
                id: page_id,
                session_id,
                url: Some(url),
                mode: PageMode::Document,
                ready_state: "interactive".into(),
                pending_requests: 0,
            },
        );
    }

    pub(crate) async fn remove_page(&self, page_id: &PageId) {
        self.inner.write().await.remove(page_id);
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
