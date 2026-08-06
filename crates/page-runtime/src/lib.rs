mod adaptive;
mod executor;
mod promotion;
mod recovery;
mod skill_recovery;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use checkpoint_store::CheckpointStore;
use tokio::sync::RwLock;
use types::{
    CommandId, CommandOutcome, CommandPhase, Evidence, OpenPageRequest, PageId, PageMode,
    PageState, RuntimeError, SessionId,
};
use worker_pool::WorkerPool;
use workflow_journal::CommandJournal;

mod context;

pub use adaptive::{AdaptiveExecution, AdaptivePageEngine, NodeSelection, SessionGate, VisionGate};
pub use context::{ContextGraph, CONTEXT_CONFIDENCE_FLOOR};
pub use executor::ExecutorError;
pub use intent_engine::VisionAssist;
pub use promotion::ContextPromotion;
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
    /// Page structure retained for context-node answers. Never optional: the graph is inert
    /// until something records into it, and an `Option` would add a branch to the one path
    /// where forgetting to invalidate is the bug.
    context: Arc<ContextGraph>,
    /// Durable promotion sink. `None` unless the runtime's engine selection
    /// carries a durable profile identity (Firefox companion); Chromium
    /// sessions never promote.
    promotion: Option<Arc<ContextPromotion>>,
}

impl PageRuntime {
    /// The context graph this runtime records page structure into.
    pub fn context(&self) -> &Arc<ContextGraph> {
        &self.context
    }

    pub fn new(journal: Arc<dyn CommandJournal>, workers: Arc<WorkerPool>) -> Self {
        Self {
            inner: Arc::default(),
            journal: Some(journal),
            workers: Some(workers),
            checkpoints: None,
            adaptive: AdaptivePageEngine::browser_only(),
            phase_observer: None,
            context: Arc::new(ContextGraph::new()),
            promotion: None,
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
            context: Arc::new(ContextGraph::new()),
            promotion: None,
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
            context: Arc::new(ContextGraph::new()),
            promotion: None,
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

    /// Attaches the durable context promotion sink (Firefox-companion
    /// runtimes only; see [`ContextPromotion`]).
    pub fn with_context_promotion(mut self, promotion: Arc<ContextPromotion>) -> Self {
        self.promotion = Some(promotion);
        self
    }

    /// The durable promotion sink, if this runtime has one.
    pub fn context_promotion(&self) -> Option<&Arc<ContextPromotion>> {
        self.promotion.as_ref()
    }

    /// Enables lazy batch vision prefill against this runtime's context
    /// graph (`[vision].prefill`). Off by default; when off, the intent path
    /// is byte-identical to before.
    pub fn with_vision_prefill_enabled(mut self) -> Self {
        self.adaptive = self
            .adaptive
            .clone()
            .with_vision_prefill(self.context.clone());
        self
    }

    /// Attaches this runtime's context graph to the adaptive engine so
    /// escalation prompts carry the recent-commands block.
    pub fn with_context_graph_attached(mut self) -> Self {
        self.adaptive = self
            .adaptive
            .clone()
            .with_context_graph(self.context.clone());
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

    /// Evidence the runtime itself recorded for a command.
    ///
    /// The journal is the only authority: a command that never ran, or never reached a
    /// terminal outcome, returns an error rather than an empty vector.
    pub async fn evidence_for_command(
        &self,
        command_id: CommandId,
    ) -> Result<Vec<Evidence>, RecoveryError> {
        let journal = self
            .journal
            .as_ref()
            .ok_or(RecoveryError::WorkersUnavailable)?;
        let scan = journal
            .history(command_id.clone())
            .await
            .map_err(|_| RecoveryError::CommandOutcomeMissing(command_id.clone()))?;
        scan.records
            .into_iter()
            .rev()
            .find_map(|record| match record.outcome {
                Some(CommandOutcome::Completed { evidence, .. })
                | Some(CommandOutcome::NeedsReconciliation { evidence, .. }) => Some(evidence),
                _ => None,
            })
            .ok_or(RecoveryError::CommandOutcomeMissing(command_id))
    }

    /// The session a command was submitted under, per the runtime's own journal.
    ///
    /// Every command's first journal record (`CommandPhase::Accepted`) stores its
    /// `CommandEnvelope`, so this is available for any command with a journal record.
    ///
    /// Authorization keys on this, never on a caller-supplied session: the journal has no
    /// notion of principal, so a command referenced only by id must be checked against the
    /// session it actually ran under before its evidence is resolved or trusted.
    pub async fn command_session(
        &self,
        command_id: &CommandId,
    ) -> Result<SessionId, RecoveryError> {
        let journal = self
            .journal
            .as_ref()
            .ok_or(RecoveryError::WorkersUnavailable)?;
        let scan = journal
            .history(command_id.clone())
            .await
            .map_err(|_| RecoveryError::CommandOutcomeMissing(command_id.clone()))?;
        scan.records
            .iter()
            .find_map(|record| {
                record
                    .envelope
                    .as_ref()
                    .map(|envelope| envelope.session_id.clone())
            })
            .ok_or_else(|| RecoveryError::CommandOutcomeMissing(command_id.clone()))
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
        max_controls: Option<u32>,
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
            .form_snapshot(page_id, max_controls)
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
