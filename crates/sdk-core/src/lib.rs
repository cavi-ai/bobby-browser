use std::sync::Arc;

use chrono::{Duration, Utc};
use config::AppConfig;
use page_runtime::PageRuntime;
use session_manager::SessionManager;
use types::{
    AttemptId, CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest, NavigateCommand,
    NavigationRequest, NavigationResult, OpenPageRequest, PageState, PrimitiveCommand,
    RuntimeError, RuntimeInfo, SessionState, WaitUntil, WorkflowId,
};
use worker_pool::{ChromiumWorkerFactory, WorkerPool};
use workflow_journal::JsonlJournal;

#[derive(Clone, Default)]
pub struct RuntimeService {
    pub sessions: SessionManager,
    pub pages: PageRuntime,
}

impl RuntimeService {
    pub fn new(sessions: SessionManager, pages: PageRuntime) -> Self {
        Self { sessions, pages }
    }

    pub async fn build(config: &AppConfig) -> Result<Self, RuntimeError> {
        let journal = Arc::new(
            JsonlJournal::open(&config.storage.journal_path)
                .await
                .map_err(|error| RuntimeError::Internal(error.to_string()))?,
        );
        let factory = Arc::new(ChromiumWorkerFactory::new(config.browser.clone()));
        let workers = Arc::new(WorkerPool::new(config.browser.max_active, factory));
        let pages = PageRuntime::new(journal, workers.clone());
        let sessions = SessionManager::new(workers);
        Ok(Self { sessions, pages })
    }

    pub async fn runtime_info(&self) -> RuntimeInfo {
        let active_sessions = self.sessions.list().await.len();
        RuntimeInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            capabilities: vec![
                "sdk".to_string(),
                "browser-primitives".to_string(),
                "durable-journal".to_string(),
            ],
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
