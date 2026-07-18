use std::{collections::VecDeque, sync::Arc};

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
use interface_core::{Authority, AuthorizationGuard, CapabilityHandle, RuntimeInterface};
use serde::Serialize;
use serde_json::{json, Value};
use tokio::sync::{Mutex, Semaphore};
use types::InterfaceError;
use uuid::Uuid;

use crate::{
    manifest::Handler, CdpError, CdpErrorCode, CdpEvent, CdpRequest, CdpResponse, MethodRegistry,
    MAX_IN_FLIGHT_REQUESTS, MAX_QUEUED_EVENTS,
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
        Ok(sessions
            .into_iter()
            .flat_map(|session| session.page_ids)
            .map(|_page| TargetDescription {
                id: Uuid::new_v4().simple().to_string(),
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
    ) -> Result<CdpConnection, DiscoveryError> {
        if path != format!("/devtools/browser/{}", self.browser_id) {
            return Err(DiscoveryError::NotFound);
        }
        let handle = self.authenticate(bearer).await?;
        Ok(CdpConnection::new(
            handle,
            self.runtime.clone(),
            self.registry.clone(),
        ))
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
}

impl CdpConnection {
    pub fn new(
        handle: CapabilityHandle,
        runtime: Arc<dyn RuntimeInterface>,
        registry: MethodRegistry,
    ) -> Self {
        Self {
            handle,
            runtime,
            registry,
            in_flight: Arc::new(Semaphore::new(MAX_IN_FLIGHT_REQUESTS)),
            events: Mutex::new(VecDeque::new()),
        }
    }

    pub async fn dispatch(&self, request: CdpRequest) -> CdpResponse {
        if let Err(error) = request.validate() {
            return CdpResponse::failure(&request, error);
        }
        let Ok(_permit) = self.in_flight.clone().try_acquire_owned() else {
            return CdpResponse::failure(
                &request,
                CdpError::new(CdpErrorCode::RuntimeFailure, "too many in-flight requests"),
            );
        };
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
            Some(Handler::TargetGetTargets) => self.runtime.list_sessions(ctx).await.map(|sessions| json!({
                "targetInfos": sessions.into_iter().map(|_| json!({"targetId": Uuid::new_v4().simple().to_string(), "type":"page", "title":"Automation Runtime", "url":"about:blank", "attached":false, "canAccessOpener":false})).collect::<Vec<Value>>()
            })),
            None => unreachable!("registry-handler bijection validated at construction"),
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }

    pub async fn queue_event(&self, event: CdpEvent) -> Result<(), CdpError> {
        let mut events = self.events.lock().await;
        if events.len() >= MAX_QUEUED_EVENTS {
            return Err(CdpError::new(
                CdpErrorCode::RuntimeFailure,
                "event queue exhausted",
            ));
        }
        events.push_back(event);
        Ok(())
    }

    pub async fn next_event(&self) -> Option<CdpEvent> {
        self.events.lock().await.pop_front()
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

async fn serve_socket(mut socket: WebSocket, connection: CdpConnection) {
    while let Some(message) = socket.recv().await {
        let bytes = match message {
            Ok(Message::Text(text)) => text.as_bytes().to_vec(),
            Ok(Message::Binary(bytes)) => bytes.to_vec(),
            Ok(Message::Close(_)) | Err(_) => break,
            Ok(Message::Ping(bytes)) => {
                if socket.send(Message::Pong(bytes)).await.is_err() {
                    break;
                }
                continue;
            }
            Ok(Message::Pong(_)) => continue,
        };
        let response = match crate::parse_frame(&bytes) {
            Ok(request) => serde_json::to_value(connection.dispatch(request).await),
            Err(error) => Ok(json!({"id": 0, "error": error})),
        };
        let Ok(response) = response.and_then(|value| serde_json::to_string(&value)) else {
            break;
        };
        if socket.send(Message::Text(response.into())).await.is_err() {
            break;
        }
    }
}
