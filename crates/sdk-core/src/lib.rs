//! Runtime service and authenticated interface adapter.
//!
//! [`RuntimeService`] is the unauthenticated application core: sessions, pages,
//! recovery, and command execution. [`AuthenticatedRuntime`] wraps it with
//! capability checks, idempotency, and session ownership — the type every
//! public adapter (HTTP, MCP, CDP, ACP) holds.

use std::sync::Arc;

use chrono::{Duration, Utc};
use config::AppConfig;
use node_registry::NodeRegistry;
use page_runtime::{
    ExecutionPhaseObserver, NodeSelection, PageRuntime, SessionGate, VisionAssist, VisionGate,
};
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
#[derive(Clone)]
pub struct RuntimeService {
    pub sessions: SessionManager,
    pub pages: PageRuntime,
    recovery: Option<RecoveryCoordinator>,
    started_at: std::time::Instant,
    in_flight: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    /// Nodes this runtime can reach. Empty by default, so a `RuntimeService`
    /// built without configuration resolves no node for any session.
    nodes: Arc<NodeRegistry>,
    /// Feature flags reported on `runtime_info`'s capability list so a caller
    /// can tell "no vision provider configured" apart from a transient vision
    /// failure without shell access (`visionAssistDenied` vs
    /// `visionAssistFailed` repairs diverge on exactly this).
    vision_assist_configured: bool,
    vision_provider_configured: bool,
}

struct InFlightGuard {
    counter: Arc<std::sync::atomic::AtomicUsize>,
}

impl InFlightGuard {
    fn acquire(counter: Arc<std::sync::atomic::AtomicUsize>) -> Self {
        counter.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Self { counter }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.counter
            .fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

impl Default for RuntimeService {
    fn default() -> Self {
        Self::new(SessionManager::default(), PageRuntime::default())
    }
}

impl RuntimeService {
    pub fn new(sessions: SessionManager, pages: PageRuntime) -> Self {
        Self {
            sessions,
            pages,
            recovery: None,
            nodes: Arc::new(NodeRegistry::default()),
            vision_assist_configured: false,
            vision_provider_configured: false,
            started_at: std::time::Instant::now(),
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
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
            nodes: Arc::new(NodeRegistry::default()),
            vision_assist_configured: false,
            vision_provider_configured: false,
            started_at: std::time::Instant::now(),
            in_flight: std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
        }
    }

    fn with_vision_state(mut self, assist: bool, provider: bool) -> Self {
        self.vision_assist_configured = assist;
        self.vision_provider_configured = provider;
        self
    }

    /// Installs the node registry a session's `visionNode` resolves against.
    pub fn with_nodes(mut self, nodes: Arc<NodeRegistry>) -> Self {
        self.vision_provider_configured = !nodes.is_empty();
        self.nodes = nodes;
        self
    }

    /// Names of the nodes this runtime can reach.
    pub fn node_names(&self) -> Vec<String> {
        self.nodes.names().map(str::to_owned).collect()
    }

    pub async fn build(config: &AppConfig) -> Result<Self, RuntimeError> {
        let factory = Arc::new(ChromiumWorkerFactory::new(config.browser.clone()));
        Self::build_inner(config, factory, None, None, None).await
    }

    /// Build with an injected [`VisionAssist`] provider (test/harness use).
    pub async fn build_with_vision_assist(
        config: &AppConfig,
        assist: Arc<dyn VisionAssist>,
    ) -> Result<Self, RuntimeError> {
        let factory = Arc::new(ChromiumWorkerFactory::new(config.browser.clone()));
        Self::build_inner(config, factory, None, Some(assist), None).await
    }

    pub async fn build_with_worker_factory(
        config: &AppConfig,
        factory: Arc<dyn WorkerFactory>,
    ) -> Result<Self, RuntimeError> {
        Self::build_inner(config, factory, None, None, None).await
    }

    /// Build with an explicit worker factory and an injected [`VisionAssist`]
    /// provider (Firefox-companion runtimes under test).
    pub async fn build_with_worker_factory_and_vision_assist(
        config: &AppConfig,
        factory: Arc<dyn WorkerFactory>,
        assist: Arc<dyn VisionAssist>,
    ) -> Result<Self, RuntimeError> {
        Self::build_inner(config, factory, None, Some(assist), None).await
    }

    /// Build with a durable profile identity (Firefox-companion runtimes):
    /// attaches context promotion so verified intent outcomes persist
    /// structural control memory under `<context.dir>/<profile-id>/`.
    /// Promotion is absent when `config.context.dir` is unset or the store
    /// cannot be opened — never a startup failure.
    pub async fn build_with_context_promotion(
        config: &AppConfig,
        factory: Arc<dyn WorkerFactory>,
        profile_id: &str,
    ) -> Result<Self, RuntimeError> {
        let promotion = match &config.context.dir {
            Some(dir) => match context_store::ContextStore::open_with_ttl(
                dir,
                profile_id,
                config.context.ttl_days,
                context_store::day_since_epoch(Utc::now()),
            )
            .await
            {
                Ok((store, report)) => {
                    if !report.skipped.is_empty() {
                        tracing::warn!(
                            skipped = report.skipped.len(),
                            "context.store_opened_with_skipped_sites"
                        );
                    }
                    // TTL sweep on open: expired records never serve an
                    // answer, and their bytes leave with the next flush.
                    let today = context_store::day_since_epoch(chrono::Utc::now());
                    match store.sweep(config.context.ttl_days, today).await {
                        Ok(dropped) if dropped > 0 => {
                            tracing::info!(dropped, "context.swept_expired_records");
                        }
                        Ok(_) => {}
                        Err(error) => {
                            tracing::warn!(%error, "context.sweep_failed");
                        }
                    }
                    Some(Arc::new(page_runtime::ContextPromotion::new(store)))
                }
                Err(error) => {
                    tracing::warn!(%error, "context.store_unavailable");
                    None
                }
            },
            None => None,
        };
        Self::build_inner(config, factory, None, None, None)
            .await
            .map(|runtime| runtime.with_promotion(promotion))
    }

    fn with_promotion(mut self, promotion: Option<Arc<page_runtime::ContextPromotion>>) -> Self {
        if let Some(promotion) = promotion {
            self.pages = self.pages.with_context_promotion(promotion);
        }
        self
    }

    #[doc(hidden)]
    pub async fn build_with_execution_phase_observer(
        config: &AppConfig,
        observer: Arc<dyn ExecutionPhaseObserver>,
    ) -> Result<Self, RuntimeError> {
        let factory = Arc::new(ChromiumWorkerFactory::new(config.browser.clone()));
        Self::build_inner(config, factory, Some(observer), None, None).await
    }

    async fn build_inner(
        config: &AppConfig,
        factory: Arc<dyn WorkerFactory>,
        observer: Option<Arc<dyn ExecutionPhaseObserver>>,
        vision_assist: Option<Arc<dyn VisionAssist>>,
        structured_extractor: Option<Arc<dyn intent_engine::StructuredExtractor>>,
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
        )
        .with_downloads_root(&config.browser.downloads_dir);
        let nodes = Arc::new(NodeRegistry::from_config(config));
        let provider: Option<Arc<dyn intent_engine::StructuredExtractor>> =
            structured_extractor.or_else(|| nodes.http_structured_extractor());
        let vision_assist_present = vision_assist.is_some();
        let provider_present = vision_assist_present || provider.is_some() || !nodes.is_empty();
        if let Some(assist) = vision_assist {
            adaptive = adaptive.with_vision_assist(assist);
        }
        if let Some(extractor) = provider {
            adaptive = adaptive.with_structured_extractor(extractor);
        }
        if let Some(corpus_dir) = &config.vision.corpus_dir {
            match intent_engine::VisionCorpus::new(corpus_dir) {
                Ok(corpus) => adaptive = adaptive.with_vision_corpus(corpus),
                Err(error) => {
                    tracing::warn!(
                        %error,
                        dir = %corpus_dir.display(),
                        "vision corpus directory unavailable; collection disabled"
                    );
                }
            }
        }
        let mut pages =
            PageRuntime::new_adaptive(journal, workers.clone(), Some(checkpoints), adaptive);
        pages = pages.with_context_graph_attached();
        if config.vision.prefill {
            pages = pages.with_vision_prefill_enabled();
        }
        if let Some(observer) = observer {
            pages = pages.with_execution_phase_observer(observer);
        }
        let sessions = SessionManager::new(workers);
        Ok(Self::with_recovery(sessions, pages, recovery)
            .with_nodes(nodes)
            .with_vision_state(vision_assist_present, provider_present))
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
        if self.vision_assist_configured {
            capabilities.push("vision-assist".to_string());
        }
        if self.vision_provider_configured {
            capabilities.push("vision-provider".to_string());
        }
        RuntimeInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            capabilities,
            active_sessions,
            queued_jobs: self.in_flight.load(std::sync::atomic::Ordering::Acquire),
            uptime_ms: self.started_at.elapsed().as_millis() as u64,
        }
    }

    pub async fn create_session(
        &self,
        req: CreateSessionRequest,
    ) -> Result<SessionState, RuntimeError> {
        if req.execution_policy.vision_assist && !self.vision_provider_configured {
            return Err(RuntimeError::InvalidRequest(
                "executionPolicy.visionAssist requires a configured vision provider; run `bobby vision connect` and start with managed vision enabled"
                    .into(),
            ));
        }
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

    pub async fn form_snapshot(
        &self,
        session_id: &types::SessionId,
        page_id: &types::PageId,
        max_controls: Option<u32>,
    ) -> Result<types::FormSnapshot, RuntimeError> {
        self.sessions.get(session_id).await?;
        self.pages
            .form_snapshot(session_id, page_id, max_controls)
            .await
    }

    pub async fn submit(&self, envelope: CommandEnvelope) -> CommandOutcome {
        self.submit_with_vision_capability(envelope, false).await
    }

    /// Submit with the authenticated principal's vision capability flag.
    /// Deny-by-default: this flag and the session's `executionPolicy.visionAssist`
    /// grant must both be true before the provider runs.
    pub async fn submit_with_vision_capability(
        &self,
        envelope: CommandEnvelope,
        vision_capability_ok: bool,
    ) -> CommandOutcome {
        self.submit_with_vision_grant(envelope, vision_capability_ok, false)
            .await
    }

    /// Mints the pre-action checkpoint a Boundary command requires, then runs it.
    ///
    /// The gateway cannot do this: a `WorkflowCheckpoint` needs `restart_url`
    /// and `current_url`, and nothing on `RuntimeInterface` exposes live page
    /// state. So the three calls an agent used to make -- pin ids, save a
    /// checkpoint naming them, submit -- collapse here, where the page
    /// registry and the context graph are both reachable.
    ///
    /// This is sugar over `Executor::validate`, never a bypass. The gate still
    /// runs and still matches on all five fields; a checkpoint that fails to
    /// save refuses the submit rather than proceeding unprotected.
    pub async fn submit_with_auto_checkpoint(
        &self,
        envelope: CommandEnvelope,
        vision_capability_ok: bool,
        one_shot_session_ok: bool,
    ) -> Result<(CommandOutcome, types::CheckpointId), RuntimeError> {
        let recovery = self
            .recovery
            .as_ref()
            .ok_or_else(|| RuntimeError::Internal("recovery is unavailable".into()))?;
        let page_id = envelope
            .page_id
            .clone()
            .ok_or_else(|| RuntimeError::NotFound("page".into()))?;
        let page = self.pages.get(&page_id).await?;
        let url = page.url.clone().unwrap_or_default();

        // Ids only -- the journal stays the authority for the evidence itself.
        let mut evidence = Vec::new();
        for command_id in self.pages.context().commands_for(&page_id) {
            if let Ok(items) = self.pages.evidence_for_command(command_id).await {
                evidence.extend(items);
            }
        }

        let checkpoint = types::WorkflowCheckpoint {
            schema_version: types::WorkflowCheckpoint::SCHEMA_VERSION,
            checkpoint_id: types::CheckpointId::new(),
            workflow_id: envelope.workflow_id.clone(),
            attempt_id: envelope.attempt_id.clone(),
            session_id: envelope.session_id.clone(),
            page_id,
            restart_url: url.clone(),
            current_url: url,
            cursor: None,
            boundary_command_id: Some(envelope.command_id.clone()),
            recovery_class: types::CommandClass::Boundary,
            // Nothing is asserted that the collected evidence does not already
            // support, so the invariant evaluation inside `save_verified`
            // cannot fail on a checkpoint this function authored.
            invariants: Vec::new(),
            replayable_inputs: Vec::new(),
            evidence: Vec::new(),
            recovery_history: Vec::new(),
            recovery_receipts: Vec::new(),
            created_at: chrono::Utc::now(),
        };
        let saved = recovery
            .save_verified(checkpoint, evidence)
            .await
            .map_err(|error| RuntimeError::Internal(format!("checkpoint save failed: {error}")))?;
        let outcome = self
            .submit_with_vision_grant(envelope, vision_capability_ok, one_shot_session_ok)
            .await;
        Ok((outcome, saved.checkpoint_id))
    }

    async fn submit_with_vision_grant(
        &self,
        envelope: CommandEnvelope,
        vision_capability_ok: bool,
        one_shot_session_ok: bool,
    ) -> CommandOutcome {
        // SECURITY(F4): authoritative deny-by-default gate for `EvaluateJavaScript`.
        // The session must have opted in (`ExecutionPolicy.javascript_evaluation ==
        // true`) or the command is refused here, before reaching `self.pages.execute`
        // or a worker. Independent of, and after, the token capability gate in
        // `AuthenticatedRuntime::submit`; both must pass. Fails closed: an unknown or
        // absent session counts as `javascript_evaluation == false`. `self.pages.execute`
        // validates against its own page registry, not `SessionManager`, so it does not
        // perform this check.
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
                tracing::warn!(session_id = %envelope.session_id.0, "policy.javascript_denied");
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

        // One session lookup for the whole policy. An absent session means every
        // flag is false: no vision escalation, fingerprint spoofing, or
        // humanized input.
        let policy = self
            .sessions
            .get(&envelope.session_id)
            .await
            .map(|session| session.execution_policy.clone())
            .unwrap_or_default();
        let vision = match &envelope.command {
            RuntimeCommand::Intent(_) => VisionGate {
                session_ok: policy.vision_assist || one_shot_session_ok,
                capability_ok: vision_capability_ok,
            },
            RuntimeCommand::Primitive(_) => VisionGate::default(),
        };
        // Fail closed on every negative path: no name, an unknown name, or a
        // name of the wrong kind all make the intent engine decline the
        // escalation. No branch substitutes a different node for the one the
        // session asked for. The one permitted convenience: an unnamed node
        // resolves only when exactly one vision node is registered — the
        // default never picks between providers.
        let vision_node = match policy.vision_node.as_deref() {
            None => match self.nodes.default_vision_node_name() {
                Some(name) => match self.nodes.vision(name) {
                    Ok(provider) => NodeSelection::Resolved(provider),
                    Err(error) => {
                        tracing::warn!(node = %name, %error, "node.vision.default_unresolved");
                        NodeSelection::Unresolved
                    }
                },
                None => NodeSelection::NotRequested,
            },
            Some(name) => match self.nodes.vision(name) {
                Ok(provider) => NodeSelection::Resolved(provider),
                Err(error) => {
                    tracing::warn!(node = %name, %error, "node.vision.unresolved");
                    NodeSelection::Unresolved
                }
            },
        };
        let gate = SessionGate {
            vision,
            fingerprint: policy.fingerprint,
            humanize: policy.humanize,
            vision_node,
        };
        let closed_page = match &envelope.command {
            RuntimeCommand::Primitive(PrimitiveCommand::ClosePage(command)) => {
                Some((envelope.session_id.clone(), command.page_id.clone()))
            }
            _ => None,
        };
        let _in_flight = InFlightGuard::acquire(Arc::clone(&self.in_flight));
        let outcome = self.pages.execute_with_session_gate(envelope, gate).await;
        if matches!(outcome, CommandOutcome::Completed { .. }) {
            if let Some((session_id, page_id)) = closed_page {
                let _ = self.sessions.remove_page(&session_id, &page_id).await;
            }
        }
        outcome
    }

    /// Save a checkpoint whose evidence the caller has already verified.
    pub(crate) async fn checkpoint_with_evidence(
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

    /// Evidence for each named command, resolved from the journal the
    /// runtime itself wrote rather than authored by the caller.
    async fn resolve_evidence(
        &self,
        evidence_refs: Vec<CommandId>,
    ) -> Result<Vec<Evidence>, RecoveryError> {
        let mut evidence = Vec::new();
        for command_id in evidence_refs {
            evidence.extend(self.pages.evidence_for_command(command_id).await?);
        }
        Ok(evidence)
    }

    /// Save a checkpoint, resolving its evidence from the journal by command
    /// id rather than accepting `Evidence` from the caller: an id with no
    /// journal record, or one that never reached a terminal outcome, fails the
    /// checkpoint.
    ///
    /// **This method performs NO ownership check.** The journal is fleet-wide,
    /// so another principal's command id resolves here exactly like one of the
    /// caller's own. Ownership is checked one layer up, in
    /// `AuthenticatedRuntime::resolve_command_evidence` (`crate::interface`),
    /// which runs `require_owned_session` before any evidence is read.
    ///
    /// Authenticated surfaces must reach checkpointing through
    /// `AuthenticatedRuntime::checkpoint`, never by calling this method with
    /// caller-supplied command ids.
    pub async fn checkpoint(
        &self,
        checkpoint: WorkflowCheckpoint,
        evidence_refs: Vec<CommandId>,
    ) -> Result<WorkflowCheckpoint, RecoveryError> {
        let evidence = self.resolve_evidence(evidence_refs).await?;
        self.checkpoint_with_evidence(checkpoint, evidence).await
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

    pub async fn recovery_status(
        &self,
        workflow_id: &WorkflowId,
    ) -> Result<types::RecoveryStatus, RecoveryError> {
        let checkpoint = self
            .recovery
            .as_ref()
            .ok_or(RecoveryError::WorkersUnavailable)?
            .load_checkpoint(workflow_id)
            .await?;
        Ok(types::RecoveryStatus {
            workflow_id: workflow_id.clone(),
            receipts: checkpoint.recovery_receipts.clone(),
            checkpoint,
        })
    }

    pub async fn recovery_session(
        &self,
        workflow_id: &WorkflowId,
    ) -> Result<types::SessionId, RecoveryError> {
        self.recovery
            .as_ref()
            .ok_or(RecoveryError::WorkersUnavailable)?
            .checkpoint_session(workflow_id)
            .await
    }

    pub async fn workflows_for_session(
        &self,
        session_id: &types::SessionId,
        limit: usize,
    ) -> Result<Vec<WorkflowId>, RecoveryError> {
        self.recovery
            .as_ref()
            .ok_or(RecoveryError::WorkersUnavailable)?
            .workflows_for_session(session_id, limit)
            .await
    }

    pub async fn recover_for_session(
        &self,
        workflow_id: &WorkflowId,
        session_id: &types::SessionId,
    ) -> Result<RecoveryDecision, RecoveryError> {
        self.recovery
            .as_ref()
            .ok_or(RecoveryError::WorkersUnavailable)?
            .recover_for_session(workflow_id, session_id)
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
