use std::sync::Arc;

use chrono::{Duration, Utc};
use config::AppConfig;
use page_runtime::PageRuntime;
use page_runtime::{RecoveryCoordinator, RecoveryError};
use session_manager::SessionManager;
use types::{
    AttemptId, CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest, Evidence,
    NavigateCommand, NavigationRequest, NavigationResult, OpenPageRequest, PageState,
    PrimitiveCommand, RecoveryDecision, RuntimeError, RuntimeInfo, SessionState, WaitUntil,
    WorkflowCheckpoint, WorkflowId,
};
use worker_pool::{ChromiumWorkerFactory, WorkerPool};
use workflow_journal::JsonlJournal;

#[derive(Clone, Default)]
pub struct RuntimeService {
    pub sessions: SessionManager,
    pub pages: PageRuntime,
    recovery: Option<RecoveryCoordinator>,
}

impl RuntimeService {
    pub fn new(sessions: SessionManager, pages: PageRuntime) -> Self {
        Self {
            sessions,
            pages,
            recovery: None,
        }
    }

    pub fn with_recovery(
        sessions: SessionManager,
        pages: PageRuntime,
        recovery: RecoveryCoordinator,
    ) -> Self {
        Self {
            sessions,
            pages,
            recovery: Some(recovery),
        }
    }

    pub async fn build(config: &AppConfig) -> Result<Self, RuntimeError> {
        let journal = Arc::new(
            JsonlJournal::open(&config.storage.journal_path)
                .await
                .map_err(|error| RuntimeError::Internal(error.to_string()))?,
        );
        let factory = Arc::new(ChromiumWorkerFactory::new(config.browser.clone()));
        let workers = Arc::new(WorkerPool::new(config.browser.max_active, factory));
        let checkpoints = checkpoint_store::CheckpointStore::open(&config.storage.checkpoints_dir)
            .await
            .map_err(|error| RuntimeError::Internal(error.to_string()))?;
        let recovery = RecoveryCoordinator::with_workers(checkpoints, workers.clone());
        let pages = PageRuntime::new(journal, workers.clone());
        let sessions = SessionManager::new(workers);
        Ok(Self::with_recovery(sessions, pages, recovery))
    }

    pub async fn runtime_info(&self) -> RuntimeInfo {
        let active_sessions = self.sessions.list().await.len();
        let mut capabilities = vec![
            "sdk".to_string(),
            "browser-primitives".to_string(),
            "durable-journal".to_string(),
        ];
        if self.recovery.is_some() {
            capabilities.push("checkpoint-recovery".to_string());
        }
        RuntimeInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            capabilities,
            active_sessions,
            queued_jobs: 0,
            uptime_ms: 0,
        }
    }

    pub async fn create_session(
        &self,
        req: CreateSessionRequest,
    ) -> Result<SessionState, RuntimeError> {
        self.sessions.create(req).await
    }

    pub async fn list_sessions(&self) -> Vec<SessionState> {
        self.sessions.list().await
    }

    pub async fn open_page(&self, req: OpenPageRequest) -> Result<PageState, RuntimeError> {
        self.sessions.get(&req.session_id).await?;
        let page = self.pages.open_browser(req.session_id).await?;
        self.sessions
            .add_page(&page.session_id, page.id.clone())
            .await?;
        Ok(page)
    }

    pub async fn submit(&self, envelope: CommandEnvelope) -> CommandOutcome {
        self.pages.execute(envelope).await
    }

    pub async fn checkpoint(
        &self,
        checkpoint: WorkflowCheckpoint,
        evidence: Vec<Evidence>,
    ) -> Result<WorkflowCheckpoint, RecoveryError> {
        self.recovery
            .as_ref()
            .ok_or(RecoveryError::WorkersUnavailable)?
            .save_verified(checkpoint, evidence)
            .await
    }

    pub async fn recover(
        &self,
        workflow_id: &WorkflowId,
    ) -> Result<RecoveryDecision, RecoveryError> {
        self.recovery
            .as_ref()
            .ok_or(RecoveryError::WorkersUnavailable)?
            .recover(workflow_id)
            .await
    }

    pub async fn navigate(&self, req: NavigationRequest) -> Result<NavigationResult, RuntimeError> {
        let page = self.pages.get(&req.page_id).await?;
        let wait_until = match req.wait_until.as_deref() {
            Some("commit") => WaitUntil::Commit,
            Some("domcontentloaded") => WaitUntil::DomContentLoaded,
            Some("networkidle") => WaitUntil::NetworkIdle,
            _ => WaitUntil::Interactive,
        };
        let timeout_ms = req.timeout_ms.unwrap_or(30_000);
        let envelope = CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: page.session_id,
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::milliseconds(timeout_ms as i64),
            command: PrimitiveCommand::Navigate(NavigateCommand {
                url: req.url,
                wait_until,
                timeout_ms,
            }),
        };
        match self.submit(envelope).await {
            CommandOutcome::Completed { evidence, .. } => {
                let url = evidence
                    .iter()
                    .find_map(|item| match item {
                        types::Evidence::Navigation { url, .. } => Some(url.clone()),
                        _ => None,
                    })
                    .ok_or_else(|| RuntimeError::Internal("navigation evidence missing".into()))?;
                Ok(NavigationResult {
                    page_id: page.id,
                    url,
                    ready_state: "interactive".into(),
                })
            }
            outcome => Err(RuntimeError::Internal(format!(
                "navigation command failed: {outcome:?}"
            ))),
        }
    }
}
