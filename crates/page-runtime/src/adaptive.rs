use std::sync::Arc;

use artifact_store::{ArtifactStore, PendingArtifact};
use async_trait::async_trait;
use intent_engine::{IntentBrowser, IntentEngine, IntentOutcome, VisionAssist, VisionContext};
use network_engine::{
    DirectHttpExecutor, EligibilityDecision, EligibilityPolicy, HttpCandidate, NetworkPolicy,
};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandEnvelope, CommandError, ControlActionCommand,
    ErrorCode, ErrorLayer, Evidence, ExecutionPath, ExecutionReason, PageId, PageState,
    PrimitiveCommand, RuntimeCommand, TargetSpec, TypeTextCommand, UploadFilesCommand,
    WaitForCommand,
};
use worker_pool::WorkerLease;

/// Session + capability flags for vision escalation. Provider lives on
/// [`AdaptivePageEngine`]; IntentEngine enforces the deny-by-default double gate.
#[derive(Debug, Clone, Copy, Default)]
pub struct VisionGate {
    pub session_ok: bool,
    pub capability_ok: bool,
}

/// Everything `ExecutionPolicy` decides that the executor has to apply, resolved
/// once per command by the layer that can see the session
/// (`sdk_core::RuntimeService`).
///
/// `Default` must stay all-off with no provider: a caller that cannot prove the session
/// opted in gets no fingerprinting, no humanization, and no node.
#[derive(Clone, Default)]
pub struct SessionGate {
    pub vision: VisionGate,
    pub fingerprint: bool,
    pub humanize: bool,
    /// The outcome of resolving this session's named vision node.
    pub vision_node: NodeSelection,
}

/// What resolving a session's `visionNode` produced.
///
/// The three states must stay distinct. Collapsing `Unresolved` into `NotRequested`
/// (an `Option<Arc<dyn VisionAssist>>`) lets a mistyped node name silently escalate to
/// whatever provider the process was built with.
#[derive(Clone, Default)]
pub enum NodeSelection {
    /// The session named no node. An embedder-installed provider, if any,
    /// applies: nothing was chosen, so nothing was overridden.
    #[default]
    NotRequested,
    /// The session named a node and it resolved.
    Resolved(Arc<dyn VisionAssist>),
    /// The session named a node that did not resolve. No provider runs, and
    /// no other provider stands in for it.
    Unresolved,
}

impl NodeSelection {
    /// The private `provider`, exposed so the three states can be asserted apart
    /// without a live browser escalation.
    pub fn provider_for_test(
        &self,
        installed: Option<Arc<dyn VisionAssist>>,
    ) -> Option<Arc<dyn VisionAssist>> {
        self.provider(installed)
    }

    /// The provider to escalate to, given the process-wide default.
    fn provider(&self, installed: Option<Arc<dyn VisionAssist>>) -> Option<Arc<dyn VisionAssist>> {
        match self {
            Self::NotRequested => installed,
            Self::Resolved(provider) => Some(Arc::clone(provider)),
            Self::Unresolved => None,
        }
    }
}

impl std::fmt::Debug for NodeSelection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::NotRequested => "NotRequested",
            Self::Resolved(_) => "Resolved",
            Self::Unresolved => "Unresolved",
        })
    }
}

impl std::fmt::Debug for SessionGate {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SessionGate")
            .field("vision", &self.vision)
            .field("fingerprint", &self.fingerprint)
            .field("humanize", &self.humanize)
            .field("vision_node", &self.vision_node)
            .finish()
    }
}

impl From<VisionGate> for SessionGate {
    fn from(vision: VisionGate) -> Self {
        Self {
            vision,
            ..Self::default()
        }
    }
}

struct WorkerIntentBrowser<'a> {
    lease: &'a WorkerLease,
}

#[async_trait]
impl IntentBrowser for WorkerIntentBrowser<'_> {
    async fn collect_candidates(
        &self,
        page_id: &PageId,
        target: &TargetSpec,
    ) -> Result<Vec<dom_engine::Candidate>, CommandError> {
        self.lease
            .worker()
            .collect_candidates(page_id, target)
            .await
    }

    async fn click(
        &self,
        page_id: &PageId,
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.lease.worker().click(page_id, command).await
    }

    async fn click_xy(
        &self,
        page_id: &PageId,
        x: f64,
        y: f64,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.lease.worker().click_xy(page_id, x, y).await
    }

    async fn type_text(
        &self,
        page_id: &PageId,
        command: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.lease.worker().type_text(page_id, command).await
    }

    async fn upload_files(
        &self,
        page_id: &PageId,
        command: &UploadFilesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.lease.worker().upload_files(page_id, command).await
    }

    async fn control_action(
        &self,
        page_id: &PageId,
        command: &ControlActionCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.lease.worker().control_action(page_id, command).await
    }

    async fn wait_for(
        &self,
        page_id: &PageId,
        command: &WaitForCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.lease.worker().wait_for(page_id, command).await
    }

    async fn capture_screenshot(
        &self,
        page_id: &PageId,
        command: &CaptureScreenshotCommand,
    ) -> Result<(Vec<u8>, Vec<Evidence>), CommandError> {
        // Real PNG bytes when the worker supports them. Workers without byte plumbing
        // stay artifact-only, and vision providers get an empty frame, which their own
        // confidence floor rejects.
        let bytes = self
            .lease
            .worker()
            .screenshot_bytes(page_id)
            .await
            .unwrap_or_default();
        let evidence = self
            .lease
            .worker()
            .capture_screenshot(page_id, command)
            .await?;
        Ok((bytes, evidence))
    }
}

#[derive(Debug)]
pub struct AdaptiveExecution {
    pub evidence: Vec<Evidence>,
    pub used_browser: bool,
    pub prepared_http: Option<PreparedHttpResult>,
}

#[derive(Debug)]
pub struct AdaptiveFailure {
    pub error: CommandError,
    pub evidence: Vec<Evidence>,
}

impl From<CommandError> for AdaptiveFailure {
    fn from(error: CommandError) -> Self {
        Self {
            error,
            evidence: Vec::new(),
        }
    }
}

#[derive(Debug)]
pub struct PreparedHttpResult {
    pub state_version: u64,
    pub state: network_engine::ResponseStateDelta,
    pub artifact: Option<PendingArtifact>,
}

#[derive(Clone, Default)]
pub struct AdaptivePageEngine {
    direct: Option<DirectComponents>,
    vision_assist: Option<Arc<dyn VisionAssist>>,
    structured_extractor: Option<Arc<dyn intent_engine::StructuredExtractor>>,
    /// Prefill proposal cache handle. `None` unless `[vision].prefill` is
    /// on; the executor attaches the session's context graph.
    proposals: Option<Arc<dyn intent_engine::ProposalLookup>>,
    /// The runtime's context graph, for the vision prompt's recent-commands
    /// block. Attached always; independent of the prefill flag.
    context_graph: Option<Arc<crate::ContextGraph>>,
}

#[derive(Clone)]
struct DirectComponents {
    eligibility: EligibilityPolicy,
    executor: DirectHttpExecutor,
    artifacts: ArtifactStore,
    network: NetworkPolicy,
}

impl AdaptivePageEngine {
    pub fn browser_only() -> Self {
        Self::default()
    }

    pub fn new(
        eligibility: EligibilityPolicy,
        executor: DirectHttpExecutor,
        artifacts: ArtifactStore,
        network: NetworkPolicy,
    ) -> Self {
        Self {
            direct: Some(DirectComponents {
                eligibility,
                executor,
                artifacts,
                network,
            }),
            vision_assist: None,
            structured_extractor: None,
            proposals: None,
            context_graph: None,
        }
    }

    /// Enables lazy batch prefill against this proposal cache (the
    /// runtime's context graph). Off by default.
    pub fn with_vision_prefill(
        mut self,
        proposals: Arc<dyn intent_engine::ProposalLookup>,
    ) -> Self {
        self.proposals = Some(proposals);
        self
    }

    /// Attaches the runtime's context graph so escalation prompts carry the
    /// recent-commands block.
    pub fn with_context_graph(mut self, graph: Arc<crate::ContextGraph>) -> Self {
        self.context_graph = Some(graph);
        self
    }

    pub fn with_vision_assist(mut self, assist: Arc<dyn VisionAssist>) -> Self {
        self.vision_assist = Some(assist);
        self
    }

    pub fn with_structured_extractor(
        mut self,
        extractor: Arc<dyn intent_engine::StructuredExtractor>,
    ) -> Self {
        self.structured_extractor = Some(extractor);
        self
    }

    pub fn finalize_prepared_artifact(
        &self,
        session_id: &types::SessionId,
        artifact_id: &str,
        staging_id: &str,
        sha256: &str,
        bytes: u64,
    ) -> Result<(), CommandError> {
        let direct = self
            .direct
            .as_ref()
            .ok_or_else(|| internal_artifact_error("adaptive artifact store is not configured"))?;
        direct
            .artifacts
            .finalize_staged(session_id, artifact_id, staging_id, sha256, bytes)
            .map_err(artifact_error)
    }

    pub async fn execute(
        &self,
        envelope: &CommandEnvelope,
        lease: &WorkerLease,
        page: Option<PageState>,
        gate: &SessionGate,
    ) -> Result<AdaptiveExecution, AdaptiveFailure> {
        let vision_gate = gate.vision;
        if let RuntimeCommand::Intent(intent) = &envelope.command {
            return execute_intent(
                envelope,
                lease,
                intent,
                vision_gate,
                gate.vision_node.provider(self.vision_assist.clone()),
                self.proposals.clone(),
                page.as_ref().and_then(|page| page.url.clone()),
                self.context_graph.clone(),
            )
            .await;
        }
        if let RuntimeCommand::Primitive(PrimitiveCommand::ExtractStructured(command)) =
            &envelope.command
        {
            return extract_structured(
                envelope,
                lease,
                command,
                vision_gate,
                self.structured_extractor.clone(),
            )
            .await;
        }
        let RuntimeCommand::Primitive(command) = &envelope.command else {
            unreachable!("Intent handled above");
        };
        // The direct-HTTP path reads and commits the worker's own HTTP state mirror.
        // A worker without that mirror (the Firefox companion) can still serve every
        // eligible command through the browser, so treat it exactly like an unconfigured
        // direct path rather than letting `http_state` fail the command outright.
        let Some(direct) = self
            .direct
            .as_ref()
            .filter(|_| lease.worker().supports_http_state())
        else {
            return browser_execute(
                envelope,
                lease,
                ExecutionPath::Chromium,
                ExecutionReason::IneligibleCommand,
                0,
            )
            .await;
        };
        let page_url = page
            .as_ref()
            .and_then(|page| page.url.as_deref())
            .unwrap_or_default();
        match direct.eligibility.classify(command, page_url) {
            EligibilityDecision::Denied(error) => Err(error.into()),
            EligibilityDecision::Chromium(reason) => {
                browser_execute(envelope, lease, ExecutionPath::Chromium, reason, 0).await
            }
            EligibilityDecision::DirectHttp(reason) => {
                let page_id = envelope.page_id.as_ref().expect("validated page id");
                let snapshot = lease.worker().http_state(page_id).await?;
                let version = snapshot.version;
                let candidate = match command {
                    PrimitiveCommand::Inspect(command) => {
                        direct.executor.inspect(&snapshot, command).await?
                    }
                    PrimitiveCommand::DownloadUrl(command) => {
                        direct.executor.download(&snapshot, command).await?
                    }
                    _ => unreachable!("eligibility only selects supported HTTP commands"),
                };
                match candidate {
                    HttpCandidate::FallbackRequired(fallback_reason) => {
                        if matches!(command, PrimitiveCommand::Inspect(_)) {
                            browser_execute(
                                envelope,
                                lease,
                                ExecutionPath::ChromiumFallback,
                                fallback_reason,
                                version,
                            )
                            .await
                        } else {
                            Err(equivalence_unproven(fallback_reason).into())
                        }
                    }
                    HttpCandidate::Inspection {
                        evidence,
                        state,
                        meta,
                    } => Ok(AdaptiveExecution {
                        evidence: vec![
                            evidence,
                            execution_evidence(
                                ExecutionPath::DirectHttp,
                                reason,
                                version,
                                ExecutionMetrics::http(meta),
                            ),
                        ],
                        used_browser: false,
                        prepared_http: Some(PreparedHttpResult {
                            state_version: version,
                            state,
                            artifact: None,
                        }),
                    }),
                    HttpCandidate::Download {
                        bytes,
                        filename,
                        media_type,
                        state,
                        meta,
                    } => {
                        let extension = safe_extension(&filename);
                        let pending = direct
                            .artifacts
                            .put_pending(
                                &envelope.session_id,
                                page_id,
                                &media_type,
                                extension,
                                &bytes,
                                direct.network.max_download_bytes,
                            )
                            .await
                            .map_err(artifact_error)?;
                        let record = pending.record().clone();
                        Ok(AdaptiveExecution {
                            evidence: vec![
                                Evidence::Download {
                                    filename,
                                    path: record.artifact_id,
                                    bytes: record.bytes,
                                    sha256: record.sha256.clone(),
                                },
                                execution_evidence(
                                    ExecutionPath::DirectHttp,
                                    reason,
                                    version,
                                    ExecutionMetrics::http(meta),
                                ),
                            ],
                            used_browser: false,
                            prepared_http: Some(PreparedHttpResult {
                                state_version: version,
                                state,
                                artifact: Some(pending),
                            }),
                        })
                    }
                }
            }
        }
    }
}

/// Byte-index slicing a `String` panics mid-codepoint; back off to a char
/// boundary so non-ASCII content cannot kill the request task.
fn truncate_utf8(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

fn safe_extension(filename: &str) -> &str {
    filename
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 16
                && extension.chars().all(|c| c.is_ascii_alphanumeric())
        })
        .unwrap_or("bin")
}

fn artifact_error(error: artifact_store::ArtifactError) -> CommandError {
    CommandError {
        code: ErrorCode::Internal,
        message: format!("download artifact persistence failed: {error}"),
        layer: ErrorLayer::Page,
        retryable: false,
    }
}

fn internal_artifact_error(message: impl Into<String>) -> CommandError {
    CommandError {
        code: ErrorCode::Internal,
        message: message.into(),
        layer: ErrorLayer::Page,
        retryable: false,
    }
}

fn equivalence_unproven(reason: ExecutionReason) -> CommandError {
    CommandError {
        code: ErrorCode::HttpEquivalenceUnproven,
        message: format!("direct download equivalence was not proven: {reason:?}"),
        layer: ErrorLayer::Network,
        retryable: false,
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_intent(
    envelope: &CommandEnvelope,
    lease: &WorkerLease,
    intent: &types::IntentCommand,
    vision_gate: VisionGate,
    assist: Option<Arc<dyn VisionAssist>>,
    proposals: Option<Arc<dyn intent_engine::ProposalLookup>>,
    page_url: Option<String>,
    context_graph: Option<Arc<crate::ContextGraph>>,
) -> Result<AdaptiveExecution, AdaptiveFailure> {
    let page_id = envelope.page_id.as_ref().expect("validated page id");
    let browser = WorkerIntentBrowser { lease };
    let gates_open = vision_gate.session_ok && vision_gate.capability_ok;
    let recent_command_kinds = context_graph
        .as_ref()
        .map(|graph| graph.recent_command_kinds(page_id))
        .unwrap_or_default();
    let prompt_context = if page_url.is_none() && recent_command_kinds.is_empty() {
        None
    } else {
        Some(intent_engine::VisionPromptContext {
            url: page_url,
            candidates: Vec::new(),
            recent_command_kinds,
        })
    };
    let vision = VisionContext {
        session_ok: vision_gate.session_ok,
        capability_ok: vision_gate.capability_ok,
        assist,
        // The cache is only ever consulted behind both gates; a closed gate
        // gets `None` and the byte-identical pre-prefill path.
        proposals: proposals.filter(|_| gates_open),
        // Escalation deferral is an engine-internal complete_form decision.
        defer_escalation: false,
        prompt_context,
    };
    match IntentEngine::execute(intent, page_id, &browser, &vision).await {
        IntentOutcome::Completed { evidence } => Ok(AdaptiveExecution {
            evidence,
            used_browser: true,
            prepared_http: None,
        }),
        IntentOutcome::Failed { error, evidence } => Err(AdaptiveFailure { error, evidence }),
    }
}

fn execution_evidence(
    path: ExecutionPath,
    reason: ExecutionReason,
    state_version: u64,
    metrics: ExecutionMetrics,
) -> Evidence {
    Evidence::ExecutionPath {
        path,
        reason,
        state_version,
        elapsed_ms: metrics.elapsed_ms,
        bytes: metrics.bytes,
        sha256: metrics.sha256,
        final_url: metrics.final_url,
        content_type: metrics.content_type,
        status: metrics.status,
        redirect_chain: metrics.redirect_chain,
    }
}

struct ExecutionMetrics {
    elapsed_ms: u64,
    bytes: Option<u64>,
    sha256: Option<String>,
    final_url: Option<String>,
    content_type: Option<String>,
    status: Option<u16>,
    redirect_chain: Vec<String>,
}

impl ExecutionMetrics {
    fn http(meta: network_engine::HttpMeta) -> Self {
        Self {
            elapsed_ms: meta.elapsed_ms,
            bytes: Some(meta.bytes),
            sha256: Some(meta.sha256),
            final_url: Some(meta.final_url),
            content_type: Some(meta.content_type),
            status: Some(meta.status),
            redirect_chain: meta.redirect_chain,
        }
    }

    fn browser(bytes: Option<u64>, sha256: Option<String>) -> Self {
        Self {
            elapsed_ms: 0,
            bytes,
            sha256,
            final_url: None,
            content_type: None,
            status: None,
            redirect_chain: Vec::new(),
        }
    }
}

async fn browser_execute(
    envelope: &CommandEnvelope,
    lease: &WorkerLease,
    path: ExecutionPath,
    reason: ExecutionReason,
    state_version: u64,
) -> Result<AdaptiveExecution, AdaptiveFailure> {
    let page_id = envelope.page_id.as_ref();
    let RuntimeCommand::Primitive(command) = &envelope.command else {
        unreachable!("intent commands use execute_intent");
    };
    let mut evidence = match command {
        PrimitiveCommand::Navigate(command) => {
            lease
                .worker()
                .navigate(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::Inspect(command) => {
            lease
                .worker()
                .inspect(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::Click(command) => {
            lease
                .worker()
                .click(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::TypeText(command) => {
            lease
                .worker()
                .type_text(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::ControlAction(command) => {
            lease
                .worker()
                .control_action(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::UploadFiles(command) => {
            lease
                .worker()
                .upload_files(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::OpenPage(command) => lease.worker().open_page_command(command).await?,
        PrimitiveCommand::ListPages(command) => lease.worker().list_pages(command).await?,
        PrimitiveCommand::ClosePage(command) => lease.worker().close_page_command(command).await?,
        PrimitiveCommand::ActivatePage(command) => lease.worker().activate_page(command).await?,
        PrimitiveCommand::AccessibilitySnapshot(command) => {
            lease
                .worker()
                .a11y_snapshot(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::ClickAndWaitForPopup(command) => {
            lease
                .worker()
                .click_and_wait_for_popup(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::ClickAndWaitForDownload(command) => {
            lease
                .worker()
                .click_and_wait_for_download(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::WaitFor(command) => {
            lease
                .worker()
                .wait_for(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::CaptureScreenshot(command) => {
            lease
                .worker()
                .capture_screenshot(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::SetFocusEmulation(command) => {
            lease
                .worker()
                .set_focus_emulation(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::SetEmulatedMedia(command) => {
            lease
                .worker()
                .set_emulated_media(page_id.expect("validated page id"), command)
                .await?
        }
        // Only ChromiumWorker::evaluate_javascript executes the JS; other workers
        // return the default unsupported CommandError.
        PrimitiveCommand::EvaluateJavaScript(command) => {
            lease
                .worker()
                .evaluate_javascript(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::NetworkLog(command) => {
            lease
                .worker()
                .network_log(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::Emulate(command) => {
            lease
                .worker()
                .emulate(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::HandleDialog(command) => {
            lease
                .worker()
                .handle_dialog(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::PrintToPdf(command) => {
            lease
                .worker()
                .print_to_pdf(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::GetCookies(command) => {
            lease
                .worker()
                .get_cookies(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::SetCookies(command) => {
            lease
                .worker()
                .set_cookies(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::DeleteCookies(command) => {
            lease
                .worker()
                .delete_cookies(page_id.expect("validated page id"), command)
                .await?
        }
        PrimitiveCommand::ExtractStructured(_) => {
            unreachable!("structured extraction is intercepted in execute")
        }
        PrimitiveCommand::DownloadUrl(_) => {
            return Err(equivalence_unproven(reason).into());
        }
    };
    let (bytes, sha256) = evidence
        .iter()
        .find_map(|item| match item {
            Evidence::Download { bytes, sha256, .. } => Some((Some(*bytes), Some(sha256.clone()))),
            _ => None,
        })
        .unwrap_or((None, None));
    evidence.push(execution_evidence(
        path,
        reason,
        state_version,
        ExecutionMetrics::browser(bytes, sha256),
    ));
    Ok(AdaptiveExecution {
        evidence,
        used_browser: true,
        prepared_http: None,
    })
}

const MAX_EXTRACT_CONTENT_BYTES: usize = 16 * 1024;
const MAX_EXTRACT_RESULT_BYTES: usize = 64 * 1024;
const MAX_EXTRACT_SCHEMA_BYTES: usize = 16 * 1024;

/// Structured extraction over the configured provider. Shares the vision
/// double gate (session policy + token capability + configured provider)
/// because page content leaves the runtime toward an external model.
async fn extract_structured(
    envelope: &CommandEnvelope,
    lease: &WorkerLease,
    command: &types::ExtractStructuredCommand,
    vision_gate: VisionGate,
    assist: Option<Arc<dyn intent_engine::StructuredExtractor>>,
) -> Result<AdaptiveExecution, AdaptiveFailure> {
    let denied = || {
        AdaptiveFailure {
        error: CommandError {
            code: ErrorCode::VisionAssistDenied,
            message: "structured extraction requires vision:assist capability, session vision policy, and a configured provider".into(),
            layer: ErrorLayer::Workflow,
            retryable: false,
        },
        evidence: Vec::new(),
    }
    };
    if !vision_gate.session_ok || !vision_gate.capability_ok {
        return Err(denied());
    }
    let Some(assist) = assist else {
        return Err(denied());
    };
    let schema_bytes = serde_json::to_vec(&command.schema).map_err(|_| AdaptiveFailure {
        error: CommandError {
            code: ErrorCode::InvalidRequest,
            message: "extraction schema is not serializable".into(),
            layer: ErrorLayer::Workflow,
            retryable: false,
        },
        evidence: Vec::new(),
    })?;
    if schema_bytes.len() > MAX_EXTRACT_SCHEMA_BYTES {
        return Err(AdaptiveFailure {
            error: CommandError {
                code: ErrorCode::InvalidRequest,
                message: format!("extraction schema exceeds {MAX_EXTRACT_SCHEMA_BYTES} bytes"),
                layer: ErrorLayer::Workflow,
                retryable: false,
            },
            evidence: Vec::new(),
        });
    }
    jsonschema::validator_for(&command.schema).map_err(|_| AdaptiveFailure {
        error: CommandError {
            code: ErrorCode::InvalidRequest,
            message: "extraction schema is not a valid JSON schema".into(),
            layer: ErrorLayer::Workflow,
            retryable: false,
        },
        evidence: Vec::new(),
    })?;

    let page_id = envelope.page_id.as_ref().expect("validated page id");
    let mut evidence = lease
        .worker()
        .inspect(page_id, &types::InspectCommand::default())
        .await?;
    let content = evidence
        .iter()
        .find_map(|item| match item {
            Evidence::Inspection { text, .. } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default();
    let content = truncate_utf8(&content, MAX_EXTRACT_CONTENT_BYTES).to_owned();

    let value = assist
        .extract_structured(intent_engine::StructuredExtractRequest {
            schema: command.schema.clone(),
            content,
            purpose: command.purpose.clone(),
        })
        .await?;

    let validator = jsonschema::validator_for(&command.schema).map_err(|_| AdaptiveFailure {
        error: CommandError {
            code: ErrorCode::InvalidRequest,
            message: "extraction schema is not a valid JSON schema".into(),
            layer: ErrorLayer::Workflow,
            retryable: false,
        },
        evidence: Vec::new(),
    })?;
    if validator.validate(&value).is_err() {
        return Err(AdaptiveFailure {
            error: CommandError {
                code: ErrorCode::VerificationFailed,
                message: "provider result does not match the extraction schema".into(),
                layer: ErrorLayer::Workflow,
                retryable: true,
            },
            evidence: Vec::new(),
        });
    }
    let result_bytes = serde_json::to_vec(&value).unwrap_or_default();
    if result_bytes.len() > MAX_EXTRACT_RESULT_BYTES {
        return Err(AdaptiveFailure {
            error: CommandError {
                code: ErrorCode::VerificationFailed,
                message: format!("provider result exceeds {MAX_EXTRACT_RESULT_BYTES} bytes"),
                layer: ErrorLayer::Workflow,
                retryable: false,
            },
            evidence: Vec::new(),
        });
    }
    evidence.push(Evidence::StructuredExtraction {
        page_id: page_id.clone(),
        value,
        truncated: false,
    });
    evidence.push(execution_evidence(
        ExecutionPath::Chromium,
        ExecutionReason::IneligibleCommand,
        0,
        ExecutionMetrics::browser(None, None),
    ));
    Ok(AdaptiveExecution {
        evidence,
        used_browser: true,
        prepared_http: None,
    })
}

#[cfg(test)]
mod tests {
    use super::truncate_utf8;

    #[test]
    fn truncation_never_splits_a_codepoint() {
        // 'é' is two bytes; a cut at 1 would panic on a byte-index slice.
        let text = "é".repeat(100);
        let cut = truncate_utf8(&text, 51);
        assert_eq!(cut.len(), 50);
        assert!(cut.is_char_boundary(cut.len()));

        let ascii = "a".repeat(100);
        assert_eq!(truncate_utf8(&ascii, 51).len(), 51);

        let short = "héllo";
        assert_eq!(truncate_utf8(short, 100), short);
    }
}
