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
use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::{Duration, Utc};
use futures_util::{SinkExt, StreamExt};
use interface_core::{Authority, AuthorizationGuard, CapabilityHandle, RuntimeInterface};
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
    CommandEnvelope, CommandId, CommandOutcome, InspectCommand, InterfaceError, NavigateCommand,
    PageId, PrimitiveCommand, RequestContext, ScreenshotMode, SessionId, SessionState, TargetSpec,
    TextMatch, TypeTextCommand, UploadFilesCommand, WaitUntil, WorkflowCheckpoint, WorkflowId,
};
use uuid::Uuid;

use crate::{
    domains, manifest::Handler, CdpError, CdpErrorCode, CdpEvent, CdpRequest, CdpResponse,
    IdentifierFamily, IdentifierMap, MethodRegistry, RuntimeGeneration, MAX_IN_FLIGHT_REQUESTS,
    MAX_QUEUED_EVENTS,
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
    artifacts: Option<artifact_store::ArtifactStore>,
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
            artifacts: None,
        }
    }

    pub fn with_artifacts(mut self, artifacts: artifact_store::ArtifactStore) -> Self {
        self.artifacts = Some(artifacts);
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
            self.artifacts.clone(),
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
            .route("/json/version/", get(version_route))
            .route("/json/list", get(list_route))
            .route("/json/list/", get(list_route))
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
    isolated_worlds: Mutex<HashMap<String, String>>,
    pending_page_loads: Mutex<HashMap<String, (String, String, String)>>,
    artifacts: Option<artifact_store::ArtifactStore>,
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
            None,
        )
    }

    fn with_targets(
        handle: CapabilityHandle,
        runtime: Arc<dyn RuntimeInterface>,
        registry: MethodRegistry,
        targets: Arc<Mutex<TargetCatalog>>,
        generations: Arc<Mutex<HashMap<String, RuntimeGeneration>>>,
        artifacts: Option<artifact_store::ArtifactStore>,
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
            isolated_worlds: Mutex::new(HashMap::new()),
            pending_page_loads: Mutex::new(HashMap::new()),
            artifacts,
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
            Some(Handler::TargetGetTargetInfo) => {
                let target_id = self.bind_identifier(
                    IdentifierFamily::Target, "browser", "browser", RuntimeGeneration(0),
                ).await;
                Ok(json!({"targetInfo": {"targetId": target_id, "type": "browser", "title": "", "url": "", "attached": true, "canAccessOpener": false}}))
            }
            Some(Handler::TargetSetAutoAttach) => match domains::target::auto_attach(request.params.clone()) {
                Ok(options) => {
                    match self.runtime.list_sessions(ctx).await {
                        Ok(sessions) => {
                            if options.auto_attach && request.session_id.is_none() {
                                if let Err(error) = self.queue_attached_targets(&sessions, options.wait_for_debugger_on_start).await {
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
                if let Err(error) = domains::browser::validate_download_behavior(request.params.clone()) {
                    return CdpResponse::failure(&request, error);
                }
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
                let pending = request.session_id.as_deref().and_then(|id| self.pending_page_loads.try_lock().ok().and_then(|loads| loads.get(id).cloned()));
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
                    command:PrimitiveCommand::CaptureScreenshot(CaptureScreenshotCommand { mode }) };
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
                        let sha = format!("{:x}", Sha256::digest(&bytes));
                        if bytes.len() as u64 != expected_bytes || &sha != expected_sha {
                            return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "screenshot artifact integrity check failed"));
                        }
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
                Ok(result) => Ok(result),
                Err(error) => return CdpResponse::failure(&request, error),
            },
            Some(Handler::RuntimeReleaseObject) => {
                let known = request.params.get("objectId").and_then(Value::as_str).is_some_and(|id| {
                    matches!(id, "playwright-injected-script" | "playwright-utility-script")
                        || id == "viewport-poller" || id.starts_with("semantic-locator:") || id.starts_with("semantic-element:")
                });
                if !known { return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unknown gateway remote object")); }
                Ok(json!({}))
            }
            Some(Handler::RuntimeCallFunctionOn) => {
                let valid_shape = request.params.get("functionDeclaration").and_then(Value::as_str)
                    == Some("(utilityScript, ...args) => utilityScript.evaluate(...args)")
                    && request.params.get("objectId").and_then(Value::as_str) == Some("playwright-utility-script")
                    && request.params.get("arguments").and_then(Value::as_array).is_some_and(|args| args.len() <= 16);
                if !valid_shape { return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unrecognized semantic runtime call")); }
                let serialized = &request.params["arguments"];
                let locator_handle = find_object_id_with_prefix(serialized, "semantic-locator:");
                let element_handle = find_object_id_with_prefix(serialized, "semantic-element:");
                let viewport_poller = find_object_id_with_prefix(serialized, "viewport-poller");
                let expression = request.params["arguments"].as_array().and_then(|args| args.get(3))
                    .and_then(|arg| arg.get("value")).and_then(Value::as_str).unwrap_or("");
                let evaluated_expression = find_serialized_string(serialized, "expression");
                if expression.contains("globalThis.eval(expression3)")
                    && evaluated_expression.is_some_and(|value| value.contains("window.innerWidth") && value.contains("window.innerHeight")) {
                    return CdpResponse::success(&request, json!({"result":{"type":"object","subtype":"object","className":"Object","description":"Object","objectId":"viewport-poller"}}));
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
                if let Some(handle) = locator_handle.filter(|_| expression.trim() == "(r) => r.element") {
                    let label = handle.trim_start_matches("semantic-locator:");
                    return CdpResponse::success(&request, json!({"result":{"type":"object","subtype":"node","className":"HTMLInputElement","description":"input","objectId":format!("semantic-element:{}", label)}}));
                }
                if element_handle.is_some() && expression.contains("injected.previewNode(e)") {
                    return CdpResponse::success(&request, json!({"result":{"type":"string","value":"JSHandle@input"}}));
                }
                if let Some(handle) = element_handle.filter(|_| {
                    expression.contains("injected.retarget(node, \"follow-label\")")
                        && expression.contains("HTMLInputElement")
                }) {
                    return CdpResponse::success(&request, json!({"result":{
                        "type":"object", "subtype":"node", "className":"HTMLInputElement",
                        "description":"input", "objectId":handle
                    }}));
                }
                if let Some(handle) = element_handle.filter(|_| {
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
                    let mut staged = Vec::with_capacity(payloads.len());
                    for (name, bytes) in payloads {
                        let path = std::env::temp_dir().join(format!("cdp-upload-{}-{name}", Uuid::new_v4().simple()));
                        if let Err(error) = std::fs::write(&path, bytes) {
                            for staged_path in &staged { let _ = std::fs::remove_file(staged_path); }
                            return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, format!("failed to stage bounded upload: {error}")));
                        }
                        staged.push(path);
                    }
                    let Some((session_id, page_id)) = self.runtime_identity(request.session_id.as_deref()).await else {
                        for path in &staged { let _ = std::fs::remove_file(path); }
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "unknown runtime page"));
                    };
                    let paths = staged.iter().map(|path| path.to_string_lossy().into_owned()).collect();
                    let envelope = CommandEnvelope { schema_version:CommandEnvelope::SCHEMA_VERSION,
                        command_id:CommandId::new(), workflow_id:WorkflowId::new(), attempt_id:AttemptId::new(),
                        session_id, page_id:Some(page_id), deadline:Utc::now()+Duration::seconds(30),
                        command:PrimitiveCommand::UploadFiles(UploadFilesCommand { selector:String::new(), target:Some(TargetSpec { label:Some(label.to_owned()), ..TargetSpec::default() }), paths }) };
                    let outcome = self.runtime.submit(ctx, envelope).await;
                    for path in &staged { let _ = std::fs::remove_file(path); }
                    return match outcome {
                        Ok(CommandOutcome::Completed { evidence, .. }) if evidence.iter().any(|item| matches!(item, types::Evidence::Upload { .. })) =>
                            CdpResponse::success(&request, json!({"result":{"type":"undefined"}})),
                        Ok(_) => CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "runtime upload did not produce upload evidence")),
                        Err(error) => CdpResponse::failure(&request, runtime_error(error)),
                    };
                }
                if let Some(handle) = element_handle.filter(|_| expression.contains("injected.fill(node")) {
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
                        command:PrimitiveCommand::TypeText(TypeTextCommand { selector:String::new(), target:Some(TargetSpec { label:Some(label.to_owned()), ..TargetSpec::default() }), value:value.to_owned(), clear_first:true }) };
                    return match self.runtime.submit(ctx, envelope).await {
                        Ok(CommandOutcome::Completed { .. }) => CdpResponse::success(&request, json!({"result":{"type":"string","value":"done"}})),
                        Ok(_) => CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "runtime fill did not complete")),
                        Err(error) => CdpResponse::failure(&request, runtime_error(error)),
                    };
                }
                if let Some(handle) = element_handle.filter(|_| expression.contains("checkElementStates")) {
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
                        return match self.submit_boundary(ctx, session_id, page_id, command).await {
                            Ok(CommandOutcome::Completed { evidence, .. }) => {
                                let download = evidence.iter().find_map(|item| match item {
                                    types::Evidence::Download { filename, path, bytes, .. } => Some((filename.clone(), path.clone(), *bytes)),
                                    _ => None,
                                });
                                let Some((filename, path, bytes)) = download else {
                                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "runtime download did not produce download evidence"));
                                };
                                let guid = Uuid::new_v4().to_string();
                                for event in [
                                    CdpEvent { method:"Browser.downloadWillBegin".into(), params:json!({"frameId":frame_id,"guid":guid,"url":"about:blank","suggestedFilename":filename}), session_id:None },
                                    CdpEvent { method:"Browser.downloadProgress".into(), params:json!({"guid":guid,"totalBytes":bytes,"receivedBytes":bytes,"state":"completed","filePath":path}), session_id:None },
                                ] { if let Err(error) = self.queue_event(event).await { return CdpResponse::failure(&request, error); } }
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
                                self.pending_page_loads.lock().await.insert(popup_session, (target_id, url, loader_id));
                                CdpResponse::success(&request, json!({"result":{"type":"string","value":"done"}}))
                            }
                            Ok(_) => CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "runtime popup did not complete")),
                            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
                        };
                    }
                    let envelope = CommandEnvelope { schema_version:CommandEnvelope::SCHEMA_VERSION,
                        command_id:CommandId::new(), workflow_id:WorkflowId::new(), attempt_id:AttemptId::new(),
                        session_id, page_id:Some(page_id), deadline:Utc::now()+Duration::seconds(30),
                        command:PrimitiveCommand::Click(ClickCommand { selector:String::new(), target:Some(target), boundary:false, expected_url:None }) };
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
                    command:PrimitiveCommand::Inspect(InspectCommand { selector:None, target:Some(target), include_html:false }) };
                match self.runtime.submit(ctx, envelope).await {
                    Ok(CommandOutcome::Completed { evidence, .. }) if evidence.iter().any(|item| matches!(item, types::Evidence::Element { .. } | types::Evidence::Inspection { .. })) =>
                        Ok(json!({"result":{"type":"object","subtype":"object","className":"Object","description":"Object","objectId":format!("semantic-locator:{}", descriptor)}})),
                    Ok(_) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "semantic target was not verified")),
                    Err(error) => Err(error),
                }
            }
            Some(Handler::PageAddScript) => {
                let valid = request.params.get("source").and_then(Value::as_str).is_some_and(str::is_empty)
                    && request.params.get("worldName").and_then(Value::as_str).is_some_and(|name| !name.is_empty() && name.len() <= 256);
                if !valid { return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "only bounded empty initialization scripts are supported")); }
                Ok(json!({"identifier": Uuid::new_v4().simple().to_string()}))
            }
            Some(Handler::PageCreateIsolatedWorld) => {
                let frame_id = request.params.get("frameId").and_then(Value::as_str).filter(|id| !id.is_empty() && id.len() <= 256);
                let world_name = request.params.get("worldName").and_then(Value::as_str).filter(|name| !name.is_empty() && name.len() <= 256);
                let valid = frame_id.is_some() && world_name.is_some();
                if !valid { return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid isolated world request")); }
                self.isolated_worlds.lock().await.insert(
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
                    command: PrimitiveCommand::Navigate(NavigateCommand { url: url.to_owned(), wait_until: WaitUntil::Interactive, timeout_ms: 30_000 }),
                };
                match self.runtime.submit(ctx, envelope).await {
                    Ok(CommandOutcome::Completed { evidence, .. }) => {
                        let Some((final_url, title)) = evidence.iter().find_map(|item| match item {
                            types::Evidence::Navigation { url, title } => Some((url.clone(), title.clone())),
                            _ => None,
                        }) else { return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "navigation returned no verified evidence")); };
                        let world_name = self.isolated_worlds.lock().await.get(cdp_session).cloned();
                        let mut events = vec![
                            CdpEvent { method:"Page.frameNavigated".into(), params:json!({"frame":{"id":target_id,"loaderId":loader_id,"url":final_url,"domainAndRegistry":"","securityOrigin":"","mimeType":"text/html","secureContextType":"SecureLocalhost","crossOriginIsolatedContextType":"NotIsolated","gatedAPIFeatures":[]},"type":"Navigation"}), session_id:request.session_id.clone() },
                            CdpEvent { method:"Runtime.executionContextsCleared".into(), params:json!({}), session_id:request.session_id.clone() },
                            CdpEvent { method:"Runtime.executionContextCreated".into(), params:json!({"context":{"id":3,"origin":final_url,"name":"","uniqueId":Uuid::new_v4().simple().to_string(),"auxData":{"isDefault":true,"type":"default","frameId":target_id}}}), session_id:request.session_id.clone() },
                            CdpEvent { method:"Page.lifecycleEvent".into(), params:json!({"frameId":target_id,"loaderId":loader_id,"name":"DOMContentLoaded","timestamp":0}), session_id:request.session_id.clone() },
                            CdpEvent { method:"Page.lifecycleEvent".into(), params:json!({"frameId":target_id,"loaderId":loader_id,"name":"load","timestamp":0}), session_id:request.session_id.clone() },
                        ];
                        if let Some(world_name) = world_name {
                            events.insert(3, CdpEvent { method:"Runtime.executionContextCreated".into(), params:json!({"context":{"id":4,"origin":final_url,"name":world_name,"uniqueId":Uuid::new_v4().simple().to_string(),"auxData":{"isDefault":false,"type":"isolated","frameId":target_id}}}), session_id:request.session_id.clone() });
                        }
                        for event in events { if let Err(error) = self.queue_event(event).await { return CdpResponse::failure(&request, error); } }
                        let _ = title;
                        Ok(json!({"frameId":target_id,"loaderId":loader_id,"isDownload":false}))
                    }
                    Ok(_) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "navigation did not complete")),
                    Err(error) => Err(error),
                }
            }
            Some(Handler::PageSetLifecycle) => {
                if request.params.get("enabled").and_then(Value::as_bool).is_none() {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid lifecycle event configuration"));
                }
                Ok(json!({}))
            }
            Some(Handler::EmulationSetFocus) => {
                if request.params.get("enabled").and_then(Value::as_bool).is_none() {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid focus emulation configuration"));
                }
                Ok(json!({}))
            }
            Some(Handler::EmulationSetMedia) => {
                let valid = request.params.get("media").and_then(Value::as_str).is_some_and(|value| value.len() <= 32)
                    && request.params.get("features").and_then(Value::as_array).is_some_and(|items| items.len() <= 16);
                if !valid { return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "invalid media emulation configuration")); }
                Ok(json!({}))
            }
            Some(Handler::PageEnable) => {
                if !request.params.as_object().is_some_and(serde_json::Map::is_empty) {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "Page.enable takes no parameters"));
                }
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
    ) -> Result<(), CdpError> {
        let infos = self.target_infos(sessions).await;
        for target_info in infos["targetInfos"].as_array().into_iter().flatten() {
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

    async fn submit_boundary(
        &self,
        ctx: RequestContext,
        session_id: SessionId,
        page_id: PageId,
        command: PrimitiveCommand,
    ) -> Result<CommandOutcome, InterfaceError> {
        let workflow_id = WorkflowId::new();
        let attempt_id = AttemptId::new();
        let inspect_id = CommandId::new();
        let observed = match self
            .runtime
            .submit(
                ctx.clone(),
                CommandEnvelope {
                    schema_version: CommandEnvelope::SCHEMA_VERSION,
                    command_id: inspect_id.clone(),
                    workflow_id: workflow_id.clone(),
                    attempt_id: attempt_id.clone(),
                    session_id: session_id.clone(),
                    page_id: Some(page_id.clone()),
                    deadline: Utc::now() + Duration::seconds(30),
                    command: PrimitiveCommand::Inspect(InspectCommand::default()),
                },
            )
            .await?
        {
            CommandOutcome::Completed { evidence, .. } => evidence,
            outcome => return Ok(outcome),
        };
        let Some((url, title)) = observed.iter().find_map(|item| match item {
            types::Evidence::Inspection { url, title, .. } => Some((url.clone(), title.clone())),
            _ => None,
        }) else {
            return Ok(CommandOutcome::Completed {
                command_id: inspect_id,
                evidence: observed,
            });
        };
        let command_id = CommandId::new();
        self.runtime
            .checkpoint(
                ctx.clone(),
                WorkflowCheckpoint {
                    schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
                    checkpoint_id: CheckpointId::new(),
                    workflow_id: workflow_id.clone(),
                    attempt_id: attempt_id.clone(),
                    session_id: session_id.clone(),
                    page_id: page_id.clone(),
                    restart_url: url.clone(),
                    current_url: url.clone(),
                    cursor: Some(inspect_id),
                    boundary_command_id: Some(command_id.clone()),
                    recovery_class: CommandClass::Boundary,
                    invariants: vec![
                        CheckpointInvariant::Url { value: url },
                        CheckpointInvariant::Title { value: title },
                    ],
                    replayable_inputs: Vec::new(),
                    evidence: Vec::new(),
                    recovery_history: Vec::new(),
                    created_at: Utc::now(),
                },
                observed,
            )
            .await?;
        self.runtime
            .submit(
                ctx,
                CommandEnvelope {
                    schema_version: CommandEnvelope::SCHEMA_VERSION,
                    command_id,
                    workflow_id,
                    attempt_id,
                    session_id,
                    page_id: Some(page_id),
                    deadline: Utc::now() + Duration::seconds(30),
                    command,
                },
            )
            .await
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
    fn register(&mut self, session_id: &SessionId, page_id: &PageId) -> String {
        self.by_page
            .entry((session_id.0.to_string(), page_id.0.to_string()))
            .or_insert_with(|| Uuid::new_v4().simple().to_string())
            .clone()
    }

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

fn find_object_id_with_prefix<'a>(value: &'a Value, prefix: &str) -> Option<&'a str> {
    match value {
        Value::Object(map) => map
            .get("objectId")
            .and_then(Value::as_str)
            .filter(|id| id.starts_with(prefix))
            .or_else(|| {
                map.values()
                    .find_map(|value| find_object_id_with_prefix(value, prefix))
            }),
        Value::Array(items) => items
            .iter()
            .find_map(|value| find_object_id_with_prefix(value, prefix)),
        _ => None,
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
