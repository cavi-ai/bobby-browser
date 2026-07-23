use std::sync::Arc;

use chrono::{Duration, Utc};
use config::AppConfig;
use page_runtime::{ExecutionPhaseObserver, PageRuntime, VisionAssist, VisionGate};
use page_runtime::{RecoveryCoordinator, RecoveryError};
use session_manager::SessionManager;
use types::{
    AttemptId, CommandEnvelope, CommandError, CommandId, CommandOutcome, CreateSessionRequest,
    ErrorCode, ErrorLayer, Evidence, NavigateCommand, NavigationRequest, NavigationResult,
    OpenPageRequest, PageState, PrimitiveCommand, RecoveryDecision, RuntimeCommand, RuntimeError,
    RuntimeInfo, SessionState, WaitUntil, WorkflowCheckpoint, WorkflowId,
};
use worker_pool::{ChromiumWorkerFactory, WorkerFactory, WorkerPool};
use workflow_journal::JsonlJournal;

mod interface;

pub use interface::AuthenticatedRuntime;

/// The unauthenticated application service is not a public interface adapter.
///
/// ```compile_fail
/// use interface_core::RuntimeInterface;
/// use sdk_core::RuntimeService;
///
/// fn requires_runtime_interface<T: RuntimeInterface>() {}
/// requires_runtime_interface::<RuntimeService>();
/// ```
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
        let factory = Arc::new(ChromiumWorkerFactory::new(config.browser.clone()));
        Self::build_inner(config, factory, None, None).await
    }

    /// Build with an injected [`VisionAssist`] provider (test/harness use).
    pub async fn build_with_vision_assist(
        config: &AppConfig,
        assist: Arc<dyn VisionAssist>,
    ) -> Result<Self, RuntimeError> {
        let factory = Arc::new(ChromiumWorkerFactory::new(config.browser.clone()));
        Self::build_inner(config, factory, None, Some(assist)).await
    }

    pub async fn build_with_worker_factory(
        config: &AppConfig,
        factory: Arc<dyn WorkerFactory>,
    ) -> Result<Self, RuntimeError> {
        Self::build_inner(config, factory, None, None).await
    }

    #[doc(hidden)]
    pub async fn build_with_execution_phase_observer(
        config: &AppConfig,
        observer: Arc<dyn ExecutionPhaseObserver>,
    ) -> Result<Self, RuntimeError> {
        let factory = Arc::new(ChromiumWorkerFactory::new(config.browser.clone()));
        Self::build_inner(config, factory, Some(observer), None).await
    }

    async fn build_inner(
        config: &AppConfig,
        factory: Arc<dyn WorkerFactory>,
        observer: Option<Arc<dyn ExecutionPhaseObserver>>,
        vision_assist: Option<Arc<dyn VisionAssist>>,
    ) -> Result<Self, RuntimeError> {
        let journal = Arc::new(
            JsonlJournal::open(&config.storage.journal_path)
                .await
                .map_err(|error| RuntimeError::Internal(error.to_string()))?,
        );
        let workers = Arc::new(WorkerPool::new(config.browser.max_active, factory));
        let checkpoints = checkpoint_store::CheckpointStore::open(&config.storage.checkpoints_dir)
            .await
            .map_err(|error| RuntimeError::Internal(error.to_string()))?;
        let recovery = RecoveryCoordinator::with_workers(checkpoints.clone(), workers.clone());
        let network = network_engine::NetworkPolicy {
            allow_loopback: config.http.allow_loopback,
            allow_private_network: config.http.allow_private_network,
            max_redirects: config.http.max_redirects,
            max_header_bytes: config.http.max_header_bytes,
            max_body_bytes: config.http.max_body_bytes,
            max_download_bytes: config.http.max_download_bytes,
            request_timeout_ms: config.http.request_timeout_ms,
            max_concurrent_requests: config.http.max_concurrent_requests,
        };
        let mut adaptive = page_runtime::AdaptivePageEngine::new(
            network_engine::EligibilityPolicy::new(network.clone()),
            network_engine::DirectHttpExecutor::new(network.clone()),
            artifact_store::ArtifactStore::new(
                &config.browser.artifacts_dir,
                config
                    .browser
                    .max_artifact_bytes
                    .max(network.max_download_bytes),
                config.browser.max_screenshot_dimension,
            ),
            network,
        );
        if let Some(assist) = vision_assist {
            adaptive = adaptive.with_vision_assist(assist);
        }
        let mut pages =
            PageRuntime::new_adaptive(journal, workers.clone(), Some(checkpoints), adaptive);
        if let Some(observer) = observer {
            pages = pages.with_execution_phase_observer(observer);
        }
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
        self.submit_with_vision_capability(envelope, false).await
    }

    /// Submit with an explicit vision capability flag from the authenticated principal.
    /// Session `executionPolicy.visionAssist` is looked up here and threaded into
    /// IntentEngine as `VisionContext.session_ok`. Vision is deny-by-default: both
    /// this capability flag and the session grant must be true before the provider runs.
    pub async fn submit_with_vision_capability(
        &self,
        envelope: CommandEnvelope,
        vision_capability_ok: bool,
    ) -> CommandOutcome {
        // SECURITY(F4): per-session execution-policy gate. This is the authoritative
        // deny-by-default check for `EvaluateJavaScript` — a session must have explicitly
        // opted in (`ExecutionPolicy.javascript_evaluation == true`) or the command is
        // refused here, before it ever reaches `self.pages.execute` / a worker. This is
        // independent of (and runs after) the token capability gate enforced in
        // `AuthenticatedRuntime::submit`; both must pass. Fails closed: an unknown or
        // absent session is treated as `javascript_evaluation == false`, not as "skip the
        // gate" — `self.pages.execute` validates against its own independent page
        // registry, not `SessionManager`, so it does not perform this check for us.
        if matches!(
            &envelope.command,
            RuntimeCommand::Primitive(PrimitiveCommand::EvaluateJavaScript(_))
        ) {
            let allowed = self
                .sessions
                .get(&envelope.session_id)
                .await
                .map(|session| session.execution_policy.javascript_evaluation)
                .unwrap_or(false);
            if !allowed {
                return CommandOutcome::PolicyDenied {
                    command_id: envelope.command_id.clone(),
                    error: CommandError {
                        code: ErrorCode::PolicyDenied,
                        message: "javascript evaluation is not permitted for this session".into(),
                        layer: ErrorLayer::Workflow,
                        retryable: false,
                    },
                };
            }
        }

        let vision_gate = match &envelope.command {
            RuntimeCommand::Intent(_) => {
                let session_ok = self
                    .sessions
                    .get(&envelope.session_id)
                    .await
                    .map(|session| session.execution_policy.vision_assist)
                    .unwrap_or(false);
                VisionGate {
                    session_ok,
                    capability_ok: vision_capability_ok,
                }
            }
            RuntimeCommand::Primitive(_) => VisionGate::default(),
        };
        self.pages
            .execute_with_vision_gate(envelope, vision_gate)
            .await
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
            command: RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
                url: req.url,
                wait_until,
                timeout_ms,
            })),
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
