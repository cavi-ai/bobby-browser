use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex as TaskMutex,
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use companion_core::{
    AttachmentLease, CompanionServerHandle, CompanionSessionError, PageBindingTicket,
};
use companion_protocol::{
    ActionRequest, BrowserEngine, CompanionEvent, InteractionPath, PROTOCOL_VERSION,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::{sync::RwLock, task::JoinHandle};
use types::{
    ClickCommand, ClosePageCommand, CommandError, CommandId, ErrorCode, ErrorLayer, Evidence,
    InspectCommand, NavigateCommand, OpenPageCommand, PageId, SessionId, TypeTextCommand,
    WaitUntil, WorkerId,
};
use url::Url;
use worker_pool::{BrowserWorker, WorkerFactory};

use crate::bidi::{BidiClient, BidiTransport};

const COMPANION_SANDBOX: &str = "automation-runtime-companion";
const DEFAULT_NAVIGATION_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_TYPE_CODEPOINTS: usize = 4_096;
const MAX_OBSERVATION_BYTES: usize = 1024 * 1024 - 64 * 1024;
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ExtensionControl {
    pub css_path: String,
    #[serde(default)]
    pub role: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub value: Option<String>,
    pub disabled: bool,
}

#[async_trait]
pub trait ExtensionPageBinding: Send {
    fn nonce(&self) -> &str;

    async fn complete(self: Box<Self>) -> Result<(), CommandError>;
}

#[async_trait]
pub trait ExtensionObserver: Send + Sync {
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
                ErrorCode::BrowserCommandFailed,
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
        }
    }
}

#[async_trait]
impl WorkerFactory for FirefoxCompanionFactory {
    async fn launch(
        &self,
        _session_id: &SessionId,
    ) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        validate_lease(&self.lease)?;
        let transport = Arc::new(BidiClient::connect(self.bidi_url.clone(), self.timeout).await?);
        let worker = FirefoxCompanionWorker::new(
            WorkerId::new(),
            self.profile_dir.clone(),
            self.lease.clone(),
            transport,
            Arc::clone(&self.observer),
        )
        .await?;
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
    lease: AttachmentLease,
    transport: Arc<dyn BidiTransport>,
    observer: Arc<dyn ExtensionObserver>,
    pages: Arc<RwLock<HashMap<PageId, PageContext>>>,
    cleanup_failure: Arc<TaskMutex<Option<CommandError>>>,
    cleanup_task: TaskMutex<Option<JoinHandle<()>>>,
}

#[derive(Clone)]
struct PageOpenResources {
    lease: AttachmentLease,
    transport: Arc<dyn BidiTransport>,
    observer: Arc<dyn ExtensionObserver>,
    pages: Arc<RwLock<HashMap<PageId, PageContext>>>,
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

struct OpenPageCleanup {
    resources: PageOpenResources,
    page_id: PageId,
    context: Option<String>,
    original_title: Option<String>,
    binding_started: bool,
}

impl OpenPageCleanup {
    async fn run(self) -> Vec<String> {
        let mut failures = Vec::new();
        if let Some(context) = self.context.as_deref() {
            if let Some(title) = self.original_title.as_deref() {
                if let Err(error) =
                    restore_context_title(&self.resources.transport, context, title).await
                {
                    failures.push(format!(
                        "restoring the original page title: {}",
                        error.message
                    ));
                }
            }
            if let Err(error) = self
                .resources
                .transport
                .send("browsingContext.close", json!({"context": context}))
                .await
            {
                failures.push(format!("closing the Firefox context: {}", error.message));
            }
        }
        remove_page_mapping(
            &self.resources.pages,
            &self.page_id,
            self.context.as_deref(),
        )
        .await;
        if self.binding_started {
            if let Err(error) = self
                .resources
                .observer
                .release_page_binding(&self.resources.lease, &self.page_id)
                .await
            {
                failures.push(format!(
                    "releasing the companion page binding: {}",
                    error.message
                ));
            }
        }
        failures
    }
}

struct OpenPageGuard {
    cleanup: Option<OpenPageCleanup>,
}

impl OpenPageGuard {
    fn new(resources: PageOpenResources, page_id: PageId) -> Self {
        Self {
            cleanup: Some(OpenPageCleanup {
                resources,
                page_id,
                context: None,
                original_title: None,
                binding_started: false,
            }),
        }
    }

    fn binding_started(&mut self) {
        self.cleanup
            .as_mut()
            .expect("open-page cleanup is armed")
            .binding_started = true;
    }

    fn context_created(&mut self, context: String) {
        self.cleanup
            .as_mut()
            .expect("open-page cleanup is armed")
            .context = Some(context);
    }

    fn title_captured(&mut self, title: String) {
        self.cleanup
            .as_mut()
            .expect("open-page cleanup is armed")
            .original_title = Some(title);
    }

    async fn fail(mut self, mut primary: CommandError) -> CommandError {
        if let Some(cleanup) = self.cleanup.take() {
            let failures = cleanup.run().await;
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

    fn disarm(mut self) {
        self.cleanup = None;
    }
}

impl Drop for OpenPageGuard {
    fn drop(&mut self) {
        let Some(cleanup) = self.cleanup.take() else {
            return;
        };
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
}

impl FirefoxCompanionWorker {
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
                json!({"events": ["browsingContext.contextDestroyed"]}),
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
        let cleanup_pages = Arc::clone(&pages);
        let cleanup_transport = Arc::clone(&transport);
        let cleanup_observer = Arc::clone(&observer);
        let cleanup_lease = lease.clone();
        let cleanup_failure = Arc::new(TaskMutex::new(None));
        let task_failure = Arc::clone(&cleanup_failure);
        let cleanup_task = Some(tokio::spawn(async move {
            loop {
                match events.recv().await {
                    Ok(event) if event.method == "browsingContext.contextDestroyed" => {
                        if let Some(context) = event.params.get("context").and_then(Value::as_str) {
                            let removals = mark_destroyed_context(&cleanup_pages, context).await;
                            release_removed_pages(
                                &cleanup_observer,
                                &cleanup_lease,
                                &cleanup_pages,
                                &task_failure,
                                removals,
                            )
                            .await;
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        reconcile_contexts(
                            &cleanup_transport,
                            &cleanup_observer,
                            &cleanup_lease,
                            &cleanup_pages,
                            &task_failure,
                        )
                        .await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        let removals = mark_all_contexts(&cleanup_pages).await;
                        release_removed_pages(
                            &cleanup_observer,
                            &cleanup_lease,
                            &cleanup_pages,
                            &task_failure,
                            removals,
                        )
                        .await;
                        break;
                    }
                }
            }
        }));
        Ok(Self {
            id,
            profile_dir,
            lease,
            transport,
            observer,
            pages,
            cleanup_failure,
            cleanup_task: TaskMutex::new(cleanup_task),
        })
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
        if let Some(error) = self
            .cleanup_failure
            .lock()
            .expect("cleanup failure mutex poisoned")
            .clone()
        {
            return Err(error);
        }
        if self.lease.expires_at <= Instant::now() {
            return Err(lease_error());
        }
        Ok(())
    }

    fn evidence(&self, interaction_path: InteractionPath) -> Evidence {
        Evidence::BrowserExecution {
            engine: engine_name(&self.lease.identity.engine).into(),
            browser_version: self.lease.identity.browser_version.clone(),
            profile_id: self.lease.profile_id.0.to_string(),
            interaction_path: interaction_path_name(interaction_path).into(),
        }
    }

    async fn reserve_page(&self, page_id: &PageId) -> Result<(), CommandError> {
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
        Ok(())
    }

    async fn open_page_owned(&self, page_id: PageId) -> Result<OpenPageGuard, CommandError> {
        self.ensure_active()?;
        if !self.lease.capabilities.tabs {
            return Err(capability_error("tab creation"));
        }
        self.reserve_page(&page_id).await?;
        let resources = PageOpenResources {
            lease: self.lease.clone(),
            transport: Arc::clone(&self.transport),
            observer: Arc::clone(&self.observer),
            pages: Arc::clone(&self.pages),
        };
        let cancelled = Arc::new(AtomicBool::new(false));
        let mut caller_cancellation = CallerCancellation::new(Arc::clone(&cancelled));
        let operation = tokio::spawn(
            PageOpenOperation {
                resources,
                page_id,
                cancelled,
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

    async fn resolve_element(&self, context: &str, selector: &str) -> Result<String, CommandError> {
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
                    ErrorCode::NotFound,
                    "native input target was not found",
                    false,
                )
            })
    }

    async fn perform_pointer_click(
        &self,
        context: &str,
        shared_id: &str,
    ) -> Result<(), CommandError> {
        self.transport
            .send("input.performActions", pointer_actions(context, shared_id))
            .await?;
        Ok(())
    }
}

impl PageOpenOperation {
    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    async fn run(self) -> Result<OpenPageGuard, CommandError> {
        let mut guard = OpenPageGuard::new(self.resources.clone(), self.page_id.clone());
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
        guard.binding_started();
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
        guard.context_created(context.clone());
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
        guard.title_captured(original_title.clone());
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
        guard.disarm();
        Ok(())
    }

    async fn navigate(
        &self,
        page_id: &PageId,
        command: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        if !self.lease.capabilities.navigate {
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
        if !self.lease.capabilities.observe {
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
        let mut observation = self.observer.observe(&self.lease, page_id, command).await?;
        if !command.include_html {
            observation.html = None;
        }
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
                text: observation.visible_text,
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
        if !self.lease.capabilities.native_input {
            return Err(capability_error("native pointer input"));
        }
        let context = self.context(page_id).await?;
        let selector = command
            .target
            .as_ref()
            .and_then(|target| target.css.as_deref())
            .unwrap_or(&command.selector);
        let shared_id = self.resolve_element(&context, selector).await?;
        self.perform_pointer_click(&context, &shared_id).await?;
        Ok(vec![
            Evidence::Element {
                selector: command.selector.clone(),
                text: None,
            },
            self.evidence(InteractionPath::EngineNative),
        ])
    }

    async fn type_text(
        &self,
        page_id: &PageId,
        command: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        if !self.lease.capabilities.native_input {
            return Err(capability_error("native keyboard input"));
        }
        if command.value.chars().count() > MAX_TYPE_CODEPOINTS {
            return Err(driver_error(
                ErrorCode::InvalidRequest,
                format!("native keyboard input exceeds {MAX_TYPE_CODEPOINTS} codepoints"),
                false,
            ));
        }
        let context = self.context(page_id).await?;
        let selector = command
            .target
            .as_ref()
            .and_then(|target| target.css.as_deref())
            .unwrap_or(&command.selector);
        let shared_id = self.resolve_element(&context, selector).await?;
        self.perform_pointer_click(&context, &shared_id).await?;
        self.transport
            .send(
                "input.performActions",
                keyboard_actions(&context, &command.value, command.clear_first),
            )
            .await?;
        Ok(vec![
            Evidence::Element {
                selector: command.selector.clone(),
                text: None,
            },
            self.evidence(InteractionPath::EngineNative),
        ])
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
        guard.disarm();
        Ok(evidence)
    }

    async fn close_page_command(
        &self,
        command: &ClosePageCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let context = self.context(&command.page_id).await?;
        self.transport
            .send("browsingContext.close", json!({"context": context.clone()}))
            .await?;
        let mut pages = self.pages.write().await;
        if matches!(
            pages.get(&command.page_id),
            Some(PageContext::Ready { context: mapped, .. }) if mapped == &context
        ) {
            pages.remove(&command.page_id);
        }
        drop(pages);
        self.observer
            .release_page_binding(&self.lease, &command.page_id)
            .await?;
        Ok(vec![self.evidence(InteractionPath::EngineNative)])
    }

    async fn close(&self) -> Result<(), CommandError> {
        if let Some(task) = self
            .cleanup_task
            .lock()
            .expect("cleanup task mutex poisoned")
            .take()
        {
            task.abort();
        }
        self.pages.write().await.clear();
        self.transport.close().await
    }
}

impl Drop for FirefoxCompanionWorker {
    fn drop(&mut self) {
        if let Ok(task) = self.cleanup_task.get_mut() {
            if let Some(task) = task.take() {
                task.abort();
            }
        }
    }
}

async fn reconcile_contexts(
    transport: &Arc<dyn BidiTransport>,
    observer: &Arc<dyn ExtensionObserver>,
    lease: &AttachmentLease,
    pages: &Arc<RwLock<HashMap<PageId, PageContext>>>,
    cleanup_failure: &Arc<TaskMutex<Option<CommandError>>>,
) {
    let live = match transport.send("browsingContext.getTree", json!({})).await {
        Ok(response) => live_contexts(&response),
        Err(_) => None,
    };
    let removals = mark_missing_contexts(pages, live.as_ref()).await;
    release_removed_pages(observer, lease, pages, cleanup_failure, removals).await;
}

#[derive(Clone)]
struct PageRemoval {
    page_id: PageId,
    context: Option<String>,
}

async fn mark_destroyed_context(
    pages: &Arc<RwLock<HashMap<PageId, PageContext>>>,
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
            Some(PageRemoval {
                page_id: page_id.clone(),
                context: Some(context),
            })
        })
        .collect::<Vec<_>>();
    for removal in &removals {
        pages.insert(
            removal.page_id.clone(),
            PageContext::Releasing {
                context: removal.context.clone(),
            },
        );
    }
    removals
}

async fn mark_all_contexts(pages: &Arc<RwLock<HashMap<PageId, PageContext>>>) -> Vec<PageRemoval> {
    mark_missing_contexts(pages, None).await
}

async fn mark_missing_contexts(
    pages: &Arc<RwLock<HashMap<PageId, PageContext>>>,
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
            Some(PageRemoval {
                page_id: page_id.clone(),
                context,
            })
        })
        .collect::<Vec<_>>();
    for removal in &removals {
        pages.insert(
            removal.page_id.clone(),
            PageContext::Releasing {
                context: removal.context.clone(),
            },
        );
    }
    removals
}

async fn release_removed_pages(
    observer: &Arc<dyn ExtensionObserver>,
    lease: &AttachmentLease,
    pages: &Arc<RwLock<HashMap<PageId, PageContext>>>,
    cleanup_failure: &Arc<TaskMutex<Option<CommandError>>>,
    removals: Vec<PageRemoval>,
) {
    for removal in removals {
        let mut last_error = None;
        for attempt in 0..PAGE_BINDING_RELEASE_ATTEMPTS {
            match observer.release_page_binding(lease, &removal.page_id).await {
                Ok(()) => {
                    last_error = None;
                    break;
                }
                Err(error) => {
                    last_error = Some(error);
                    if attempt + 1 < PAGE_BINDING_RELEASE_ATTEMPTS {
                        tokio::task::yield_now().await;
                    }
                }
            }
        }
        remove_page_mapping(pages, &removal.page_id, removal.context.as_deref()).await;
        if let Some(mut error) = last_error {
            error.message = format!(
                "Firefox page-binding cleanup failed after {PAGE_BINDING_RELEASE_ATTEMPTS} attempts: {}",
                error.message
            );
            let mut failure = cleanup_failure
                .lock()
                .expect("cleanup failure mutex poisoned");
            if failure.is_none() {
                *failure = Some(error);
            }
        }
    }
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
