use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex as TaskMutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use async_trait::async_trait;
use companion_core::{AttachmentLease, CompanionServerHandle, CompanionSessionError};
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
pub const MAX_TRACKED_PAGES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionObservation {
    pub url: String,
    pub title: String,
    pub text: String,
    pub html: Option<String>,
}

#[async_trait]
pub trait ExtensionObserver: Send + Sync {
    async fn observe(
        &self,
        lease: &AttachmentLease,
        page_id: &PageId,
        command: &InspectCommand,
    ) -> Result<ExtensionObservation, CommandError>;
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
                serde_json::from_value(result.output).map_err(|error| {
                    driver_error(
                        ErrorCode::BrowserCommandFailed,
                        format!("invalid extension observation: {error}"),
                        false,
                    )
                })
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
        )?;
        Ok(Arc::new(worker))
    }
}

pub struct FirefoxCompanionWorker {
    id: WorkerId,
    profile_dir: PathBuf,
    lease: AttachmentLease,
    transport: Arc<dyn BidiTransport>,
    observer: Arc<dyn ExtensionObserver>,
    pages: Arc<RwLock<HashMap<PageId, String>>>,
    cleanup_task: TaskMutex<Option<JoinHandle<()>>>,
}

impl FirefoxCompanionWorker {
    pub fn new(
        id: WorkerId,
        profile_dir: PathBuf,
        lease: AttachmentLease,
        transport: Arc<dyn BidiTransport>,
        observer: Arc<dyn ExtensionObserver>,
    ) -> Result<Self, CommandError> {
        validate_lease(&lease)?;
        let pages = Arc::new(RwLock::new(HashMap::<PageId, String>::new()));
        let cleanup_task = transport.subscribe_events().and_then(|mut events| {
            let pages = Arc::clone(&pages);
            tokio::runtime::Handle::try_current().ok().map(|runtime| {
                runtime.spawn(async move {
                    loop {
                        match events.recv().await {
                            Ok(event) if event.method == "browsingContext.contextDestroyed" => {
                                if let Some(context) =
                                    event.params.get("context").and_then(Value::as_str)
                                {
                                    pages.write().await.retain(|_, mapped| mapped != context);
                                }
                            }
                            Ok(_) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                })
            })
        });
        Ok(Self {
            id,
            profile_dir,
            lease,
            transport,
            observer,
            pages,
            cleanup_task: TaskMutex::new(cleanup_task),
        })
    }

    async fn context(&self, page_id: &PageId) -> Result<String, CommandError> {
        self.ensure_active()?;
        self.pages
            .read()
            .await
            .get(page_id)
            .cloned()
            .ok_or_else(page_missing)
    }

    fn ensure_active(&self) -> Result<(), CommandError> {
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

#[async_trait]
impl BrowserWorker for FirefoxCompanionWorker {
    fn worker_id(&self) -> WorkerId {
        self.id.clone()
    }

    fn profile_dir(&self) -> &Path {
        &self.profile_dir
    }

    async fn open_page(&self, page_id: PageId) -> Result<(), CommandError> {
        self.ensure_active()?;
        if !self.lease.capabilities.tabs {
            return Err(capability_error("tab creation"));
        }
        let mut pages = self.pages.write().await;
        if pages.contains_key(&page_id) {
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
        let response = self
            .transport
            .send("browsingContext.create", json!({"type": "tab"}))
            .await?;
        let context = response
            .get("context")
            .and_then(Value::as_str)
            .filter(|context| !context.is_empty())
            .ok_or_else(|| {
                driver_error(
                    ErrorCode::BrowserCommandFailed,
                    "Firefox BiDi did not return a browsing context",
                    false,
                )
            })?;
        pages.insert(page_id, context.to_owned());
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
                text: observation.text,
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
        self.open_page(page_id.clone()).await?;
        let (url, title) = if let Some(url) = &command.url {
            let navigation = self
                .navigate(
                    &page_id,
                    &NavigateCommand {
                        url: url.clone(),
                        wait_until: WaitUntil::NetworkIdle,
                        timeout_ms: DEFAULT_NAVIGATION_TIMEOUT.as_millis() as u64,
                    },
                )
                .await?;
            navigation
                .into_iter()
                .find_map(|evidence| match evidence {
                    Evidence::Navigation { url, title } => Some((url, title)),
                    _ => None,
                })
                .unwrap_or_else(|| (url.clone(), String::new()))
        } else {
            ("about:blank".into(), String::new())
        };
        Ok(vec![
            Evidence::Page {
                page_id,
                url,
                title,
            },
            self.evidence(InteractionPath::EngineNative),
        ])
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
        if pages.get(&command.page_id) == Some(&context) {
            pages.remove(&command.page_id);
        }
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
        CompanionSessionError::DeadlineExceeded | CompanionSessionError::ResponseTimeout => {
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
        CompanionSessionError::PendingCapacity => {
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
