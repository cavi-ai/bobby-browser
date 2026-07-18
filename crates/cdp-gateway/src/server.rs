use std::{
    collections::{HashMap, VecDeque},
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
use chrono::{Duration, Utc};
use futures_util::{SinkExt, StreamExt};
use interface_core::{Authority, AuthorizationGuard, CapabilityHandle, RuntimeInterface};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::{
    sync::{mpsc, Mutex, Notify, OwnedSemaphorePermit, Semaphore},
    task::JoinSet,
};
use types::{InterfaceError, SessionState};
use uuid::Uuid;

use crate::{
    manifest::Handler, CdpError, CdpErrorCode, CdpEvent, CdpRequest, CdpResponse, IdentifierFamily,
    IdentifierMap, MethodRegistry, RuntimeGeneration, MAX_IN_FLIGHT_REQUESTS, MAX_QUEUED_EVENTS,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiscoveryError {
    Unauthorized,
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

pub struct CdpGateway {
    authority: Arc<dyn Authority>,
    runtime: Arc<dyn RuntimeInterface>,
    registry: MethodRegistry,
    websocket_base: String,
    browser_id: String,
    targets: Arc<Mutex<TargetCatalog>>,
    connections: Mutex<Vec<Weak<CdpConnection>>>,
    generations: Arc<Mutex<HashMap<String, RuntimeGeneration>>>,
}

impl CdpGateway {
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
        Self {
            authority,
            runtime,
            registry,
            websocket_base: websocket_base.into().trim_end_matches('/').to_owned(),
            browser_id: Uuid::new_v4().simple().to_string(),
            targets: Arc::new(Mutex::new(TargetCatalog::default())),
            connections: Mutex::new(Vec::new()),
            generations: Arc::new(Mutex::new(HashMap::new())),
        }
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
        self.authenticate(bearer).await?;
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
        let sessions = self
            .runtime
            .list_sessions(ctx)
            .await
            .map_err(|_| DiscoveryError::Runtime)?;
        let targets = self.targets.lock().await.targets_for(&sessions);
        Ok(targets
            .into_iter()
            .map(|target| TargetDescription {
                id: target.opaque,
                r#type: "page".into(),
                title: "Automation Runtime".into(),
                url: "about:blank".into(),
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
        let connection = Arc::new(CdpConnection::with_targets(
            handle,
            self.runtime.clone(),
            self.registry.clone(),
            self.targets.clone(),
            self.generations.clone(),
        ));
        let mut connections = self.connections.lock().await;
        connections.retain(|existing| existing.strong_count() > 0);
        connections.push(Arc::downgrade(&connection));
        Ok(connection)
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
            .route("/json/list", get(list_route))
            .route("/devtools/browser/{id}", get(websocket_route))
            .with_state(self)
    }
}

pub struct CdpConnection {
    handle: CapabilityHandle,
    runtime: Arc<dyn RuntimeInterface>,
    registry: MethodRegistry,
    in_flight: Arc<Semaphore>,
    events: Mutex<VecDeque<CdpEvent>>,
    event_notify: Notify,
    identifiers: Mutex<IdentifierMap>,
    targets: Arc<Mutex<TargetCatalog>>,
    generations: Arc<Mutex<HashMap<String, RuntimeGeneration>>>,
}

impl CdpConnection {
    pub fn new(
        handle: CapabilityHandle,
        runtime: Arc<dyn RuntimeInterface>,
        registry: MethodRegistry,
    ) -> Self {
        Self::with_targets(
            handle,
            runtime,
            registry,
            Arc::new(Mutex::new(TargetCatalog::default())),
            Arc::new(Mutex::new(HashMap::new())),
        )
    }

    fn with_targets(
        handle: CapabilityHandle,
        runtime: Arc<dyn RuntimeInterface>,
        registry: MethodRegistry,
        targets: Arc<Mutex<TargetCatalog>>,
        generations: Arc<Mutex<HashMap<String, RuntimeGeneration>>>,
    ) -> Self {
        Self {
            handle,
            runtime,
            registry,
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT_REQUESTS)),
            events: Mutex::new(VecDeque::new()),
            event_notify: Notify::new(),
            identifiers: Mutex::new(IdentifierMap::new()),
            targets,
            generations,
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
            Some(Handler::BrowserGetVersion) => self.runtime.runtime_info(ctx).await.map(|info| json!({
                "protocolVersion": "1.3", "product": "AutomationRuntime/0.1", "revision": info.version,
                "userAgent": "AutomationRuntime/0.1", "jsVersion": "unknown"
            })),
            Some(Handler::TargetGetTargets) => match self.runtime.list_sessions(ctx).await {
                Ok(sessions) => Ok(self.target_infos(&sessions).await),
                Err(error) => Err(error),
            },
            None => unreachable!("registry-handler bijection validated at construction"),
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }

    pub async fn queue_event(&self, event: CdpEvent) -> Result<(), CdpError> {
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
        let event = self.registry.translate_event(event)?;
        let mut events = self.events.lock().await;
        if events.len() >= MAX_QUEUED_EVENTS {
            return Err(CdpError::new(
                CdpErrorCode::RuntimeFailure,
                "event queue exhausted",
            ));
        }
        events.push_back(event);
        self.event_notify.notify_one();
        Ok(())
    }

    pub async fn next_event(&self) -> Option<CdpEvent> {
        self.events.lock().await.pop_front()
    }

    pub async fn drain_events(&self) -> Vec<CdpEvent> {
        self.events.lock().await.drain(..).collect()
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
                json!({"targetId": target.opaque, "type":"page", "title":"Automation Runtime", "url":"about:blank", "attached":false, "canAccessOpener":false})
            })
            .collect::<Vec<_>>();
        json!({"targetInfos": infos})
    }
}

#[derive(Clone)]
struct CatalogTarget {
    opaque: String,
    runtime_session: String,
    page: String,
}

#[derive(Default)]
struct TargetCatalog {
    by_page: HashMap<(String, String), String>,
}

impl TargetCatalog {
    fn targets_for(&mut self, sessions: &[SessionState]) -> Vec<CatalogTarget> {
        let mut live = Vec::new();
        for session in sessions {
            let runtime_session = session.id.0.to_string();
            for page in &session.page_ids {
                let page = page.0.to_string();
                let key = (runtime_session.clone(), page.clone());
                let opaque = self
                    .by_page
                    .entry(key.clone())
                    .or_insert_with(|| Uuid::new_v4().simple().to_string())
                    .clone();
                live.push(CatalogTarget {
                    opaque,
                    runtime_session: key.0,
                    page: key.1,
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
