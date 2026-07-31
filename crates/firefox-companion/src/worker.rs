use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc, Mutex as TaskMutex, Weak,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use artifact_store::ArtifactStore;
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use companion_core::{
    AttachmentLease, CompanionServerHandle, CompanionSessionError, PageBindingTicket,
};
use companion_protocol::{
    ActionRequest, BrowserEngine, CompanionEvent, InteractionPath, PROTOCOL_VERSION,
};
use dom_engine::{
    resolve_candidates, Candidate, CandidateState, ResolutionDecision, ResolutionPolicy,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::{
    runtime::Handle,
    sync::{watch, Mutex as AsyncMutex, RwLock},
    task::JoinHandle,
};
use types::{
    CaptureScreenshotCommand, ClickAndWaitForDownloadCommand, ClickAndWaitForPopupCommand,
    ClickCommand, ClosePageCommand, CommandError, CommandId, ErrorCode, ErrorLayer, Evidence,
    InspectCommand, NavigateCommand, OpenPageCommand, PageId, ScreenshotMode, SessionId, TextMatch,
    TypeTextCommand, UploadFilesCommand, WaitCondition, WaitForCommand, WaitUntil, WorkerId,
};
use url::Url;
use worker_pool::{resolve_upload_paths, BrowserWorker, WorkerFactory};

use crate::bidi::{BidiClient, BidiEvent, BidiTransport, SharedBiDiTransport};

const COMPANION_SANDBOX: &str = "automation-runtime-companion";
const DEFAULT_NAVIGATION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TYPE_CODEPOINTS: usize = 4_096;
const MAX_OBSERVATION_BYTES: usize = 1024 * 1024 - 64 * 1024;
const MAX_SCREENSHOT_BYTES: usize = 16 * 1024 * 1024;
const MAX_VISIBLE_TEXT_BYTES: usize = 64 * 1024;
const MAX_SANITIZED_HTML_BYTES: usize = 128 * 1024;
const MAX_CONTROL_COUNT: usize = 512;
const MAX_SELECTOR_BYTES: usize = 512;
const MAX_CONTROL_FIELD_BYTES: usize = 256;
const MAX_URL_BYTES: usize = 2 * 1024;
const MAX_TITLE_BYTES: usize = 1024;
const PAGE_BINDING_TITLE_PREFIX: &str = "automation-runtime-binding:";
pub const MAX_TRACKED_PAGES: usize = 256;
const PAGE_BINDING_RELEASE_ATTEMPTS: usize = 3;
const MAX_FRAME_PATH_DEPTH: usize = 8;
const MAX_UPLOAD_FILES: usize = 32;
const MAX_UPLOAD_BYTES: u64 = 256 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionObservation {
    pub url: String,
    pub title: String,
    pub visible_text: String,
    pub controls: Vec<ExtensionControl>,
    #[serde(default)]
    pub html: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExtensionAccessibilitySnapshot {
    nodes: Vec<types::AccessibilityNode>,
    truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionControl {
    pub css_path: String,
    #[serde(default)]
    pub test_id: Option<String>,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
    #[serde(default)]
    pub attributes: std::collections::BTreeMap<String, String>,
    pub disabled: bool,
}

#[async_trait]
pub trait ExtensionPageBinding: Send {
    fn nonce(&self) -> &str;

    async fn complete(self: Box<Self>) -> Result<(), CommandError>;
}

#[async_trait]
pub trait ExtensionObserver: Send + Sync {
    fn operation_timeout(&self) -> Duration {
        DEFAULT_NAVIGATION_TIMEOUT
    }

    async fn begin_page_binding(
        &self,
        lease: &AttachmentLease,
        page_id: &PageId,
    ) -> Result<Box<dyn ExtensionPageBinding>, CommandError>;

    async fn observe(
        &self,
        lease: &AttachmentLease,
        page_id: &PageId,
        command: &InspectCommand,
    ) -> Result<ExtensionObservation, CommandError>;

    async fn release_page_binding(
        &self,
        lease: &AttachmentLease,
        page_id: &PageId,
    ) -> Result<(), CommandError>;

    /// Capture a compact accessibility tree for the page. Returns the tree
    /// and whether it was truncated to the node bound.
    async fn a11y_snapshot(
        &self,
        lease: &AttachmentLease,
        page_id: &PageId,
        max_nodes: u32,
    ) -> Result<(Vec<types::AccessibilityNode>, bool), CommandError> {
        let _ = (lease, page_id, max_nodes);
        Err(driver_error(
            ErrorCode::BrowserCommandFailed,
            "accessibility snapshot is not supported by this observer",
            false,
        ))
    }

    /// Renew a live attachment lease, returning the extended lease. Observers
    /// that cannot renew leave leases to expire at their original TTL.
    async fn renew_lease(&self, lease: &AttachmentLease) -> Result<AttachmentLease, CommandError> {
        let _ = lease;
        Err(driver_error(
            ErrorCode::BrowserCommandFailed,
            "attachment lease renewal is not supported by this observer",
            false,
        ))
    }
}

struct CompanionPageBinding {
    ticket: PageBindingTicket,
    expected_page_id: PageId,
    timeout: Duration,
}

#[async_trait]
impl ExtensionPageBinding for CompanionPageBinding {
    fn nonce(&self) -> &str {
        self.ticket.binding_nonce()
    }

    async fn complete(self: Box<Self>) -> Result<(), CommandError> {
        let grant = self
            .ticket
            .complete(self.timeout)
            .await
            .map_err(session_error)?;
        if !grant
            .pages
            .iter()
            .any(|page| page.page_id == self.expected_page_id)
        {
            return Err(driver_error(
                ErrorCode::BrowserCommandFailed,
                "page binding grant omitted the expected page ID",
                false,
            ));
        }
        Ok(())
    }
}

pub struct CompanionExtensionObserver {
    server: Arc<CompanionServerHandle>,
    timeout: Duration,
}

impl CompanionExtensionObserver {
    pub fn new(server: Arc<CompanionServerHandle>, timeout: Duration) -> Self {
        Self { server, timeout }
    }
}

#[async_trait]
impl ExtensionObserver for CompanionExtensionObserver {
    fn operation_timeout(&self) -> Duration {
        self.timeout
    }

    async fn renew_lease(&self, lease: &AttachmentLease) -> Result<AttachmentLease, CommandError> {
        let grant = self
            .server
            .renew_grant(&lease.attachment_id)
            .await
            .map_err(session_error)?;
        self.server
            .registry()
            .resolve_attachment(&grant.attachment_id)
            .await
            .map_err(|error| driver_error(ErrorCode::BrowserCommandFailed, error.to_string(), true))
    }

    async fn begin_page_binding(
        &self,
        lease: &AttachmentLease,
        page_id: &PageId,
    ) -> Result<Box<dyn ExtensionPageBinding>, CommandError> {
        if lease.expires_at <= Instant::now() {
            return Err(lease_error());
        }
        let ticket = self
            .server
            .begin_page_binding(&lease.attachment_id, page_id.clone())
            .await
            .map_err(session_error)?;
        Ok(Box::new(CompanionPageBinding {
            ticket,
            expected_page_id: page_id.clone(),
            timeout: self.timeout,
        }))
    }

    async fn observe(
        &self,
        lease: &AttachmentLease,
        page_id: &PageId,
        command: &InspectCommand,
    ) -> Result<ExtensionObservation, CommandError> {
        if lease.expires_at <= Instant::now() {
            return Err(lease_error());
        }
        let command_id = CommandId::new();
        let action = ActionRequest {
            protocol_version: PROTOCOL_VERSION,
            attachment_id: lease.attachment_id.clone(),
            command_id: command_id.clone(),
            page_id: page_id.clone(),
            operation: "observe".into(),
            input: json!({
                "selector": command.selector,
                "target": command.target,
                "includeHtml": command.include_html,
            }),
            deadline_unix_ms: deadline_unix_ms(self.timeout),
        };
        match self
            .server
            .dispatch_action(action)
            .await
            .map_err(session_error)?
        {
            CompanionEvent::ActionCompleted(result) => {
                if result.command_id != command_id
                    || result.interaction_path != InteractionPath::ExtensionApi
                {
                    return Err(driver_error(
                        ErrorCode::BrowserCommandFailed,
                        "extension observation returned an invalid execution path",
                        false,
                    ));
                }
                let observation: ExtensionObservation = serde_json::from_value(result.output)
                    .map_err(|error| {
                        driver_error(
                            ErrorCode::BrowserCommandFailed,
                            format!("invalid extension observation: {error}"),
                            false,
                        )
                    })?;
                validate_observation(&observation)?;
                Ok(observation)
            }
            CompanionEvent::ActionFailed { code, message, .. } => Err(driver_error(
                if command.target.is_some() || command.selector.is_some() {
                    ErrorCode::TargetNotFound
                } else {
                    ErrorCode::BrowserCommandFailed
                },
                format!("extension observation failed ({code}): {message}"),
                false,
            )),
            _ => Err(driver_error(
                ErrorCode::BrowserCommandFailed,
                "extension returned an unexpected observation event",
                false,
            )),
        }
    }

    async fn a11y_snapshot(
        &self,
        lease: &AttachmentLease,
        page_id: &PageId,
        max_nodes: u32,
    ) -> Result<(Vec<types::AccessibilityNode>, bool), CommandError> {
        if lease.expires_at <= Instant::now() {
            return Err(lease_error());
        }
        let command_id = CommandId::new();
        let action = ActionRequest {
            protocol_version: PROTOCOL_VERSION,
            attachment_id: lease.attachment_id.clone(),
            command_id: command_id.clone(),
            page_id: page_id.clone(),
            operation: "a11yTree".into(),
            input: json!({"maxNodes": max_nodes}),
            deadline_unix_ms: deadline_unix_ms(self.timeout),
        };
        match self
            .server
            .dispatch_action(action)
            .await
            .map_err(session_error)?
        {
            CompanionEvent::ActionCompleted(result)
                if result.command_id == command_id
                    && result.interaction_path == InteractionPath::ExtensionApi =>
            {
                let snapshot: ExtensionAccessibilitySnapshot =
                    serde_json::from_value(result.output).map_err(|error| {
                        driver_error(
                            ErrorCode::BrowserCommandFailed,
                            format!("invalid extension accessibility snapshot: {error}"),
                            false,
                        )
                    })?;
                if snapshot.nodes.len() > max_nodes as usize {
                    return Err(driver_error(
                        ErrorCode::BrowserCommandFailed,
                        "extension accessibility snapshot exceeded its node bound",
                        false,
                    ));
                }
                Ok((snapshot.nodes, snapshot.truncated))
            }
            CompanionEvent::ActionCompleted(_) => Err(driver_error(
                ErrorCode::BrowserCommandFailed,
                "extension accessibility snapshot returned an invalid execution path",
                false,
            )),
            CompanionEvent::ActionFailed { code, message, .. } => Err(driver_error(
                ErrorCode::BrowserCommandFailed,
                format!("extension accessibility snapshot failed ({code}): {message}"),
                false,
            )),
            _ => Err(driver_error(
                ErrorCode::BrowserCommandFailed,
                "extension returned an unexpected accessibility snapshot event",
                false,
            )),
        }
    }

    async fn release_page_binding(
        &self,
        lease: &AttachmentLease,
        page_id: &PageId,
    ) -> Result<(), CommandError> {
        self.server
            .release_page_binding(&lease.attachment_id, page_id)
            .await
            .map_err(session_error)
    }
}

pub struct FirefoxCompanionFactory {
    bidi_url: Url,
    timeout: Duration,
    profile_dir: PathBuf,
    lease: AttachmentLease,
    observer: Arc<dyn ExtensionObserver>,
    artifacts: Option<ArtifactStore>,
    upload_roots: Vec<PathBuf>,
    downloads_dir: Option<PathBuf>,
    shared_transport: Option<BidiClient>,
}

impl FirefoxCompanionFactory {
    pub fn new(
        bidi_url: Url,
        timeout: Duration,
        profile_dir: PathBuf,
        lease: AttachmentLease,
        observer: Arc<dyn ExtensionObserver>,
    ) -> Self {
        Self {
            bidi_url,
            timeout,
            profile_dir,
            lease,
            observer,
            artifacts: None,
            upload_roots: Vec::new(),
            downloads_dir: None,
            shared_transport: None,
        }
    }

    /// Reuse an existing profile-wide BiDi session instead of opening one per
    /// worker. Firefox's RemoteAgent accepts exactly one active WebDriver
    /// session, so multi-session runtimes must share the connection.
    pub fn with_shared_transport(mut self, client: BidiClient) -> Self {
        self.shared_transport = Some(client);
        self
    }

    pub fn with_artifacts(mut self, artifacts: ArtifactStore) -> Self {
        self.artifacts = Some(artifacts);
        self
    }

    pub fn with_upload_roots(mut self, upload_roots: Vec<PathBuf>) -> Self {
        self.upload_roots = upload_roots;
        self
    }

    pub fn with_downloads_dir(mut self, downloads_dir: PathBuf) -> Self {
        self.downloads_dir = Some(downloads_dir);
        self
    }
}

#[async_trait]
impl WorkerFactory for FirefoxCompanionFactory {
    async fn launch(&self, session_id: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        validate_lease(&self.lease)?;
        let transport: Arc<dyn BidiTransport> = match &self.shared_transport {
            Some(client) => Arc::new(SharedBiDiTransport::new(client.clone())),
            None => {
                Arc::new(BidiClient::connect_session(self.bidi_url.clone(), self.timeout).await?)
            }
        };
        let mut worker = FirefoxCompanionWorker::new(
            WorkerId::new(),
            self.profile_dir.clone(),
            self.lease.clone(),
            transport,
            Arc::clone(&self.observer),
        )
        .await?;
        worker.session_id = Some(session_id.clone());
        worker.artifacts = self.artifacts.clone();
        worker.upload_roots = self.upload_roots.clone();
        worker.downloads_dir = self.downloads_dir.clone();
        Ok(Arc::new(worker))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PageContext {
    Opening(Option<String>),
    Ready { context: String, title: String },
    Releasing { context: Option<String> },
}

pub struct FirefoxCompanionWorker {
    id: WorkerId,
    profile_dir: PathBuf,
    lease: Arc<std::sync::RwLock<AttachmentLease>>,
    transport: Arc<dyn BidiTransport>,
    observer: Arc<dyn ExtensionObserver>,
    pages: Arc<RwLock<HashMap<PageId, PageContext>>>,
    page_cleanups: Arc<RwLock<HashMap<PageId, OpenPageCleanup>>>,
    closed: AtomicBool,
    lifecycle: AsyncMutex<()>,
    shutdown: Arc<WorkerShutdown>,
    cleanup_failure: Arc<TaskMutex<Option<CommandError>>>,
    cleanup_task: Arc<TaskMutex<Option<JoinHandle<()>>>>,
    renewal_task: Arc<TaskMutex<Option<JoinHandle<()>>>>,
    session_id: Option<SessionId>,
    artifacts: Option<ArtifactStore>,
    upload_roots: Vec<PathBuf>,
    downloads_dir: Option<PathBuf>,
}

struct WorkerShutdown {
    started: AtomicBool,
    result: watch::Sender<Option<Result<(), CommandError>>>,
    runtime: Handle,
}

#[derive(Clone)]
struct WorkerShutdownResources {
    transport: Arc<dyn BidiTransport>,
    observer: Arc<dyn ExtensionObserver>,
    pages: Arc<RwLock<HashMap<PageId, PageContext>>>,
    page_cleanups: Arc<RwLock<HashMap<PageId, OpenPageCleanup>>>,
    cleanup_failure: Arc<TaskMutex<Option<CommandError>>>,
    cleanup_task: Arc<TaskMutex<Option<JoinHandle<()>>>>,
    renewal_task: Arc<TaskMutex<Option<JoinHandle<()>>>>,
}

#[derive(Clone)]
struct PageOpenResources {
    lease: AttachmentLease,
    transport: Arc<dyn BidiTransport>,
    observer: Arc<dyn ExtensionObserver>,
    pages: Arc<RwLock<HashMap<PageId, PageContext>>>,
    page_cleanups: Weak<RwLock<HashMap<PageId, OpenPageCleanup>>>,
    cleanup_timeout: Duration,
}

struct CallerCancellation {
    cancelled: Arc<AtomicBool>,
    armed: bool,
}

impl CallerCancellation {
    fn new(cancelled: Arc<AtomicBool>) -> Self {
        Self {
            cancelled,
            armed: true,
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for CallerCancellation {
    fn drop(&mut self) {
        if self.armed {
            self.cancelled.store(true, Ordering::Release);
        }
    }
}

#[derive(Clone)]
struct OpenPageCleanup {
    resources: PageOpenResources,
    page_id: PageId,
    details: Arc<TaskMutex<OpenPageCleanupDetails>>,
    cancelled: Arc<AtomicBool>,
    execution: Arc<OpenPageCleanupExecution>,
}

#[derive(Clone, Default)]
struct OpenPageCleanupDetails {
    context: Option<String>,
    original_title: Option<String>,
    binding_started: bool,
    exposed: bool,
    opening_settled: bool,
}

struct OpenPageCleanupExecution {
    run: AsyncMutex<()>,
    stage: AtomicUsize,
    failures: TaskMutex<Vec<String>>,
}

const CLEANUP_RESTORE_DONE: usize = 1 << 0;
const CLEANUP_CLOSE_DONE: usize = 1 << 1;
const CLEANUP_MAPPING_DONE: usize = 1 << 2;
const CLEANUP_BINDING_DONE: usize = 1 << 3;
const CLEANUP_REGISTRY_DONE: usize = 1 << 4;
const CLEANUP_ACTIONS_DONE: usize =
    CLEANUP_RESTORE_DONE | CLEANUP_CLOSE_DONE | CLEANUP_MAPPING_DONE | CLEANUP_BINDING_DONE;

impl OpenPageCleanup {
    fn new(resources: PageOpenResources, page_id: PageId, cancelled: Arc<AtomicBool>) -> Self {
        Self {
            resources,
            page_id,
            details: Arc::new(TaskMutex::new(OpenPageCleanupDetails::default())),
            cancelled,
            execution: Arc::new(OpenPageCleanupExecution {
                run: AsyncMutex::new(()),
                stage: AtomicUsize::new(0),
                failures: TaskMutex::new(Vec::new()),
            }),
        }
    }

    async fn run(&self) -> Vec<String> {
        let _run = self.execution.run.lock().await;
        self.run_locked(true).await
    }

    async fn run_destroyed(&self) -> Vec<String> {
        let _run = self.execution.run.lock().await;
        if self.details().exposed {
            self.execution
                .stage
                .fetch_or(CLEANUP_RESTORE_DONE | CLEANUP_CLOSE_DONE, Ordering::AcqRel);
        }
        self.run_locked(false).await
    }

    async fn run_locked(&self, include_context: bool) -> Vec<String> {
        if include_context
            && self.execution.stage.load(Ordering::Acquire) & CLEANUP_RESTORE_DONE == 0
        {
            let details = self.details();
            if let (Some(context), Some(title)) = (details.context, details.original_title) {
                let restored = tokio::time::timeout(
                    self.resources.cleanup_timeout,
                    restore_context_title(&self.resources.transport, &context, &title),
                )
                .await;
                if let Err(error) = restored.unwrap_or_else(|_| {
                    Err(cleanup_deadline_error("restoring the original page title"))
                }) {
                    self.record_failure(format!(
                        "restoring the original page title: {}",
                        error.message
                    ));
                }
            }
            self.execution
                .stage
                .fetch_or(CLEANUP_RESTORE_DONE, Ordering::AcqRel);
        }
        if include_context && self.execution.stage.load(Ordering::Acquire) & CLEANUP_CLOSE_DONE == 0
        {
            if let Some(context) = self.details().context {
                let closed = tokio::time::timeout(
                    self.resources.cleanup_timeout,
                    self.resources
                        .transport
                        .send("browsingContext.close", json!({"context": context})),
                )
                .await;
                if let Err(error) = closed
                    .unwrap_or_else(|_| Err(cleanup_deadline_error("closing the Firefox context")))
                {
                    self.record_failure(format!("closing the Firefox context: {}", error.message));
                }
            }
            self.execution
                .stage
                .fetch_or(CLEANUP_CLOSE_DONE, Ordering::AcqRel);
        }
        if self.execution.stage.load(Ordering::Acquire) & CLEANUP_MAPPING_DONE == 0 {
            let context = self.details().context;
            remove_page_mapping(&self.resources.pages, &self.page_id, context.as_deref()).await;
            self.execution
                .stage
                .fetch_or(CLEANUP_MAPPING_DONE, Ordering::AcqRel);
        }
        if self.execution.stage.load(Ordering::Acquire) & CLEANUP_BINDING_DONE == 0 {
            if self.details().binding_started {
                if let Err(error) = release_page_binding_with_retries(
                    &self.resources.observer,
                    &self.resources.lease,
                    &self.page_id,
                )
                .await
                {
                    self.record_failure(format!(
                        "releasing the companion page binding: {}",
                        error.message
                    ));
                }
            }
            self.execution
                .stage
                .fetch_or(CLEANUP_BINDING_DONE, Ordering::AcqRel);
        }
        let progress = self.execution.stage.load(Ordering::Acquire);
        if progress & CLEANUP_ACTIONS_DONE == CLEANUP_ACTIONS_DONE
            && progress & CLEANUP_REGISTRY_DONE == 0
            && self.details().opening_settled
        {
            if let Some(cleanups) = self.resources.page_cleanups.upgrade() {
                cleanups.write().await.remove(&self.page_id);
            }
            self.execution
                .stage
                .fetch_or(CLEANUP_REGISTRY_DONE, Ordering::AcqRel);
        }
        self.failures()
    }

    async fn binding_started(&self) {
        let _run = self.execution.run.lock().await;
        self.details
            .lock()
            .expect("open-page cleanup details mutex poisoned")
            .binding_started = true;
        self.execution
            .stage
            .fetch_and(!CLEANUP_BINDING_DONE, Ordering::AcqRel);
    }

    async fn context_created(&self, context: String) {
        let _run = self.execution.run.lock().await;
        self.details
            .lock()
            .expect("open-page cleanup details mutex poisoned")
            .context = Some(context);
        self.execution.stage.fetch_and(
            !(CLEANUP_RESTORE_DONE | CLEANUP_CLOSE_DONE),
            Ordering::AcqRel,
        );
    }

    async fn title_captured(&self, title: String) {
        let _run = self.execution.run.lock().await;
        self.details
            .lock()
            .expect("open-page cleanup details mutex poisoned")
            .original_title = Some(title);
        self.execution
            .stage
            .fetch_and(!CLEANUP_RESTORE_DONE, Ordering::AcqRel);
    }

    async fn commit_exposure(&self) -> Result<(), Vec<String>> {
        let _run = self.execution.run.lock().await;
        let details = self.details();
        let ready = self
            .resources
            .pages
            .read()
            .await
            .get(&self.page_id)
            .is_some_and(|mapped| {
                matches!(
                    (mapped, details.context.as_deref()),
                    (PageContext::Ready { context, .. }, Some(expected)) if context == expected
                )
            });
        if ready {
            let mut details = self
                .details
                .lock()
                .expect("open-page cleanup details mutex poisoned");
            details.exposed = true;
            details.opening_settled = true;
            return Ok(());
        }

        self.settle_opening();
        Err(self.run_locked(true).await)
    }

    fn settle_opening(&self) {
        self.details
            .lock()
            .expect("open-page cleanup details mutex poisoned")
            .opening_settled = true;
    }

    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    fn details(&self) -> OpenPageCleanupDetails {
        self.details
            .lock()
            .expect("open-page cleanup details mutex poisoned")
            .clone()
    }

    fn record_failure(&self, failure: String) {
        self.execution
            .failures
            .lock()
            .expect("open-page cleanup failure mutex poisoned")
            .push(failure);
    }

    fn failures(&self) -> Vec<String> {
        self.execution
            .failures
            .lock()
            .expect("open-page cleanup failure mutex poisoned")
            .clone()
    }
}

struct OpenPageGuard {
    cleanup: Option<OpenPageCleanup>,
}

impl OpenPageGuard {
    fn new(cleanup: OpenPageCleanup) -> Self {
        Self {
            cleanup: Some(cleanup),
        }
    }

    async fn binding_started(&mut self) {
        self.cleanup
            .as_ref()
            .expect("open-page cleanup is armed")
            .binding_started()
            .await;
    }

    async fn context_created(&mut self, context: String) {
        self.cleanup
            .as_ref()
            .expect("open-page cleanup is armed")
            .context_created(context)
            .await;
    }

    async fn title_captured(&mut self, title: String) {
        self.cleanup
            .as_ref()
            .expect("open-page cleanup is armed")
            .title_captured(title)
            .await;
    }

    async fn fail(mut self, mut primary: CommandError) -> CommandError {
        if let Some(cleanup) = self.cleanup.as_ref() {
            cleanup.settle_opening();
            let failures = cleanup.run().await;
            self.cleanup = None;
            if !failures.is_empty() {
                primary.message = format!(
                    "{}; cleanup failed while {}",
                    primary.message,
                    failures.join("; ")
                );
            }
        }
        primary
    }

    async fn disarm(mut self) -> Result<(), CommandError> {
        let cleanup = self.cleanup.as_ref().expect("open-page cleanup is armed");
        match cleanup.commit_exposure().await {
            Ok(()) => {
                self.cleanup = None;
                Ok(())
            }
            Err(failures) => {
                self.cleanup = None;
                let mut error = driver_error(
                    ErrorCode::BrowserCommandFailed,
                    "Firefox page was destroyed before exposure commit",
                    true,
                );
                if !failures.is_empty() {
                    error.message = format!(
                        "{}; cleanup failed while {}",
                        error.message,
                        failures.join("; ")
                    );
                }
                Err(error)
            }
        }
    }

    fn settle_opening(&self) {
        self.cleanup
            .as_ref()
            .expect("open-page cleanup is armed")
            .settle_opening();
    }
}

impl Drop for OpenPageGuard {
    fn drop(&mut self) {
        let Some(cleanup) = self.cleanup.as_ref().cloned() else {
            return;
        };
        cleanup.settle_opening();
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        runtime.spawn(async move {
            let _ = cleanup.run().await;
        });
    }
}

struct PageOpenOperation {
    resources: PageOpenResources,
    page_id: PageId,
    cancelled: Arc<AtomicBool>,
    guard: Option<OpenPageGuard>,
}

impl FirefoxCompanionWorker {
    pub fn with_upload_roots(mut self, upload_roots: Vec<PathBuf>) -> Self {
        self.upload_roots = upload_roots;
        self
    }

    pub fn with_downloads_dir(mut self, downloads_dir: PathBuf) -> Self {
        self.downloads_dir = Some(downloads_dir);
        self
    }

    pub fn with_runtime_storage(
        mut self,
        session_id: SessionId,
        artifacts: ArtifactStore,
        downloads_dir: PathBuf,
    ) -> Self {
        self.session_id = Some(session_id);
        self.artifacts = Some(artifacts);
        self.downloads_dir = Some(downloads_dir);
        self
    }

    pub async fn new(
        id: WorkerId,
        profile_dir: PathBuf,
        lease: AttachmentLease,
        transport: Arc<dyn BidiTransport>,
        observer: Arc<dyn ExtensionObserver>,
    ) -> Result<Self, CommandError> {
        validate_lease(&lease)?;
        let mut events = transport.subscribe_events().ok_or_else(|| {
            driver_error(
                ErrorCode::BrowserCommandFailed,
                "Firefox BiDi transport cannot receive subscribed context events",
                false,
            )
        })?;
        let subscription = transport
            .send(
                "session.subscribe",
                json!({"events": ["browsingContext.contextCreated", "browsingContext.contextDestroyed", "browsingContext.downloadWillBegin", "browsingContext.downloadEnd"]}),
            )
            .await?;
        if !subscription.is_object() {
            return Err(driver_error(
                ErrorCode::BrowserCommandFailed,
                "Firefox BiDi session.subscribe result was not an object",
                false,
            ));
        }
        let pages = Arc::new(RwLock::new(HashMap::<PageId, PageContext>::new()));
        let page_cleanups = Arc::new(RwLock::new(HashMap::<PageId, OpenPageCleanup>::new()));
        let cleanup_pages = Arc::clone(&pages);
        let cleanup_registry = Arc::clone(&page_cleanups);
        let cleanup_transport = Arc::clone(&transport);
        let cleanup_failure = Arc::new(TaskMutex::new(None));
        let task_failure = Arc::clone(&cleanup_failure);
        let cleanup_task = Arc::new(TaskMutex::new(Some(tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) if event.method == "browsingContext.contextDestroyed" => {
                        if let Some(context) = event.params.get("context").and_then(Value::as_str) {
                            let removals =
                                mark_destroyed_context(&cleanup_pages, &cleanup_registry, context)
                                    .await;
                            release_removed_pages(&task_failure, removals).await;
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        reconcile_contexts(
                            &cleanup_transport,
                            &cleanup_pages,
                            &cleanup_registry,
                            &task_failure,
                        )
                        .await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        let removals = mark_all_contexts(&cleanup_pages, &cleanup_registry).await;
                        release_removed_pages(&task_failure, removals).await;
                        break;
                    }
                }
            }
        }))));
        let (shutdown_result, _) = watch::channel(None);
        let lease = Arc::new(std::sync::RwLock::new(lease));
        let renewal_task = Arc::new(TaskMutex::new(Some(tokio::spawn(renew_lease_task(
            Arc::clone(&lease),
            Arc::clone(&observer),
        )))));
        Ok(Self {
            id,
            profile_dir,
            lease,
            transport,
            observer,
            pages,
            page_cleanups,
            closed: AtomicBool::new(false),
            lifecycle: AsyncMutex::new(()),
            shutdown: Arc::new(WorkerShutdown {
                started: AtomicBool::new(false),
                result: shutdown_result,
                runtime: Handle::current(),
            }),
            cleanup_failure,
            cleanup_task,
            renewal_task,
            session_id: None,
            artifacts: None,
            upload_roots: Vec::new(),
            downloads_dir: None,
        })
    }

    fn start_shutdown(&self) {
        if self.shutdown.started.swap(true, Ordering::AcqRel) {
            return;
        }

        self.closed.store(true, Ordering::Release);
        let resources = WorkerShutdownResources {
            transport: Arc::clone(&self.transport),
            observer: Arc::clone(&self.observer),
            pages: Arc::clone(&self.pages),
            page_cleanups: Arc::clone(&self.page_cleanups),
            cleanup_failure: Arc::clone(&self.cleanup_failure),
            cleanup_task: Arc::clone(&self.cleanup_task),
            renewal_task: Arc::clone(&self.renewal_task),
        };
        let shutdown = Arc::clone(&self.shutdown);
        let runtime = shutdown.runtime.clone();
        runtime.spawn(async move {
            let result = run_worker_shutdown(resources).await;
            shutdown.result.send_replace(Some(result));
        });
    }

    async fn wait_for_shutdown(&self) -> Result<(), CommandError> {
        let mut result = self.shutdown.result.subscribe();
        loop {
            if let Some(result) = result.borrow().clone() {
                return result;
            }
            result.changed().await.map_err(|_| {
                driver_error(
                    ErrorCode::BrowserCommandFailed,
                    "Firefox worker shutdown ended without a result",
                    false,
                )
            })?;
        }
    }

    async fn context(&self, page_id: &PageId) -> Result<String, CommandError> {
        self.ensure_active()?;
        self.pages
            .read()
            .await
            .get(page_id)
            .and_then(|context| match context {
                PageContext::Ready { context, .. } => Some(context.clone()),
                PageContext::Opening(_) | PageContext::Releasing { .. } => None,
            })
            .ok_or_else(page_missing)
    }

    async fn page_title(&self, page_id: &PageId) -> Result<String, CommandError> {
        self.pages
            .read()
            .await
            .get(page_id)
            .and_then(|context| match context {
                PageContext::Ready { title, .. } => Some(title.clone()),
                PageContext::Opening(_) | PageContext::Releasing { .. } => None,
            })
            .ok_or_else(page_missing)
    }

    fn ensure_active(&self) -> Result<(), CommandError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(driver_error(
                ErrorCode::BrowserCommandFailed,
                "Firefox companion worker is closed",
                false,
            ));
        }
        if let Some(error) = self
            .cleanup_failure
            .lock()
            .expect("cleanup failure mutex poisoned")
            .clone()
        {
            return Err(error);
        }
        if self.current_lease().expires_at <= Instant::now() {
            return Err(lease_error());
        }
        Ok(())
    }

    fn current_lease(&self) -> AttachmentLease {
        self.lease
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }

    fn evidence(&self, interaction_path: InteractionPath) -> Evidence {
        let lease = self.current_lease();
        Evidence::BrowserExecution {
            engine: engine_name(&lease.identity.engine).into(),
            browser_version: lease.identity.browser_version.clone(),
            profile_id: lease.profile_id.0.to_string(),
            interaction_path: interaction_path_name(interaction_path).into(),
        }
    }

    async fn reserve_page(
        &self,
        page_id: &PageId,
        cleanup: OpenPageCleanup,
    ) -> Result<(), CommandError> {
        let _lifecycle = self.lifecycle.lock().await;
        self.ensure_active()?;
        let mut pages = self.pages.write().await;
        if pages.contains_key(page_id) {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "page ID already has a Firefox browsing context",
                false,
            ));
        }
        if pages.len() >= MAX_TRACKED_PAGES {
            return Err(driver_error(
                ErrorCode::ResourceExhausted,
                "Firefox companion page-context capacity is exhausted",
                true,
            ));
        }
        pages.insert(page_id.clone(), PageContext::Opening(None));
        self.page_cleanups
            .write()
            .await
            .insert(page_id.clone(), cleanup);
        Ok(())
    }

    async fn open_page_owned(&self, page_id: PageId) -> Result<OpenPageGuard, CommandError> {
        self.ensure_active()?;
        if !self.current_lease().capabilities.tabs {
            return Err(capability_error("tab creation"));
        }
        let resources = PageOpenResources {
            lease: self.current_lease(),
            transport: Arc::clone(&self.transport),
            observer: Arc::clone(&self.observer),
            pages: Arc::clone(&self.pages),
            page_cleanups: Arc::downgrade(&self.page_cleanups),
            cleanup_timeout: self.observer.operation_timeout(),
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let cleanup =
            OpenPageCleanup::new(resources.clone(), page_id.clone(), Arc::clone(&cancelled));
        self.reserve_page(&page_id, cleanup.clone()).await?;
        let guard = OpenPageGuard::new(cleanup);
        let mut caller_cancellation = CallerCancellation::new(Arc::clone(&cancelled));
        let operation = tokio::spawn(
            PageOpenOperation {
                resources,
                page_id,
                cancelled,
                guard: Some(guard),
            }
            .run(),
        );
        let result = match operation.await {
            Ok(result) => result,
            Err(error) => Err(driver_error(
                ErrorCode::BrowserCommandFailed,
                format!("Firefox page-opening task failed: {error}"),
                true,
            )),
        };
        caller_cancellation.disarm();
        result
    }

    async fn resolve_element(
        &self,
        context: &str,
        selector: &str,
        target_scoped: bool,
    ) -> Result<String, CommandError> {
        if selector.is_empty() {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "native input requires a CSS selector",
                false,
            ));
        }
        let selector = serde_json::to_string(selector).map_err(|error| {
            driver_error(
                ErrorCode::InvalidRequest,
                format!("invalid native input selector: {error}"),
                false,
            )
        })?;
        let response = self
            .transport
            .send(
                "script.evaluate",
                json!({
                    "expression": format!("document.querySelector({selector})"),
                    "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                    "awaitPromise": false,
                    "resultOwnership": "none",
                }),
            )
            .await?;
        response
            .pointer("/result/sharedId")
            .or_else(|| response.get("sharedId"))
            .and_then(Value::as_str)
            .filter(|shared_id| !shared_id.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| {
                driver_error(
                    if target_scoped {
                        ErrorCode::TargetNotFound
                    } else {
                        ErrorCode::NotFound
                    },
                    "native input target was not found",
                    false,
                )
            })
    }

    async fn resolve_input_target(
        &self,
        page_id: &PageId,
        top_context: &str,
        selector: &str,
        target: Option<&types::TargetSpec>,
    ) -> Result<(String, String), CommandError> {
        let Some(target) = target else {
            return Ok((top_context.to_owned(), selector.to_owned()));
        };
        if target.frame_path.len() > MAX_FRAME_PATH_DEPTH {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                format!("Firefox frame path exceeds {MAX_FRAME_PATH_DEPTH} segments"),
                false,
            ));
        }
        let mut context = top_context.to_owned();
        for frame in &target.frame_path {
            let frame_selector = direct_target_selector(frame).ok_or_else(|| {
                driver_error(
                    ErrorCode::FrameNotFound,
                    "Firefox frame path requires an exact CSS or test-id segment",
                    false,
                )
            })?;
            context = self
                .descend_frame_context(&context, &frame_selector)
                .await?;
        }
        if let Some(selector) = direct_target_selector(target) {
            return Ok((context, selector));
        }
        let observation = self
            .observer
            .observe(
                &self.current_lease(),
                page_id,
                &InspectCommand {
                    selector: None,
                    target: None,
                    include_html: false,
                },
            )
            .await?;
        validate_observation(&observation)?;
        let candidates = observation
            .controls
            .into_iter()
            .enumerate()
            .map(|(index, control)| {
                let text = control
                    .name
                    .clone()
                    .or_else(|| control.label.clone())
                    .or_else(|| control.value.clone())
                    .unwrap_or_default();
                Candidate {
                    id: format!("control-{index}"),
                    css: Some(control.css_path),
                    test_id: control.test_id,
                    role: control.role,
                    name: control.name,
                    label: control.label,
                    text,
                    attributes: control.attributes,
                    state: CandidateState {
                        attached: true,
                        visible: true,
                        enabled: !control.disabled,
                    },
                }
            })
            .collect::<Vec<_>>();
        match resolve_candidates(target, &candidates, &ResolutionPolicy::default()) {
            Ok(ResolutionDecision::Resolved { candidate, .. }) => candidate
                .css
                .map(|selector| (context, selector))
                .ok_or_else(|| {
                    driver_error(
                        ErrorCode::TargetDetached,
                        "resolved Firefox target has no CSS identity",
                        false,
                    )
                }),
            Ok(ResolutionDecision::Ambiguous { .. }) => Err(driver_error(
                ErrorCode::TargetAmbiguous,
                "Firefox semantic target is ambiguous",
                false,
            )),
            Ok(ResolutionDecision::NotFound) => Err(driver_error(
                ErrorCode::TargetNotFound,
                "Firefox semantic target was not found",
                false,
            )),
            Err(error) => Err(driver_error(
                ErrorCode::InvalidRequest,
                error.to_string(),
                false,
            )),
        }
    }

    async fn descend_frame_context(
        &self,
        context: &str,
        selector: &str,
    ) -> Result<String, CommandError> {
        let selector_json = serde_json::to_string(selector)
            .map_err(|error| driver_error(ErrorCode::InvalidRequest, error.to_string(), false))?;
        let probe = self.transport.send("script.evaluate", json!({
            "expression": format!("(()=>{{const matches=[...document.querySelectorAll({selector_json})];if(matches.length===0)return 'missing';if(matches.length!==1)return 'ambiguous';const frame=matches[0];if(!(frame instanceof HTMLIFrameElement||frame instanceof HTMLFrameElement))return 'non-frame';return `index:${{[...document.querySelectorAll('iframe,frame')].indexOf(frame)}}`;}})()"),
            "target": {"context": context, "sandbox": COMPANION_SANDBOX},
            "awaitPromise": false,
            "resultOwnership": "none",
        })).await?;
        let result = probe
            .pointer("/result/value")
            .and_then(Value::as_str)
            .unwrap_or("");
        let index = match result {
            "missing" => {
                return Err(driver_error(
                    ErrorCode::FrameNotFound,
                    "Firefox frame target was not found",
                    false,
                ))
            }
            "ambiguous" => {
                return Err(driver_error(
                    ErrorCode::TargetAmbiguous,
                    "Firefox frame target is ambiguous",
                    false,
                ))
            }
            "non-frame" => {
                return Err(driver_error(
                    ErrorCode::FrameNotFound,
                    "Firefox frame target resolved to a non-frame element",
                    false,
                ))
            }
            value => value
                .strip_prefix("index:")
                .and_then(|value| value.parse::<usize>().ok())
                .ok_or_else(|| {
                    driver_error(
                        ErrorCode::BrowserCommandFailed,
                        "Firefox frame probe returned an invalid result",
                        false,
                    )
                })?,
        };
        let tree = self
            .transport
            .send(
                "browsingContext.getTree",
                json!({"root": context, "maxDepth": 1}),
            )
            .await?;
        tree.pointer("/contexts/0/children")
            .and_then(Value::as_array)
            .and_then(|children| children.get(index))
            .and_then(|child| child.get("context"))
            .and_then(Value::as_str)
            .map(str::to_owned)
            .ok_or_else(|| {
                driver_error(
                    ErrorCode::FrameNotFound,
                    "Firefox frame has no matching child browsing context",
                    false,
                )
            })
    }

    async fn resolve_shadow_element(
        &self,
        context: &str,
        target: &types::TargetSpec,
    ) -> Result<String, CommandError> {
        if target.shadow_path.len() > MAX_FRAME_PATH_DEPTH {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                format!("Firefox shadow path exceeds {MAX_FRAME_PATH_DEPTH} segments"),
                false,
            ));
        }
        let hosts = target
            .shadow_path
            .iter()
            .map(|host| direct_target_selector(host))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| {
                driver_error(
                    ErrorCode::ShadowRootUnavailable,
                    "Firefox shadow path requires exact CSS or test-id hosts",
                    false,
                )
            })?;
        let target_selector = direct_target_selector(target).ok_or_else(|| {
            driver_error(
                ErrorCode::TargetNotFound,
                "Firefox shadow target requires exact CSS or test-id identity",
                false,
            )
        })?;
        let hosts_json = serde_json::to_string(&hosts)
            .map_err(|error| driver_error(ErrorCode::InvalidRequest, error.to_string(), false))?;
        let target_json = serde_json::to_string(&target_selector)
            .map_err(|error| driver_error(ErrorCode::InvalidRequest, error.to_string(), false))?;
        let response = self.transport.send("script.evaluate", json!({
            "expression": format!("(()=>{{let root=document;for(const selector of {hosts_json}){{const matches=[...root.querySelectorAll(selector)];if(matches.length===0)return 'host-missing';if(matches.length!==1)return 'host-ambiguous';if(!matches[0].shadowRoot)return 'shadow-unavailable';root=matches[0].shadowRoot;}}const matches=[...root.querySelectorAll({target_json})];if(matches.length===0)return 'target-missing';if(matches.length!==1)return 'target-ambiguous';return matches[0];}})()"),
            "target": {"context": context, "sandbox": COMPANION_SANDBOX},
            "awaitPromise": false,
            "resultOwnership": "none",
        })).await?;
        if let Some(shared_id) = response.pointer("/result/sharedId").and_then(Value::as_str) {
            return Ok(shared_id.to_owned());
        }
        match response.pointer("/result/value").and_then(Value::as_str) {
            Some("host-ambiguous") | Some("target-ambiguous") => Err(driver_error(
                ErrorCode::TargetAmbiguous,
                "Firefox shadow path or target is ambiguous",
                false,
            )),
            Some("target-missing") => Err(driver_error(
                ErrorCode::TargetNotFound,
                "Firefox shadow target was not found",
                false,
            )),
            Some("host-missing") | Some("shadow-unavailable") => Err(driver_error(
                ErrorCode::ShadowRootUnavailable,
                "Firefox shadow host or open root is unavailable",
                false,
            )),
            _ => Err(driver_error(
                ErrorCode::BrowserCommandFailed,
                "Firefox shadow probe returned an invalid result",
                false,
            )),
        }
    }

    async fn perform_pointer_click(
        &self,
        context: &str,
        shared_id: &str,
    ) -> Result<(), CommandError> {
        let shared_id = self.preflight_pointer_target(context, shared_id).await?;
        self.transport
            .send("input.performActions", pointer_actions(context, &shared_id))
            .await?;
        Ok(())
    }

    async fn preflight_pointer_target(
        &self,
        context: &str,
        shared_id: &str,
    ) -> Result<String, CommandError> {
        let response = self
            .transport
            .send(
                "script.callFunction",
                json!({
                    "functionDeclaration": "async(element)=>{if(!(element instanceof Element)||!element.isConnected)return 'detached';element.scrollIntoView({block:'center',inline:'center'});await new Promise(resolve=>requestAnimationFrame(()=>resolve()));if(!element.isConnected)return 'detached';const rect=element.getBoundingClientRect();const width=document.documentElement.clientWidth;const height=document.documentElement.clientHeight;if(rect.width<=0||rect.height<=0||rect.right<=0||rect.bottom<=0||rect.left>=width||rect.top>=height)return 'out-of-bounds';const x=Math.min(Math.max(rect.left+rect.width/2,0),width-1);const y=Math.min(Math.max(rect.top+rect.height/2,0),height-1);const root=element.getRootNode();const hit=typeof root.elementFromPoint==='function'?root.elementFromPoint(x,y):document.elementFromPoint(x,y);if(hit===null||(hit!==element&&!element.contains(hit)))return 'obscured';return element;}",
                    "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                    "arguments": [{"sharedId": shared_id}],
                    "awaitPromise": true,
                    "resultOwnership": "none",
                }),
            )
            .await?;
        if let Some(shared_id) = response
            .pointer("/result/sharedId")
            .and_then(Value::as_str)
            .filter(|shared_id| !shared_id.is_empty())
        {
            return Ok(shared_id.to_owned());
        }
        match response.pointer("/result/value").and_then(Value::as_str) {
            Some("detached") => Err(driver_error(
                ErrorCode::TargetDetached,
                "Firefox native click target detached during viewport preflight",
                false,
            )),
            Some("obscured") => Err(driver_error(
                ErrorCode::TargetObscured,
                "Firefox native click target is obscured after viewport preflight",
                false,
            )),
            Some("out-of-bounds") => Err(driver_error(
                ErrorCode::TargetOutOfBounds,
                "Firefox native click target has no clickable point in the viewport",
                false,
            )),
            _ => Err(driver_error(
                ErrorCode::BrowserCommandFailed,
                "Firefox native click viewport preflight returned an invalid result",
                false,
            )),
        }
    }

    async fn bind_existing_popup(&self, context: &str) -> Result<(PageId, String), CommandError> {
        let page_id = PageId::new();
        let binding = self
            .observer
            .begin_page_binding(&self.current_lease(), &page_id)
            .await?;
        let original_title = capture_context_title(&self.transport, context).await?;
        set_context_binding_title(&self.transport, context, binding.nonce()).await?;
        binding.complete().await?;
        restore_context_title(&self.transport, context, &original_title).await?;
        let mut pages = self.pages.write().await;
        if pages.len() >= MAX_TRACKED_PAGES {
            return Err(driver_error(
                ErrorCode::ResourceExhausted,
                "Firefox popup would exceed the tracked page bound",
                false,
            ));
        }
        pages.insert(
            page_id.clone(),
            PageContext::Ready {
                context: context.to_owned(),
                title: original_title.clone(),
            },
        );
        Ok((page_id, original_title))
    }
}

impl PageOpenOperation {
    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    async fn run(mut self) -> Result<OpenPageGuard, CommandError> {
        let mut guard = self.guard.take().expect("page-open cleanup guard exists");
        if self.cancelled() {
            return Err(guard.fail(open_cancelled_error()).await);
        }

        let binding = match self
            .resources
            .observer
            .begin_page_binding(&self.resources.lease, &self.page_id)
            .await
        {
            Ok(binding) => binding,
            Err(error) => return Err(guard.fail(error).await),
        };
        guard.binding_started().await;
        if self.cancelled() {
            drop(binding);
            return Err(guard.fail(open_cancelled_error()).await);
        }

        let response = match self
            .resources
            .transport
            .send("browsingContext.create", json!({"type": "tab"}))
            .await
        {
            Ok(response) => response,
            Err(error) => return Err(guard.fail(error).await),
        };
        let context = match response
            .get("context")
            .and_then(Value::as_str)
            .filter(|context| !context.is_empty())
        {
            Some(context) => context.to_owned(),
            None => {
                return Err(guard
                    .fail(driver_error(
                        ErrorCode::BrowserCommandFailed,
                        "Firefox BiDi did not return a browsing context",
                        false,
                    ))
                    .await);
            }
        };
        guard.context_created(context.clone()).await;
        if self.cancelled() {
            drop(binding);
            return Err(guard.fail(open_cancelled_error()).await);
        }
        if let Err(error) =
            record_opening_context(&self.resources.pages, &self.page_id, &context).await
        {
            drop(binding);
            return Err(guard.fail(error).await);
        }
        if self.cancelled() {
            drop(binding);
            return Err(guard.fail(open_cancelled_error()).await);
        }

        let original_title = match capture_context_title(&self.resources.transport, &context).await
        {
            Ok(title) => title,
            Err(error) => {
                drop(binding);
                return Err(guard.fail(error).await);
            }
        };
        guard.title_captured(original_title.clone()).await;
        if self.cancelled() {
            drop(binding);
            return Err(guard.fail(open_cancelled_error()).await);
        }
        if let Err(error) =
            set_context_binding_title(&self.resources.transport, &context, binding.nonce()).await
        {
            drop(binding);
            return Err(guard.fail(error).await);
        }
        if self.cancelled() {
            drop(binding);
            return Err(guard.fail(open_cancelled_error()).await);
        }
        if let Err(error) = binding.complete().await {
            return Err(guard.fail(error).await);
        }
        if self.cancelled() {
            return Err(guard.fail(open_cancelled_error()).await);
        }
        if let Err(error) =
            restore_context_title(&self.resources.transport, &context, &original_title).await
        {
            return Err(guard.fail(error).await);
        }
        if self.cancelled() {
            return Err(guard.fail(open_cancelled_error()).await);
        }

        let mut pages = self.resources.pages.write().await;
        if self.cancelled() {
            drop(pages);
            return Err(guard.fail(open_cancelled_error()).await);
        }
        if pages.get(&self.page_id) != Some(&PageContext::Opening(Some(context.clone()))) {
            drop(pages);
            return Err(guard
                .fail(driver_error(
                    ErrorCode::BrowserCommandFailed,
                    "page binding was invalidated before context activation",
                    true,
                ))
                .await);
        }
        pages.insert(
            self.page_id,
            PageContext::Ready {
                context,
                title: original_title,
            },
        );
        guard.settle_opening();
        Ok(guard)
    }
}

async fn capture_context_title(
    transport: &Arc<dyn BidiTransport>,
    context: &str,
) -> Result<String, CommandError> {
    let response = transport
        .send(
            "script.evaluate",
            json!({
                "expression": "document.title",
                "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                "awaitPromise": false,
                "resultOwnership": "none",
            }),
        )
        .await?;
    response
        .pointer("/result/value")
        .or_else(|| response.get("value"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| {
            driver_error(
                ErrorCode::BrowserCommandFailed,
                "Firefox BiDi did not return the original page title",
                false,
            )
        })
}

async fn set_context_binding_title(
    transport: &Arc<dyn BidiTransport>,
    context: &str,
    binding_nonce: &str,
) -> Result<(), CommandError> {
    let marker = serde_json::to_string(&format!("{PAGE_BINDING_TITLE_PREFIX}{binding_nonce}"))
        .map_err(|error| {
            driver_error(
                ErrorCode::BrowserCommandFailed,
                format!("failed to encode page-binding title: {error}"),
                false,
            )
        })?;
    let response = transport
        .send(
            "script.evaluate",
            json!({
                "expression": format!(
                    "(()=>{{document.title={marker};return document.title==={marker};}})()"
                ),
                "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                "awaitPromise": false,
                "resultOwnership": "none",
            }),
        )
        .await?;
    require_remote_true(
        &response,
        "Firefox BiDi could not mark the new context for companion binding",
    )
}

async fn restore_context_title(
    transport: &Arc<dyn BidiTransport>,
    context: &str,
    original_title: &str,
) -> Result<(), CommandError> {
    let title = serde_json::to_string(original_title).map_err(|error| {
        driver_error(
            ErrorCode::BrowserCommandFailed,
            format!("failed to encode the original page title: {error}"),
            false,
        )
    })?;
    let response = transport
        .send(
            "script.evaluate",
            json!({
                "expression": format!(
                    "(()=>{{document.title={title};return document.title==={title};}})()"
                ),
                "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                "awaitPromise": false,
                "resultOwnership": "none",
            }),
        )
        .await?;
    require_remote_true(
        &response,
        "Firefox BiDi could not restore the original page title",
    )
}

fn require_remote_true(response: &Value, message: &'static str) -> Result<(), CommandError> {
    if response.pointer("/result/value").and_then(Value::as_bool) == Some(true)
        || response.get("value").and_then(Value::as_bool) == Some(true)
    {
        return Ok(());
    }
    Err(driver_error(
        ErrorCode::BrowserCommandFailed,
        message,
        false,
    ))
}

async fn form_control_validity_evidence(
    transport: &Arc<dyn BidiTransport>,
    context: &str,
    selector_json: &str,
) -> Result<Vec<Evidence>, CommandError> {
    let response = transport
        .send(
            "script.evaluate",
            json!({
                "expression": format!("(()=>{{const el=document.querySelector({selector_json});if(!el)throw new Error('target detached');const validates=typeof el.willValidate==='boolean'&&el.willValidate;return JSON.stringify({{valid:!validates||el.validity.valid,message:validates&&!el.validity.valid?el.validationMessage.slice(0,1024):''}});}})()"),
                "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                "awaitPromise": false,
                "resultOwnership": "none",
            }),
        )
        .await?;
    let encoded = response
        .pointer("/result/value")
        .and_then(Value::as_str)
        .ok_or_else(|| {
            driver_error(
                ErrorCode::BrowserCommandFailed,
                "Firefox form validity probe returned an invalid result",
                false,
            )
        })?;
    let decoded: Value = serde_json::from_str(encoded).map_err(|_| {
        driver_error(
            ErrorCode::BrowserCommandFailed,
            "Firefox form validity probe returned malformed JSON",
            false,
        )
    })?;
    let valid = decoded
        .get("valid")
        .and_then(Value::as_bool)
        .ok_or_else(|| {
            driver_error(
                ErrorCode::BrowserCommandFailed,
                "Firefox form validity probe omitted validity",
                false,
            )
        })?;
    let message = decoded
        .get("message")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    Ok(vec![
        Evidence::Configuration {
            name: "formControlValid".into(),
            value: valid.to_string(),
        },
        Evidence::Configuration {
            name: "formControlValidationMessage".into(),
            value: message,
        },
    ])
}

async fn record_opening_context(
    pages: &Arc<RwLock<HashMap<PageId, PageContext>>>,
    page_id: &PageId,
    context: &str,
) -> Result<(), CommandError> {
    let mut pages = pages.write().await;
    if pages.get(page_id) != Some(&PageContext::Opening(None)) {
        return Err(driver_error(
            ErrorCode::BrowserCommandFailed,
            "page creation was invalidated before Firefox returned its context",
            true,
        ));
    }
    pages.insert(
        page_id.clone(),
        PageContext::Opening(Some(context.to_owned())),
    );
    Ok(())
}

async fn remove_page_mapping(
    pages: &Arc<RwLock<HashMap<PageId, PageContext>>>,
    page_id: &PageId,
    context: Option<&str>,
) {
    let mut pages = pages.write().await;
    let remove = match (pages.get(page_id), context) {
        (Some(PageContext::Opening(None)), None | Some(_)) => true,
        (Some(PageContext::Opening(Some(mapped))), Some(context)) => mapped == context,
        (
            Some(PageContext::Ready {
                context: mapped, ..
            }),
            Some(context),
        ) => mapped == context,
        (Some(PageContext::Releasing { context: None }), None | Some(_)) => true,
        (
            Some(PageContext::Releasing {
                context: Some(mapped),
            }),
            Some(context),
        ) => mapped == context,
        _ => false,
    };
    if remove {
        pages.remove(page_id);
    }
}

fn open_cancelled_error() -> CommandError {
    driver_error(
        ErrorCode::BrowserCommandFailed,
        "Firefox page opening was cancelled",
        true,
    )
}

#[async_trait]
impl BrowserWorker for FirefoxCompanionWorker {
    fn worker_id(&self) -> WorkerId {
        self.id.clone()
    }

    fn profile_dir(&self) -> &Path {
        &self.profile_dir
    }

    async fn open_page(&self, page_id: PageId) -> Result<(), CommandError> {
        let guard = self.open_page_owned(page_id).await?;
        guard.disarm().await
    }

    async fn navigate(
        &self,
        page_id: &PageId,
        command: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        if !self.current_lease().capabilities.navigate {
            return Err(capability_error("navigation"));
        }
        let context = self.context(page_id).await?;
        let wait = match command.wait_until {
            WaitUntil::NetworkIdle => "complete",
            WaitUntil::Commit | WaitUntil::DomContentLoaded | WaitUntil::Interactive => {
                "interactive"
            }
        };
        let response = tokio::time::timeout(
            Duration::from_millis(command.timeout_ms),
            self.transport.send(
                "browsingContext.navigate",
                json!({"context": context, "url": command.url, "wait": wait}),
            ),
        )
        .await
        .map_err(|_| {
            driver_error(
                ErrorCode::DeadlineExceeded,
                format!("Firefox navigation exceeded {} ms", command.timeout_ms),
                true,
            )
        })??;
        let url = response
            .get("url")
            .and_then(Value::as_str)
            .unwrap_or(&command.url)
            .to_owned();
        let title = response
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        Ok(vec![
            Evidence::Navigation { url, title },
            self.evidence(InteractionPath::EngineNative),
        ])
    }

    async fn inspect(
        &self,
        page_id: &PageId,
        command: &InspectCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        if !self.current_lease().capabilities.observe {
            return Err(capability_error("extension observation"));
        }
        let context = self.context(page_id).await?;
        self.transport
            .send(
                "script.evaluate",
                json!({
                    "expression": "Boolean(globalThis.document)",
                    "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                    "awaitPromise": false,
                    "resultOwnership": "none",
                }),
            )
            .await?;
        if let Some(selector) = command.selector.as_deref() {
            let selector_json = serde_json::to_string(selector).map_err(|error| {
                driver_error(ErrorCode::InvalidRequest, error.to_string(), false)
            })?;
            let response = self
                .transport
                .send(
                    "script.evaluate",
                    json!({
                        "expression": format!("(()=>{{const node=document.querySelector({selector_json});return node instanceof HTMLScriptElement && node.type==='application/json' ? node.textContent : null;}})()"),
                        "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                        "awaitPromise": false,
                        "resultOwnership": "none",
                    }),
                )
                .await?;
            if let Some(text) = response.pointer("/result/value").and_then(Value::as_str) {
                if text.len() > MAX_VISIBLE_TEXT_BYTES || contains_sensitive_material(text) {
                    return Err(driver_error(
                        ErrorCode::BrowserCommandFailed,
                        "inert JSON inspection failed its content safety bound",
                        false,
                    ));
                }
                return Ok(vec![
                    Evidence::Inspection {
                        selector: Some(selector.to_owned()),
                        url: String::new(),
                        title: String::new(),
                        text: text.to_owned(),
                        html: None,
                    },
                    self.evidence(InteractionPath::EngineNative),
                ]);
            }
        }
        let semantic_target = command
            .target
            .as_ref()
            .is_some_and(|target| target.css.is_none());
        let effective;
        let command = if semantic_target {
            let context = self.context(page_id).await?;
            let (_, selector) = self
                .resolve_input_target(page_id, &context, "", command.target.as_ref())
                .await?;
            effective = InspectCommand {
                selector: Some(selector),
                target: None,
                include_html: command.include_html,
            };
            &effective
        } else {
            command
        };
        let mut observation = self
            .observer
            .observe(&self.current_lease(), page_id, command)
            .await?;
        if !command.include_html {
            observation.html = None;
        }
        let scoped_control_value = (semantic_target || command.selector.is_some())
            .then(|| {
                observation
                    .controls
                    .first()
                    .and_then(|control| control.value.clone())
            })
            .flatten();
        let text = if let Some(value) = scoped_control_value {
            value
        } else if observation.visible_text.is_empty()
            && (semantic_target || command.selector.is_some())
        {
            observation
                .controls
                .first()
                .and_then(|control| control.name.clone())
                .unwrap_or_else(|| observation.visible_text.clone())
        } else {
            observation.visible_text.clone()
        };
        Ok(vec![
            Evidence::Inspection {
                selector: command.selector.clone().or_else(|| {
                    command
                        .target
                        .as_ref()
                        .and_then(|target| target.css.clone())
                }),
                url: observation.url,
                title: observation.title,
                text,
                html: observation.html,
            },
            self.evidence(InteractionPath::ExtensionApi),
        ])
    }

    async fn click(
        &self,
        page_id: &PageId,
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let context = self.context(page_id).await?;
        let (context, selector) = self
            .resolve_input_target(
                page_id,
                &context,
                &command.selector,
                command.target.as_ref(),
            )
            .await?;
        let shared_id = match command
            .target
            .as_ref()
            .filter(|target| !target.shadow_path.is_empty())
        {
            Some(target) => self.resolve_shadow_element(&context, target).await?,
            None => {
                self.resolve_element(&context, &selector, command.target.is_some())
                    .await?
            }
        };
        self.perform_pointer_click(&context, &shared_id).await?;
        Ok(vec![
            Evidence::Element {
                selector: command.selector.clone(),
                text: None,
            },
            self.evidence(InteractionPath::EngineNative),
        ])
    }

    async fn click_and_wait_for_popup(
        &self,
        page_id: &PageId,
        command: &ClickAndWaitForPopupCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let opener = self.context(page_id).await?;
        let mut events = self.transport.subscribe_events().ok_or_else(|| {
            driver_error(
                ErrorCode::BrowserCommandFailed,
                "Firefox BiDi transport cannot observe popup contexts",
                false,
            )
        })?;
        let click_evidence = self
            .click(
                page_id,
                &ClickCommand {
                    selector: command.selector.clone(),
                    target: command.target.clone(),
                    boundary: true,
                    expected_url: None,
                },
            )
            .await?;
        let timeout = Duration::from_millis(command.timeout_ms.max(1));
        let (popup_context, popup_url) = tokio::time::timeout(timeout, async {
            loop {
                let event = events.recv().await.map_err(|_| {
                    driver_error(
                        ErrorCode::BrowserCommandFailed,
                        "Firefox popup event stream closed",
                        false,
                    )
                })?;
                if let Some(popup) = popup_context_from_event(&event, &opener) {
                    return Ok::<_, CommandError>(popup);
                }
            }
        })
        .await
        .map_err(|_| {
            driver_error(
                ErrorCode::WaitConditionTimedOut,
                format!(
                    "Firefox popup did not open within {} ms",
                    command.timeout_ms
                ),
                false,
            )
        })??;

        tokio::task::yield_now().await;
        while let Ok(event) = events.try_recv() {
            if popup_context_from_event(&event, &opener).is_some() {
                return Err(driver_error(
                    ErrorCode::TargetAmbiguous,
                    "Firefox click opened multiple popup contexts",
                    false,
                ));
            }
        }
        let (popup_page_id, title) = self.bind_existing_popup(&popup_context).await?;
        let mut evidence = vec![Evidence::Popup {
            opener_page_id: page_id.clone(),
            page_id: popup_page_id,
            url: popup_url,
            title,
        }];
        evidence.extend(click_evidence);
        Ok(evidence)
    }

    async fn upload_files(
        &self,
        page_id: &PageId,
        command: &UploadFilesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        if command.paths.is_empty() || command.paths.len() > MAX_UPLOAD_FILES {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                format!("Firefox upload requires 1..={MAX_UPLOAD_FILES} files"),
                false,
            ));
        }
        let mut paths = Vec::with_capacity(command.paths.len());
        for source in &command.paths {
            if let Some(artifact_id) = source.strip_prefix("artifact://") {
                if artifact_id.len() != 64
                    || !artifact_id.bytes().all(|byte| byte.is_ascii_hexdigit())
                {
                    return Err(driver_error(
                        ErrorCode::PolicyDenied,
                        "upload artifact reference is malformed",
                        false,
                    ));
                }
                let session = self.session_id.as_ref().ok_or_else(|| {
                    driver_error(
                        ErrorCode::PolicyDenied,
                        "upload artifact has no owning runtime session",
                        false,
                    )
                })?;
                let store = self.artifacts.as_ref().ok_or_else(|| {
                    driver_error(
                        ErrorCode::PolicyDenied,
                        "upload artifact store is not configured",
                        false,
                    )
                })?;
                let bytes = store.get(session, artifact_id).await.map_err(|_| {
                    driver_error(
                        ErrorCode::PolicyDenied,
                        "upload artifact is unavailable",
                        false,
                    )
                })?;
                if format!("{:x}", Sha256::digest(&bytes)) != artifact_id.to_ascii_lowercase() {
                    return Err(driver_error(
                        ErrorCode::PolicyDenied,
                        "upload artifact digest verification failed",
                        false,
                    ));
                }
                let root = self.downloads_dir.as_ref().ok_or_else(|| {
                    driver_error(
                        ErrorCode::PolicyDenied,
                        "upload artifact materialization is not configured",
                        false,
                    )
                })?;
                let directory = root.join(session.0.to_string()).join("upload-artifacts");
                std::fs::create_dir_all(&directory).map_err(|_| {
                    driver_error(
                        ErrorCode::PolicyDenied,
                        "upload artifact materialization failed",
                        false,
                    )
                })?;
                let path = directory.join(format!("{artifact_id}.bin"));
                std::fs::write(&path, &bytes).map_err(|_| {
                    driver_error(
                        ErrorCode::PolicyDenied,
                        "upload artifact materialization failed",
                        false,
                    )
                })?;
                paths.push(std::fs::canonicalize(path).map_err(|_| {
                    driver_error(
                        ErrorCode::PolicyDenied,
                        "upload artifact materialization failed",
                        false,
                    )
                })?);
            } else {
                let resolved = resolve_upload_paths(&self.upload_roots, &[PathBuf::from(source)])?;
                paths.extend(resolved);
            }
        }
        let mut total_bytes = 0_u64;
        for path in &paths {
            total_bytes = total_bytes
                .checked_add(
                    std::fs::metadata(path)
                        .map_err(|_| {
                            driver_error(
                                ErrorCode::PolicyDenied,
                                "approved upload file is unavailable",
                                false,
                            )
                        })?
                        .len(),
                )
                .ok_or_else(|| {
                    driver_error(
                        ErrorCode::InvalidRequest,
                        "upload byte count overflowed",
                        false,
                    )
                })?;
        }
        if total_bytes > MAX_UPLOAD_BYTES {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                format!("Firefox upload exceeds the {MAX_UPLOAD_BYTES} byte bound"),
                false,
            ));
        }
        let context = self.context(page_id).await?;
        let (context, selector) = self
            .resolve_input_target(
                page_id,
                &context,
                &command.selector,
                command.target.as_ref(),
            )
            .await?;
        let selector_json = serde_json::to_string(&selector)
            .map_err(|error| driver_error(ErrorCode::InvalidRequest, error.to_string(), false))?;
        let probe = self.transport.send("script.evaluate", json!({
            "expression": format!("(()=>{{const matches=[...document.querySelectorAll({selector_json})];if(matches.length===0)return 'missing';if(matches.length!==1)return 'ambiguous';const input=matches[0];if(!(input instanceof HTMLInputElement)||input.type!=='file')return 'non-file';if(input.disabled)return 'disabled';return 'valid';}})()"),
            "target": {"context": context, "sandbox": COMPANION_SANDBOX},
            "awaitPromise": false,
            "resultOwnership": "none",
        })).await?;
        match probe.pointer("/result/value").and_then(Value::as_str) {
            Some("valid") => {}
            Some("ambiguous") => {
                return Err(driver_error(
                    ErrorCode::TargetAmbiguous,
                    "Firefox file input is ambiguous",
                    false,
                ))
            }
            Some("missing") => {
                return Err(driver_error(
                    ErrorCode::TargetNotFound,
                    "Firefox file input was not found",
                    false,
                ))
            }
            Some("non-file") | Some("disabled") => {
                return Err(driver_error(
                    ErrorCode::TargetNotFound,
                    "Firefox target is not an enabled file input",
                    false,
                ))
            }
            _ => {
                return Err(driver_error(
                    ErrorCode::BrowserCommandFailed,
                    "Firefox file input probe returned an invalid result",
                    false,
                ))
            }
        }
        let shared_id = self.resolve_element(&context, &selector, true).await?;
        let files = paths
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        self.transport
            .send(
                "input.setFiles",
                json!({
                    "context": context,
                    "element": {"sharedId": shared_id},
                    "files": files,
                }),
            )
            .await?;
        let verified = self.transport.send("script.evaluate", json!({
            "expression": format!("document.querySelector({selector_json})?.files?.length ?? -1"),
            "target": {"context": context, "sandbox": COMPANION_SANDBOX},
            "awaitPromise": false,
            "resultOwnership": "none",
        })).await?;
        if verified.pointer("/result/value").and_then(Value::as_u64) != Some(paths.len() as u64) {
            return Err(driver_error(
                ErrorCode::VerificationFailed,
                "Firefox file selection count did not match",
                false,
            ));
        }
        let opaque = paths
            .iter()
            .map(|path| {
                format!(
                    "upload://sha256/{:x}",
                    Sha256::digest(path.as_os_str().as_encoded_bytes())
                )
            })
            .collect();
        Ok(vec![
            Evidence::Upload {
                selector: command.selector.clone(),
                paths: opaque,
            },
            self.evidence(InteractionPath::EngineNative),
        ])
    }

    async fn click_and_wait_for_download(
        &self,
        page_id: &PageId,
        command: &ClickAndWaitForDownloadCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let context = self.context(page_id).await?;
        let session = self.session_id.as_ref().ok_or_else(|| {
            driver_error(
                ErrorCode::InvalidRequest,
                "Firefox download requires a runtime session",
                false,
            )
        })?;
        let artifacts = self.artifacts.as_ref().ok_or_else(|| {
            driver_error(
                ErrorCode::InvalidRequest,
                "Firefox download artifact store is not configured",
                false,
            )
        })?;
        let root = self.downloads_dir.as_ref().ok_or_else(|| {
            driver_error(
                ErrorCode::InvalidRequest,
                "Firefox download directory is not configured",
                false,
            )
        })?;
        let destination = root.join(session.0.to_string());
        std::fs::create_dir_all(&destination).map_err(|_| {
            driver_error(
                ErrorCode::PolicyDenied,
                "Firefox download directory is unavailable",
                false,
            )
        })?;
        let destination = std::fs::canonicalize(&destination).map_err(|_| {
            driver_error(
                ErrorCode::PolicyDenied,
                "Firefox download directory is invalid",
                false,
            )
        })?;
        self.transport
            .send(
                "browser.setDownloadBehavior",
                json!({
                    "downloadBehavior": {"type": "allowed", "destinationFolder": destination},
                }),
            )
            .await?;
        let mut events = self.transport.subscribe_events().ok_or_else(|| {
            driver_error(
                ErrorCode::BrowserCommandFailed,
                "Firefox BiDi transport cannot observe downloads",
                false,
            )
        })?;
        let click_evidence = self
            .click(
                page_id,
                &ClickCommand {
                    selector: command.selector.clone(),
                    target: command.target.clone(),
                    boundary: true,
                    expected_url: None,
                },
            )
            .await?;
        let timeout = Duration::from_millis(command.timeout_ms.max(1));
        let (navigation, filename) = tokio::time::timeout(timeout, async {
            loop {
                let event = events.recv().await.map_err(|_| {
                    driver_error(
                        ErrorCode::BrowserCommandFailed,
                        "Firefox download event stream closed",
                        false,
                    )
                })?;
                if event.method == "browsingContext.downloadWillBegin"
                    && event.params.get("context").and_then(Value::as_str) == Some(context.as_str())
                {
                    let navigation = event
                        .params
                        .get("navigation")
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                    let filename = event
                        .params
                        .get("suggestedFilename")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            driver_error(
                                ErrorCode::BrowserCommandFailed,
                                "Firefox download event has no filename",
                                false,
                            )
                        })?;
                    return Ok::<_, CommandError>((navigation, filename.to_owned()));
                }
            }
        })
        .await
        .map_err(|_| {
            driver_error(
                ErrorCode::WaitConditionTimedOut,
                "Firefox download did not begin before timeout",
                false,
            )
        })??;
        tokio::time::timeout(timeout, async {
            loop {
                let event = events.recv().await.map_err(|_| {
                    driver_error(
                        ErrorCode::BrowserCommandFailed,
                        "Firefox download event stream closed",
                        false,
                    )
                })?;
                let event_context = event.params.get("context").and_then(Value::as_str);
                if event.method == "browsingContext.downloadWillBegin"
                    && event_context == Some(context.as_str())
                {
                    return Err(driver_error(
                        ErrorCode::TargetAmbiguous,
                        "Firefox click began multiple downloads",
                        false,
                    ));
                }
                if event.method == "browsingContext.downloadEnd"
                    && event_context == Some(context.as_str())
                    && event.params.get("navigation").and_then(Value::as_str)
                        == navigation.as_deref()
                {
                    return match event.params.get("status").and_then(Value::as_str) {
                        Some("complete") => Ok(()),
                        _ => Err(driver_error(
                            ErrorCode::BrowserCommandFailed,
                            "Firefox download was canceled or failed",
                            false,
                        )),
                    };
                }
            }
        })
        .await
        .map_err(|_| {
            driver_error(
                ErrorCode::WaitConditionTimedOut,
                "Firefox download did not complete before timeout",
                false,
            )
        })??;
        let safe_name = std::path::Path::new(&filename)
            .file_name()
            .and_then(|name| name.to_str())
            .filter(|name| *name == filename && !name.is_empty())
            .ok_or_else(|| {
                driver_error(
                    ErrorCode::BrowserCommandFailed,
                    "Firefox suggested an unsafe download filename",
                    false,
                )
            })?;
        let path = destination.join(safe_name);
        let canonical = std::fs::canonicalize(&path).map_err(|_| {
            driver_error(
                ErrorCode::BrowserCommandFailed,
                "Firefox completed download file is unavailable",
                false,
            )
        })?;
        if !canonical.starts_with(&destination) {
            return Err(driver_error(
                ErrorCode::PolicyDenied,
                "Firefox download escaped its owned directory",
                false,
            ));
        }
        let bytes = std::fs::read(&canonical).map_err(|_| {
            driver_error(
                ErrorCode::BrowserCommandFailed,
                "Firefox completed download cannot be read",
                false,
            )
        })?;
        let record = artifacts
            .put(
                session,
                page_id,
                "application/octet-stream",
                "bin",
                &bytes,
                MAX_UPLOAD_BYTES as usize,
            )
            .await
            .map_err(|error| {
                driver_error(
                    ErrorCode::BrowserCommandFailed,
                    format!("Firefox download artifact failed: {error}"),
                    false,
                )
            })?;
        let mut evidence = vec![Evidence::Download {
            filename: safe_name.to_owned(),
            path: format!("artifact://{}", record.artifact_id),
            bytes: record.bytes,
            sha256: record.sha256,
        }];
        evidence.extend(click_evidence);
        Ok(evidence)
    }

    async fn type_text(
        &self,
        page_id: &PageId,
        command: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        if command.value.chars().count() > MAX_TYPE_CODEPOINTS {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                format!("native keyboard input exceeds {MAX_TYPE_CODEPOINTS} codepoints"),
                false,
            ));
        }
        let context = self.context(page_id).await?;
        let (context, selector) = self
            .resolve_input_target(
                page_id,
                &context,
                &command.selector,
                command.target.as_ref(),
            )
            .await?;
        let selector_json = serde_json::to_string(&selector)
            .map_err(|error| driver_error(ErrorCode::InvalidRequest, error.to_string(), false))?;
        let value_json = serde_json::to_string(&command.value)
            .map_err(|error| driver_error(ErrorCode::InvalidRequest, error.to_string(), false))?;
        let selection = self.transport.send("script.evaluate", json!({
            "expression": format!("(()=>{{const element=document.querySelector({selector_json});if(element instanceof HTMLInputElement&&(element.type==='checkbox'||element.type==='radio')){{if({value_json}!=='true'&&{value_json}!=='false')return 'invalid-checked';const checked={value_json}==='true';if(element.type==='radio'&&!checked)return 'radio-uncheck';if(element.checked!==checked)element.click();return `checked:${{element.checked}}`;}}if(!(element instanceof HTMLSelectElement))return 'not-select';const options=[...element.options].filter(option=>option.value==={value_json});if(options.length===0)return 'missing';if(options.length!==1)return 'ambiguous';if(options[0].disabled)return 'disabled';element.value={value_json};element.dispatchEvent(new Event('input',{{bubbles:true}}));element.dispatchEvent(new Event('change',{{bubbles:true}}));return element.value==={value_json}?'selected':'missing';}})()"),
            "target": {"context": context, "sandbox": COMPANION_SANDBOX},
            "awaitPromise": false,
            "resultOwnership": "none",
        })).await?;
        match selection.pointer("/result/value").and_then(Value::as_str) {
            Some("selected") => {
                let mut evidence = vec![
                    Evidence::Element {
                        selector: command.selector.clone(),
                        text: Some(command.value.clone()),
                    },
                    self.evidence(InteractionPath::ExtensionApi),
                ];
                evidence.extend(
                    form_control_validity_evidence(&self.transport, &context, &selector_json)
                        .await?,
                );
                return Ok(evidence);
            }
            Some("missing") | Some("disabled") => {
                return Err(driver_error(
                    ErrorCode::TargetNotFound,
                    "select option value is missing or disabled",
                    false,
                ))
            }
            Some("ambiguous") => {
                return Err(driver_error(
                    ErrorCode::TargetAmbiguous,
                    "select option value is ambiguous",
                    false,
                ))
            }
            Some("not-select") => {}
            Some(value @ ("checked:true" | "checked:false")) => {
                let mut evidence = vec![
                    Evidence::Element {
                        selector: command.selector.clone(),
                        text: Some(value.trim_start_matches("checked:").into()),
                    },
                    self.evidence(InteractionPath::ExtensionApi),
                ];
                evidence.extend(
                    form_control_validity_evidence(&self.transport, &context, &selector_json)
                        .await?,
                );
                return Ok(evidence);
            }
            Some("invalid-checked") | Some("radio-uncheck") => {
                return Err(driver_error(
                    ErrorCode::InvalidRequest,
                    "checkable control requires a valid supported checked state",
                    false,
                ))
            }
            _ => {
                return Err(driver_error(
                    ErrorCode::BrowserCommandFailed,
                    "Firefox select probe returned an invalid result",
                    false,
                ))
            }
        }
        let shared_id = self
            .resolve_element(&context, &selector, command.target.is_some())
            .await?;
        self.perform_pointer_click(&context, &shared_id).await?;
        self.transport
            .send(
                "input.performActions",
                keyboard_actions(&context, &command.value, command.clear_first),
            )
            .await?;
        let mut evidence = vec![
            Evidence::Element {
                selector: command.selector.clone(),
                text: None,
            },
            self.evidence(InteractionPath::EngineNative),
        ];
        evidence.extend(
            form_control_validity_evidence(&self.transport, &context, &selector_json).await?,
        );
        Ok(evidence)
    }

    async fn wait_for(
        &self,
        page_id: &PageId,
        command: &WaitForCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        if command.timeout_ms == 0 {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "Firefox wait timeout must be positive",
                false,
            ));
        }
        let started = Instant::now();
        let deadline = started + Duration::from_millis(command.timeout_ms);
        let mut observations = 0;
        loop {
            observations += 1;
            let satisfied = match &command.condition {
                WaitCondition::Url { matcher } => {
                    let context = self.context(page_id).await?;
                    let response = self
                        .transport
                        .send(
                            "script.evaluate",
                            json!({
                                "expression": "globalThis.location.href",
                                "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                                "awaitPromise": false,
                                "resultOwnership": "none",
                            }),
                        )
                        .await?;
                    let url = response
                        .pointer("/result/value")
                        .and_then(Value::as_str)
                        .ok_or_else(|| {
                            driver_error(
                                ErrorCode::BrowserCommandFailed,
                                "Firefox URL wait returned no location",
                                false,
                            )
                        })?;
                    if url.len() > MAX_URL_BYTES * 4 {
                        return Err(driver_error(
                            ErrorCode::BrowserCommandFailed,
                            "Firefox URL wait exceeded its bound",
                            false,
                        ));
                    }
                    bounded_text_matches(matcher, url)?
                }
                WaitCondition::Element { target, state } => {
                    let context = self.context(page_id).await?;
                    let resolved = self
                        .resolve_input_target(page_id, &context, "", Some(target))
                        .await;
                    match resolved {
                        Ok((context, selector)) => {
                            let selector = serde_json::to_string(&selector).map_err(|error| {
                                driver_error(ErrorCode::InvalidRequest, error.to_string(), false)
                            })?;
                            let expression = match state {
                                types::ElementState::Attached | types::ElementState::Visible => {
                                    format!("Boolean(document.querySelector({selector}))")
                                }
                                types::ElementState::Detached => {
                                    format!("!document.querySelector({selector})")
                                }
                                types::ElementState::Enabled => format!("!document.querySelector({selector})?.matches(':disabled,[aria-disabled=\"true\"]')"),
                                types::ElementState::Disabled => format!("Boolean(document.querySelector({selector})?.matches(':disabled,[aria-disabled=\"true\"]'))"),
                                types::ElementState::Hidden => format!("Boolean(document.querySelector({selector})) && !document.querySelector({selector}).checkVisibility()"),
                            };
                            let response = self.transport.send("script.evaluate", json!({
                                "expression": expression,
                                "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                                "awaitPromise": false,
                                "resultOwnership": "none",
                            })).await?;
                            response
                                .pointer("/result/value")
                                .and_then(Value::as_bool)
                                .unwrap_or(false)
                        }
                        Err(error) if error.code == ErrorCode::TargetNotFound => {
                            matches!(state, types::ElementState::Detached)
                        }
                        Err(error) => return Err(error),
                    }
                }
                _ => {
                    return Err(driver_error(
                        ErrorCode::InvalidRequest,
                        "Firefox wait condition is not supported",
                        false,
                    ))
                }
            };
            if satisfied {
                return Ok(vec![Evidence::Wait {
                    condition: command.condition.clone(),
                    elapsed_ms: started.elapsed().as_millis() as u64,
                    observations,
                    excluded_classes: Vec::new(),
                }]);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(driver_error(
                    ErrorCode::WaitConditionTimedOut,
                    format!(
                        "wait condition was not satisfied within {}ms",
                        command.timeout_ms
                    ),
                    false,
                ));
            }
            tokio::time::sleep((deadline - now).min(Duration::from_millis(25))).await;
        }
    }

    async fn capture_screenshot(
        &self,
        page_id: &PageId,
        command: &CaptureScreenshotCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        if !matches!(command.mode, ScreenshotMode::Viewport) {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                "Firefox currently supports viewport screenshots only",
                false,
            ));
        }
        let session_id = self.session_id.as_ref().ok_or_else(|| {
            driver_error(
                ErrorCode::ScreenshotCaptureFailed,
                "Firefox screenshot artifact storage is not configured",
                false,
            )
        })?;
        let artifacts = self.artifacts.as_ref().ok_or_else(|| {
            driver_error(
                ErrorCode::ScreenshotCaptureFailed,
                "Firefox screenshot artifact storage is not configured",
                false,
            )
        })?;
        let context = self.context(page_id).await?;
        let response = self
            .transport
            .send(
                "browsingContext.captureScreenshot",
                json!({"context": context, "origin": "viewport"}),
            )
            .await?;
        let encoded = response
            .get("data")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                driver_error(
                    ErrorCode::ScreenshotCaptureFailed,
                    "Firefox screenshot omitted PNG data",
                    false,
                )
            })?;
        if encoded.len() > MAX_SCREENSHOT_BYTES.saturating_mul(4) / 3 + 8 {
            return Err(driver_error(
                ErrorCode::ScreenshotCaptureFailed,
                "Firefox screenshot exceeded its encoded bound",
                false,
            ));
        }
        let bytes = BASE64.decode(encoded).map_err(|_| {
            driver_error(
                ErrorCode::ScreenshotCaptureFailed,
                "Firefox screenshot returned invalid base64",
                false,
            )
        })?;
        if bytes.len() > MAX_SCREENSHOT_BYTES {
            return Err(driver_error(
                ErrorCode::ScreenshotCaptureFailed,
                "Firefox screenshot exceeded its byte bound",
                false,
            ));
        }
        let record = artifacts
            .put_png(session_id, page_id, &bytes)
            .await
            .map_err(|error| {
                driver_error(ErrorCode::ScreenshotCaptureFailed, error.to_string(), false)
            })?;
        Ok(vec![Evidence::Screenshot {
            artifact_id: record.artifact_id,
            media_type: record.media_type,
            width: record.width,
            height: record.height,
            bytes: record.bytes,
            sha256: record.sha256,
        }])
    }

    async fn open_page_command(
        &self,
        command: &OpenPageCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page_id = PageId::new();
        let guard = self.open_page_owned(page_id.clone()).await?;
        let (url, title) = if let Some(url) = &command.url {
            let navigation = match self
                .navigate(
                    &page_id,
                    &NavigateCommand {
                        url: url.clone(),
                        wait_until: WaitUntil::NetworkIdle,
                        timeout_ms: DEFAULT_NAVIGATION_TIMEOUT.as_millis() as u64,
                    },
                )
                .await
            {
                Ok(navigation) => navigation,
                Err(error) => return Err(guard.fail(error).await),
            };
            navigation
                .into_iter()
                .find_map(|evidence| match evidence {
                    Evidence::Navigation { url, title } => Some((url, title)),
                    _ => None,
                })
                .unwrap_or_else(|| (url.clone(), String::new()))
        } else {
            let title = match self.page_title(&page_id).await {
                Ok(title) => title,
                Err(error) => return Err(guard.fail(error).await),
            };
            ("about:blank".into(), title)
        };
        let evidence = vec![
            Evidence::Page {
                page_id,
                url,
                title,
            },
            self.evidence(InteractionPath::EngineNative),
        ];
        guard.disarm().await?;
        Ok(evidence)
    }

    async fn close_page_command(
        &self,
        command: &ClosePageCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.context(&command.page_id).await?;
        let cleanup = self
            .page_cleanups
            .read()
            .await
            .get(&command.page_id)
            .cloned()
            .ok_or_else(page_missing)?;
        let failures = cleanup.run().await;
        if !failures.is_empty() {
            return Err(cleanup_failures_error(&failures));
        }
        Ok(vec![self.evidence(InteractionPath::EngineNative)])
    }

    async fn a11y_snapshot(
        &self,
        page_id: &PageId,
        command: &types::AccessibilitySnapshotCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.context(page_id).await?;
        let max_nodes = command.max_nodes.unwrap_or(256).clamp(1, 2048);
        let (mut nodes, truncated) = self
            .observer
            .a11y_snapshot(&self.current_lease(), page_id, max_nodes)
            .await?;
        worker_pool::annotate_accessibility_targets(&mut nodes);
        Ok(vec![
            Evidence::AccessibilitySnapshot {
                page_id: page_id.clone(),
                nodes,
                truncated,
            },
            self.evidence(InteractionPath::EngineNative),
        ])
    }

    async fn activate_page(
        &self,
        command: &types::ActivatePageCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let context = self.context(&command.page_id).await?;
        self.transport
            .send("browsingContext.activate", json!({"context": context}))
            .await
            .map_err(|error| {
                driver_error(
                    ErrorCode::BrowserCommandFailed,
                    format!("Firefox page activation failed: {}", error.message),
                    true,
                )
            })?;
        Ok(vec![self.evidence(InteractionPath::EngineNative)])
    }

    async fn close(&self) -> Result<(), CommandError> {
        {
            let _lifecycle = self.lifecycle.lock().await;
            self.start_shutdown();
        }
        self.wait_for_shutdown().await
    }
}

impl Drop for FirefoxCompanionWorker {
    fn drop(&mut self) {
        self.start_shutdown();
    }
}

async fn run_worker_shutdown(resources: WorkerShutdownResources) -> Result<(), CommandError> {
    let cleanups = resources
        .page_cleanups
        .read()
        .await
        .values()
        .cloned()
        .collect::<Vec<_>>();
    {
        let mut pages = resources.pages.write().await;
        for context in pages.values_mut() {
            let owned = match context {
                PageContext::Opening(context) | PageContext::Releasing { context } => {
                    context.clone()
                }
                PageContext::Ready { context, .. } => Some(context.clone()),
            };
            *context = PageContext::Releasing { context: owned };
        }
    }

    let mut failures = Vec::new();
    if let Some(error) = resources
        .cleanup_failure
        .lock()
        .expect("cleanup failure mutex poisoned")
        .clone()
    {
        failures.push(error.message);
    }
    for cleanup in cleanups {
        cleanup.cancel();
        failures.extend(cleanup.run().await);
    }
    resources.pages.write().await.clear();
    for slot in [&resources.cleanup_task, &resources.renewal_task] {
        if let Some(task) = slot.lock().expect("worker task mutex poisoned").take() {
            task.abort();
        }
    }
    match tokio::time::timeout(
        resources.observer.operation_timeout(),
        resources.transport.close(),
    )
    .await
    {
        Ok(Ok(())) => {}
        Ok(Err(error)) => {
            failures.push(format!("closing the Firefox transport: {}", error.message))
        }
        Err(_) => failures.push(format!(
            "closing the Firefox transport: {}",
            cleanup_deadline_error("closing the Firefox transport").message
        )),
    }
    if failures.is_empty() {
        Ok(())
    } else {
        Err(cleanup_failures_error(&failures))
    }
}

async fn reconcile_contexts(
    transport: &Arc<dyn BidiTransport>,
    pages: &Arc<RwLock<HashMap<PageId, PageContext>>>,
    page_cleanups: &Arc<RwLock<HashMap<PageId, OpenPageCleanup>>>,
    cleanup_failure: &Arc<TaskMutex<Option<CommandError>>>,
) {
    let live = match transport.send("browsingContext.getTree", json!({})).await {
        Ok(response) => live_contexts(&response),
        Err(_) => None,
    };
    let removals = mark_missing_contexts(pages, page_cleanups, live.as_ref()).await;
    release_removed_pages(cleanup_failure, removals).await;
}

#[derive(Clone)]
struct PageRemoval {
    cleanup: OpenPageCleanup,
}

async fn mark_destroyed_context(
    pages: &Arc<RwLock<HashMap<PageId, PageContext>>>,
    page_cleanups: &Arc<RwLock<HashMap<PageId, OpenPageCleanup>>>,
    destroyed: &str,
) -> Vec<PageRemoval> {
    let mut pages = pages.write().await;
    let removals = pages
        .iter()
        .filter_map(|(page_id, mapped)| {
            let context = match mapped {
                PageContext::Opening(Some(context)) | PageContext::Ready { context, .. }
                    if context == destroyed =>
                {
                    context.clone()
                }
                _ => return None,
            };
            Some((page_id.clone(), context))
        })
        .collect::<Vec<_>>();
    for (page_id, context) in &removals {
        pages.insert(
            page_id.clone(),
            PageContext::Releasing {
                context: Some(context.clone()),
            },
        );
    }
    drop(pages);
    let cleanups = page_cleanups.read().await;
    removals
        .into_iter()
        .filter_map(|(page_id, _)| cleanups.get(&page_id).cloned())
        .map(|cleanup| PageRemoval { cleanup })
        .collect()
}

async fn mark_all_contexts(
    pages: &Arc<RwLock<HashMap<PageId, PageContext>>>,
    page_cleanups: &Arc<RwLock<HashMap<PageId, OpenPageCleanup>>>,
) -> Vec<PageRemoval> {
    mark_missing_contexts(pages, page_cleanups, None).await
}

async fn mark_missing_contexts(
    pages: &Arc<RwLock<HashMap<PageId, PageContext>>>,
    page_cleanups: &Arc<RwLock<HashMap<PageId, OpenPageCleanup>>>,
    live: Option<&HashSet<String>>,
) -> Vec<PageRemoval> {
    let mut pages = pages.write().await;
    let removals = pages
        .iter()
        .filter_map(|(page_id, mapped)| {
            let context = match mapped {
                PageContext::Opening(None) => None,
                PageContext::Opening(Some(context)) | PageContext::Ready { context, .. } => {
                    Some(context.clone())
                }
                PageContext::Releasing { .. } => return None,
            };
            if live.is_some_and(|live| {
                context
                    .as_ref()
                    .is_some_and(|context| live.contains(context))
            }) {
                return None;
            }
            Some((page_id.clone(), context))
        })
        .collect::<Vec<_>>();
    for (page_id, context) in &removals {
        pages.insert(
            page_id.clone(),
            PageContext::Releasing {
                context: context.clone(),
            },
        );
    }
    drop(pages);
    let cleanups = page_cleanups.read().await;
    removals
        .into_iter()
        .filter_map(|(page_id, _)| cleanups.get(&page_id).cloned())
        .map(|cleanup| PageRemoval { cleanup })
        .collect()
}

async fn release_removed_pages(
    cleanup_failure: &Arc<TaskMutex<Option<CommandError>>>,
    removals: Vec<PageRemoval>,
) {
    for removal in removals {
        let failures = removal.cleanup.run_destroyed().await;
        if !failures.is_empty() {
            let error = cleanup_failures_error(&failures);
            let mut failure = cleanup_failure
                .lock()
                .expect("cleanup failure mutex poisoned");
            if failure.is_none() {
                *failure = Some(error);
            }
        }
    }
}

async fn release_page_binding_with_retries(
    observer: &Arc<dyn ExtensionObserver>,
    lease: &AttachmentLease,
    page_id: &PageId,
) -> Result<(), CommandError> {
    let timeout = observer.operation_timeout();
    let mut last_error = None;
    for attempt in 0..PAGE_BINDING_RELEASE_ATTEMPTS {
        match tokio::time::timeout(timeout, observer.release_page_binding(lease, page_id)).await {
            Ok(Ok(())) => return Ok(()),
            Ok(Err(error)) => last_error = Some(error),
            Err(_) => {
                last_error = Some(driver_error(
                    ErrorCode::BrowserCommandFailed,
                    format!(
                        "companion page-binding release timed out after {} ms",
                        timeout.as_millis()
                    ),
                    true,
                ));
            }
        }
        if attempt + 1 < PAGE_BINDING_RELEASE_ATTEMPTS {
            tokio::task::yield_now().await;
        }
    }
    let mut error = last_error.expect("at least one binding release attempt is configured");
    error.message = format!(
        "Firefox page-binding cleanup failed after {PAGE_BINDING_RELEASE_ATTEMPTS} attempts: {}",
        error.message
    );
    Err(error)
}

fn cleanup_deadline_error(operation: &str) -> CommandError {
    driver_error(
        ErrorCode::DeadlineExceeded,
        format!("Firefox lifecycle cleanup timed out while {operation}"),
        true,
    )
}

fn cleanup_failures_error(failures: &[String]) -> CommandError {
    driver_error(
        ErrorCode::BrowserCommandFailed,
        format!(
            "Firefox lifecycle cleanup failed while {}",
            failures.join("; ")
        ),
        false,
    )
}

fn live_contexts(response: &Value) -> Option<HashSet<String>> {
    let roots = response.get("contexts")?.as_array()?;
    let mut pending = roots.iter().collect::<Vec<_>>();
    let mut contexts = HashSet::new();
    let mut visited = 0_usize;
    while let Some(context) = pending.pop() {
        visited += 1;
        if visited > MAX_TRACKED_PAGES * 4 {
            return None;
        }
        let id = context
            .get("context")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())?;
        contexts.insert(id.to_owned());
        if let Some(children) = context.get("children") {
            let children = children.as_array()?;
            pending.extend(children);
        }
    }
    Some(contexts)
}

fn validate_observation(observation: &ExtensionObservation) -> Result<(), CommandError> {
    let bounded = observation.url.len() <= MAX_URL_BYTES
        && observation.title.len() <= MAX_TITLE_BYTES
        && observation.visible_text.len() <= MAX_VISIBLE_TEXT_BYTES
        && observation.controls.len() <= MAX_CONTROL_COUNT
        && observation
            .html
            .as_ref()
            .is_none_or(|html| html.len() <= MAX_SANITIZED_HTML_BYTES)
        && observation.controls.iter().all(|control| {
            control.css_path.len() <= MAX_SELECTOR_BYTES
                && [
                    control.role.as_deref(),
                    control.name.as_deref(),
                    control.label.as_deref(),
                    control.value.as_deref(),
                ]
                .into_iter()
                .flatten()
                .all(|value| value.len() <= MAX_CONTROL_FIELD_BYTES)
        })
        && serde_json::to_vec(observation)
            .is_ok_and(|encoded| encoded.len() <= MAX_OBSERVATION_BYTES);
    if !bounded {
        return Err(driver_error(
            ErrorCode::BrowserCommandFailed,
            "extension observation exceeded a companion safety bound",
            false,
        ));
    }
    let safe = [
        Some(observation.url.as_str()),
        Some(observation.title.as_str()),
        Some(observation.visible_text.as_str()),
        observation.html.as_deref(),
    ]
    .into_iter()
    .flatten()
    .chain(observation.controls.iter().flat_map(|control| {
        [
            Some(control.css_path.as_str()),
            control.role.as_deref(),
            control.name.as_deref(),
            control.label.as_deref(),
            control.value.as_deref(),
        ]
        .into_iter()
        .flatten()
    }))
    .all(|value| !contains_sensitive_material(value));
    if !safe {
        return Err(driver_error(
            ErrorCode::BrowserCommandFailed,
            "extension observation contained unsanitized sensitive material",
            false,
        ));
    }
    Ok(())
}

fn contains_sensitive_material(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "authorization",
        "bearer ",
        "token",
        "secret",
        "password",
        "passwd",
        "api-key",
        "api_key",
        "credential",
        "<script",
        " onclick=",
        " onload=",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn bounded_text_matches(matcher: &TextMatch, value: &str) -> Result<bool, CommandError> {
    let target = types::TargetSpec {
        text: Some(matcher.clone()),
        ..Default::default()
    };
    let candidate = Candidate {
        id: "wait-value".into(),
        css: None,
        test_id: None,
        role: None,
        name: None,
        label: None,
        text: value.to_owned(),
        attributes: Default::default(),
        state: CandidateState {
            attached: true,
            visible: true,
            enabled: true,
        },
    };
    resolve_candidates(&target, &[candidate], &ResolutionPolicy::default())
        .map(|decision| matches!(decision, ResolutionDecision::Resolved { .. }))
        .map_err(|error| driver_error(ErrorCode::InvalidRequest, error.to_string(), false))
}

fn pointer_actions(context: &str, shared_id: &str) -> Value {
    json!({
        "context": context,
        "actions": [{
            "type": "pointer",
            "id": "automation-runtime-pointer",
            "parameters": {"pointerType": "mouse"},
            "actions": [
                {
                    "type": "pointerMove",
                    "x": 0,
                    "y": 0,
                    "duration": 0,
                    "origin": {"type": "element", "element": {"sharedId": shared_id}}
                },
                {"type": "pointerDown", "button": 0},
                {"type": "pointerUp", "button": 0}
            ]
        }]
    })
}

fn keyboard_actions(context: &str, value: &str, clear_first: bool) -> Value {
    let mut actions = Vec::new();
    if clear_first {
        let modifier = if cfg!(target_os = "macos") {
            "\u{e03d}"
        } else {
            "\u{e009}"
        };
        actions.extend([
            json!({"type": "keyDown", "value": modifier}),
            json!({"type": "keyDown", "value": "a"}),
            json!({"type": "keyUp", "value": "a"}),
            json!({"type": "keyUp", "value": modifier}),
            json!({"type": "keyDown", "value": "\u{e003}"}),
            json!({"type": "keyUp", "value": "\u{e003}"}),
        ]);
    }
    for character in value.chars() {
        let character = character.to_string();
        actions.push(json!({"type": "keyDown", "value": character}));
        actions.push(json!({"type": "keyUp", "value": character}));
    }
    json!({
        "context": context,
        "actions": [{
            "type": "key",
            "id": "automation-runtime-keyboard",
            "actions": actions,
        }]
    })
}

/// Renews the worker's attachment lease at half the remaining TTL, keeping
/// long-lived sessions usable past the original attachment expiry. A renewal
/// failure is retried shortly; once the lease actually expires the task stops
/// and operation-level lease validation reports the expiry.
async fn renew_lease_task(
    lease: Arc<std::sync::RwLock<AttachmentLease>>,
    observer: Arc<dyn ExtensionObserver>,
) {
    loop {
        let wait = {
            let current = lease
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .clone();
            let remaining = current.expires_at.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return;
            }
            remaining.div_f64(2.0).max(Duration::from_millis(250))
        };
        tokio::time::sleep(wait).await;
        let current = lease
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        match observer.renew_lease(&current).await {
            Ok(renewed) => {
                *lease
                    .write()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = renewed;
            }
            Err(_) => {
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        }
    }
}

fn validate_lease(lease: &AttachmentLease) -> Result<(), CommandError> {
    if lease.expires_at <= Instant::now() {
        return Err(lease_error());
    }
    if lease.identity.engine != BrowserEngine::Firefox {
        return Err(driver_error(
            ErrorCode::InvalidRequest,
            "Firefox companion worker requires a Firefox attachment lease",
            false,
        ));
    }
    Ok(())
}

fn deadline_unix_ms(timeout: Duration) -> i64 {
    let deadline = SystemTime::now()
        .checked_add(timeout)
        .unwrap_or(SystemTime::now());
    deadline
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

fn session_error(error: CompanionSessionError) -> CommandError {
    match error {
        CompanionSessionError::DeadlineExceeded
        | CompanionSessionError::ResponseTimeout
        | CompanionSessionError::BindingExpired => {
            driver_error(ErrorCode::DeadlineExceeded, error.to_string(), true)
        }
        CompanionSessionError::ConnectionClosed
        | CompanionSessionError::QueueClosed
        | CompanionSessionError::ProfileUnavailable => {
            driver_error(ErrorCode::BrowserCommandFailed, error.to_string(), true)
        }
        CompanionSessionError::PageMismatch => {
            driver_error(ErrorCode::NotFound, error.to_string(), false)
        }
        CompanionSessionError::PendingCapacity | CompanionSessionError::BindingCapacity => {
            driver_error(ErrorCode::ResourceExhausted, error.to_string(), true)
        }
        _ => driver_error(ErrorCode::BrowserCommandFailed, error.to_string(), false),
    }
}

fn page_missing() -> CommandError {
    driver_error(
        ErrorCode::NotFound,
        "page has no Firefox browsing context",
        false,
    )
}

fn lease_error() -> CommandError {
    driver_error(
        ErrorCode::PolicyDenied,
        "Firefox companion attachment lease is expired",
        false,
    )
}

fn capability_error(capability: &str) -> CommandError {
    driver_error(
        ErrorCode::PolicyDenied,
        format!("Firefox companion lease does not grant {capability}"),
        false,
    )
}

fn direct_target_selector(target: &types::TargetSpec) -> Option<String> {
    if let Some(css) = target.css.as_ref().filter(|css| !css.is_empty()) {
        return Some(css.clone());
    }
    target
        .test_id
        .as_ref()
        .filter(|value| !value.is_empty())
        .map(|value| {
            let escaped = value.chars().fold(String::new(), |mut output, character| {
                if character == '\\' || character == '"' {
                    output.push('\\');
                }
                output.push(character);
                output
            });
            format!("[data-testid=\"{escaped}\"]")
        })
}

fn popup_context_from_event(event: &BidiEvent, opener: &str) -> Option<(String, String)> {
    if event.method != "browsingContext.contextCreated" {
        return None;
    }
    if event.params.get("originalOpener").and_then(Value::as_str) != Some(opener) {
        return None;
    }
    let id = event.params.get("context")?.as_str()?.to_owned();
    let url = event
        .params
        .get("url")
        .and_then(Value::as_str)
        .unwrap_or("about:blank")
        .to_owned();
    Some((id, url))
}

fn driver_error(code: ErrorCode, message: impl Into<String>, retryable: bool) -> CommandError {
    CommandError {
        code,
        message: message.into(),
        layer: ErrorLayer::Driver,
        retryable,
    }
}

fn engine_name(engine: &BrowserEngine) -> &'static str {
    match engine {
        BrowserEngine::Firefox => "firefox",
        BrowserEngine::Chromium => "chromium",
        BrowserEngine::WebKit => "webkit",
    }
}

fn interaction_path_name(path: InteractionPath) -> &'static str {
    match path {
        InteractionPath::EngineNative => "engineNative",
        InteractionPath::ExtensionApi => "extensionApi",
        InteractionPath::HostNative => "hostNative",
    }
}

#[cfg(test)]
mod cleanup_state_tests {
    use super::*;
    use companion_protocol::{BrowserIdentity, CompanionCapabilities};
    use tokio::sync::broadcast;
    use types::{AttachmentId, CompanionId, ProfileId};

    struct UnusedTransport;

    #[async_trait]
    impl BidiTransport for UnusedTransport {
        async fn send(&self, _method: &str, _params: Value) -> Result<Value, CommandError> {
            panic!("cleanup state-boundary test must not use the transport")
        }

        fn subscribe_events(&self) -> Option<broadcast::Receiver<crate::bidi::BidiEvent>> {
            None
        }
    }

    struct UnusedObserver;

    #[async_trait]
    impl ExtensionObserver for UnusedObserver {
        async fn begin_page_binding(
            &self,
            _lease: &AttachmentLease,
            _page_id: &PageId,
        ) -> Result<Box<dyn ExtensionPageBinding>, CommandError> {
            panic!("cleanup state-boundary test must not begin a binding")
        }

        async fn observe(
            &self,
            _lease: &AttachmentLease,
            _page_id: &PageId,
            _command: &InspectCommand,
        ) -> Result<ExtensionObservation, CommandError> {
            panic!("cleanup state-boundary test must not observe")
        }

        async fn release_page_binding(
            &self,
            _lease: &AttachmentLease,
            _page_id: &PageId,
        ) -> Result<(), CommandError> {
            panic!("cleanup state-boundary test must not release a binding")
        }
    }

    fn test_cleanup() -> OpenPageCleanup {
        let registry = Arc::new(RwLock::new(HashMap::new()));
        OpenPageCleanup::new(
            PageOpenResources {
                lease: AttachmentLease {
                    attachment_id: AttachmentId::new(),
                    companion_id: CompanionId::new(),
                    profile_id: ProfileId::new(),
                    identity: BrowserIdentity {
                        engine: BrowserEngine::Firefox,
                        browser_name: "Firefox".into(),
                        browser_version: "128.0".into(),
                        os: "test".into(),
                        profile_label: "default-release".into(),
                    },
                    capabilities: CompanionCapabilities {
                        observe: true,
                        navigate: true,
                        native_input: true,
                        tabs: true,
                        frames: true,
                        native_dialogs: false,
                    },
                    expires_at: Instant::now() + Duration::from_secs(30),
                },
                transport: Arc::new(UnusedTransport),
                observer: Arc::new(UnusedObserver),
                pages: Arc::new(RwLock::new(HashMap::new())),
                page_cleanups: Arc::downgrade(&registry),
                cleanup_timeout: Duration::from_millis(10),
            },
            PageId::new(),
            Arc::new(AtomicBool::new(false)),
        )
    }

    #[tokio::test]
    async fn resource_setters_wait_for_cleanup_boundary_before_clearing_completed_stages() {
        let cleanup = test_cleanup();
        cleanup.execution.stage.store(
            CLEANUP_RESTORE_DONE | CLEANUP_CLOSE_DONE | CLEANUP_BINDING_DONE,
            Ordering::Release,
        );
        let boundary = cleanup.execution.run.lock().await;
        let setter_cleanup = cleanup.clone();
        let setters = tokio::spawn(async move {
            setter_cleanup.binding_started().await;
            setter_cleanup.context_created("late-context".into()).await;
            setter_cleanup.title_captured("Late title".into()).await;
        });

        tokio::task::yield_now().await;
        assert!(!setters.is_finished());
        assert_eq!(cleanup.details().context, None);
        assert_eq!(
            cleanup.execution.stage.load(Ordering::Acquire),
            CLEANUP_RESTORE_DONE | CLEANUP_CLOSE_DONE | CLEANUP_BINDING_DONE
        );

        drop(boundary);
        setters.await.unwrap();
        let details = cleanup.details();
        assert!(details.binding_started);
        assert_eq!(details.context.as_deref(), Some("late-context"));
        assert_eq!(details.original_title.as_deref(), Some("Late title"));
        assert_eq!(
            cleanup.execution.stage.load(Ordering::Acquire)
                & (CLEANUP_RESTORE_DONE | CLEANUP_CLOSE_DONE | CLEANUP_BINDING_DONE),
            0
        );
    }
}
