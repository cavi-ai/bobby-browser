//! Individually callable reserved CDP method handlers.
use super::super::*;

impl CdpConnection {
    pub(super) async fn handle_page_get_frame_tree(
        &self,
        request: CdpRequest,
        _ctx: RequestContext,
    ) -> CdpResponse {
        let result = {
            if !request
                .params
                .as_object()
                .is_some_and(serde_json::Map::is_empty)
            {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(
                        CdpErrorCode::InvalidParams,
                        "Page.getFrameTree takes no parameters",
                    ),
                );
            }
            let scope = request.session_id.as_deref().unwrap_or("browser");
            let frame_id = if let Some(session_id) = request.session_id.as_deref() {
                match self
                    .resolve_identifier(IdentifierFamily::CdpSession, session_id)
                    .await
                {
                    Some(target_id) => target_id,
                    None => {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(CdpErrorCode::InvalidParams, "unknown CDP session"),
                        )
                    }
                }
            } else {
                self.bind_identifier(IdentifierFamily::Frame, scope, "main", RuntimeGeneration(0))
                    .await
            };
            // Not try_lock: contention must wait, not silently report a
            // fabricated about:blank frame.
            let pending = match request.session_id.as_deref() {
                Some(id) => self.pending_page_loads.lock().await.get(id).cloned(),
                None => None,
            };
            let (loader_id, url) = pending
                .map(|(_, url, loader)| (loader, url))
                .unwrap_or_else(|| ("initial".into(), "about:blank".into()));
            Ok(
                json!({"frameTree":{"frame":{"id":frame_id,"loaderId":loader_id,"url":url,"domainAndRegistry":"","securityOrigin":"://","mimeType":"text/html","secureContextType":"SecureLocalhost","crossOriginIsolatedContextType":"NotIsolated","gatedAPIFeatures":[]}}}),
            )
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
    pub(super) async fn handle_page_get_layout_metrics(
        &self,
        request: CdpRequest,
        _ctx: RequestContext,
    ) -> CdpResponse {
        let result = {
            if !request
                .params
                .as_object()
                .is_some_and(serde_json::Map::is_empty)
            {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(
                        CdpErrorCode::InvalidParams,
                        "Page.getLayoutMetrics takes no parameters",
                    ),
                );
            }
            Ok(json!({
                "layoutViewport":{"pageX":0,"pageY":0,"clientWidth":1280,"clientHeight":720},
                "visualViewport":{"offsetX":0,"offsetY":0,"pageX":0,"pageY":0,"clientWidth":1280,"clientHeight":720,"scale":1,"zoom":1},
                "contentSize":{"x":0,"y":0,"width":1280,"height":720},
                "cssLayoutViewport":{"pageX":0,"pageY":0,"clientWidth":1280,"clientHeight":720},
                "cssVisualViewport":{"offsetX":0,"offsetY":0,"pageX":0,"pageY":0,"clientWidth":1280,"clientHeight":720,"scale":1,"zoom":1},
                "cssContentSize":{"x":0,"y":0,"width":1280,"height":720}
            }))
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
    pub(super) async fn handle_page_capture_screenshot(
        &self,
        request: CdpRequest,
        ctx: RequestContext,
    ) -> CdpResponse {
        let result = {
            let Some(params) = request.params.as_object() else {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(CdpErrorCode::InvalidParams, "invalid screenshot parameters"),
                );
            };
            let allowed = [
                "format",
                "fromSurface",
                "captureBeyondViewport",
                "optimizeForSpeed",
                "clip",
            ];
            let valid = params.keys().all(|key| allowed.contains(&key.as_str()))
                && params
                    .get("format")
                    .and_then(Value::as_str)
                    .is_none_or(|format| format == "png")
                && ["fromSurface", "captureBeyondViewport", "optimizeForSpeed"]
                    .into_iter()
                    .all(|key| params.get(key).is_none_or(Value::is_boolean));
            if !valid {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(
                        CdpErrorCode::InvalidParams,
                        "only bounded PNG viewport screenshots are supported",
                    ),
                );
            }
            let Some(store) = self.artifacts.as_ref() else {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(
                        CdpErrorCode::RuntimeFailure,
                        "artifact reader is not configured",
                    ),
                );
            };
            let Some((session_id, page_id)) =
                self.runtime_identity(request.session_id.as_deref()).await
            else {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(CdpErrorCode::InvalidParams, "unknown runtime page"),
                );
            };
            let mode = if let Some(clip) = params.get("clip").and_then(Value::as_object) {
                let number = |key: &str| clip.get(key).and_then(Value::as_f64);
                let (Some(x), Some(y), Some(width), Some(height), Some(scale)) = (
                    number("x"),
                    number("y"),
                    number("width"),
                    number("height"),
                    number("scale"),
                ) else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(CdpErrorCode::InvalidParams, "invalid screenshot clip"),
                    );
                };
                let bounded = [x, y, width, height, scale].into_iter().all(f64::is_finite)
                    && x >= 0.0
                    && y >= 0.0
                    && width > 0.0
                    && height > 0.0
                    && x + width <= 16_384.0
                    && y + height <= 16_384.0
                    && scale == 1.0;
                if !bounded || clip.len() != 5 {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "screenshot clip exceeds bounds",
                        ),
                    );
                }
                ScreenshotMode::Clip {
                    x,
                    y,
                    width,
                    height,
                }
            } else {
                ScreenshotMode::Viewport
            };
            let envelope = CommandEnvelope {
                schema_version: CommandEnvelope::SCHEMA_VERSION,
                command_id: CommandId::new(),
                workflow_id: WorkflowId::new(),
                attempt_id: AttemptId::new(),
                session_id: session_id.clone(),
                page_id: Some(page_id),
                deadline: Utc::now() + Duration::seconds(30),
                command: RuntimeCommand::Primitive(PrimitiveCommand::CaptureScreenshot(
                    CaptureScreenshotCommand { mode },
                )),
            };
            match self.runtime.submit(ctx, envelope).await {
                Ok(CommandOutcome::Completed { evidence, .. }) => {
                    let screenshot = evidence.iter().find_map(|item| match item {
                        types::Evidence::Screenshot {
                            artifact_id,
                            media_type,
                            bytes,
                            sha256,
                            ..
                        } => Some((artifact_id, media_type, *bytes, sha256)),
                        _ => None,
                    });
                    let Some((artifact_id, _media_type, expected_bytes, expected_sha)) =
                        screenshot.filter(|(_, media_type, _, _)| *media_type == "image/png")
                    else {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(
                                CdpErrorCode::RuntimeFailure,
                                "runtime screenshot evidence was missing or invalid",
                            ),
                        );
                    };
                    let bytes = match store.get(&session_id, artifact_id).await {
                        Ok(bytes) => bytes,
                        Err(_) => {
                            return CdpResponse::failure(
                                &request,
                                CdpError::new(
                                    CdpErrorCode::RuntimeFailure,
                                    "verified screenshot artifact was unavailable",
                                ),
                            )
                        }
                    };
                    let sha = hex::encode(Sha256::digest(&bytes));
                    if bytes.len() as u64 != expected_bytes || &sha != expected_sha {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(
                                CdpErrorCode::RuntimeFailure,
                                "screenshot artifact integrity check failed",
                            ),
                        );
                    }
                    self.record_interface_event(
                        "screenshot.verified",
                        json!({"evidence":evidence}),
                    )
                    .await;
                    Ok(json!({"data":BASE64.encode(bytes)}))
                }
                Ok(_) => {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::RuntimeFailure,
                            "runtime screenshot did not complete",
                        ),
                    )
                }
                Err(error) => Err(error),
            }
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
    pub(super) async fn handle_page_add_script(
        &self,
        request: CdpRequest,
        _ctx: RequestContext,
    ) -> CdpResponse {
        let result = {
            if !supported_client_initialization(&request.params) {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(
                        CdpErrorCode::InvalidParams,
                        "only pinned bounded client initialization signatures are supported",
                    ),
                );
            }
            Ok(json!({"identifier": Uuid::new_v4().simple().to_string()}))
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
    pub(super) async fn handle_page_create_isolated_world(
        &self,
        request: CdpRequest,
        _ctx: RequestContext,
    ) -> CdpResponse {
        let result = {
            let frame_id = request
                .params
                .get("frameId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty() && id.len() <= 256);
            let world_name = request
                .params
                .get("worldName")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty() && name.len() <= 256);
            let valid = frame_id.is_some() && world_name.is_some();
            if !valid {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(
                        CdpErrorCode::InvalidParams,
                        "invalid isolated world request",
                    ),
                );
            }
            let mut worlds = self.isolated_worlds.lock().await;
            if worlds.len() >= MAX_ISOLATED_WORLDS {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(
                        CdpErrorCode::RuntimeFailure,
                        "isolated world registry exhausted",
                    ),
                );
            }
            worlds.insert(
                request
                    .session_id
                    .clone()
                    .unwrap_or_else(|| "browser".into()),
                world_name.unwrap().to_owned(),
            );
            let unique_id = self
                .bind_identifier(
                    IdentifierFamily::ExecutionContext,
                    request.session_id.as_deref().unwrap_or("browser"),
                    world_name.unwrap(),
                    RuntimeGeneration(0),
                )
                .await;
            if let Err(error) = self.queue_event(CdpEvent {
                    method: "Runtime.executionContextCreated".into(),
                    params: json!({"context":{"id":2,"origin":"","name":world_name.unwrap(),"uniqueId":unique_id,"auxData":{"isDefault":false,"type":"isolated","frameId":frame_id.unwrap()}}}),
                    session_id: request.session_id.clone(),
                }).await { return CdpResponse::failure(&request, error); }
            Ok(json!({"executionContextId":2}))
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
    pub(super) async fn handle_page_navigate(
        &self,
        request: CdpRequest,
        ctx: RequestContext,
    ) -> CdpResponse {
        let result = {
            let Some(url) = request
                .params
                .get("url")
                .and_then(Value::as_str)
                .filter(|url| !url.is_empty() && url.len() <= 16_384)
            else {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(CdpErrorCode::InvalidParams, "invalid navigation URL"),
                );
            };
            let Some(cdp_session) = request.session_id.as_deref() else {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(
                        CdpErrorCode::InvalidParams,
                        "navigation requires a CDP session",
                    ),
                );
            };
            let Some(target_id) = self
                .resolve_identifier(IdentifierFamily::CdpSession, cdp_session)
                .await
            else {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(CdpErrorCode::InvalidParams, "unknown CDP session"),
                );
            };
            let Some(page) = self
                .resolve_identifier(IdentifierFamily::Target, &target_id)
                .await
            else {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(CdpErrorCode::InvalidParams, "unknown CDP target"),
                );
            };
            let Some(runtime_session) = self
                .runtime_session_for(IdentifierFamily::Target, &target_id)
                .await
            else {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(CdpErrorCode::InvalidParams, "unknown runtime session"),
                );
            };
            let (Ok(session_uuid), Ok(page_uuid)) =
                (Uuid::parse_str(&runtime_session), Uuid::parse_str(&page))
            else {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(CdpErrorCode::RuntimeFailure, "invalid runtime identity"),
                );
            };
            let loader_id = Uuid::new_v4().simple().to_string();
            let envelope = CommandEnvelope {
                schema_version: CommandEnvelope::SCHEMA_VERSION,
                command_id: CommandId::new(),
                workflow_id: WorkflowId::new(),
                attempt_id: AttemptId::new(),
                session_id: SessionId(session_uuid),
                page_id: Some(PageId(page_uuid)),
                deadline: Utc::now() + Duration::seconds(30),
                command: RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
                    url: url.to_owned(),
                    wait_until: WaitUntil::Interactive,
                    timeout_ms: 30_000,
                })),
            };
            match self.runtime.submit(ctx, envelope).await {
                Ok(CommandOutcome::Completed { evidence, .. }) => {
                    let Some((final_url, title)) = evidence.iter().find_map(|item| match item {
                        types::Evidence::Navigation { url, title } => {
                            Some((url.clone(), title.clone()))
                        }
                        _ => None,
                    }) else {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(
                                CdpErrorCode::RuntimeFailure,
                                "navigation returned no verified evidence",
                            ),
                        );
                    };
                    self.advance_execution_generation(request.session_id.as_deref())
                        .await;
                    let world_name = self.isolated_worlds.lock().await.get(cdp_session).cloned();
                    let mut events = vec![
                        CdpEvent {
                            method: "Page.frameNavigated".into(),
                            params: json!({"frame":{"id":target_id,"loaderId":loader_id,"url":final_url,"domainAndRegistry":"","securityOrigin":"","mimeType":"text/html","secureContextType":"SecureLocalhost","crossOriginIsolatedContextType":"NotIsolated","gatedAPIFeatures":[]},"type":"Navigation"}),
                            session_id: request.session_id.clone(),
                        },
                        CdpEvent {
                            method: "Runtime.executionContextsCleared".into(),
                            params: json!({}),
                            session_id: request.session_id.clone(),
                        },
                        CdpEvent {
                            method: "Runtime.executionContextCreated".into(),
                            params: json!({"context":{"id":1,"origin":final_url,"name":"","uniqueId":Uuid::new_v4().simple().to_string(),"auxData":{"isDefault":true,"type":"default","frameId":target_id}}}),
                            session_id: request.session_id.clone(),
                        },
                        CdpEvent {
                            method: "Page.lifecycleEvent".into(),
                            params: json!({"frameId":target_id,"loaderId":loader_id,"name":"init","timestamp":0}),
                            session_id: request.session_id.clone(),
                        },
                        CdpEvent {
                            method: "Page.lifecycleEvent".into(),
                            params: json!({"frameId":target_id,"loaderId":loader_id,"name":"DOMContentLoaded","timestamp":0}),
                            session_id: request.session_id.clone(),
                        },
                        CdpEvent {
                            method: "Page.lifecycleEvent".into(),
                            params: json!({"frameId":target_id,"loaderId":loader_id,"name":"load","timestamp":0}),
                            session_id: request.session_id.clone(),
                        },
                    ];
                    if let Some(world_name) = world_name {
                        events.insert(3, CdpEvent { method:"Runtime.executionContextCreated".into(), params:json!({"context":{"id":2,"origin":final_url,"name":world_name,"uniqueId":Uuid::new_v4().simple().to_string(),"auxData":{"isDefault":false,"type":"isolated","frameId":target_id}}}), session_id:request.session_id.clone() });
                    }
                    if let Err(error) = self.queue_events(events).await {
                        return CdpResponse::failure(&request, error);
                    }
                    self.record_interface_event(
                        "navigation.completed",
                        json!({"evidence":evidence}),
                    )
                    .await;
                    self.targets.lock().await.note_navigation(
                        &runtime_session,
                        &page,
                        &final_url,
                        &title,
                    );
                    Ok(json!({"frameId":target_id,"loaderId":loader_id,"isDownload":false}))
                }
                Ok(_) => {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(CdpErrorCode::RuntimeFailure, "navigation did not complete"),
                    )
                }
                Err(error) => Err(error),
            }
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
    pub(super) async fn handle_page_set_lifecycle(
        &self,
        request: CdpRequest,
        _ctx: RequestContext,
    ) -> CdpResponse {
        let result = {
            let Some(enabled) = request.params.get("enabled").and_then(Value::as_bool) else {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(
                        CdpErrorCode::InvalidParams,
                        "invalid lifecycle event configuration",
                    ),
                );
            };
            let scope = request
                .session_id
                .clone()
                .unwrap_or_else(|| "browser".into());
            if enabled {
                self.lifecycle_events.lock().await.insert(scope);
            } else {
                self.lifecycle_events.lock().await.remove(&scope);
            }
            Ok(json!({}))
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
    pub(super) async fn handle_page_enable(
        &self,
        request: CdpRequest,
        _ctx: RequestContext,
    ) -> CdpResponse {
        let result = {
            if !request
                .params
                .as_object()
                .is_some_and(serde_json::Map::is_empty)
            {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(
                        CdpErrorCode::InvalidParams,
                        "Page.enable takes no parameters",
                    ),
                );
            }
            self.enable_domain(request.session_id.as_deref(), "Page")
                .await;
            if let Some(cdp_session) = request.session_id.as_deref() {
                if let Some((frame_id, url, loader_id)) = self
                    .pending_page_loads
                    .lock()
                    .await
                    .get(cdp_session)
                    .cloned()
                {
                    for event in [
                        CdpEvent {
                            method: "Page.frameNavigated".into(),
                            params: json!({"frame":{"id":frame_id,"loaderId":loader_id,"url":url,"domainAndRegistry":"","securityOrigin":"","mimeType":"text/html","secureContextType":"SecureLocalhost","crossOriginIsolatedContextType":"NotIsolated","gatedAPIFeatures":[]},"type":"Navigation"}),
                            session_id: request.session_id.clone(),
                        },
                        CdpEvent {
                            method: "Page.lifecycleEvent".into(),
                            params: json!({"frameId":frame_id,"loaderId":loader_id,"name":"DOMContentLoaded","timestamp":0}),
                            session_id: request.session_id.clone(),
                        },
                        CdpEvent {
                            method: "Page.lifecycleEvent".into(),
                            params: json!({"frameId":frame_id,"loaderId":loader_id,"name":"load","timestamp":0}),
                            session_id: request.session_id.clone(),
                        },
                    ] {
                        if let Err(error) = self.queue_event(event).await {
                            return CdpResponse::failure(&request, error);
                        }
                    }
                }
            }
            Ok(json!({}))
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
}
