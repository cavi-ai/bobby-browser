//! Shared test helpers. Compiled into every integration binary, so a
//! helper only some binaries use reads as dead in the others.
#![allow(dead_code)]

//! Shared live-runtime harness for gateway integration tests.
//!
//! `RuntimeService::default()` has no journal, no worker pool, and no
//! recovery coordinator, so every command dispatched through it fails
//! downstream — tests built on it can only ever exercise auth and schema
//! validation. This harness wires the real `RuntimeService` (journal,
//! worker pool, recovery coordinator) over a fake worker that returns real
//! evidence per command kind, so tests assert actual outcomes.

use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use interface_core::{CapabilityHandle, EventStore, InterfaceResult, RuntimeInterface};
use mcp_gateway::{ArtifactResources, Server};
use page_runtime::{PageRuntime, RecoveryCoordinator};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use session_manager::SessionManager;
use tempfile::TempDir;
use types::{
    AccessibilitySnapshotCommand, ClickCommand, ClosePageCommand, CommandError, ErrorCode,
    ErrorLayer, Evidence, InspectCommand, ListPagesCommand, NavigateCommand, PageEvidence, PageId,
    SessionId, TypeTextCommand, WorkerId,
};
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};
use workflow_journal::{CommandJournal, JournalError, JournalRecord, JournalScan, JsonlJournal};

/// A fake worker that returns the evidence variant each command verifies
/// against, so outcomes reach `completed` instead of failing verification.
pub struct LiveWorker {
    id: WorkerId,
    profile: PathBuf,
    probe: Arc<LiveProbe>,
    open_mode: LiveOpenMode,
    block_delete: bool,
}

#[derive(Clone, Copy)]
enum LiveOpenMode {
    Succeed,
    Block,
    Fail,
}

#[derive(Default)]
pub struct LiveProbe {
    pub accessibility_calls: AtomicUsize,
    pub form_calls: AtomicUsize,
    pub accessibility_failures_remaining: AtomicUsize,
    pub form_failures_remaining: AtomicUsize,
    pub last_accessibility_max_nodes: AtomicUsize,
    pub last_form_max_controls: AtomicUsize,
    pub accessibility_restarts_remaining: AtomicUsize,
    pub navigation_interface_failures_remaining: AtomicUsize,
    pub open_entered: tokio::sync::Notify,
    pub open_release: tokio::sync::Notify,
    pub navigation_entered: tokio::sync::Notify,
    pub navigation_release: tokio::sync::Notify,
    pub delete_entered: tokio::sync::Notify,
    pub delete_release: tokio::sync::Notify,
    pub worker_closes: AtomicUsize,
    pub delete_failures_remaining: AtomicUsize,
    pub opened_page: std::sync::Mutex<Option<PageId>>,
    pub current_url: std::sync::Mutex<Option<String>>,
}

#[async_trait::async_trait]
impl BrowserWorker for LiveWorker {
    fn worker_id(&self) -> WorkerId {
        self.id.clone()
    }

    fn profile_dir(&self) -> &Path {
        &self.profile
    }

    async fn open_page(&self, page_id: PageId) -> Result<(), CommandError> {
        *self
            .probe
            .opened_page
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(page_id);
        self.probe.open_entered.notify_one();
        match self.open_mode {
            LiveOpenMode::Succeed => {}
            LiveOpenMode::Block => self.probe.open_release.notified().await,
            LiveOpenMode::Fail => {
                self.probe.open_release.notified().await;
                return Err(CommandError {
                    code: ErrorCode::BrowserCommandFailed,
                    message: "injected live-harness page-open failure".into(),
                    layer: ErrorLayer::Driver,
                    retryable: false,
                });
            }
        }
        Ok(())
    }

    async fn navigate(
        &self,
        _: &PageId,
        command: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        *self
            .probe
            .current_url
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = Some(command.url.clone());
        self.probe.navigation_entered.notify_one();
        if matches!(
            command.url.as_str(),
            "https://live-harness.test/block" | "https://live-harness.test/block-fail"
        ) {
            self.probe.navigation_release.notified().await;
        }
        if matches!(
            command.url.as_str(),
            "https://live-harness.test/fail" | "https://live-harness.test/block-fail"
        ) {
            return Err(CommandError {
                code: ErrorCode::BrowserCommandFailed,
                message: "injected live-harness navigation failure".into(),
                layer: ErrorLayer::Driver,
                retryable: false,
            });
        }
        Ok(vec![Evidence::Navigation {
            url: command.url.clone(),
            title: "live-harness".into(),
        }])
    }

    async fn inspect(
        &self,
        _: &PageId,
        command: &InspectCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Inspection {
            selector: command.selector.clone(),
            url: "https://live-harness.test/".into(),
            title: "live-harness".into(),
            text: "live-harness-text".into(),
            html: None,
        }])
    }

    async fn click(
        &self,
        _: &PageId,
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Element {
            selector: command.selector.clone(),
            text: None,
        }])
    }

    async fn type_text(
        &self,
        _: &PageId,
        command: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Element {
            selector: command.selector.clone(),
            text: Some(command.value.clone()),
        }])
    }

    async fn list_pages(&self, _: &ListPagesCommand) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Pages {
            pages: vec![PageEvidence {
                page_id: PageId::new(),
                url: "https://live-harness.test/".into(),
                title: "live-harness".into(),
            }],
        }])
    }

    async fn close_page_command(
        &self,
        command: &ClosePageCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Page {
            page_id: command.page_id.clone(),
            url: "https://live-harness.test/".into(),
            title: "live-harness".into(),
        }])
    }

    async fn network_log(
        &self,
        _: &PageId,
        _: &types::NetworkLogCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::HarArtifact {
            artifact_id: "live-harness-har".into(),
            media_type: "application/json".into(),
            bytes: 2,
            sha256: "a".repeat(64),
            entries: 0,
        }])
    }

    async fn a11y_snapshot(
        &self,
        page_id: &PageId,
        command: &AccessibilitySnapshotCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.probe
            .accessibility_calls
            .fetch_add(1, Ordering::SeqCst);
        self.probe.last_accessibility_max_nodes.store(
            command.max_nodes.map_or(0, |value| value as usize),
            Ordering::SeqCst,
        );
        if self
            .probe
            .accessibility_failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(CommandError {
                code: ErrorCode::BrowserCommandFailed,
                message: "injected live-harness accessibility failure".into(),
                layer: ErrorLayer::Driver,
                retryable: false,
            });
        }
        Ok(vec![Evidence::AccessibilitySnapshot {
            page_id: page_id.clone(),
            nodes: vec![types::AccessibilityNode {
                role: Some("textbox".into()),
                name: Some("Email address".into()),
                target: Some(types::AccessibilityTarget {
                    role: "textbox".into(),
                    accessible_name: "Email address".into(),
                    ordinal: Some(1),
                }),
                ..types::AccessibilityNode::default()
            }],
            truncated: false,
        }])
    }

    async fn form_snapshot(
        &self,
        page_id: &PageId,
        max_controls: Option<u32>,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.probe.form_calls.fetch_add(1, Ordering::SeqCst);
        self.probe.last_form_max_controls.store(
            max_controls.map_or(0, |value| value as usize),
            Ordering::SeqCst,
        );
        if self
            .probe
            .form_failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(CommandError {
                code: ErrorCode::BrowserCommandFailed,
                message: "injected live-harness form snapshot failure".into(),
                layer: ErrorLayer::Driver,
                retryable: false,
            });
        }
        Ok(vec![Evidence::FormSnapshot {
            snapshot: types::FormSnapshot {
                schema_version: types::FORM_SNAPSHOT_SCHEMA_VERSION,
                page_id: page_id.clone(),
                forms: Vec::new(),
                unowned_controls: Vec::new(),
                truncated: false,
            },
        }])
    }

    async fn collect_candidates(
        &self,
        _: &PageId,
        _: &types::TargetSpec,
    ) -> Result<Vec<dom_engine::Candidate>, CommandError> {
        // No DOM behind the fake: intents that must resolve a target fail
        // with the engine's own domain error (e.g. targetNotFound), which is
        // exactly the deterministic terminal outcome tests assert against.
        Ok(Vec::new())
    }

    fn supports_http_state(&self) -> bool {
        true
    }

    async fn http_state(
        &self,
        _: &PageId,
    ) -> Result<network_engine::state::HttpStateSnapshot, CommandError> {
        Ok(network_engine::state::HttpStateSnapshot {
            version: 0,
            current_url: self
                .probe
                .current_url
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone()
                .unwrap_or_default(),
            cookies: Vec::new(),
            cache_validators: Default::default(),
            user_agent: "live-harness".into(),
            language: "en".into(),
        })
    }

    async fn commit_http_state(
        &self,
        _: &PageId,
        _: u64,
        _: network_engine::state::ResponseStateDelta,
    ) -> Result<(), CommandError> {
        Ok(())
    }

    async fn close(&self) -> Result<(), CommandError> {
        self.probe.worker_closes.fetch_add(1, Ordering::SeqCst);
        self.probe.delete_entered.notify_one();
        if self.block_delete {
            self.probe.delete_release.notified().await;
        }
        if self
            .probe
            .delete_failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(CommandError {
                code: ErrorCode::BrowserCommandFailed,
                message: "injected live-harness session deletion failure".into(),
                layer: ErrorLayer::Driver,
                retryable: true,
            });
        }
        Ok(())
    }
}

pub struct LiveFactory {
    probe: Arc<LiveProbe>,
    open_mode: LiveOpenMode,
    block_delete: bool,
}

struct FaultInjectingRuntime {
    inner: Arc<AuthenticatedRuntime>,
    probe: Arc<LiveProbe>,
}

#[async_trait::async_trait]
impl RuntimeInterface for FaultInjectingRuntime {
    async fn runtime_info(
        &self,
        ctx: types::RequestContext,
    ) -> InterfaceResult<types::RuntimeInfo> {
        self.inner.runtime_info(ctx).await
    }

    async fn authorize_operation(
        &self,
        ctx: types::RequestContext,
        operation: types::InterfaceOperation,
    ) -> InterfaceResult<()> {
        self.inner.authorize_operation(ctx, operation).await
    }

    async fn list_sessions(
        &self,
        ctx: types::RequestContext,
    ) -> InterfaceResult<Vec<types::SessionState>> {
        self.inner.list_sessions(ctx).await
    }

    async fn delete_session(
        &self,
        ctx: types::RequestContext,
        session: SessionId,
    ) -> InterfaceResult<()> {
        self.inner.delete_session(ctx, session).await
    }

    async fn create_session(
        &self,
        ctx: types::RequestContext,
        request: types::CreateSessionRequest,
    ) -> InterfaceResult<types::SessionState> {
        self.inner.create_session(ctx, request).await
    }

    async fn open_page(
        &self,
        ctx: types::RequestContext,
        request: types::OpenPageRequest,
    ) -> InterfaceResult<types::PageState> {
        self.inner.open_page(ctx, request).await
    }

    async fn context_ask(
        &self,
        ctx: types::RequestContext,
        session: SessionId,
        page: PageId,
        description: String,
    ) -> InterfaceResult<Option<types::ContextAnswer>> {
        self.inner
            .context_ask(ctx, session, page, description)
            .await
    }

    async fn context_neighbors(
        &self,
        ctx: types::RequestContext,
        session: SessionId,
        page: PageId,
        description: String,
    ) -> InterfaceResult<Option<types::ContextNeighbors>> {
        self.inner
            .context_neighbors(ctx, session, page, description)
            .await
    }

    async fn form_snapshot(
        &self,
        ctx: types::RequestContext,
        session: SessionId,
        page: PageId,
        max_controls: Option<u32>,
    ) -> InterfaceResult<types::FormSnapshot> {
        self.inner
            .form_snapshot(ctx, session, page, max_controls)
            .await
    }

    async fn submit(
        &self,
        ctx: types::RequestContext,
        envelope: types::CommandEnvelope,
    ) -> InterfaceResult<types::CommandOutcome> {
        let fail_navigation = matches!(
            envelope.command,
            types::RuntimeCommand::Primitive(types::PrimitiveCommand::Navigate(_))
        ) && self
            .probe
            .navigation_interface_failures_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok();
        if fail_navigation {
            return Err(types::InterfaceError {
                code: types::InterfaceErrorCode::Internal,
                layer: types::ErrorLayer::Interface,
                message: "injected navigation interface failure".into(),
                correlation_id: ctx.correlation_id,
                command_id: Some(envelope.command_id),
                retryable: false,
                retry_after_ms: None,
                reconciliation_required: false,
                required_capability: None,
            });
        }
        let restart = matches!(
            envelope.command,
            types::RuntimeCommand::Primitive(types::PrimitiveCommand::AccessibilitySnapshot(_))
        ) && self
            .probe
            .accessibility_restarts_remaining
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok();
        let outcome = self.inner.submit(ctx, envelope.clone()).await?;
        if !restart {
            return Ok(outcome);
        }
        match outcome {
            types::CommandOutcome::Completed { evidence, .. } => {
                Ok(types::CommandOutcome::Restarted {
                    command_id: envelope.command_id,
                    prior_attempt_id: types::AttemptId::new(),
                    attempt_id: envelope.attempt_id,
                    reason: "injected live-harness restart".into(),
                    evidence,
                })
            }
            outcome => Ok(outcome),
        }
    }

    async fn checkpoint(
        &self,
        ctx: types::RequestContext,
        checkpoint: types::WorkflowCheckpoint,
        evidence: Vec<Evidence>,
    ) -> InterfaceResult<types::WorkflowCheckpoint> {
        self.inner.checkpoint(ctx, checkpoint, evidence).await
    }

    async fn resolve_command_evidence(
        &self,
        ctx: types::RequestContext,
        command_ids: Vec<types::CommandId>,
    ) -> InterfaceResult<Vec<Evidence>> {
        self.inner.resolve_command_evidence(ctx, command_ids).await
    }

    async fn recover(
        &self,
        ctx: types::RequestContext,
        workflow: types::WorkflowId,
    ) -> InterfaceResult<types::RecoveryDecision> {
        self.inner.recover(ctx, workflow).await
    }

    async fn recovery_status(
        &self,
        ctx: types::RequestContext,
        workflow: types::WorkflowId,
    ) -> InterfaceResult<types::RecoveryStatus> {
        self.inner.recovery_status(ctx, workflow).await
    }

    async fn submit_with_auto_checkpoint(
        &self,
        ctx: types::RequestContext,
        envelope: types::CommandEnvelope,
    ) -> InterfaceResult<(types::CommandOutcome, types::CheckpointId)> {
        self.inner.submit_with_auto_checkpoint(ctx, envelope).await
    }

    async fn workflows_for_session(
        &self,
        ctx: types::RequestContext,
        session: SessionId,
        limit: usize,
    ) -> InterfaceResult<Vec<types::WorkflowId>> {
        self.inner.workflows_for_session(ctx, session, limit).await
    }
}

#[async_trait::async_trait]
impl WorkerFactory for LiveFactory {
    async fn launch(&self, session_id: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(Arc::new(LiveWorker {
            id: WorkerId::new(),
            profile: PathBuf::from(format!("/profiles/{}", session_id.0)),
            probe: Arc::clone(&self.probe),
            open_mode: self.open_mode,
            block_delete: self.block_delete,
        }))
    }
}

/// A gateway server over a fully wired runtime, plus the journal every
/// command writes and the tempdir holding it (dropped when the caller drops
/// it).
pub struct LiveServer {
    pub server: Arc<Server>,
    pub journal: Arc<LiveJournal>,
    pub probe: Arc<LiveProbe>,
    pub runtime: RuntimeService,
    pub handle: CapabilityHandle,
    pub downloads_dir: PathBuf,
    _root: TempDir,
}

impl LiveServer {
    pub fn accessibility_calls(&self) -> usize {
        self.probe.accessibility_calls.load(Ordering::SeqCst)
    }

    pub fn form_calls(&self) -> usize {
        self.probe.form_calls.load(Ordering::SeqCst)
    }
}

pub struct LiveJournal {
    inner: JsonlJournal,
    records: tokio::sync::Mutex<Vec<JournalRecord>>,
}

impl LiveJournal {
    pub async fn records(&self) -> Vec<JournalRecord> {
        self.records.lock().await.clone()
    }
}

#[async_trait::async_trait]
impl CommandJournal for LiveJournal {
    async fn append(&self, record: JournalRecord) -> Result<(), JournalError> {
        self.inner.append(record.clone()).await?;
        self.records.lock().await.push(record);
        Ok(())
    }

    async fn history(&self, id: types::CommandId) -> Result<JournalScan, JournalError> {
        self.inner.history(id).await
    }
}

pub async fn live_server(handle: CapabilityHandle) -> LiveServer {
    live_server_with_modes(handle, LiveOpenMode::Succeed, false, 0, false, false, false).await
}

pub async fn live_adaptive_server(handle: CapabilityHandle) -> LiveServer {
    live_server_with_modes(handle, LiveOpenMode::Succeed, false, 0, false, false, true).await
}

pub async fn live_server_restarting_accessibility(handle: CapabilityHandle) -> LiveServer {
    live_server_with_modes(handle, LiveOpenMode::Succeed, false, 0, true, false, false).await
}

pub async fn live_server_failing_navigation_interface(handle: CapabilityHandle) -> LiveServer {
    live_server_with_modes(handle, LiveOpenMode::Succeed, false, 0, false, true, false).await
}

pub async fn live_server_blocking_open(handle: CapabilityHandle) -> LiveServer {
    live_server_with_modes(handle, LiveOpenMode::Block, false, 0, false, false, false).await
}

pub async fn live_server_failing_open(handle: CapabilityHandle) -> LiveServer {
    live_server_with_modes(handle, LiveOpenMode::Fail, false, 0, false, false, false).await
}

pub async fn live_server_failing_open_and_delete_once(handle: CapabilityHandle) -> LiveServer {
    live_server_with_modes(handle, LiveOpenMode::Fail, false, 1, false, false, false).await
}

pub async fn live_server_blocking_delete(handle: CapabilityHandle) -> LiveServer {
    live_server_with_modes(handle, LiveOpenMode::Succeed, true, 0, false, false, false).await
}

pub async fn live_server_failing_delete_once(handle: CapabilityHandle) -> LiveServer {
    live_server_with_modes(handle, LiveOpenMode::Succeed, false, 1, false, false, false).await
}

pub async fn live_server_blocking_failing_delete_once(handle: CapabilityHandle) -> LiveServer {
    live_server_with_modes(handle, LiveOpenMode::Succeed, true, 1, false, false, false).await
}

async fn live_server_with_modes(
    handle: CapabilityHandle,
    open_mode: LiveOpenMode,
    block_delete: bool,
    delete_failures: usize,
    restart_accessibility: bool,
    fail_navigation_interface: bool,
    adaptive_http: bool,
) -> LiveServer {
    let root = tempfile::tempdir().expect("harness tempdir");
    let journal = Arc::new(LiveJournal {
        inner: JsonlJournal::open(root.path().join("journal.jsonl"))
            .await
            .expect("harness journal opens"),
        records: tokio::sync::Mutex::new(Vec::new()),
    });
    let probe = Arc::new(LiveProbe::default());
    probe
        .delete_failures_remaining
        .store(delete_failures, Ordering::SeqCst);
    probe
        .navigation_interface_failures_remaining
        .store(usize::from(fail_navigation_interface), Ordering::SeqCst);
    let workers = Arc::new(WorkerPool::new(
        4,
        Arc::new(LiveFactory {
            probe: Arc::clone(&probe),
            open_mode,
            block_delete,
        }),
    ));
    let checkpoints = checkpoint_store::CheckpointStore::open(root.path().join("checkpoints"))
        .await
        .expect("harness checkpoint store opens");
    let downloads_dir = root.path().join("downloads");
    let pages = if adaptive_http {
        let network = network_engine::NetworkPolicy {
            allow_loopback: true,
            ..Default::default()
        };
        let adaptive = page_runtime::AdaptivePageEngine::new(
            network_engine::EligibilityPolicy::new(network.clone()),
            network_engine::DirectHttpExecutor::new(network.clone()),
            artifact_store::ArtifactStore::new(
                root.path().join("artifacts"),
                network.max_download_bytes,
                16_384,
            ),
            network,
        )
        .with_downloads_root(&downloads_dir);
        PageRuntime::new_adaptive(journal.clone(), workers.clone(), None, adaptive)
    } else {
        PageRuntime::new(journal.clone(), workers.clone())
    };
    let runtime = RuntimeService::with_recovery(
        SessionManager::new(workers.clone()),
        pages,
        RecoveryCoordinator::new(checkpoints),
    );
    let authenticated = Arc::new(AuthenticatedRuntime::new(runtime.clone(), handle.clone()));
    let interface: Arc<dyn RuntimeInterface> = if restart_accessibility || fail_navigation_interface
    {
        probe
            .accessibility_restarts_remaining
            .store(usize::from(restart_accessibility), Ordering::SeqCst);
        Arc::new(FaultInjectingRuntime {
            inner: authenticated,
            probe: Arc::clone(&probe),
        })
    } else {
        authenticated
    };
    let server = Arc::new(Server::for_interface(
        interface,
        handle.clone(),
        EventStore::new(16_384),
        ArtifactResources::default(),
    ));
    LiveServer {
        server,
        journal,
        probe,
        runtime,
        handle,
        downloads_dir,
        _root: root,
    }
}

pub async fn initialize(server: &Server) {
    server
        .handle_message(serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-11-25","capabilities":{},
                      "clientInfo":{"name":"harness","version":"1"}}
        }))
        .await;
    server
        .handle_message(serde_json::json!({
            "jsonrpc":"2.0","method":"notifications/initialized","params":{}
        }))
        .await;
}

pub fn request(id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}

/// Create a session and open a page through the real runtime; both ids are
/// owned by the caller's principal and safe to put in envelopes.
pub async fn create_session_and_page(server: &Server, next_id: &mut u64) -> (SessionId, PageId) {
    *next_id += 1;
    let created = server
        .handle_message(request(
            *next_id,
            "tools/call",
            serde_json::json!({"name":"session_create","arguments":{"profile":"harness"}}),
        ))
        .await
        .expect("session_create answered");
    let session_id = created["result"]["structuredContent"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();
    *next_id += 1;
    let opened = server
        .handle_message(request(
            *next_id,
            "tools/call",
            serde_json::json!({
                "name":"page_open",
                "arguments":{"sessionId":session_id}
            }),
        ))
        .await
        .expect("page_open answered");
    let page_id = opened["result"]["structuredContent"]["id"]
        .as_str()
        .expect("page id")
        .to_owned();
    (
        SessionId(uuid::Uuid::parse_str(&session_id).expect("session uuid")),
        PageId(uuid::Uuid::parse_str(&page_id).expect("page uuid")),
    )
}

pub async fn execute_envelope(
    server: &Server,
    next_id: &mut u64,
    envelope: &types::CommandEnvelope,
) -> serde_json::Value {
    *next_id += 1;
    server
        .handle_message(request(
            *next_id,
            "tools/call",
            serde_json::json!({
                "name":"command_execute",
                "arguments":{"envelope":envelope}
            }),
        ))
        .await
        .expect("command_execute answered")
}

/// The harness DOM has no candidates, so a target-resolving intent must fail
/// with the engine's own deterministic domain outcome -- proof the envelope
/// executed through the real runtime rather than merely passing validation.
pub fn assert_intent_domain_failure(
    response: &serde_json::Value,
    envelope: &types::CommandEnvelope,
    intent_kind: &str,
) {
    let content = &response["result"]["structuredContent"];
    assert!(response["error"].is_null(), "{response}");
    assert_eq!(
        content["commandId"],
        envelope.command_id.0.to_string(),
        "{response}"
    );
    assert_eq!(content["status"], "failed", "{response}");
    assert_eq!(response["result"]["isError"], true, "{response}");
    let record = content["evidence"]
        .as_array()
        .expect("evidence is an array")
        .iter()
        .find(|item| item["kind"] == "intentExecution")
        .unwrap_or_else(|| panic!("no intentExecution evidence: {response}"))["record"]
        .clone();
    assert_eq!(record["intentKind"], intent_kind, "{response}");
    assert_eq!(record["resolutionPath"], "deterministic", "{response}");
    assert_eq!(record["candidates"], serde_json::json!([]), "{response}");
    let verification = record["verification"].as_str().unwrap_or_default();
    assert!(
        verification.starts_with("target"),
        "expected a target-resolution failure, got {verification}: {response}"
    );
}
