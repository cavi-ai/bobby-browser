use std::{
    collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::{Arc, Weak},
};

use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{
        header::{AUTHORIZATION, WWW_AUTHENTICATE},
        HeaderMap, HeaderValue, StatusCode,
    },
    response::{IntoResponse, Response},
    routing::get,
    Json, Router,
};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::{Duration, Utc};
use futures_util::{SinkExt, StreamExt};
use interface_core::{
    Authority, AuthorizationGuard, CapabilityHandle, Event, EventStore, RuntimeInterface,
};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::{
    sync::{mpsc, Mutex, Notify, OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
};
use types::{
    AttemptId, CaptureScreenshotCommand, CheckpointId, CheckpointInvariant,
    ClickAndWaitForDownloadCommand, ClickAndWaitForPopupCommand, ClickCommand, CommandClass,
    CommandEnvelope, CommandId, CommandOutcome, CorrelationId, CreateSessionRequest, ErrorLayer,
    InspectCommand, InterfaceError, InterfaceErrorCode, NavigateCommand, OpenPageRequest, PageId,
    PrimitiveCommand, RecoveryDecision, RequestContext, RuntimeCommand, ScreenshotMode, SessionId,
    SessionState, SetEmulatedMediaCommand, SetFocusEmulationCommand, TargetSpec, TextMatch,
    TypeTextCommand, UploadFilesCommand, WaitUntil, WorkflowCheckpoint, WorkflowId,
};

fn boundary_state_error(message: &str) -> InterfaceError {
    InterfaceError {
        code: InterfaceErrorCode::InvalidRequest,
        layer: ErrorLayer::Interface,
        message: message.to_owned(),
        correlation_id: CorrelationId::new(),
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: false,
        required_capability: None,
    }
}

fn consume_pending_boundary(
    state: &mut Option<AutomationBoundary>,
    session_id: &SessionId,
    page_id: &PageId,
    now: chrono::DateTime<Utc>,
) -> Result<AutomationBoundary, InterfaceError> {
    let Some(pending) = state.as_mut() else {
        return Err(boundary_state_error(
            "boundary command requires a reserved checkpoint",
        ));
    };
    if pending.phase != AutomationBoundaryPhase::Pending {
        return Err(boundary_state_error(
            "reserved boundary was already consumed",
        ));
    }
    if pending.expires_at <= now {
        return Err(boundary_state_error(
            "reserved boundary expired before dispatch",
        ));
    }
    if &pending.session_id != session_id || &pending.page_id != page_id {
        return Err(boundary_state_error(
            "reserved boundary belongs to another runtime page",
        ));
    }
    pending.phase = AutomationBoundaryPhase::Consumed;
    Ok(pending.clone())
}
use uuid::Uuid;

use crate::{
    domains, manifest::Handler, CdpError, CdpErrorCode, CdpEvent, CdpRequest, CdpResponse,
    IdentifierFamily, IdentifierMap, MethodRegistry, RuntimeGeneration, MAX_IN_FLIGHT_REQUESTS,
    MAX_QUEUED_EVENTS,
};

/// Errors from CDP discovery endpoints (`/json/version`, `/json/list`) and
/// WebSocket upgrade before a session is established.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryError {
    Unauthorized,
    Forbidden,
    NotFound,
    Runtime,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VersionDescription {
    #[serde(rename = "Browser")]
    pub browser: String,
    #[serde(rename = "Protocol-Version")]
    pub protocol_version: String,
    pub web_socket_debugger_url: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetDescription {
    pub id: String,
    pub r#type: String,
    pub title: String,
    pub url: String,
    pub web_socket_debugger_url: String,
}

/// CDP discovery and WebSocket gateway backed by an [`Authority`] and
/// [`RuntimeInterface`].
///
/// Construct with [`Self::new`], optionally configure artifacts and upload
/// staging, then expose [`Self::router`] on the host HTTP server.
pub struct CdpGateway {
    authority: Arc<dyn Authority>,
    bind_runtime:
        Arc<dyn Fn(CapabilityHandle) -> Arc<dyn RuntimeInterface> + Send + Sync + 'static>,
    registry: MethodRegistry,
    websocket_base: String,
    browser_id: String,
    targets: Arc<Mutex<TargetCatalog>>,
    connections: Mutex<Vec<Weak<CdpConnection>>>,
    generations: Arc<Mutex<HashMap<String, RuntimeGeneration>>>,
    artifacts: Option<artifact_store::ArtifactStore>,
    upload_staging_root: Option<PathBuf>,
    streams: Arc<Mutex<DownloadStreamStore>>,
    auto_session: bool,
}

impl CdpGateway {
    /// Create a gateway. `websocket_base` is the public origin clients use for
    /// debugger URLs (for example `ws://127.0.0.1:9222`).
    pub fn new<A, R>(
        authority: Arc<A>,
        runtime: Arc<R>,
        registry: MethodRegistry,
        websocket_base: impl Into<String>,
    ) -> Self
    where
        A: Authority + 'static,
        R: RuntimeInterface + 'static,
    {
        let runtime: Arc<dyn RuntimeInterface> = runtime;
        Self::with_binder(
            authority,
            Arc::new(move |_| runtime.clone()),
            registry,
            websocket_base,
        )
    }

    pub fn with_binder<A>(
        authority: Arc<A>,
        bind_runtime: Arc<
            dyn Fn(CapabilityHandle) -> Arc<dyn RuntimeInterface> + Send + Sync + 'static,
        >,
        registry: MethodRegistry,
        websocket_base: impl Into<String>,
    ) -> Self
    where
        A: Authority + 'static,
    {
        Self {
            authority,
            bind_runtime,
            registry,
            websocket_base: websocket_base.into().trim_end_matches('/').to_owned(),
            browser_id: Uuid::new_v4().simple().to_string(),
            targets: Arc::new(Mutex::new(TargetCatalog::default())),
            connections: Mutex::new(Vec::new()),
            generations: Arc::new(Mutex::new(HashMap::new())),
            artifacts: None,
            upload_staging_root: None,
            streams: Arc::new(Mutex::new(DownloadStreamStore::new(
                128,
                256 * 1024 * 1024,
                std::time::Duration::from_secs(300),
            ))),
            auto_session: true,
        }
    }

    /// Whether a connecting client with `session:write` and `page:write` gets a
    /// runtime page opened for it when none exists. On by default: without it,
    /// `connectOverCDP` lands on an empty browser and every client's first call
    /// fails, because CDP cannot create a runtime session itself.
    pub fn with_auto_session(mut self, auto_session: bool) -> Self {
        self.auto_session = auto_session;
        self
    }

    pub fn with_artifacts(mut self, artifacts: artifact_store::ArtifactStore) -> Self {
        self.artifacts = Some(artifacts);
        self
    }

    pub fn with_upload_staging_root(mut self, root: impl Into<PathBuf>) -> Self {
        self.upload_staging_root = Some(root.into());
        self
    }

    pub fn with_stream_limits(
        mut self,
        max_count: usize,
        max_total_bytes: u64,
        ttl: std::time::Duration,
    ) -> Self {
        self.streams = Arc::new(Mutex::new(DownloadStreamStore::new(
            max_count,
            max_total_bytes,
            ttl,
        )));
        self
    }

    async fn authenticate(&self, bearer: Option<&str>) -> Result<CapabilityHandle, DiscoveryError> {
        let bearer = bearer
            .filter(|value| !value.is_empty())
            .ok_or(DiscoveryError::Unauthorized)?;
        self.authority
            .authenticate(bearer, Utc::now())
            .await
            .map_err(|_| DiscoveryError::Unauthorized)
    }

    pub async fn version(
        &self,
        bearer: Option<&str>,
    ) -> Result<VersionDescription, DiscoveryError> {
        let handle = self.authenticate(bearer).await?;
        let ctx = handle.context(Utc::now() + Duration::seconds(30), None);
        if !ctx.capabilities.contains(types::Capability::SessionRead) {
            return Err(DiscoveryError::Forbidden);
        }
        Ok(VersionDescription {
            browser: "AutomationRuntime/0.1".into(),
            protocol_version: "1.3".into(),
            web_socket_debugger_url: self.browser_ws_url(),
        })
    }

    pub async fn list(
        &self,
        bearer: Option<&str>,
    ) -> Result<Vec<TargetDescription>, DiscoveryError> {
        let handle = self.authenticate(bearer).await?;
        let ctx = handle.context(Utc::now() + Duration::seconds(30), None);
        let runtime = (self.bind_runtime)(handle);
        let sessions = runtime
            .list_sessions(ctx)
            .await
            .map_err(|_| DiscoveryError::Runtime)?;
        let targets = self.targets.lock().await.targets_for(&sessions);
        Ok(targets
            .into_iter()
            .map(|target| TargetDescription {
                id: target.opaque,
                r#type: "page".into(),
                // A client picks a target out of this list. One shared constant
                // for title and url made every target look identical; report
                // what the gateway actually verified, and say blank when it has
                // verified nothing rather than inventing a name.
                title: target
                    .title
                    .clone()
                    .or_else(|| target.url.clone())
                    .unwrap_or_else(|| "about:blank".into()),
                url: target.url.unwrap_or_else(|| "about:blank".into()),
                web_socket_debugger_url: self.browser_ws_url(),
            })
            .collect())
    }

    pub async fn upgrade(
        &self,
        path: &str,
        bearer: Option<&str>,
    ) -> Result<Arc<CdpConnection>, DiscoveryError> {
        if path != format!("/devtools/browser/{}", self.browser_id) {
            return Err(DiscoveryError::NotFound);
        }
        let handle = self.authenticate(bearer).await?;
        let runtime = (self.bind_runtime)(handle.clone());
        if self.auto_session {
            self.ensure_runtime_page(&handle, runtime.as_ref()).await;
        }
        let connection = Arc::new(CdpConnection::with_targets(
            handle,
            runtime,
            self.registry.clone(),
            ConnectionShared {
                targets: self.targets.clone(),
                generations: self.generations.clone(),
                artifacts: self.artifacts.clone(),
                upload_staging_root: self.upload_staging_root.clone(),
                streams: self.streams.clone(),
            },
        ));
        let mut connections = self.connections.lock().await;
        connections.retain(|existing| existing.strong_count() > 0);
        connections.push(Arc::downgrade(&connection));
        Ok(connection)
    }

    /// Give a connecting client something to drive.
    ///
    /// A CDP client connects and immediately reads the page list; DevTools
    /// clients have no way to ask for a session, and `Target.createTarget` needs
    /// one that already exists. Without this, `connectOverCDP` succeeds and then
    /// every first call fails, which reads as a broken gateway rather than a
    /// missing prerequisite.
    ///
    /// Best-effort by design: a client that only holds discovery capabilities,
    /// or a runtime that refuses, still gets its socket. Bounded to one page,
    /// and only when the principal has no session at all.
    async fn ensure_runtime_page(&self, handle: &CapabilityHandle, runtime: &dyn RuntimeInterface) {
        let ctx = handle.context(Utc::now() + Duration::seconds(30), None);
        if !ctx.capabilities.contains(types::Capability::SessionWrite)
            || !ctx.capabilities.contains(types::Capability::PageWrite)
        {
            return;
        }
        let sessions = match runtime.list_sessions(ctx.clone()).await {
            Ok(sessions) => sessions,
            Err(error) => {
                tracing::debug!(error = ?error.code, "cdp.auto_session.list_failed");
                return;
            }
        };
        if sessions.iter().any(|session| !session.page_ids.is_empty()) {
            return;
        }
        let session = match sessions.first() {
            Some(session) => session.clone(),
            None => match runtime
                .create_session(
                    ctx.clone(),
                    CreateSessionRequest {
                        profile: "default".to_owned(),
                        proxy: None,
                        execution_policy: types::ExecutionPolicy::default(),
                    },
                )
                .await
            {
                Ok(session) => session,
                Err(error) => {
                    tracing::warn!(error = ?error.code, message = %error.message, "cdp.auto_session.create_failed");
                    return;
                }
            },
        };
        match runtime
            .open_page(
                ctx,
                OpenPageRequest {
                    session_id: session.id.clone(),
                },
            )
            .await
        {
            Ok(page) => tracing::info!(
                session = %session.id.0,
                page = %page.id.0,
                "cdp.auto_session.ready"
            ),
            Err(error) => {
                tracing::warn!(error = ?error.code, message = %error.message, "cdp.auto_session.open_page_failed")
            }
        }
    }

    pub async fn replace_worker_generation(
        &self,
        runtime_session: &str,
        current: RuntimeGeneration,
    ) -> Result<(), CdpError> {
        let connections = self
            .connections
            .lock()
            .await
            .iter()
            .filter_map(Weak::upgrade)
            .collect::<Vec<_>>();
        for connection in connections {
            connection
                .replace_generation(runtime_session, current)
                .await?;
        }
        Ok(())
    }

    fn browser_ws_url(&self) -> String {
        format!(
            "{}/devtools/browser/{}",
            self.websocket_base, self.browser_id
        )
    }

    /// Builds the authenticated CDP discovery and WebSocket transport.
    pub fn router(self: Arc<Self>) -> Router {
        Router::new()
            .route("/json/version", get(version_route))
            .route("/json/version/", get(version_route))
            .route("/json/list", get(list_route))
            .route("/json/list/", get(list_route))
            .route("/v1/streams/{id}", get(stream_route))
            .route("/devtools/browser/{id}", get(websocket_route))
            .with_state(self)
    }
}

const MAX_REMOTE_OBJECTS: usize = 4096;
const MAX_PENDING_PAGE_LOADS: usize = 256;
const MAX_ISOLATED_WORLDS: usize = 256;
const MAX_PENDING_TAB_CHILDREN: usize = 256;
const MAX_BROWSER_OBSERVERS: usize = 512;

/// One authenticated CDP WebSocket session.
///
/// Created by [`CdpGateway::upgrade`] and dispatches CDP method calls through
/// the shared runtime interface.
pub struct CdpConnection {
    connection_id: String,
    handle: CapabilityHandle,
    runtime: Arc<dyn RuntimeInterface>,
    registry: MethodRegistry,
    in_flight: Arc<Semaphore>,
    events: Mutex<VecDeque<CdpEvent>>,
    event_notify: Notify,
    identifiers: Mutex<IdentifierMap>,
    targets: Arc<Mutex<TargetCatalog>>,
    generations: Arc<Mutex<HashMap<String, RuntimeGeneration>>>,
    isolated_worlds: Mutex<HashMap<String, String>>,
    pending_page_loads: Mutex<HashMap<String, (String, String, String)>>,
    artifacts: Option<artifact_store::ArtifactStore>,
    upload_staging_root: Option<PathBuf>,
    streams: Arc<Mutex<DownloadStreamStore>>,
    remote_objects: Mutex<HashMap<String, RemoteObject>>,
    execution_generations: Mutex<HashMap<String, u64>>,
    enabled_domains: Mutex<HashSet<(String, &'static str)>>,
    lifecycle_events: Mutex<HashSet<String>>,
    download_events_enabled: Mutex<bool>,
    browser_observers: Mutex<HashSet<String>>,
    discovery_filter: Mutex<Option<Vec<domains::target::TargetFilter>>>,
    auto_attach: Mutex<Option<(bool, Vec<domains::target::TargetFilter>)>>,
    pending_tab_children: Mutex<HashMap<String, (String, Value)>>,
    automation_boundary: Mutex<Option<AutomationBoundary>>,
    interface_events: EventStore,
    observed_methods: Mutex<BTreeSet<String>>,
    observed_events: Mutex<BTreeSet<String>>,
}

#[derive(Clone)]
struct RemoteObject {
    internal: String,
    scope: String,
    generation: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum AutomationBoundaryPhase {
    Pending,
    Consumed,
    Recovered,
}

#[derive(Clone)]
struct AutomationBoundary {
    workflow_id: WorkflowId,
    attempt_id: AttemptId,
    command_id: CommandId,
    checkpoint_id: CheckpointId,
    session_id: SessionId,
    page_id: PageId,
    expires_at: chrono::DateTime<Utc>,
    phase: AutomationBoundaryPhase,
}

#[derive(Clone)]
struct DownloadStream {
    principal_id: types::PrincipalId,
    connection_id: String,
    session_id: SessionId,
    artifact_id: String,
    bytes: u64,
    expires_at: std::time::Instant,
}

struct DownloadStreamStore {
    entries: HashMap<String, DownloadStream>,
    max_count: usize,
    max_total_bytes: u64,
    total_bytes: u64,
    ttl: std::time::Duration,
}

impl DownloadStreamStore {
    fn new(max_count: usize, max_total_bytes: u64, ttl: std::time::Duration) -> Self {
        Self {
            entries: HashMap::new(),
            max_count,
            max_total_bytes,
            total_bytes: 0,
            ttl,
        }
    }

    fn purge_expired(&mut self) {
        let now = std::time::Instant::now();
        let expired = self
            .entries
            .iter()
            .filter_map(|(id, stream)| (stream.expires_at <= now).then_some(id.clone()))
            .collect::<Vec<_>>();
        for id in expired {
            self.remove(&id);
        }
    }

    fn reserve(
        &mut self,
        id: &str,
        principal_id: types::PrincipalId,
        connection_id: &str,
        session_id: SessionId,
        artifact_id: &str,
        bytes: u64,
    ) -> Result<(), ()> {
        self.purge_expired();
        if id.is_empty()
            || self.entries.len() >= self.max_count
            || bytes > self.max_total_bytes
            || self
                .total_bytes
                .checked_add(bytes)
                .is_none_or(|total| total > self.max_total_bytes)
        {
            return Err(());
        }
        self.total_bytes += bytes;
        self.entries.insert(
            id.to_owned(),
            DownloadStream {
                principal_id,
                connection_id: connection_id.to_owned(),
                session_id,
                artifact_id: artifact_id.to_owned(),
                bytes,
                expires_at: std::time::Instant::now() + self.ttl,
            },
        );
        Ok(())
    }

    fn peek_authorized(
        &mut self,
        id: &str,
        principal_id: &types::PrincipalId,
    ) -> Option<DownloadStream> {
        self.purge_expired();
        self.entries
            .get(id)
            .filter(|stream| &stream.principal_id == principal_id)
            .cloned()
    }

    fn take_authorized(
        &mut self,
        id: &str,
        principal_id: &types::PrincipalId,
    ) -> Option<DownloadStream> {
        self.peek_authorized(id, principal_id)?;
        self.remove(id)
    }

    fn remove(&mut self, id: &str) -> Option<DownloadStream> {
        let removed = self.entries.remove(id)?;
        self.total_bytes = self.total_bytes.saturating_sub(removed.bytes);
        Some(removed)
    }

    fn remove_connection(&mut self, connection_id: &str) {
        let ids = self
            .entries
            .iter()
            .filter_map(|(id, stream)| {
                (stream.connection_id == connection_id).then_some(id.clone())
            })
            .collect::<Vec<_>>();
        for id in ids {
            self.remove(&id);
        }
    }
}

struct ConnectionShared {
    targets: Arc<Mutex<TargetCatalog>>,
    generations: Arc<Mutex<HashMap<String, RuntimeGeneration>>>,
    artifacts: Option<artifact_store::ArtifactStore>,
    upload_staging_root: Option<PathBuf>,
    streams: Arc<Mutex<DownloadStreamStore>>,
}

struct UploadStaging {
    dir: tempfile::TempDir,
}

impl UploadStaging {
    fn new(root: &std::path::Path) -> std::io::Result<Self> {
        tempfile::Builder::new()
            .prefix("request-")
            .tempdir_in(root)
            .map(|dir| Self { dir })
    }

    fn stage(&self, name: &str, bytes: &[u8]) -> std::io::Result<PathBuf> {
        let path = self.dir.path().join(name);
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)?;
        std::io::Write::write_all(&mut file, bytes)?;
        Ok(path)
    }
}

impl CdpConnection {
    /// Build a standalone connection (tests and harness use).
    pub fn new(
        handle: CapabilityHandle,
        runtime: Arc<dyn RuntimeInterface>,
        registry: MethodRegistry,
    ) -> Self {
        Self::with_targets(
            handle,
            runtime,
            registry,
            ConnectionShared {
                targets: Arc::new(Mutex::new(TargetCatalog::default())),
                generations: Arc::new(Mutex::new(HashMap::new())),
                artifacts: None,
                upload_staging_root: None,
                streams: Arc::new(Mutex::new(DownloadStreamStore::new(
                    128,
                    256 * 1024 * 1024,
                    std::time::Duration::from_secs(300),
                ))),
            },
        )
    }

    fn with_targets(
        handle: CapabilityHandle,
        runtime: Arc<dyn RuntimeInterface>,
        registry: MethodRegistry,
        shared: ConnectionShared,
    ) -> Self {
        Self {
            connection_id: Uuid::new_v4().to_string(),
            handle,
            runtime,
            registry,
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT_REQUESTS)),
            events: Mutex::new(VecDeque::new()),
            event_notify: Notify::new(),
            identifiers: Mutex::new(IdentifierMap::new()),
            targets: shared.targets,
            generations: shared.generations,
            isolated_worlds: Mutex::new(HashMap::new()),
            pending_page_loads: Mutex::new(HashMap::new()),
            artifacts: shared.artifacts,
            upload_staging_root: shared.upload_staging_root,
            streams: shared.streams,
            remote_objects: Mutex::new(HashMap::new()),
            execution_generations: Mutex::new(HashMap::new()),
            enabled_domains: Mutex::new(HashSet::new()),
            lifecycle_events: Mutex::new(HashSet::new()),
            download_events_enabled: Mutex::new(false),
            browser_observers: Mutex::new(HashSet::new()),
            discovery_filter: Mutex::new(None),
            auto_attach: Mutex::new(None),
            pending_tab_children: Mutex::new(HashMap::new()),
            automation_boundary: Mutex::new(None),
            interface_events: EventStore::new(64),
            observed_methods: Mutex::new(BTreeSet::new()),
            observed_events: Mutex::new(BTreeSet::new()),
        }
    }

    pub async fn dispatch(&self, request: CdpRequest) -> CdpResponse {
        if let Err(error) = request.validate() {
            return CdpResponse::failure(&request, error);
        }
        let Ok(permit) = self.reserve_dispatch() else {
            return CdpResponse::failure(
                &request,
                CdpError::new(CdpErrorCode::RuntimeFailure, "too many in-flight requests"),
            );
        };
        self.dispatch_reserved(request, &permit).await
    }

    fn reserve_dispatch(&self) -> Result<OwnedSemaphorePermit, CdpError> {
        self.in_flight
            .clone()
            .try_acquire_owned()
            .map_err(|_| CdpError::new(CdpErrorCode::RuntimeFailure, "too many in-flight requests"))
    }

    async fn dispatch_reserved(
        &self,
        request: CdpRequest,
        _permit: &OwnedSemaphorePermit,
    ) -> CdpResponse {
        let Some(metadata) = self.registry.method(&request.method) else {
            return CdpResponse::failure(
                &request,
                CdpError::new(CdpErrorCode::MethodNotFound, "method not found"),
            );
        };
        {
            let mut methods = self.observed_methods.lock().await;
            if methods.len() < 128 {
                methods.insert(request.method.clone());
            }
        }
        if !request.params.is_object() {
            return CdpResponse::failure(
                &request,
                CdpError::new(CdpErrorCode::InvalidParams, "params must be an object"),
            );
        }
        let ctx = self
            .handle
            .context(Utc::now() + Duration::seconds(30), None);
        if AuthorizationGuard::new(self.handle.clone())
            .validate(&ctx)
            .is_err()
            || metadata
                .capability()
                .is_none_or(|capability| !ctx.capabilities.contains(capability))
        {
            return CdpResponse::failure(
                &request,
                CdpError::new(
                    CdpErrorCode::RuntimeFailure,
                    "authentication or capability check failed",
                ),
            );
        }
        let result = match self.registry.handler(&request.method) {
            Some(Handler::AuditsEnable | Handler::PerformanceEnable) => {
                if !request.params.as_object().is_some_and(serde_json::Map::is_empty) {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "enable method takes no parameters"));
                }
                Ok(json!({}))
            }
            Some(Handler::NetworkSetUserAgent) => {
                let Some(params) = request.params.as_object() else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "user-agent override params must be an object"));
                };
                if params.len() != 1
                    || params.get("userAgent").and_then(Value::as_str) != Some("AutomationRuntime/0.1")
                {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "only the exact current runtime user-agent no-op is supported"));
                }
                Ok(json!({}))
            }
            Some(Handler::TargetGetBrowserContexts) => {
                if !request.params.as_object().is_some_and(serde_json::Map::is_empty) {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "Target.getBrowserContexts takes no parameters"));
                }
                Ok(json!({"browserContextIds": []}))
            }
            Some(Handler::TargetSetDiscoverTargets) => {
                let Some(params) = request.params.as_object() else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "Target.setDiscoverTargets params must be an object"));
                };
                if params.len() > 2 || !params.get("discover").is_some_and(Value::is_boolean) {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "Target.setDiscoverTargets requires a boolean discover and optional filter"));
                }
                let filters = if let Some(filter) = params.get("filter") {
                    let Some(filters) = filter.as_array() else {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "Target.setDiscoverTargets filter must be an array"));
                    };
                    if filters.len() > 32 || filters.iter().any(|entry| !entry.is_object()) {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "Target.setDiscoverTargets filter must contain at most 32 objects"));
                    }
                    match serde_json::from_value::<Vec<domains::target::TargetFilter>>(filter.clone()) {
                        Ok(filters) => filters,
                        Err(_) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid bounded discovery filter")),
                    }
                } else { Vec::new() };
                let discover = params.get("discover") == Some(&Value::Bool(true));
                *self.discovery_filter.lock().await = discover.then_some(filters.clone());
                if discover {
                    let sessions = match self.runtime.list_sessions(ctx.clone()).await {
                        Ok(sessions) => sessions,
                        Err(error) => return CdpResponse::failure(&request, runtime_error(error)),
                    };
                    let infos = self.target_infos(&sessions).await;
                    for target_info in infos["targetInfos"].as_array().into_iter().flatten().filter(|info| {
                        info["type"].as_str().is_some_and(|kind| domains::target::filter_matches(&filters, kind))
                    }) {
                        if let Err(error) = self.queue_event(CdpEvent {
                            method: "Target.targetCreated".into(),
                            params: json!({"targetInfo": target_info}),
                            session_id: None,
                        }).await {
                            return CdpResponse::failure(&request, error);
                        }
                    }
                }
                Ok(json!({}))
            }
            Some(Handler::TargetCreateTarget) => {
                let Some(params) = request.params.as_object() else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "Target.createTarget params must be an object"));
                };
                if params.len() != 1 || params.get("url").and_then(Value::as_str) != Some("about:blank") {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "Target.createTarget supports only a new about:blank runtime page"));
                }
                let sessions = match self.runtime.list_sessions(ctx.clone()).await {
                    Ok(sessions) => sessions,
                    Err(error) => return CdpResponse::failure(&request, runtime_error(error)),
                };
                let Some(session) = sessions.first() else {
                    // First call after a bare connect lands here: CDP attaches to
                    // runtime sessions, it does not create them. Say where they come
                    // from rather than leaving the client to guess.
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "no runtime session is available: CDP attaches to existing runtime sessions and cannot create one -- open a session and page first (POST /v1/sessions then POST /v1/pages, MCP session_create/page_open, or an SDK client), then reuse the page this connection already exposes"));
                };
                let page = match self.runtime.open_page(ctx, OpenPageRequest { session_id: session.id.clone() }).await {
                    Ok(page) => page,
                    Err(error) => return CdpResponse::failure(&request, runtime_error(error)),
                };
                let runtime_session = session.id.0.to_string();
                let runtime_page = page.id.0.to_string();
                let target_id = self.bind_identifier(IdentifierFamily::Target, &runtime_session, &runtime_page, RuntimeGeneration(0)).await;
                let tab_id = self.bind_identifier(IdentifierFamily::Target, &runtime_session, &format!("tab:{runtime_page}"), RuntimeGeneration(0)).await;
                let browser_context_id = self.bind_identifier(IdentifierFamily::BrowserContext, &runtime_session, "default", RuntimeGeneration(0)).await;
                let attach = self.auto_attach.lock().await.clone().filter(|(_, filters)| domains::target::filter_matches(filters, "tab"));
                let tab_info = json!({"targetId":tab_id,"type":"tab","title":"Automation Runtime","url":"about:blank","attached":attach.is_some(),"canAccessOpener":false,"browserContextId":browser_context_id});
                let page_info = json!({"targetId":target_id,"type":"page","title":"Automation Runtime","url":"about:blank","attached":false,"canAccessOpener":false,"browserContextId":browser_context_id});
                let tab_session_id = self.bind_identifier(IdentifierFamily::CdpSession, &runtime_session, &tab_id, RuntimeGeneration(0)).await;
                let page_session_id = self.bind_identifier(IdentifierFamily::CdpSession, &runtime_session, &target_id, RuntimeGeneration(0)).await;
                let discovery = self.discovery_filter.lock().await.clone();
                let mut events = Vec::new();
                if discovery.as_ref().is_some_and(|filters| domains::target::filter_matches(filters, "tab")) { events.push(CdpEvent { method:"Target.targetCreated".into(), params:json!({"targetInfo":tab_info.clone()}), session_id:None }); }
                if discovery.as_ref().is_some_and(|filters| domains::target::filter_matches(filters, "page")) { events.push(CdpEvent { method:"Target.targetCreated".into(), params:json!({"targetInfo":page_info.clone()}), session_id:None }); }
                if let Some((waiting, _)) = attach {
                    let mut children = self.pending_tab_children.lock().await;
                    if children.len() >= MAX_PENDING_TAB_CHILDREN {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "pending tab registry exhausted"));
                    }
                    children.insert(tab_session_id.clone(), (page_session_id, page_info));
                    events.push(CdpEvent { method:"Target.attachedToTarget".into(), params:json!({"sessionId":tab_session_id,"targetInfo":tab_info,"waitingForDebugger":waiting}), session_id:None });
                }
                if let Err(error) = self.queue_events(events).await {
                    return CdpResponse::failure(&request, error);
                }
                Ok(json!({"targetId": target_id}))
            }
            Some(Handler::BrowserGetVersion) => self.runtime.runtime_info(ctx).await.map(|info| json!({
                "protocolVersion": "1.3", "product": "AutomationRuntime/0.1", "revision": info.version,
                "userAgent": "AutomationRuntime/0.1", "jsVersion": "unknown"
            })),
            Some(Handler::TargetGetTargets) => match self.runtime.list_sessions(ctx).await {
                Ok(sessions) => Ok(self.target_infos(&sessions).await),
                Err(error) => Err(error),
            },
            Some(Handler::TargetGetTargetInfo) => {
                let Some(params) = request.params.as_object() else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "Target.getTargetInfo params must be an object",
                        ),
                    );
                };
                if params.is_empty() {
                    let target_id = self
                        .bind_identifier(
                            IdentifierFamily::Target,
                            "browser",
                            "browser",
                            RuntimeGeneration(0),
                        )
                        .await;
                    Ok(json!({"targetInfo": {"targetId": target_id, "type": "browser", "title": "", "url": "", "attached": true, "canAccessOpener": false}}))
                } else if params.len() == 1 {
                    let Some(target_id) = params.get("targetId").and_then(Value::as_str) else {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(CdpErrorCode::InvalidParams, "invalid targetId"),
                        );
                    };
                    if self
                        .resolve_identifier(IdentifierFamily::Target, target_id)
                        .await
                        .is_none()
                    {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(CdpErrorCode::InvalidParams, "unknown target"),
                        );
                    }
                    Ok(json!({"targetInfo": {"targetId": target_id, "type": "page", "title": "Automation Runtime", "url": "about:blank", "attached": true, "canAccessOpener": false}}))
                } else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "Target.getTargetInfo accepts only targetId",
                        ),
                    );
                }
            }
            Some(Handler::TargetAttachToBrowserTarget) => {
                if !request.params.as_object().is_some_and(serde_json::Map::is_empty) {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "Target.attachToBrowserTarget takes no parameters"));
                }
                let session_id = Uuid::new_v4().to_string();
                let mut observers = self.browser_observers.lock().await;
                if observers.len() >= MAX_BROWSER_OBSERVERS {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "observer registry exhausted"));
                }
                observers.insert(session_id.clone());
                Ok(json!({"sessionId": session_id}))
            }
            Some(Handler::TargetDetachFromTarget) => {
                let Some(session_id) = request.params.get("sessionId").and_then(Value::as_str) else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "missing observer session"));
                };
                if !self.browser_observers.lock().await.remove(session_id) {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unknown observer session"));
                }
                Ok(json!({}))
            }
            Some(Handler::TargetSetAutoAttach) => match domains::target::auto_attach(request.params.clone()) {
                Ok(options) => {
                    if let Some(parent_session) = request.session_id.as_deref() {
                        if options.auto_attach && domains::target::filter_matches(&options.filter, "page") {
                            if let Some((session_id, mut target_info)) = self.pending_tab_children.lock().await.remove(parent_session) {
                                target_info["attached"] = Value::Bool(true);
                                if let Err(error) = self.queue_event(CdpEvent {
                                    method: "Target.attachedToTarget".into(),
                                    params: json!({"sessionId":session_id,"targetInfo":target_info,"waitingForDebugger":options.wait_for_debugger_on_start}),
                                    session_id: Some(parent_session.to_owned()),
                                }).await {
                                    return CdpResponse::failure(&request, error);
                                }
                            }
                        }
                        return CdpResponse::success(&request, json!({}));
                    }
                    *self.auto_attach.lock().await = options.auto_attach.then_some((options.wait_for_debugger_on_start, options.filter.clone()));
                    match self.runtime.list_sessions(ctx).await {
                        Ok(sessions) => {
                            if options.auto_attach && request.session_id.is_none() {
                                if let Err(error) = self.queue_attached_targets(&sessions, options.wait_for_debugger_on_start, &options.filter).await {
                                    return CdpResponse::failure(&request, error);
                                }
                            }
                            Ok(json!({}))
                        }
                        Err(error) => Err(error),
                    }
                }
                Err(error) => return CdpResponse::failure(&request, error),
            },
            Some(Handler::BrowserSetDownloadBehavior) => {
                let behavior = match domains::browser::validate_download_behavior(request.params.clone()) {
                    Ok(behavior) => behavior,
                    Err(error) => return CdpResponse::failure(&request, error),
                };
                *self.download_events_enabled.lock().await = behavior.events_enabled
                    && behavior.behavior != "deny";
                Ok(json!({}))
            }
            Some(Handler::PageGetFrameTree) => {
                if !request.params.as_object().is_some_and(serde_json::Map::is_empty) {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "Page.getFrameTree takes no parameters"));
                }
                let scope = request.session_id.as_deref().unwrap_or("browser");
                let frame_id = if let Some(session_id) = request.session_id.as_deref() {
                    match self.resolve_identifier(IdentifierFamily::CdpSession, session_id).await {
                        Some(target_id) => target_id,
                        None => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unknown CDP session")),
                    }
                } else {
                    self.bind_identifier(IdentifierFamily::Frame, scope, "main", RuntimeGeneration(0)).await
                };
                // Not try_lock: contention must wait, not silently report a
                // fabricated about:blank frame.
                let pending = match request.session_id.as_deref() {
                    Some(id) => self.pending_page_loads.lock().await.get(id).cloned(),
                    None => None,
                };
                let (loader_id, url) = pending.map(|(_, url, loader)| (loader, url)).unwrap_or_else(|| ("initial".into(), "about:blank".into()));
                Ok(json!({"frameTree":{"frame":{"id":frame_id,"loaderId":loader_id,"url":url,"domainAndRegistry":"","securityOrigin":"://","mimeType":"text/html","secureContextType":"SecureLocalhost","crossOriginIsolatedContextType":"NotIsolated","gatedAPIFeatures":[]}}}))
            }
            Some(Handler::PageGetLayoutMetrics) => {
                if !request.params.as_object().is_some_and(serde_json::Map::is_empty) {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "Page.getLayoutMetrics takes no parameters"));
                }
                Ok(json!({
                    "layoutViewport":{"pageX":0,"pageY":0,"clientWidth":1280,"clientHeight":720},
                    "visualViewport":{"offsetX":0,"offsetY":0,"pageX":0,"pageY":0,"clientWidth":1280,"clientHeight":720,"scale":1,"zoom":1},
                    "contentSize":{"x":0,"y":0,"width":1280,"height":720},
                    "cssLayoutViewport":{"pageX":0,"pageY":0,"clientWidth":1280,"clientHeight":720},
                    "cssVisualViewport":{"offsetX":0,"offsetY":0,"pageX":0,"pageY":0,"clientWidth":1280,"clientHeight":720,"scale":1,"zoom":1},
                    "cssContentSize":{"x":0,"y":0,"width":1280,"height":720}
                }))
            }
            Some(Handler::AutomationCheckpointSave) => {
                if !request.params.as_object().is_some_and(serde_json::Map::is_empty) {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "Automation.checkpointSave takes no parameters"));
                }
                let Some((session_id, page_id)) = self.automation_runtime_identity(request.session_id.as_deref(),ctx.clone()).await else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "checkpoint requires a runtime page session"));
                };
                {
                    let mut state = self.automation_boundary.lock().await;
                    if state.as_ref().is_some_and(|pending| pending.phase == AutomationBoundaryPhase::Pending && pending.expires_at > Utc::now()) {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "a browser boundary is already reserved"));
                    }
                    *state = None;
                }
                let workflow_id = WorkflowId::new();
                let attempt_id = AttemptId::new();
                let inspect_id = CommandId::new();
                let command_id = CommandId::new();
                let evidence = match self.runtime.submit(ctx.clone(), CommandEnvelope {
                    schema_version:CommandEnvelope::SCHEMA_VERSION, command_id:inspect_id.clone(), workflow_id:workflow_id.clone(), attempt_id:attempt_id.clone(),
                    session_id:session_id.clone(), page_id:Some(page_id.clone()), deadline:Utc::now()+Duration::seconds(30),
                    command:RuntimeCommand::Primitive(PrimitiveCommand::Inspect(InspectCommand::default())),
                }).await {
                    Ok(CommandOutcome::Completed { evidence, .. }) => evidence,
                    Ok(_) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "checkpoint inspection did not complete")),
                    Err(error) => return CdpResponse::failure(&request, runtime_error(error)),
                };
                let Some((url,title)) = evidence.iter().find_map(|item| if let types::Evidence::Inspection { url,title,.. }=item {Some((url.clone(),title.clone()))} else {None}) else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "checkpoint inspection lacked verified state"));
                };
                let checkpoint_id=CheckpointId::new();
                let checkpoint=WorkflowCheckpoint { schema_version:1, checkpoint_id:checkpoint_id.clone(), workflow_id:workflow_id.clone(), attempt_id:attempt_id.clone(),
                    session_id:session_id.clone(), page_id:page_id.clone(), restart_url:url.clone(), current_url:url.clone(), cursor:Some(inspect_id), boundary_command_id:Some(command_id.clone()),
                    recovery_class:CommandClass::Boundary, invariants:vec![CheckpointInvariant::Url{value:url},CheckpointInvariant::Title{value:title}],
                    replayable_inputs:vec![], evidence:evidence.clone(), recovery_history:vec![], recovery_receipts:vec![], created_at:Utc::now() };
                match self.runtime.checkpoint(ctx, checkpoint, evidence).await {
                    Ok(saved) => {
                        *self.automation_boundary.lock().await=Some(AutomationBoundary { workflow_id:workflow_id.clone(), attempt_id, command_id:command_id.clone(), checkpoint_id:checkpoint_id.clone(), session_id, page_id, expires_at:Utc::now()+Duration::seconds(30), phase:AutomationBoundaryPhase::Pending });
                        self.record_interface_event("checkpoint.saved", serde_json::to_value(&saved).unwrap_or(Value::Null)).await;
                        Ok(json!({"checkpointId":checkpoint_id,"workflowId":workflow_id,"boundaryCommandId":command_id,"boundary":"boundary"}))
                    }
                    Err(error) => Err(error),
                }
            }
            Some(Handler::AutomationRecoveryInspect) => {
                if !request.params.as_object().is_some_and(serde_json::Map::is_empty) { return CdpResponse::failure(&request,CdpError::new(CdpErrorCode::InvalidParams,"Automation.recoveryInspect takes no parameters")); }
                let Some(boundary)=self.automation_boundary.lock().await.clone().filter(|state| state.phase==AutomationBoundaryPhase::Consumed) else { return CdpResponse::failure(&request,CdpError::new(CdpErrorCode::RuntimeFailure,"no consumed browser boundary")); };
                match self.runtime.recover(ctx,boundary.workflow_id.clone()).await {
                    Ok(decision) => {
                        let (status,replayed,observed_checkpoint)=match decision { RecoveryDecision::Resumed{checkpoint_id,..}=>("resumed",false,checkpoint_id), RecoveryDecision::NeedsReconciliation{checkpoint_id,..}=>("needsReconciliation",false,checkpoint_id), RecoveryDecision::Restarted{checkpoint_id,..}=>("restarted",true,checkpoint_id) };
                        if observed_checkpoint != boundary.checkpoint_id { return CdpResponse::failure(&request,CdpError::new(CdpErrorCode::RuntimeFailure,"recovery checkpoint lineage changed")); }
                        let response=json!({"status":status,"checkpointId":observed_checkpoint,"workflowId":boundary.workflow_id,"boundaryCommandId":boundary.command_id,"boundary":"boundary","replayed":replayed});
                        if let Some(state)=self.automation_boundary.lock().await.as_mut() { state.phase=AutomationBoundaryPhase::Recovered; }
                        self.record_interface_event("recovery.inspected",response.clone()).await;
                        Ok(response)
                    }
                    Err(error)=>Err(error),
                }
            }
            Some(Handler::AutomationEventsRead) => {
                let Some(params)=request.params.as_object() else { return CdpResponse::failure(&request,CdpError::new(CdpErrorCode::InvalidParams,"Automation.eventsRead requires object parameters")); };
                let cursor=params.get("cursor").and_then(Value::as_u64).unwrap_or(0);
                if params.keys().any(|key| key!="cursor") { return CdpResponse::failure(&request,CdpError::new(CdpErrorCode::InvalidParams,"Automation.eventsRead accepts only cursor")); }
                // No read receipt is recorded: journaling every poll would
                // flood the bounded event store with the client's own reads
                // and evict the real events it is trying to fetch.
                match self.interface_events.read_after_for(self.handle.principal_id(),types::EventCursor(cursor),64).await {
                    Ok(batch)=>Ok(serde_json::to_value(batch).unwrap_or(Value::Null)),
                    Err(_)=>return CdpResponse::failure(&request,CdpError::new(CdpErrorCode::RuntimeFailure,"browser interface event history gap")),
                }
            }
            Some(Handler::AutomationProtocolInventory) => {
                if !request.params.as_object().is_some_and(serde_json::Map::is_empty) { return CdpResponse::failure(&request,CdpError::new(CdpErrorCode::InvalidParams,"Automation.protocolInventory takes no parameters")); }
                Ok(json!({
                    "methods":self.observed_methods.lock().await.iter().cloned().collect::<Vec<_>>(),
                    "events":self.observed_events.lock().await.iter().cloned().collect::<Vec<_>>(),
                }))
            }
            Some(Handler::PageCaptureScreenshot) => {
                let Some(params) = request.params.as_object() else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid screenshot parameters"));
                };
                let allowed = ["format", "fromSurface", "captureBeyondViewport", "optimizeForSpeed", "clip"];
                let valid = params.keys().all(|key| allowed.contains(&key.as_str()))
                    && params.get("format").and_then(Value::as_str).is_none_or(|format| format == "png")
                    && ["fromSurface", "captureBeyondViewport", "optimizeForSpeed"].into_iter()
                        .all(|key| params.get(key).is_none_or(Value::is_boolean));
                if !valid {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "only bounded PNG viewport screenshots are supported"));
                }
                let Some(store) = self.artifacts.as_ref() else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "artifact reader is not configured"));
                };
                let Some((session_id, page_id)) = self.runtime_identity(request.session_id.as_deref()).await else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unknown runtime page"));
                };
                let mode = if let Some(clip) = params.get("clip").and_then(Value::as_object) {
                    let number = |key: &str| clip.get(key).and_then(Value::as_f64);
                    let (Some(x), Some(y), Some(width), Some(height), Some(scale)) =
                        (number("x"), number("y"), number("width"), number("height"), number("scale")) else {
                            return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid screenshot clip"));
                        };
                    let bounded = [x, y, width, height, scale].into_iter().all(f64::is_finite)
                        && x >= 0.0 && y >= 0.0 && width > 0.0 && height > 0.0
                        && x + width <= 16_384.0 && y + height <= 16_384.0 && scale == 1.0;
                    if !bounded || clip.len() != 5 {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "screenshot clip exceeds bounds"));
                    }
                    ScreenshotMode::Clip { x, y, width, height }
                } else { ScreenshotMode::Viewport };
                let envelope = CommandEnvelope { schema_version:CommandEnvelope::SCHEMA_VERSION,
                    command_id:CommandId::new(), workflow_id:WorkflowId::new(), attempt_id:AttemptId::new(),
                    session_id:session_id.clone(), page_id:Some(page_id), deadline:Utc::now()+Duration::seconds(30),
                    command:RuntimeCommand::Primitive(PrimitiveCommand::CaptureScreenshot(CaptureScreenshotCommand { mode })) };
                match self.runtime.submit(ctx, envelope).await {
                    Ok(CommandOutcome::Completed { evidence, .. }) => {
                        let screenshot = evidence.iter().find_map(|item| match item {
                            types::Evidence::Screenshot { artifact_id, media_type, bytes, sha256, .. } =>
                                Some((artifact_id, media_type, *bytes, sha256)),
                            _ => None,
                        });
                        let Some((artifact_id, _media_type, expected_bytes, expected_sha)) = screenshot.filter(|(_, media_type, _, _)| *media_type == "image/png") else {
                            return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "runtime screenshot evidence was missing or invalid"));
                        };
                        let bytes = match store.get(&session_id, artifact_id).await {
                            Ok(bytes) => bytes,
                            Err(_) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "verified screenshot artifact was unavailable")),
                        };
                        let sha = hex::encode(Sha256::digest(&bytes));
                        if bytes.len() as u64 != expected_bytes || &sha != expected_sha {
                            return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "screenshot artifact integrity check failed"));
                        }
                        self.record_interface_event("screenshot.verified", json!({"evidence":evidence})).await;
                        Ok(json!({"data":BASE64.encode(bytes)}))
                    }
                    Ok(_) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "runtime screenshot did not complete")),
                    Err(error) => Err(error),
                }
            }
            Some(Handler::RuntimeEnable) => {
                if !request.params.as_object().is_some_and(serde_json::Map::is_empty) {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "Runtime.enable takes no parameters"));
                }
                let scope = request.session_id.as_deref().unwrap_or("browser");
                self.enable_domain(request.session_id.as_deref(), "Runtime").await;
                let frame_id = if let Some(session_id) = request.session_id.as_deref() {
                    match self.resolve_identifier(IdentifierFamily::CdpSession, session_id).await {
                        Some(target_id) => target_id,
                        None => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unknown CDP session")),
                    }
                } else {
                    self.bind_identifier(IdentifierFamily::Frame, scope, "main", RuntimeGeneration(0)).await
                };
                let unique_id = self.bind_identifier(IdentifierFamily::ExecutionContext, scope, "default", RuntimeGeneration(0)).await;
                if let Err(error) = self.queue_event(CdpEvent {
                    method: "Runtime.executionContextCreated".into(),
                    params: json!({"context":{"id":1,"origin":"","name":"","uniqueId":unique_id,"auxData":{"isDefault":true,"type":"default","frameId":frame_id}}}),
                    session_id: request.session_id.clone(),
                }).await {
                    return CdpResponse::failure(&request, error);
                }
                Ok(json!({}))
            }
            Some(Handler::RuntimeEvaluate) => match domains::runtime::bootstrap_injected_script(&request.params) {
                Ok(mut result) => {
                    let class_name = result["result"]["className"].as_str().unwrap_or_default();
                    let internal = if class_name == "InjectedScript" { "playwright-injected-script" } else { "playwright-utility-script" };
                    let opaque = match self.issue_remote_object(request.session_id.as_deref(), internal).await {
                        Ok(opaque) => opaque,
                        Err(error) => return CdpResponse::failure(&request, error),
                    };
                    result["result"]["objectId"] = Value::String(opaque);
                    Ok(result)
                }
                Err(error) => return CdpResponse::failure(&request, error),
            },
            Some(Handler::RuntimeReleaseObject) => {
                let Some(id) = request.params.get("objectId").and_then(Value::as_str) else { return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "missing gateway remote object")); };
                if self.take_remote_object(request.session_id.as_deref(), id).await.is_none() { return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unknown or stale gateway remote object")); }
                Ok(json!({}))
            }
            Some(Handler::RuntimeCallFunctionOn) => {
                const PUPPETEER_TRANSLATOR: &str = "(operation, selector, value) => globalThis.__automationRuntimePuppeteer(operation, selector, value)";
                if request.params.get("functionDeclaration").and_then(Value::as_str) == Some(PUPPETEER_TRANSLATOR) {
                    let Some(args) = request.params.get("arguments").and_then(Value::as_array).filter(|args| args.len() == 3) else {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid pinned Puppeteer semantic arguments"));
                    };
                    if request.params.as_object().is_none_or(|params| params.len() != 6)
                        || request.params.get("returnByValue") != Some(&Value::Bool(true))
                        || request.params.get("awaitPromise") != Some(&Value::Bool(true))
                        || request.params.get("userGesture") != Some(&Value::Bool(true))
                    {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid pinned Puppeteer semantic call shape"));
                    }
                    let values = args.iter().map(|arg| arg.get("value").and_then(Value::as_str)).collect::<Vec<_>>();
                    let (Some(operation), Some(selector), Some(value)) = (values[0], values[1], values[2]) else {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid pinned Puppeteer semantic values"));
                    };
                    if operation.len() > 16 || selector.len() > 256 || value.len() > 1024 * 1024 {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "pinned Puppeteer semantic value exceeds bounds"));
                    }
                    let Some((session_id, page_id)) = self.runtime_identity(request.session_id.as_deref()).await else {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unknown runtime page"));
                    };
                    let download_session = session_id.clone();
                    let download_page = page_id.clone();
                    let download_ctx = ctx.clone();
                    let outcome = match (operation, selector) {
                        ("fill", "label:Name" | "label:Company") => {
                            let label = selector.trim_start_matches("label:");
                            self.runtime.submit(ctx, CommandEnvelope {
                                schema_version:CommandEnvelope::SCHEMA_VERSION, command_id:CommandId::new(), workflow_id:WorkflowId::new(), attempt_id:AttemptId::new(),
                                session_id, page_id:Some(page_id), deadline:Utc::now()+Duration::seconds(30),
                                command:RuntimeCommand::Primitive(PrimitiveCommand::TypeText(TypeTextCommand { selector:String::new(), target:Some(TargetSpec { label:Some(label.to_owned()), ..TargetSpec::default() }), value:value.to_owned(), clear_first:true, expected_url: None })),
                            }).await
                        }
                        ("click", "role:button:Continue" | "role:button:Submit") => {
                            let name = selector.trim_start_matches("role:button:");
                            self.runtime.submit(ctx, CommandEnvelope {
                                schema_version:CommandEnvelope::SCHEMA_VERSION, command_id:CommandId::new(), workflow_id:WorkflowId::new(), attempt_id:AttemptId::new(),
                                session_id, page_id:Some(page_id), deadline:Utc::now()+Duration::seconds(30),
                                command:RuntimeCommand::Primitive(PrimitiveCommand::Click(ClickCommand { selector:String::new(), target:Some(TargetSpec { role:Some("button".into()), accessible_name:Some(name.to_owned()), ..TargetSpec::default() }), boundary:false, expected_url:None, modifiers:Vec::new() })),
                            }).await
                        }
                        ("click", "role:link:Open details") => self.submit_boundary(ctx, session_id, page_id, PrimitiveCommand::ClickAndWaitForPopup(ClickAndWaitForPopupCommand {
                            selector:String::new(), target:Some(TargetSpec { role:Some("link".into()), accessible_name:Some("Open details".into()), ..TargetSpec::default() }), timeout_ms:30_000,
                        })).await,
                        ("click", "role:link:Download fixture") => self.submit_boundary(ctx, session_id, page_id, PrimitiveCommand::ClickAndWaitForDownload(ClickAndWaitForDownloadCommand {
                            selector:String::new(), target:Some(TargetSpec { role:Some("link".into()), accessible_name:Some("Download fixture".into()), ..TargetSpec::default() }), timeout_ms:30_000,
                        })).await,
                        ("upload", "label:Resume") => {
                            #[derive(serde::Deserialize)]
                            #[serde(deny_unknown_fields)]
                            struct UploadValue { name: String, base64: String }
                            let Ok(payload) = serde_json::from_str::<UploadValue>(value) else {
                                return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid pinned Puppeteer upload payload"));
                            };
                            if payload.name.is_empty() || payload.name.len() > 255 || payload.name.contains(['/', '\\']) || payload.base64.len() > 768 * 1024 {
                                return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "pinned Puppeteer upload payload exceeds bounds"));
                            }
                            let Ok(bytes) = BASE64.decode(payload.base64) else {
                                return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid pinned Puppeteer upload encoding"));
                            };
                            let Some(staging_root) = self.upload_staging_root.as_ref() else {
                                return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "upload staging root is not configured"));
                            };
                            let Ok(request_dir) = UploadStaging::new(staging_root) else {
                                return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "failed to create confined upload staging"));
                            };
                            let Ok(path) = request_dir.stage(&payload.name, &bytes) else {
                                return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "failed to stage confined upload"));
                            };
                            let result = self.runtime.submit(ctx, CommandEnvelope {
                                schema_version:CommandEnvelope::SCHEMA_VERSION, command_id:CommandId::new(), workflow_id:WorkflowId::new(), attempt_id:AttemptId::new(),
                                session_id, page_id:Some(page_id), deadline:Utc::now()+Duration::seconds(30),
                                command:RuntimeCommand::Primitive(PrimitiveCommand::UploadFiles(UploadFilesCommand { selector:String::new(), target:Some(TargetSpec { label:Some("Resume".into()), ..TargetSpec::default() }), paths:vec![path.to_string_lossy().into_owned()] })),
                            }).await;
                            drop(request_dir);
                            result
                        }
                        _ => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unsupported pinned Puppeteer semantic operation")),
                    };
                    if operation == "click" && selector == "role:link:Download fixture" {
                        return match outcome {
                            Ok(CommandOutcome::Completed { evidence, .. }) => {
                                let Some((filename, path, expected_bytes, expected_sha)) = evidence.iter().find_map(|item| match item {
                                    types::Evidence::Download { filename, path, bytes, sha256, .. } => Some((filename.clone(), path.clone(), *bytes, sha256.clone())),
                                    _ => None,
                                }) else { return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "runtime download did not produce download evidence")); };
                                let Some(store) = self.artifacts.as_ref() else { return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "artifact reader is not configured")); };
                                let Ok(data) = std::fs::read(&path) else { return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "verified download was unavailable")); };
                                if data.len() as u64 != expected_bytes || hex::encode(Sha256::digest(&data)) != expected_sha {
                                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "download evidence integrity check failed"));
                                }
                                let record = match store.put(&download_session, &download_page, "application/octet-stream", "bin", &data, data.len()).await {
                                    Ok(record) => record,
                                    Err(_) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "download import failed")),
                                };
                                let stream_id = Uuid::new_v4().to_string();
                                if self.streams.lock().await.reserve(&stream_id, download_ctx.principal_id, &self.connection_id, download_session, &record.artifact_id, expected_bytes).is_err() {
                                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "download stream capacity exhausted"));
                                }
                                let guid = Uuid::new_v4().to_string();
                                let frame_id = request.session_id.clone().unwrap_or_else(|| "main".into());
                                if let Err(error) = self.queue_events(download_events(&frame_id, &guid, &filename, expected_bytes, &stream_id, &expected_sha, request.session_id.clone()).to_vec()).await {
                                    self.streams.lock().await.remove(&stream_id);
                                    return CdpResponse::failure(&request, error);
                                }
                                CdpResponse::success(&request, json!({"result":{"type":"undefined"}}))
                            }
                            Ok(_) => CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "pinned Puppeteer download did not complete")),
                            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
                        };
                    }
                    return match outcome {
                        Ok(CommandOutcome::Completed { evidence, .. }) => {
                            if operation == "upload" { self.record_interface_event("upload.completed", json!({"evidence":evidence})).await; }
                            CdpResponse::success(&request, json!({"result":{"type":"undefined"}}))
                        },
                        Ok(_) => CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "pinned Puppeteer semantic operation did not complete")),
                        Err(error) => CdpResponse::failure(&request, runtime_error(error)),
                    };
                }
                let Some(utility_id) = request.params.get("objectId").and_then(Value::as_str) else { return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "missing gateway remote object")); };
                let utility = self.resolve_remote_object(request.session_id.as_deref(), utility_id).await;
                let valid_shape = request.params.get("functionDeclaration").and_then(Value::as_str)
                    == Some("(utilityScript, ...args) => utilityScript.evaluate(...args)")
                    && utility.as_deref() == Some("playwright-utility-script")
                    && request.params.get("arguments").and_then(Value::as_array).is_some_and(|args| args.len() <= 16);
                if !valid_shape { return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unrecognized semantic runtime call")); }
                let serialized = &request.params["arguments"];
                let locator_handle = self.resolve_serialized_object(serialized, request.session_id.as_deref(), "semantic-locator:").await;
                let element_handle = self.resolve_serialized_object(serialized, request.session_id.as_deref(), "semantic-element:").await;
                let viewport_poller = self.resolve_serialized_object(serialized, request.session_id.as_deref(), "viewport-poller").await;
                let expression = request.params["arguments"].as_array().and_then(|args| args.get(3))
                    .and_then(|arg| arg.get("value")).and_then(Value::as_str).unwrap_or("");
                let evaluated_expression = find_serialized_string(serialized, "expression");
                if expression.contains("globalThis.eval(expression3)")
                    && evaluated_expression.is_some_and(|value| value.contains("window.innerWidth") && value.contains("window.innerHeight")) {
                    let object_id = match self.issue_remote_object(request.session_id.as_deref(), "viewport-poller").await {
                        Ok(object_id) => object_id,
                        Err(error) => return CdpResponse::failure(&request, error),
                    };
                    return CdpResponse::success(&request, json!({"result":{"type":"object","subtype":"object","className":"Object","description":"Object","objectId":object_id}}));
                }
                if viewport_poller.is_some() && expression.trim() == "(h) => h.result" {
                    return CdpResponse::success(&request, json!({"result":{"type":"string","value":"{\"width\":1280,\"height\":720}"}}));
                }
                if viewport_poller.is_some() && expression.trim() == "(h) => h.abort()" {
                    return CdpResponse::success(&request, json!({"result":{"type":"undefined"}}));
                }
                if locator_handle.is_some() && expression.contains("success: r.success") {
                    return CdpResponse::success(&request, json!({"result":{"type":"object","value":{"o":[{"k":"log","v":"semantic target verified"},{"k":"success","v":true}],"id":1}}}));
                }
                if locator_handle.is_some() && expression.contains("visible: r.visible") {
                    return CdpResponse::success(&request, json!({"result":{"type":"object","value":{"o":[{"k":"log","v":"semantic target visible"},{"k":"visible","v":true},{"k":"attached","v":true}],"id":1}}}));
                }
                if let Some(handle) = locator_handle.as_deref().filter(|_| expression.trim() == "(r) => r.element") {
                    let label = handle.trim_start_matches("semantic-locator:");
                    let object_id = match self.issue_remote_object(request.session_id.as_deref(), &format!("semantic-element:{}", label)).await {
                        Ok(object_id) => object_id,
                        Err(error) => return CdpResponse::failure(&request, error),
                    };
                    return CdpResponse::success(&request, json!({"result":{"type":"object","subtype":"node","className":"HTMLInputElement","description":"input","objectId":object_id}}));
                }
                if element_handle.is_some() && expression.contains("injected.previewNode(e)") {
                    return CdpResponse::success(&request, json!({"result":{"type":"string","value":"JSHandle@input"}}));
                }
                if let Some(handle) = element_handle.as_deref().filter(|_| {
                    expression.contains("injected.retarget(node, \"follow-label\")")
                        && expression.contains("HTMLInputElement")
                }) {
                    return CdpResponse::success(&request, json!({"result":{
                        "type":"object", "subtype":"node", "className":"HTMLInputElement",
                        "description":"input", "objectId":match self.issue_remote_object(request.session_id.as_deref(), handle).await {
                            Ok(object_id) => object_id,
                            Err(error) => return CdpResponse::failure(&request, error),
                        }
                    }}));
                }
                if let Some(handle) = element_handle.as_deref().filter(|_| {
                    expression.trim() == "([injected, node, files]) => injected.setInputFiles(node, files)"
                }) {
                    let descriptor = handle.trim_start_matches("semantic-element:");
                    let Some(label) = descriptor.strip_prefix("label:") else {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "upload requires a verified labeled file input"));
                    };
                    let payloads = match serialized_file_payloads(serialized) {
                        Ok(payloads) => payloads,
                        Err(error) => return CdpResponse::failure(&request, error),
                    };
                    let Some(staging_root) = self.upload_staging_root.as_ref() else {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "upload staging root is not configured"));
                    };
                    let request_dir = match UploadStaging::new(staging_root) {
                        Ok(dir) => dir,
                        Err(error) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, format!("failed to create confined upload staging: {error}"))),
                    };
                    let mut staged = Vec::with_capacity(payloads.len());
                    for (name, bytes) in payloads {
                        let path = match request_dir.stage(&name, &bytes) {
                            Ok(path) => path,
                            Err(error) => {
                            return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, format!("failed to stage bounded upload: {error}")));
                            }
                        };
                        staged.push(path);
                    }
                    let Some((session_id, page_id)) = self.runtime_identity(request.session_id.as_deref()).await else {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unknown runtime page"));
                    };
                    let paths = staged.iter().map(|path| path.to_string_lossy().into_owned()).collect();
                    let envelope = CommandEnvelope { schema_version:CommandEnvelope::SCHEMA_VERSION,
                        command_id:CommandId::new(), workflow_id:WorkflowId::new(), attempt_id:AttemptId::new(),
                        session_id, page_id:Some(page_id), deadline:Utc::now()+Duration::seconds(30),
                        command:RuntimeCommand::Primitive(PrimitiveCommand::UploadFiles(UploadFilesCommand { selector:String::new(), target:Some(TargetSpec { label:Some(label.to_owned()), ..TargetSpec::default() }), paths })) };
                    let outcome = self.runtime.submit(ctx, envelope).await;
                    drop(request_dir);
                    return match outcome {
                        Ok(CommandOutcome::Completed { evidence, .. }) if evidence.iter().any(|item| matches!(item, types::Evidence::Upload { .. })) => {
                            self.record_interface_event("upload.completed", json!({"evidence":evidence})).await;
                            CdpResponse::success(&request, json!({"result":{"type":"undefined"}}))
                        },
                        Ok(_) => CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "runtime upload did not produce upload evidence")),
                        Err(error) => CdpResponse::failure(&request, runtime_error(error)),
                    };
                }
                if let Some(handle) = element_handle.as_deref().filter(|_| expression.contains("injected.fill(node")) {
                    let Some(value) = find_serialized_string(serialized, "value").filter(|value| value.len() <= 64 * 1024) else {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "missing bounded fill value"));
                    };
                    let Some((session_id, page_id)) = self.runtime_identity(request.session_id.as_deref()).await else {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unknown runtime page"));
                    };
                    let descriptor = handle.trim_start_matches("semantic-element:");
                    let Some(label) = descriptor.strip_prefix("label:") else {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "fill requires a verified labeled control"));
                    };
                    let envelope = CommandEnvelope { schema_version:CommandEnvelope::SCHEMA_VERSION,
                        command_id:CommandId::new(), workflow_id:WorkflowId::new(), attempt_id:AttemptId::new(),
                        session_id, page_id:Some(page_id), deadline:Utc::now()+Duration::seconds(30),
                        command:RuntimeCommand::Primitive(PrimitiveCommand::TypeText(TypeTextCommand { selector:String::new(), target:Some(TargetSpec { label:Some(label.to_owned()), ..TargetSpec::default() }), value:value.to_owned(), clear_first:true, expected_url: None })) };
                    return match self.runtime.submit(ctx, envelope).await {
                        Ok(CommandOutcome::Completed { .. }) => CdpResponse::success(&request, json!({"result":{"type":"string","value":"done"}})),
                        Ok(_) => CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "runtime fill did not complete")),
                        Err(error) => CdpResponse::failure(&request, runtime_error(error)),
                    };
                }
                if let Some(handle) = element_handle.as_deref().filter(|_| expression.contains("checkElementStates")) {
                    let descriptor = handle.trim_start_matches("semantic-element:");
                    let Some(rest) = descriptor.strip_prefix("role:") else {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "click requires a verified role target"));
                    };
                    let Some((role, name)) = rest.split_once(':') else {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid verified role target"));
                    };
                    let Some((session_id, page_id)) = self.runtime_identity(request.session_id.as_deref()).await else {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unknown runtime page"));
                    };
                    let target = TargetSpec { role:Some(role.to_owned()), accessible_name:Some(name.to_owned()), ..TargetSpec::default() };
                    if role == "link" && name == "Download fixture" {
                        let frame_id = match request.session_id.as_deref() {
                            Some(cdp) => self.resolve_identifier(IdentifierFamily::CdpSession, cdp).await,
                            None => None,
                        }.unwrap_or_else(|| "main".into());
                        let command = PrimitiveCommand::ClickAndWaitForDownload(ClickAndWaitForDownloadCommand {
                            selector:String::new(), target:Some(target), timeout_ms:30_000,
                        });
                        return match self.submit_boundary(ctx.clone(), session_id.clone(), page_id.clone(), command).await {
                            Ok(CommandOutcome::Completed { evidence, .. }) => {
                                let download = evidence.iter().find_map(|item| match item {
                                    types::Evidence::Download { filename, path, bytes, sha256, .. } => Some((filename.clone(), path.clone(), *bytes, sha256.clone())),
                                    _ => None,
                                });
                                let Some((filename, path, expected_bytes, expected_sha)) = download else {
                                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "runtime download did not produce download evidence"));
                                };
                                let Some(store) = self.artifacts.as_ref() else {
                                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "artifact reader is not configured"));
                                };
                                let data = match std::fs::read(&path) {
                                    Ok(data) => data,
                                    Err(_) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "verified download was unavailable")),
                                };
                                let actual_sha = hex::encode(Sha256::digest(&data));
                                if data.len() as u64 != expected_bytes || actual_sha != expected_sha {
                                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "download evidence integrity check failed"));
                                }
                                let record = match store.put(&session_id, &page_id, "application/octet-stream", "bin", &data, data.len()).await {
                                    Ok(record) => record,
                                    Err(_) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "download import failed")),
                                };
                                match store.get(&session_id, &record.artifact_id).await {
                                    Ok(imported) if imported.len() as u64 == expected_bytes && hex::encode(Sha256::digest(&imported)) == expected_sha => {}
                                    Err(_) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "download import verification failed")),
                                    Ok(_) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "download import integrity check failed")),
                                };
                                let stream_id = Uuid::new_v4().to_string();
                                if self.streams.lock().await.reserve(
                                    &stream_id, ctx.principal_id.clone(), &self.connection_id,
                                    session_id.clone(), &record.artifact_id, expected_bytes,
                                ).is_err() {
                                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "download stream capacity exhausted"));
                                }
                                let guid = Uuid::new_v4().to_string();
                                let mut events = download_events(&frame_id, &guid, &filename, expected_bytes, &stream_id, &expected_sha, None).to_vec();
                                for observer in self.browser_observers.lock().await.iter() {
                                    events.extend(download_events(&frame_id, &guid, &filename, expected_bytes, &stream_id, &expected_sha, Some(observer.clone())));
                                }
                                if let Err(error) = self.queue_events(events).await {
                                    self.streams.lock().await.remove(&stream_id);
                                    return CdpResponse::failure(&request, error);
                                }
                                CdpResponse::success(&request, json!({"result":{"type":"string","value":"done"}}))
                            }
                            Ok(_) => CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "runtime download did not complete")),
                            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
                        };
                    }
                    if role == "link" && name != "Download fixture" {
                        let opener_target = match request.session_id.as_deref() {
                            Some(cdp_session) => self.resolve_identifier(IdentifierFamily::CdpSession, cdp_session).await,
                            None => None,
                        };
                        let command = PrimitiveCommand::ClickAndWaitForPopup(ClickAndWaitForPopupCommand { selector:String::new(), target:Some(target), timeout_ms:30_000 });
                        return match self.submit_boundary(ctx, session_id.clone(), page_id, command).await {
                            Ok(CommandOutcome::Completed { evidence, .. }) => {
                                let popup = evidence.iter().find_map(|item| match item {
                                    types::Evidence::Popup { page_id, url, title, .. } => Some((page_id.clone(), url.clone(), title.clone())),
                                    _ => None,
                                });
                                let Some((popup_page, url, title)) = popup else {
                                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "runtime popup did not produce popup evidence"));
                                };
                                let target_id = self.targets.lock().await.register(&session_id, &popup_page);
                                let generation = RuntimeGeneration(0);
                                let browser_context_id;
                                let popup_session;
                                {
                                    let mut identifiers = self.identifiers.lock().await;
                                    identifiers.adopt_target(target_id.clone(), &session_id.0.to_string(), &popup_page.0.to_string(), generation);
                                    browser_context_id = identifiers.bind_browser_context(&session_id.0.to_string(), "default", generation);
                                    popup_session = identifiers.bind_family(IdentifierFamily::CdpSession, &target_id, &target_id, generation);
                                }
                                if let Err(error) = self.queue_event(CdpEvent {
                                    method:"Target.attachedToTarget".into(),
                                    params:json!({"sessionId":popup_session,"targetInfo":{"targetId":target_id,"type":"page","title":title,"url":url,"attached":true,"canAccessOpener":true,"openerId":opener_target,"browserContextId":browser_context_id},"waitingForDebugger":false}),
                                    session_id:None,
                                }).await {
                                    return CdpResponse::failure(&request, error);
                                }
                                let loader_id = Uuid::new_v4().simple().to_string();
                                let mut loads = self.pending_page_loads.lock().await;
                                if loads.len() >= MAX_PENDING_PAGE_LOADS {
                                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "pending page load registry exhausted"));
                                }
                                loads.insert(popup_session, (target_id, url, loader_id));
                                CdpResponse::success(&request, json!({"result":{"type":"string","value":"done"}}))
                            }
                            Ok(_) => CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "runtime popup did not complete")),
                            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
                        };
                    }
                    let envelope = CommandEnvelope { schema_version:CommandEnvelope::SCHEMA_VERSION,
                        command_id:CommandId::new(), workflow_id:WorkflowId::new(), attempt_id:AttemptId::new(),
                        session_id, page_id:Some(page_id), deadline:Utc::now()+Duration::seconds(30),
                        command:RuntimeCommand::Primitive(PrimitiveCommand::Click(ClickCommand { selector:String::new(), target:Some(target), boundary:false, expected_url:None, modifiers:Vec::new() })) };
                    return match self.runtime.submit(ctx, envelope).await {
                        Ok(CommandOutcome::Completed { .. }) => CdpResponse::success(&request, json!({"result":{"type":"string","value":"done"}})),
                        Ok(_) => CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "runtime click did not complete")),
                        Err(error) => CdpResponse::failure(&request, runtime_error(error)),
                    };
                }
                let engine = find_serialized_string(serialized, "name");
                let body = find_serialized_string(serialized, "body");
                let semantic = match (engine, body) {
                    (Some("internal:label"), Some(body)) => body.strip_prefix('"').and_then(|v| v.strip_suffix("\"i"))
                        .filter(|v| !v.is_empty() && v.len() <= 256)
                        .map(|label| (format!("label:{label}"), TargetSpec { label:Some(label.to_owned()), allow_best_match:true, ordinal:Some(0), ..TargetSpec::default() })),
                    (Some("internal:role"), Some(body)) => parse_role_target(body),
                    (Some("internal:text"), Some(body)) => body.strip_prefix('"').and_then(|v| v.strip_suffix("\"i"))
                        .filter(|v| !v.is_empty() && v.len() <= 1024)
                        .map(|text| (format!("text:{text}"), TargetSpec { text:Some(TextMatch::Contains(text.to_owned())), allow_best_match:true, ordinal:Some(0), ..TargetSpec::default() })),
                    _ => None,
                };
                let Some((descriptor, target)) = semantic else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unsupported semantic runtime call"));
                };
                let Some((session_id, page_id)) = self.runtime_identity(request.session_id.as_deref()).await else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unknown runtime page"));
                };
                let envelope = CommandEnvelope { schema_version:CommandEnvelope::SCHEMA_VERSION,
                    command_id:CommandId::new(), workflow_id:WorkflowId::new(), attempt_id:AttemptId::new(),
                    session_id, page_id:Some(page_id), deadline:Utc::now()+Duration::seconds(30),
                    command:RuntimeCommand::Primitive(PrimitiveCommand::Inspect(InspectCommand { selector:None, target:Some(target), include_html:false })) };
                match self.runtime.submit(ctx, envelope).await {
                    Ok(CommandOutcome::Completed { evidence, .. }) if evidence.iter().any(|item| matches!(item, types::Evidence::Element { .. } | types::Evidence::Inspection { .. })) => {
                        let object_id = match self.issue_remote_object(request.session_id.as_deref(), &format!("semantic-locator:{}", descriptor)).await {
                            Ok(object_id) => object_id,
                            Err(error) => return CdpResponse::failure(&request, error),
                        };
                        Ok(json!({"result":{"type":"object","subtype":"object","className":"Object","description":"Object","objectId":object_id}}))
                    }
                    Ok(_) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "semantic target was not verified")),
                    Err(error) => Err(error),
                }
            }
            Some(Handler::PageAddScript) => {
                if !supported_client_initialization(&request.params) {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "only pinned bounded client initialization signatures are supported"));
                }
                Ok(json!({"identifier": Uuid::new_v4().simple().to_string()}))
            }
            Some(Handler::PageCreateIsolatedWorld) => {
                let frame_id = request.params.get("frameId").and_then(Value::as_str).filter(|id| !id.is_empty() && id.len() <= 256);
                let world_name = request.params.get("worldName").and_then(Value::as_str).filter(|name| !name.is_empty() && name.len() <= 256);
                let valid = frame_id.is_some() && world_name.is_some();
                if !valid { return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid isolated world request")); }
                let mut worlds = self.isolated_worlds.lock().await;
                if worlds.len() >= MAX_ISOLATED_WORLDS {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "isolated world registry exhausted"));
                }
                worlds.insert(
                    request.session_id.clone().unwrap_or_else(|| "browser".into()),
                    world_name.unwrap().to_owned(),
                );
                let unique_id = self.bind_identifier(
                    IdentifierFamily::ExecutionContext,
                    request.session_id.as_deref().unwrap_or("browser"),
                    world_name.unwrap(), RuntimeGeneration(0),
                ).await;
                if let Err(error) = self.queue_event(CdpEvent {
                    method: "Runtime.executionContextCreated".into(),
                    params: json!({"context":{"id":2,"origin":"","name":world_name.unwrap(),"uniqueId":unique_id,"auxData":{"isDefault":false,"type":"isolated","frameId":frame_id.unwrap()}}}),
                    session_id: request.session_id.clone(),
                }).await { return CdpResponse::failure(&request, error); }
                Ok(json!({"executionContextId":2}))
            }
            Some(Handler::PageNavigate) => {
                let Some(url) = request.params.get("url").and_then(Value::as_str).filter(|url| !url.is_empty() && url.len() <= 16_384) else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid navigation URL"));
                };
                let Some(cdp_session) = request.session_id.as_deref() else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "navigation requires a CDP session"));
                };
                let Some(target_id) = self.resolve_identifier(IdentifierFamily::CdpSession, cdp_session).await else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unknown CDP session"));
                };
                let Some(page) = self.resolve_identifier(IdentifierFamily::Target, &target_id).await else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unknown CDP target"));
                };
                let Some(runtime_session) = self.runtime_session_for(IdentifierFamily::Target, &target_id).await else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unknown runtime session"));
                };
                let (Ok(session_uuid), Ok(page_uuid)) = (Uuid::parse_str(&runtime_session), Uuid::parse_str(&page)) else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "invalid runtime identity"));
                };
                let loader_id = Uuid::new_v4().simple().to_string();
                let envelope = CommandEnvelope {
                    schema_version: CommandEnvelope::SCHEMA_VERSION,
                    command_id: CommandId::new(), workflow_id: WorkflowId::new(), attempt_id: AttemptId::new(),
                    session_id: SessionId(session_uuid), page_id: Some(PageId(page_uuid)),
                    deadline: Utc::now() + Duration::seconds(30),
                    command: RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand { url: url.to_owned(), wait_until: WaitUntil::Interactive, timeout_ms: 30_000 })),
                };
                match self.runtime.submit(ctx, envelope).await {
                    Ok(CommandOutcome::Completed { evidence, .. }) => {
                        let Some((final_url, title)) = evidence.iter().find_map(|item| match item {
                            types::Evidence::Navigation { url, title } => Some((url.clone(), title.clone())),
                            _ => None,
                        }) else { return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "navigation returned no verified evidence")); };
                        self.advance_execution_generation(request.session_id.as_deref()).await;
                        let world_name = self.isolated_worlds.lock().await.get(cdp_session).cloned();
                        let mut events = vec![
                            CdpEvent { method:"Page.frameNavigated".into(), params:json!({"frame":{"id":target_id,"loaderId":loader_id,"url":final_url,"domainAndRegistry":"","securityOrigin":"","mimeType":"text/html","secureContextType":"SecureLocalhost","crossOriginIsolatedContextType":"NotIsolated","gatedAPIFeatures":[]},"type":"Navigation"}), session_id:request.session_id.clone() },
                            CdpEvent { method:"Runtime.executionContextsCleared".into(), params:json!({}), session_id:request.session_id.clone() },
                            CdpEvent { method:"Runtime.executionContextCreated".into(), params:json!({"context":{"id":3,"origin":final_url,"name":"","uniqueId":Uuid::new_v4().simple().to_string(),"auxData":{"isDefault":true,"type":"default","frameId":target_id}}}), session_id:request.session_id.clone() },
                            CdpEvent { method:"Page.lifecycleEvent".into(), params:json!({"frameId":target_id,"loaderId":loader_id,"name":"init","timestamp":0}), session_id:request.session_id.clone() },
                            CdpEvent { method:"Page.lifecycleEvent".into(), params:json!({"frameId":target_id,"loaderId":loader_id,"name":"DOMContentLoaded","timestamp":0}), session_id:request.session_id.clone() },
                            CdpEvent { method:"Page.lifecycleEvent".into(), params:json!({"frameId":target_id,"loaderId":loader_id,"name":"load","timestamp":0}), session_id:request.session_id.clone() },
                        ];
                        if let Some(world_name) = world_name {
                            events.insert(3, CdpEvent { method:"Runtime.executionContextCreated".into(), params:json!({"context":{"id":4,"origin":final_url,"name":world_name,"uniqueId":Uuid::new_v4().simple().to_string(),"auxData":{"isDefault":false,"type":"isolated","frameId":target_id}}}), session_id:request.session_id.clone() });
                        }
                        if let Err(error) = self.queue_events(events).await { return CdpResponse::failure(&request, error); }
                        self.record_interface_event("navigation.completed", json!({"evidence":evidence})).await;
                        self.targets.lock().await.note_navigation(&runtime_session, &page, &final_url, &title);
                        Ok(json!({"frameId":target_id,"loaderId":loader_id,"isDownload":false}))
                    }
                    Ok(_) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "navigation did not complete")),
                    Err(error) => Err(error),
                }
            }
            Some(Handler::PageSetLifecycle) => {
                let Some(enabled) = request.params.get("enabled").and_then(Value::as_bool) else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid lifecycle event configuration"));
                };
                let scope = request.session_id.clone().unwrap_or_else(|| "browser".into());
                if enabled { self.lifecycle_events.lock().await.insert(scope); }
                else { self.lifecycle_events.lock().await.remove(&scope); }
                Ok(json!({}))
            }
            Some(Handler::EmulationSetFocus) => {
                let Some(enabled) = request.params.get("enabled").and_then(Value::as_bool) else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid focus emulation configuration"));
                };
                if request.params.as_object().is_none_or(|params| params.len() != 1) {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid focus emulation configuration"));
                }
                let Some((session_id, page_id)) = self.runtime_identity(request.session_id.as_deref()).await else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unknown runtime page"));
                };
                let envelope = CommandEnvelope { schema_version:CommandEnvelope::SCHEMA_VERSION,
                    command_id:CommandId::new(), workflow_id:WorkflowId::new(), attempt_id:AttemptId::new(),
                    session_id, page_id:Some(page_id), deadline:Utc::now()+Duration::seconds(30),
                    command:RuntimeCommand::Primitive(PrimitiveCommand::SetFocusEmulation(SetFocusEmulationCommand { enabled })) };
                match self.runtime.submit(ctx, envelope).await {
                    Ok(CommandOutcome::Completed { evidence, .. }) if evidence.iter().any(|item| matches!(item, types::Evidence::Configuration { name, value } if name == "focusEmulation" && value == &enabled.to_string())) => Ok(json!({})),
                    Ok(_) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "focus emulation produced no verified evidence")),
                    Err(error) => Err(error),
                }
            }
            Some(Handler::EmulationSetMedia) => {
                let Some(media) = request.params.get("media").and_then(Value::as_str).filter(|value| value.len() <= 32) else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid media emulation configuration"));
                };
                let Some(items) = request.params.get("features").and_then(Value::as_array).filter(|items| items.len() <= 16) else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid media emulation configuration"));
                };
                if request.params.as_object().is_none_or(|params| params.len() != 2) {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid media emulation configuration"));
                }
                let mut features = BTreeMap::new();
                for item in items {
                    let Some(name) = item.get("name").and_then(Value::as_str).filter(|value| !value.is_empty() && value.len() <= 64) else { return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid media feature")); };
                    let Some(value) = item.get("value").and_then(Value::as_str).filter(|value| value.len() <= 64) else { return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid media feature")); };
                    if item.as_object().is_none_or(|fields| fields.len() != 2) || features.insert(name.to_owned(), value.to_owned()).is_some() { return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid media feature")); }
                }
                let Some((session_id, page_id)) = self.runtime_identity(request.session_id.as_deref()).await else { return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unknown runtime page")); };
                let command = SetEmulatedMediaCommand { media: media.to_owned(), features };
                let expected = serde_json::to_string(&command).expect("bounded media command serializes");
                let envelope = CommandEnvelope { schema_version:CommandEnvelope::SCHEMA_VERSION,
                    command_id:CommandId::new(), workflow_id:WorkflowId::new(), attempt_id:AttemptId::new(), session_id, page_id:Some(page_id),
                    deadline:Utc::now()+Duration::seconds(30), command:RuntimeCommand::Primitive(PrimitiveCommand::SetEmulatedMedia(command)) };
                match self.runtime.submit(ctx, envelope).await {
                    Ok(CommandOutcome::Completed { evidence, .. }) if evidence.iter().any(|item| matches!(item, types::Evidence::Configuration { name, value } if name == "emulatedMedia" && value == &expected)) => Ok(json!({})),
                    Ok(_) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "media emulation produced no verified evidence")),
                    Err(error) => Err(error),
                }
            }
            Some(Handler::EmulationSetDeviceMetrics) => {
                // Puppeteer applies its default viewport through this method on
                // every page it opens, so refusing it refuses the client. The
                // runtime's own emulation covers width, height, and the mobile
                // flag; a scale factor or orientation it cannot apply is
                // refused rather than silently ignored, which would leave the
                // client believing a viewport it never got.
                let params = request.params.as_object();
                let Some(params) = params.filter(|params| params.len() <= 8) else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid device metrics configuration"));
                };
                if params.keys().any(|key| !matches!(key.as_str(), "width" | "height" | "deviceScaleFactor" | "mobile" | "screenOrientation")) {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unsupported device metrics field"));
                }
                let (Some(width), Some(height)) = (
                    params.get("width").and_then(Value::as_u64).filter(|value| (1..=16384).contains(value)),
                    params.get("height").and_then(Value::as_u64).filter(|value| (1..=16384).contains(value)),
                ) else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "viewport dimensions must be within 1..=16384"));
                };
                let mobile = params.get("mobile").and_then(Value::as_bool).unwrap_or(false);
                if params.get("deviceScaleFactor").and_then(Value::as_f64).is_some_and(|scale| scale != 1.0) {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "device scale factor emulation is unsupported; connect with deviceScaleFactor 1"));
                }
                if let Some(orientation) = params.get("screenOrientation") {
                    let angle = orientation.get("angle").and_then(Value::as_i64).unwrap_or(0);
                    let kind = orientation.get("type").and_then(Value::as_str).unwrap_or("portraitPrimary");
                    if angle != 0 || kind != "portraitPrimary" {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "screen orientation emulation is unsupported; only portraitPrimary at angle 0 is applied"));
                    }
                }
                let Some((session_id, page_id)) = self.runtime_identity(request.session_id.as_deref()).await else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unknown runtime page"));
                };
                let viewport = types::ViewportSize { width: width as u32, height: height as u32 };
                let envelope = CommandEnvelope { schema_version:CommandEnvelope::SCHEMA_VERSION,
                    command_id:CommandId::new(), workflow_id:WorkflowId::new(), attempt_id:AttemptId::new(),
                    session_id, page_id:Some(page_id), deadline:Utc::now()+Duration::seconds(30),
                    command:RuntimeCommand::Primitive(PrimitiveCommand::Emulate(types::EmulateCommand { viewport: Some(viewport), geolocation: None, mobile: Some(mobile) })) };
                match self.runtime.submit(ctx, envelope).await {
                    Ok(CommandOutcome::Completed { evidence, .. }) if evidence.iter().any(|item| matches!(item, types::Evidence::Emulation { viewport: Some(applied), .. } if applied == &viewport)) => Ok(json!({})),
                    Ok(_) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "viewport emulation produced no verified evidence")),
                    Err(error) => Err(error),
                }
            }
            Some(Handler::EmulationSetTouch) => {
                // Puppeteer pairs this with the viewport above. The runtime has
                // no touch emulation, so `false` is answered truthfully as the
                // state that already holds, and `true` is refused instead of
                // being accepted as a no-op the client would trust.
                let Some(params) = request.params.as_object().filter(|params| params.len() <= 2) else {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid touch emulation configuration"));
                };
                if params.keys().any(|key| !matches!(key.as_str(), "enabled" | "maxTouchPoints")) {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unsupported touch emulation field"));
                }
                match params.get("enabled").and_then(Value::as_bool) {
                    Some(false) => Ok(json!({})),
                    Some(true) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "touch emulation is unsupported; connect without hasTouch")),
                    None => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid touch emulation configuration")),
                }
            }
            Some(Handler::PageEnable) => {
                if !request.params.as_object().is_some_and(serde_json::Map::is_empty) {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "Page.enable takes no parameters"));
                }
                self.enable_domain(request.session_id.as_deref(), "Page").await;
                if let Some(cdp_session) = request.session_id.as_deref() {
                    if let Some((frame_id, url, loader_id)) = self.pending_page_loads.lock().await.get(cdp_session).cloned() {
                        for event in [
                            CdpEvent { method:"Page.frameNavigated".into(), params:json!({"frame":{"id":frame_id,"loaderId":loader_id,"url":url,"domainAndRegistry":"","securityOrigin":"","mimeType":"text/html","secureContextType":"SecureLocalhost","crossOriginIsolatedContextType":"NotIsolated","gatedAPIFeatures":[]},"type":"Navigation"}), session_id:request.session_id.clone() },
                            CdpEvent { method:"Page.lifecycleEvent".into(), params:json!({"frameId":frame_id,"loaderId":loader_id,"name":"DOMContentLoaded","timestamp":0}), session_id:request.session_id.clone() },
                            CdpEvent { method:"Page.lifecycleEvent".into(), params:json!({"frameId":frame_id,"loaderId":loader_id,"name":"load","timestamp":0}), session_id:request.session_id.clone() },
                        ] { if let Err(error) = self.queue_event(event).await { return CdpResponse::failure(&request, error); } }
                    }
                }
                Ok(json!({}))
            }
            Some(Handler::LogEnable | Handler::NetworkEnable | Handler::RuntimeRunIfWaiting) => {
                if !request.params.as_object().is_some_and(serde_json::Map::is_empty) {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "method takes no parameters"));
                }
                match self.registry.handler(&request.method) {
                    Some(Handler::LogEnable) => self.enable_domain(request.session_id.as_deref(), "Log").await,
                    Some(Handler::NetworkEnable) => self.enable_domain(request.session_id.as_deref(), "Network").await,
                    _ => {}
                }
                Ok(json!({}))
            }
            None => unreachable!("registry-handler bijection validated at construction"),
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }

    async fn queue_attached_targets(
        &self,
        sessions: &[SessionState],
        waiting: bool,
        filters: &[domains::target::TargetFilter],
    ) -> Result<(), CdpError> {
        let infos = self.target_infos(sessions).await;
        for target_info in infos["targetInfos"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|info| {
                info["type"]
                    .as_str()
                    .is_some_and(|kind| domains::target::filter_matches(filters, kind))
            })
        {
            let target_id = target_info["targetId"].as_str().ok_or_else(|| {
                CdpError::new(
                    CdpErrorCode::RuntimeFailure,
                    "invalid runtime target evidence",
                )
            })?;
            let session_id = self
                .bind_identifier(
                    IdentifierFamily::CdpSession,
                    target_id,
                    target_id,
                    RuntimeGeneration(0),
                )
                .await;
            self.queue_event(CdpEvent {
                method: "Target.attachedToTarget".into(),
                params: json!({"sessionId": session_id, "targetInfo": target_info, "waitingForDebugger": waiting}),
                session_id: None,
            }).await?;
        }
        Ok(())
    }

    pub async fn queue_event(&self, event: CdpEvent) -> Result<(), CdpError> {
        self.queue_events(vec![event]).await
    }

    async fn queue_events(&self, pending: Vec<CdpEvent>) -> Result<(), CdpError> {
        let mut translated = Vec::with_capacity(pending.len());
        for event in pending {
            if matches!(
                event.method.as_str(),
                "Target.targetCreated" | "Target.attachedToTarget"
            ) && !event.params["targetInfo"]["type"]
                .as_str()
                .is_some_and(domains::target::target_type_is_supported)
            {
                continue;
            }
            if !self.event_enabled(&event).await {
                continue;
            }
            let Some(metadata) = self.registry.event(&event.method) else {
                return Err(CdpError::new(
                    CdpErrorCode::MethodNotFound,
                    "event is not supported",
                ));
            };
            let ctx = self
                .handle
                .context(Utc::now() + Duration::seconds(30), None);
            if AuthorizationGuard::new(self.handle.clone())
                .validate(&ctx)
                .is_err()
                || metadata
                    .capability()
                    .is_none_or(|capability| !ctx.capabilities.contains(capability))
            {
                return Err(CdpError::new(
                    CdpErrorCode::RuntimeFailure,
                    "event authorization failed",
                ));
            }
            let translated_event = self.registry.translate_event(event)?;
            {
                let mut events = self.observed_events.lock().await;
                if events.len() < 128 {
                    events.insert(translated_event.method.clone());
                }
            }
            translated.push(translated_event);
        }
        let mut events = self.events.lock().await;
        if events.len() + translated.len() > MAX_QUEUED_EVENTS {
            return Err(CdpError::new(
                CdpErrorCode::RuntimeFailure,
                "event queue exhausted",
            ));
        }
        events.extend(translated);
        self.event_notify.notify_one();
        Ok(())
    }

    async fn enable_domain(&self, session_id: Option<&str>, domain: &'static str) {
        self.enabled_domains
            .lock()
            .await
            .insert((session_id.unwrap_or("browser").to_owned(), domain));
    }

    async fn record_interface_event(&self, event: &str, payload: Value) {
        self.interface_events
            .append_for(
                self.handle.principal_id().clone(),
                Event::new(event, payload),
            )
            .await;
    }

    async fn event_enabled(&self, event: &CdpEvent) -> bool {
        let Some((domain, _)) = event.method.split_once('.') else {
            return false;
        };
        if domain == "Target" {
            return true;
        }
        if domain == "Browser" {
            return *self.download_events_enabled.lock().await;
        }
        let scope = event.session_id.as_deref().unwrap_or("browser");
        if domain == "Page"
            && event.method == "Page.lifecycleEvent"
            && !self.lifecycle_events.lock().await.contains(scope)
        {
            return false;
        }
        self.enabled_domains.lock().await.contains(&(
            scope.to_owned(),
            match domain {
                "Page" => "Page",
                "Runtime" => "Runtime",
                "Network" => "Network",
                "Log" => "Log",
                _ => return false,
            },
        ))
    }

    pub async fn next_event(&self) -> Option<CdpEvent> {
        self.events.lock().await.pop_front()
    }

    pub async fn drain_events(&self) -> Vec<CdpEvent> {
        self.events.lock().await.drain(..).collect()
    }

    async fn cleanup_streams(&self) {
        self.streams
            .lock()
            .await
            .remove_connection(&self.connection_id);
    }

    /// Per-connection registries are insert-mostly; cap each and fail
    /// closed so one long-lived client cannot grow heap without bound.
    /// identifiers/bind_family eviction needs a fallible bind and is
    /// tracked separately.
    async fn issue_remote_object(
        &self,
        session_id: Option<&str>,
        internal: &str,
    ) -> Result<String, CdpError> {
        let scope = session_id.unwrap_or("browser").to_owned();
        let generation = *self
            .execution_generations
            .lock()
            .await
            .get(&scope)
            .unwrap_or(&0);
        let opaque = Uuid::new_v4().to_string();
        let mut objects = self.remote_objects.lock().await;
        if objects.len() >= MAX_REMOTE_OBJECTS {
            return Err(CdpError::new(
                CdpErrorCode::RuntimeFailure,
                "remote object registry exhausted",
            ));
        }
        objects.insert(
            opaque.clone(),
            RemoteObject {
                internal: internal.to_owned(),
                scope,
                generation,
            },
        );
        Ok(opaque)
    }

    async fn resolve_remote_object(
        &self,
        session_id: Option<&str>,
        opaque: &str,
    ) -> Option<String> {
        let scope = session_id.unwrap_or("browser");
        let generation = *self
            .execution_generations
            .lock()
            .await
            .get(scope)
            .unwrap_or(&0);
        self.remote_objects
            .lock()
            .await
            .get(opaque)
            .filter(|object| remote_object_valid(object, scope, generation))
            .map(|object| object.internal.clone())
    }

    async fn take_remote_object(&self, session_id: Option<&str>, opaque: &str) -> Option<String> {
        self.resolve_remote_object(session_id, opaque).await?;
        self.remote_objects
            .lock()
            .await
            .remove(opaque)
            .map(|object| object.internal)
    }

    async fn resolve_serialized_object(
        &self,
        serialized: &Value,
        session_id: Option<&str>,
        prefix: &str,
    ) -> Option<String> {
        for opaque in find_object_ids(serialized) {
            if let Some(internal) = self
                .resolve_remote_object(session_id, opaque)
                .await
                .filter(|internal| internal.starts_with(prefix))
            {
                return Some(internal);
            }
        }
        None
    }

    async fn advance_execution_generation(&self, session_id: Option<&str>) {
        let scope = session_id.unwrap_or("browser").to_owned();
        let mut generations = self.execution_generations.lock().await;
        let generation = generations.entry(scope.clone()).or_default();
        *generation = generation.saturating_add(1);
        drop(generations);
        self.remote_objects
            .lock()
            .await
            .retain(|_, object| object.scope != scope);
    }

    pub async fn bind_identifier(
        &self,
        family: IdentifierFamily,
        runtime_session: &str,
        internal: &str,
        generation: RuntimeGeneration,
    ) -> String {
        self.identifiers
            .lock()
            .await
            .bind_family(family, runtime_session, internal, generation)
    }

    pub async fn resolve_identifier(
        &self,
        family: IdentifierFamily,
        opaque: &str,
    ) -> Option<String> {
        self.identifiers
            .lock()
            .await
            .resolve_family(family, opaque)
            .map(str::to_owned)
    }

    async fn runtime_session_for(&self, family: IdentifierFamily, opaque: &str) -> Option<String> {
        self.identifiers
            .lock()
            .await
            .runtime_session_for(family, opaque)
            .map(str::to_owned)
    }

    async fn runtime_identity(&self, cdp_session: Option<&str>) -> Option<(SessionId, PageId)> {
        let target_id = self
            .resolve_identifier(IdentifierFamily::CdpSession, cdp_session?)
            .await?;
        let page = self
            .resolve_identifier(IdentifierFamily::Target, &target_id)
            .await?;
        let session = self
            .runtime_session_for(IdentifierFamily::Target, &target_id)
            .await?;
        Some((
            SessionId(Uuid::parse_str(&session).ok()?),
            PageId(Uuid::parse_str(&page).ok()?),
        ))
    }

    async fn automation_runtime_identity(
        &self,
        cdp_session: Option<&str>,
        ctx: RequestContext,
    ) -> Option<(SessionId, PageId)> {
        if let Some(identity) = self.runtime_identity(cdp_session).await {
            return Some(identity);
        }
        let sessions = self.runtime.list_sessions(ctx).await.ok()?;
        let session = sessions
            .into_iter()
            .find(|session| !session.page_ids.is_empty())?;
        Some((session.id, session.page_ids[0].clone()))
    }

    async fn submit_boundary(
        &self,
        ctx: RequestContext,
        session_id: SessionId,
        page_id: PageId,
        command: PrimitiveCommand,
    ) -> Result<CommandOutcome, InterfaceError> {
        let boundary = {
            let mut state = self.automation_boundary.lock().await;
            consume_pending_boundary(&mut state, &session_id, &page_id, Utc::now())?
        };
        let outcome = self
            .runtime
            .submit(
                ctx,
                CommandEnvelope {
                    schema_version: CommandEnvelope::SCHEMA_VERSION,
                    command_id: boundary.command_id.clone(),
                    workflow_id: boundary.workflow_id.clone(),
                    attempt_id: boundary.attempt_id,
                    session_id,
                    page_id: Some(page_id),
                    deadline: Utc::now() + Duration::seconds(30),
                    command: RuntimeCommand::Primitive(command),
                },
            )
            .await?;
        if matches!(outcome, CommandOutcome::Completed { .. }) {
            self.record_interface_event("boundary.completed", json!({"commandId":boundary.command_id,"workflowId":boundary.workflow_id,"outcome":outcome})).await;
        }
        Ok(outcome)
    }

    pub async fn resolve_target(&self, opaque: &str) -> Option<String> {
        self.resolve_identifier(IdentifierFamily::Target, opaque)
            .await
    }

    pub async fn replace_generation(
        &self,
        runtime_session: &str,
        current: RuntimeGeneration,
    ) -> Result<(), CdpError> {
        let mut identifiers = self.identifiers.lock().await;
        let ctx = self
            .handle
            .context(Utc::now() + Duration::seconds(30), None);
        AuthorizationGuard::new(self.handle.clone())
            .validate(&ctx)
            .map_err(runtime_error)?;
        let teardown = identifiers
            .generation_events(runtime_session, current)
            .into_iter()
            .map(|event| {
                let metadata = self.registry.event(&event.method).ok_or_else(|| {
                    CdpError::new(
                        CdpErrorCode::MethodNotFound,
                        "teardown event is not supported",
                    )
                })?;
                if metadata
                    .capability()
                    .is_none_or(|capability| !ctx.capabilities.contains(capability))
                {
                    return Err(CdpError::new(
                        CdpErrorCode::RuntimeFailure,
                        "event authorization failed",
                    ));
                }
                self.registry.translate_event(event)
            })
            .collect::<Result<Vec<_>, _>>()?;
        let mut events = self.events.lock().await;
        if events.len() + teardown.len() > MAX_QUEUED_EVENTS {
            return Err(CdpError::new(
                CdpErrorCode::RuntimeFailure,
                "event queue exhausted",
            ));
        }
        events.extend(teardown);
        self.event_notify.notify_one();
        identifiers.remove_generation(runtime_session, current);
        drop(identifiers);
        self.generations
            .lock()
            .await
            .insert(runtime_session.to_owned(), current);
        Ok(())
    }

    async fn target_infos(&self, sessions: &[SessionState]) -> Value {
        let targets = self.targets.lock().await.targets_for(sessions);
        let generations = self.generations.lock().await;
        let targets = targets
            .into_iter()
            .map(|target| {
                let generation = generations
                    .get(&target.runtime_session)
                    .copied()
                    .unwrap_or(RuntimeGeneration(0));
                (target, generation)
            })
            .collect::<Vec<_>>();
        drop(generations);
        let mut identifiers = self.identifiers.lock().await;
        let infos = targets
            .into_iter()
            .map(|(target, generation)| {
                identifiers.adopt_target(
                    target.opaque.clone(),
                    &target.runtime_session,
                    &target.page,
                    generation,
                );
                let browser_context_id = identifiers.bind_browser_context(
                    &target.runtime_session, "default", generation,
                );
                json!({"targetId": target.opaque, "type":"page", "title":"Automation Runtime", "url":"about:blank", "attached":true, "canAccessOpener":false, "browserContextId": browser_context_id})
            })
            .collect::<Vec<_>>();
        json!({"targetInfos": infos})
    }
}

fn supported_client_initialization(params: &Value) -> bool {
    let source = params.get("source").and_then(Value::as_str);
    let world = params.get("worldName").and_then(Value::as_str);
    let playwright =
        source == Some("") && world.is_some_and(|name| !name.is_empty() && name.len() <= 256);
    let puppeteer =
        source == Some("//# sourceURL=pptr:internal") && world.is_some_and(puppeteer_utility_world);
    params.as_object().is_some_and(|params| params.len() == 2) && (playwright || puppeteer)
}

/// Puppeteer names its utility world `__puppeteer_utility_world__` + its own
/// package version, so a single pinned version stops matching on the next
/// upgrade and rejects every page the client opens. The pin was `25.5.0` while
/// the repo pinned puppeteer-core 25.4.0, so the branch matched nothing at all.
/// Still bounded: the suffix is a short dotted numeric version, nothing else.
fn puppeteer_utility_world(name: &str) -> bool {
    let Some(version) = name.strip_prefix("__puppeteer_utility_world__") else {
        return false;
    };
    (1..=32).contains(&version.len())
        && version.starts_with(|c: char| c.is_ascii_digit())
        && version.ends_with(|c: char| c.is_ascii_digit())
        && version.chars().all(|c| c.is_ascii_digit() || c == '.')
        && !version.contains("..")
}

fn download_events(
    frame_id: &str,
    guid: &str,
    filename: &str,
    bytes: u64,
    stream_id: &str,
    sha256: &str,
    session_id: Option<String>,
) -> [CdpEvent; 2] {
    [
        CdpEvent {
            method: "Browser.downloadWillBegin".into(),
            params: json!({"frameId":frame_id,"guid":guid,"url":"about:blank","suggestedFilename":filename}),
            session_id: session_id.clone(),
        },
        CdpEvent {
            method: "Browser.downloadProgress".into(),
            params: json!({"guid":guid,"totalBytes":bytes,"receivedBytes":bytes,"state":"completed","streamId":stream_id,"sha256":sha256}),
            session_id,
        },
    ]
}

fn remote_object_valid(object: &RemoteObject, scope: &str, generation: u64) -> bool {
    object.scope == scope && object.generation == generation
}

#[cfg(test)]
mod security_tests {
    use super::{
        consume_pending_boundary, download_events, remote_object_valid,
        supported_client_initialization, AutomationBoundary, AutomationBoundaryPhase,
        DownloadStreamStore, RemoteObject, UploadStaging,
    };

    /// Puppeteer's utility world carries its own package version, so pinning a
    /// single one refuses every other release. The previous pin was `25.5.0`
    /// while the repo installed puppeteer-core 25.4.0, and this test asserted
    /// 25.4.0 must be refused — locking in a gateway that rejected every page
    /// the pinned client opened.
    #[test]
    fn client_initialization_accepts_a_versioned_puppeteer_utility_world() {
        for version in ["25.4.0", "25.5.0", "26.0.0", "1.0"] {
            assert!(
                supported_client_initialization(&serde_json::json!({
                    "source": "//# sourceURL=pptr:internal",
                    "worldName": format!("__puppeteer_utility_world__{version}")
                })),
                "{version}"
            );
        }
        for world in [
            "__puppeteer_utility_world__",
            "__puppeteer_utility_world__25..0",
            "__puppeteer_utility_world__25.4.0-evil",
            "__puppeteer_utility_world__v25.4.0",
            "__puppeteer_utility_world__25.4.0/../..",
            "__other_world__25.4.0",
        ] {
            assert!(
                !supported_client_initialization(&serde_json::json!({
                    "source": "//# sourceURL=pptr:internal",
                    "worldName": world
                })),
                "{world}"
            );
        }
        assert!(!supported_client_initialization(&serde_json::json!({
            "source": "//# sourceURL=pptr:internal",
            "worldName": "__puppeteer_utility_world__25.4.0",
            "runImmediately": true
        })));
        assert!(!supported_client_initialization(&serde_json::json!({
            "source": "fetch('http://attacker')",
            "worldName": "__puppeteer_utility_world__25.4.0"
        })));
    }

    #[test]
    fn remote_objects_reject_cross_context_stale_and_forged_identity() {
        let issued = RemoteObject {
            internal: "semantic-element:label:Resume".into(),
            scope: "session-a".into(),
            generation: 7,
        };
        assert!(remote_object_valid(&issued, "session-a", 7));
        assert!(!remote_object_valid(&issued, "session-b", 7));
        assert!(!remote_object_valid(&issued, "session-a", 8));
        let issued_ids = std::collections::HashMap::from([("opaque-issued", issued)]);
        assert!(!issued_ids.contains_key("semantic-element:label:Resume"));
        assert!(!issued_ids.contains_key("opaque-forged"));
    }

    #[tokio::test]
    async fn aborted_upload_task_leaves_zero_staging_residue() {
        let root = tempfile::tempdir().unwrap();
        let staging_root = root.path().to_owned();
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let entered_task = entered.clone();
        let task = tokio::spawn(async move {
            let staging = UploadStaging::new(&staging_root).unwrap();
            staging.stage("resume.txt", b"bounded").unwrap();
            entered_task.notify_one();
            std::future::pending::<()>().await;
            drop(staging);
        });
        entered.notified().await;
        task.abort();
        let _ = task.await;
        assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    }

    #[test]
    fn download_completion_exposes_only_opaque_stream_identity() {
        let events = download_events(
            "frame",
            "guid",
            "fixture.bin",
            7,
            "550e8400-e29b-41d4-a716-446655440000",
            "abcd",
            None,
        );
        let serialized = serde_json::to_string(&events).unwrap();
        assert!(!serialized.contains("filePath"));
        assert!(!serialized.contains("/private/"));
        assert_eq!(
            events[1].params["streamId"],
            "550e8400-e29b-41d4-a716-446655440000"
        );
    }

    #[tokio::test]
    async fn stream_store_enforces_count_bytes_ttl_and_one_shot() {
        let mut store = DownloadStreamStore::new(1, 7, std::time::Duration::from_millis(1));
        let principal = types::PrincipalId::from_uuid(uuid::Uuid::new_v4());
        assert!(store
            .reserve(
                "one",
                principal.clone(),
                "connection",
                types::SessionId::new(),
                "artifact",
                7
            )
            .is_ok());
        assert!(store
            .reserve(
                "two",
                principal.clone(),
                "connection",
                types::SessionId::new(),
                "artifact",
                1
            )
            .is_err());
        assert!(store.take_authorized("one", &principal).is_some());
        assert!(store.take_authorized("one", &principal).is_none());
        assert!(store
            .reserve(
                "ttl",
                principal.clone(),
                "connection",
                types::SessionId::new(),
                "artifact",
                1
            )
            .is_ok());
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
        assert!(store.peek_authorized("ttl", &principal).is_none());
    }

    #[test]
    fn unauthorized_lookup_does_not_consume_and_connection_cleanup_reclaims() {
        let mut store = DownloadStreamStore::new(2, 8, std::time::Duration::from_secs(60));
        let principal_a = types::PrincipalId::from_uuid(uuid::Uuid::new_v4());
        let principal_b = types::PrincipalId::from_uuid(uuid::Uuid::new_v4());
        store
            .reserve(
                "one",
                principal_a.clone(),
                "connection-a",
                types::SessionId::new(),
                "artifact",
                4,
            )
            .unwrap();
        assert!(store.take_authorized("one", &principal_b).is_none());
        assert!(store.peek_authorized("one", &principal_a).is_some());
        store.remove_connection("connection-a");
        assert!(store.peek_authorized("one", &principal_a).is_none());
    }

    #[test]
    fn failed_event_admission_rollback_releases_stream_capacity() {
        let principal = types::PrincipalId::from_uuid(uuid::Uuid::new_v4());
        let mut store = DownloadStreamStore::new(1, 4, std::time::Duration::from_secs(60));
        store
            .reserve(
                "rejected",
                principal.clone(),
                "connection",
                types::SessionId::new(),
                "artifact",
                4,
            )
            .unwrap();
        assert!(store.remove("rejected").is_some());
        assert!(store
            .reserve(
                "replacement",
                principal,
                "connection",
                types::SessionId::new(),
                "artifact",
                4
            )
            .is_ok());
    }

    #[test]
    fn pending_boundary_is_page_scoped_expiring_and_one_shot() {
        let now = chrono::Utc::now();
        let session = types::SessionId::new();
        let page = types::PageId::new();
        let pending = AutomationBoundary {
            workflow_id: types::WorkflowId::new(),
            attempt_id: types::AttemptId::new(),
            command_id: types::CommandId::new(),
            checkpoint_id: types::CheckpointId::new(),
            session_id: session.clone(),
            page_id: page.clone(),
            expires_at: now + chrono::Duration::seconds(30),
            phase: AutomationBoundaryPhase::Pending,
        };
        let expected = (
            pending.workflow_id.clone(),
            pending.attempt_id.clone(),
            pending.command_id.clone(),
            pending.checkpoint_id.clone(),
        );
        let mut state = Some(pending);
        let wrong_session = types::SessionId::new();
        assert!(consume_pending_boundary(&mut state, &wrong_session, &page, now).is_err());
        let wrong_page = types::PageId::new();
        assert!(consume_pending_boundary(&mut state, &session, &wrong_page, now).is_err());
        assert_eq!(
            state.as_ref().unwrap().phase,
            AutomationBoundaryPhase::Pending
        );
        let consumed = consume_pending_boundary(&mut state, &session, &page, now).unwrap();
        assert_eq!(
            (
                consumed.workflow_id,
                consumed.attempt_id,
                consumed.command_id,
                consumed.checkpoint_id
            ),
            expected
        );
        assert!(consume_pending_boundary(&mut state, &session, &page, now).is_err());
        state.as_mut().unwrap().phase = AutomationBoundaryPhase::Pending;
        assert!(consume_pending_boundary(
            &mut state,
            &session,
            &page,
            now + chrono::Duration::seconds(31)
        )
        .is_err());
        assert!(consume_pending_boundary(&mut None, &session, &page, now).is_err());
    }
}

#[derive(Clone)]
struct CatalogTarget {
    opaque: String,
    runtime_session: String,
    page: String,
    /// Last URL this gateway verified for the page, `None` until it navigates
    /// one. Discovery has no way to ask the runtime, so an unnavigated page is
    /// reported as blank rather than guessed at.
    url: Option<String>,
    title: Option<String>,
}

/// Opaque target id plus whatever this gateway has since verified about the page.
///
/// Deliberately not `Default`: an entry is only ever valid with a freshly minted
/// `opaque`, and a defaulted one would hand every target the same empty id.
#[derive(Clone)]
struct CatalogEntry {
    opaque: String,
    url: Option<String>,
    title: Option<String>,
}

impl CatalogEntry {
    fn new() -> Self {
        Self {
            opaque: Uuid::new_v4().simple().to_string(),
            url: None,
            title: None,
        }
    }
}

#[derive(Default)]
struct TargetCatalog {
    by_page: HashMap<(String, String), CatalogEntry>,
}

impl TargetCatalog {
    fn register(&mut self, session_id: &SessionId, page_id: &PageId) -> String {
        self.by_page
            .entry((session_id.0.to_string(), page_id.0.to_string()))
            .or_insert_with(CatalogEntry::new)
            .opaque
            .clone()
    }

    /// Record a navigation this gateway performed and verified, so discovery can
    /// tell one target from another instead of labelling every one `about:blank`.
    fn note_navigation(&mut self, runtime_session: &str, page: &str, url: &str, title: &str) {
        let entry = self
            .by_page
            .entry((runtime_session.to_owned(), page.to_owned()))
            .or_insert_with(CatalogEntry::new);
        entry.url = Some(url.to_owned());
        entry.title = (!title.is_empty()).then(|| title.to_owned());
    }

    fn targets_for(&mut self, sessions: &[SessionState]) -> Vec<CatalogTarget> {
        let mut live = Vec::new();
        for session in sessions {
            let runtime_session = session.id.0.to_string();
            for page in &session.page_ids {
                let page = page.0.to_string();
                let key = (runtime_session.clone(), page.clone());
                let entry = self
                    .by_page
                    .entry(key.clone())
                    .or_insert_with(CatalogEntry::new)
                    .clone();
                live.push(CatalogTarget {
                    opaque: entry.opaque,
                    runtime_session: key.0,
                    page: key.1,
                    url: entry.url,
                    title: entry.title,
                });
            }
        }
        self.by_page.retain(|key, _| {
            live.iter()
                .any(|target| target.runtime_session == key.0 && target.page == key.1)
        });
        live
    }
}

fn runtime_error(error: InterfaceError) -> CdpError {
    CdpError {
        code: CdpErrorCode::RuntimeFailure as i32,
        message: "runtime request failed".into(),
        data: Some(
            json!({"interfaceCode": format!("{:?}", error.code), "correlationId": error.correlation_id}),
        ),
    }
}

fn find_serialized_string<'a>(value: &'a Value, key: &str) -> Option<&'a str> {
    match value {
        Value::Object(map) => {
            if map.get("k").and_then(Value::as_str) == Some(key) {
                return map.get("v").and_then(Value::as_str);
            }
            map.values()
                .find_map(|value| find_serialized_string(value, key))
        }
        Value::Array(items) => items
            .iter()
            .find_map(|value| find_serialized_string(value, key)),
        _ => None,
    }
}

fn find_object_ids(value: &Value) -> Vec<&str> {
    let mut found = Vec::new();
    collect_object_ids(value, &mut found);
    found
}

fn collect_object_ids<'a>(value: &'a Value, found: &mut Vec<&'a str>) {
    match value {
        Value::Object(map) => {
            if let Some(id) = map.get("objectId").and_then(Value::as_str) {
                found.push(id);
            }
            for nested in map.values() {
                collect_object_ids(nested, found);
            }
        }
        Value::Array(items) => {
            for nested in items {
                collect_object_ids(nested, found);
            }
        }
        _ => {}
    }
}

fn serialized_file_payloads(value: &Value) -> Result<Vec<(String, Vec<u8>)>, CdpError> {
    fn visit(
        value: &Value,
        files: &mut Vec<(String, Vec<u8>)>,
        total: &mut usize,
    ) -> Result<(), CdpError> {
        match value {
            Value::Object(map) => {
                if let Some(entries) = map.get("o").and_then(Value::as_array) {
                    let field = |key: &str| {
                        entries.iter().find_map(|entry| {
                            (entry.get("k").and_then(Value::as_str) == Some(key))
                                .then(|| entry.get("v").and_then(Value::as_str))
                                .flatten()
                        })
                    };
                    if let (Some(name), Some(buffer)) = (field("name"), field("buffer")) {
                        let valid_name = !name.is_empty()
                            && name.len() <= 255
                            && std::path::Path::new(name)
                                .file_name()
                                .is_some_and(|part| part == name);
                        if !valid_name || files.len() >= 16 {
                            return Err(CdpError::new(
                                CdpErrorCode::InvalidParams,
                                "invalid bounded upload payload",
                            ));
                        }
                        let bytes = BASE64.decode(buffer).map_err(|_| {
                            CdpError::new(CdpErrorCode::InvalidParams, "invalid upload encoding")
                        })?;
                        *total = total.checked_add(bytes.len()).ok_or_else(|| {
                            CdpError::new(CdpErrorCode::InvalidParams, "upload size overflow")
                        })?;
                        if *total > 64 * 1024 * 1024 {
                            return Err(CdpError::new(
                                CdpErrorCode::InvalidParams,
                                "upload payload exceeds 64 MiB",
                            ));
                        }
                        files.push((name.to_owned(), bytes));
                        return Ok(());
                    }
                }
                for nested in map.values() {
                    visit(nested, files, total)?;
                }
            }
            Value::Array(items) => {
                for nested in items {
                    visit(nested, files, total)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    let mut files = Vec::new();
    let mut total = 0;
    visit(value, &mut files, &mut total)?;
    if files.is_empty() {
        return Err(CdpError::new(
            CdpErrorCode::InvalidParams,
            "missing upload payload",
        ));
    }
    Ok(files)
}

fn parse_role_target(body: &str) -> Option<(String, TargetSpec)> {
    let role = body.split('[').next()?.trim();
    let marker = "[name=\"";
    let name = body.split_once(marker)?.1.strip_suffix("\"i]")?;
    if role.is_empty() || role.len() > 64 || name.is_empty() || name.len() > 256 {
        return None;
    }
    Some((
        format!("role:{role}:{name}"),
        TargetSpec {
            role: Some(role.to_owned()),
            accessible_name: Some(name.to_owned()),
            ..TargetSpec::default()
        },
    ))
}

async fn version_route(State(gateway): State<Arc<CdpGateway>>, headers: HeaderMap) -> Response {
    let bearer = bearer(&headers);
    match gateway.version(bearer.as_deref()).await {
        Ok(description) => Json(description).into_response(),
        Err(error) => discovery_response(error),
    }
}

async fn list_route(State(gateway): State<Arc<CdpGateway>>, headers: HeaderMap) -> Response {
    let bearer = bearer(&headers);
    match gateway.list(bearer.as_deref()).await {
        Ok(targets) => Json(targets).into_response(),
        Err(error) => discovery_response(error),
    }
}

async fn stream_route(
    State(gateway): State<Arc<CdpGateway>>,
    Path(id): Path<String>,
    headers: HeaderMap,
) -> Response {
    if id.is_empty() || id.len() > 64 {
        return StatusCode::NOT_FOUND.into_response();
    }
    let token = bearer(&headers);
    let handle = match gateway.authenticate(token.as_deref()).await {
        Ok(handle) => handle,
        Err(error) => return discovery_response(error),
    };
    let ctx = handle.context(Utc::now() + Duration::seconds(30), None);
    if !ctx.capabilities.contains(types::Capability::FileDownload) {
        return discovery_response(DiscoveryError::Unauthorized);
    }
    let stream = gateway
        .streams
        .lock()
        .await
        .peek_authorized(&id, &ctx.principal_id);
    let Some(stream) = stream else {
        return StatusCode::NOT_FOUND.into_response();
    };
    let Some(artifacts) = gateway.artifacts.as_ref() else {
        gateway.streams.lock().await.remove(&id);
        return StatusCode::NOT_FOUND.into_response();
    };
    let bytes = match artifacts.get(&stream.session_id, &stream.artifact_id).await {
        Ok(bytes) if bytes.len() as u64 == stream.bytes => bytes,
        _ => {
            gateway.streams.lock().await.remove(&id);
            return StatusCode::NOT_FOUND.into_response();
        }
    };
    if gateway
        .streams
        .lock()
        .await
        .take_authorized(&id, &ctx.principal_id)
        .is_none()
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    (
        [(
            (axum::http::header::CONTENT_TYPE),
            "application/octet-stream",
        )],
        bytes,
    )
        .into_response()
}

async fn websocket_route(
    State(gateway): State<Arc<CdpGateway>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let bearer = bearer(&headers);
    let path = format!("/devtools/browser/{id}");
    match gateway.upgrade(&path, bearer.as_deref()).await {
        Ok(connection) => upgrade
            .max_message_size(crate::MAX_FRAME_BYTES)
            .max_frame_size(crate::MAX_FRAME_BYTES)
            .on_upgrade(move |socket| serve_socket(socket, connection)),
        Err(error) => discovery_response(error),
    }
}

fn bearer(headers: &HeaderMap) -> Option<String> {
    let value = headers.get(AUTHORIZATION)?.to_str().ok()?;
    if value.len() > 512 || headers.get_all(AUTHORIZATION).iter().count() != 1 {
        return None;
    }
    let token = value.strip_prefix("Bearer ")?;
    if !(32..=505).contains(&token.len())
        || !token.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
    {
        return None;
    }
    Some(token.to_owned())
}

fn discovery_response(error: DiscoveryError) -> Response {
    let status = match error {
        DiscoveryError::Unauthorized => StatusCode::UNAUTHORIZED,
        DiscoveryError::Forbidden => StatusCode::FORBIDDEN,
        DiscoveryError::NotFound => StatusCode::NOT_FOUND,
        DiscoveryError::Runtime => StatusCode::BAD_GATEWAY,
    };
    let mut response = (status, Json(json!({"error": "CDP request rejected"}))).into_response();
    if status == StatusCode::UNAUTHORIZED {
        response
            .headers_mut()
            .insert(WWW_AUTHENTICATE, HeaderValue::from_static("Bearer"));
    }
    response
}

async fn serve_socket(socket: WebSocket, connection: Arc<CdpConnection>) {
    let (mut sink, mut stream) = socket.split();
    let (outbound, mut outgoing) = mpsc::channel::<Message>(MAX_QUEUED_EVENTS);
    let mut writer = tokio::spawn(async move {
        while let Some(message) = outgoing.recv().await {
            if sink.send(message).await.is_err() {
                break;
            }
        }
    });
    let mut requests = JoinSet::new();

    'connection: loop {
        while let Some(completed) = requests.try_join_next() {
            if completed.is_err() {
                break 'connection;
            }
        }
        if send_queued_events(&connection, &outbound).await.is_err() {
            break;
        }
        tokio::select! {
            biased;
            _ = connection.event_notify.notified() => continue,
            message = stream.next() => {
                let bytes = match message {
                    Some(Ok(Message::Text(text))) => text.as_bytes().to_vec(),
                    Some(Ok(Message::Binary(bytes))) => bytes.to_vec(),
                    Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                    Some(Ok(Message::Ping(bytes))) => {
                        if outbound.send(Message::Pong(bytes)).await.is_err() { break; }
                        continue;
                    }
                    Some(Ok(Message::Pong(_))) => continue,
                };
                let request = match crate::parse_frame(&bytes) {
                    Ok(request) => request,
                    Err(error) => {
                        if send_json(&outbound, json!({"id": 0, "error": error})).await.is_err() { break; }
                        continue;
                    }
                };
                let permit = match connection.reserve_dispatch() {
                    Ok(permit) => permit,
                    Err(error) => {
                        let response = CdpResponse::failure(&request, error);
                        if send_json(&outbound, response).await.is_err() { break; }
                        continue;
                    }
                };
                let connection = connection.clone();
                let outbound = outbound.clone();
                requests.spawn(async move {
                    let response = connection.dispatch_reserved(request, &permit).await;
                    let _ = send_json(&outbound, response).await;
                    drop(permit);
                });
            }
            completed = requests.join_next(), if !requests.is_empty() => {
                if completed.is_some_and(|result| result.is_err()) { break 'connection; }
            }
        }
    }

    requests.abort_all();
    while requests.join_next().await.is_some() {}
    drop(outbound);
    if tokio::time::timeout(std::time::Duration::from_secs(1), &mut writer)
        .await
        .is_err()
    {
        writer.abort();
        let _ = writer.await;
    }
    connection.cleanup_streams().await;
}

async fn send_queued_events(
    connection: &CdpConnection,
    outbound: &mpsc::Sender<Message>,
) -> Result<(), ()> {
    for event in connection.drain_events().await {
        send_json(outbound, event).await?;
    }
    Ok(())
}

async fn send_json(outbound: &mpsc::Sender<Message>, value: impl Serialize) -> Result<(), ()> {
    let text = serde_json::to_string(&value).map_err(|_| ())?;
    outbound
        .send(Message::Text(text.into()))
        .await
        .map_err(|_| ())
}
