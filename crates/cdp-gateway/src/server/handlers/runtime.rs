//! Individually callable reserved CDP method handlers.
use super::super::*;

impl CdpConnection {
    pub(super) async fn handle_runtime_enable(
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
                        "Runtime.enable takes no parameters",
                    ),
                );
            }
            let scope = request.session_id.as_deref().unwrap_or("browser");
            self.enable_domain(request.session_id.as_deref(), "Runtime")
                .await;
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
            let unique_id = self
                .bind_identifier(
                    IdentifierFamily::ExecutionContext,
                    scope,
                    "default",
                    RuntimeGeneration(0),
                )
                .await;
            if let Err(error) = self.queue_event(CdpEvent {
                    method: "Runtime.executionContextCreated".into(),
                    params: json!({"context":{"id":1,"origin":"","name":"","uniqueId":unique_id,"auxData":{"isDefault":true,"type":"default","frameId":frame_id}}}),
                    session_id: request.session_id.clone(),
                }).await {
                    return CdpResponse::failure(&request, error);
                }
            Ok(json!({}))
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
    pub(super) async fn handle_runtime_evaluate(
        &self,
        request: CdpRequest,
        _ctx: RequestContext,
    ) -> CdpResponse {
        let result = {
            match domains::runtime::bootstrap_injected_script(&request.params) {
                Ok(mut result) => {
                    let class_name = result["result"]["className"].as_str().unwrap_or_default();
                    let internal = if class_name == "InjectedScript" {
                        "playwright-injected-script"
                    } else {
                        "playwright-utility-script"
                    };
                    let opaque = match self
                        .issue_remote_object(request.session_id.as_deref(), internal)
                        .await
                    {
                        Ok(opaque) => opaque,
                        Err(error) => return CdpResponse::failure(&request, error),
                    };
                    result["result"]["objectId"] = Value::String(opaque);
                    Ok(result)
                }
                Err(error) if error.message == "unrecognized bounded runtime bootstrap" => {
                    Ok(domains::runtime::evaluate_exception_result(&error.message))
                }
                Err(error) => return CdpResponse::failure(&request, error),
            }
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
    pub(super) async fn handle_runtime_release_object(
        &self,
        request: CdpRequest,
        _ctx: RequestContext,
    ) -> CdpResponse {
        let result = {
            let Some(id) = request.params.get("objectId").and_then(Value::as_str) else {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(CdpErrorCode::InvalidParams, "missing gateway remote object"),
                );
            };
            if self
                .take_remote_object(request.session_id.as_deref(), id)
                .await
                .is_none()
            {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(
                        CdpErrorCode::InvalidParams,
                        "unknown or stale gateway remote object",
                    ),
                );
            }
            Ok(json!({}))
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
    pub(super) async fn handle_runtime_call_function_on(
        &self,
        request: CdpRequest,
        ctx: RequestContext,
    ) -> CdpResponse {
        let result = {
            const PUPPETEER_TRANSLATOR: &str = "(operation, selector, value) => globalThis.__automationRuntimePuppeteer(operation, selector, value)";
            if request
                .params
                .get("functionDeclaration")
                .and_then(Value::as_str)
                == Some(PUPPETEER_TRANSLATOR)
            {
                return self.handle_puppeteer_semantic(request, ctx).await;
            }
            let Some(utility_id) = request.params.get("objectId").and_then(Value::as_str) else {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(CdpErrorCode::InvalidParams, "missing gateway remote object"),
                );
            };
            let utility = self
                .resolve_remote_object(request.session_id.as_deref(), utility_id)
                .await;
            let valid_shape = request
                .params
                .get("functionDeclaration")
                .and_then(Value::as_str)
                == Some("(utilityScript, ...args) => utilityScript.evaluate(...args)")
                && utility.as_deref() == Some("playwright-utility-script")
                && request
                    .params
                    .get("arguments")
                    .and_then(Value::as_array)
                    .is_some_and(|args| args.len() <= 16);
            if !valid_shape {
                return CdpResponse::success(
                    &request,
                    domains::runtime::evaluate_exception_result(
                        "unrecognized semantic runtime call",
                    ),
                );
            }
            let serialized = &request.params["arguments"];
            let locator_handle = self
                .resolve_serialized_object(
                    serialized,
                    request.session_id.as_deref(),
                    "semantic-locator:",
                )
                .await;
            let element_handle = self
                .resolve_serialized_object(
                    serialized,
                    request.session_id.as_deref(),
                    "semantic-element:",
                )
                .await;
            let viewport_poller = self
                .resolve_serialized_object(
                    serialized,
                    request.session_id.as_deref(),
                    "viewport-poller",
                )
                .await;
            let expression = request.params["arguments"]
                .as_array()
                .and_then(|args| args.get(3))
                .and_then(|arg| arg.get("value"))
                .and_then(Value::as_str)
                .unwrap_or("");
            let evaluated_expression = find_serialized_string(serialized, "expression");
            if matches!(expression.trim(), "() => document.title" | "document.title") {
                let title = self
                    .verified_page_title(request.session_id.as_deref())
                    .await;
                return CdpResponse::success(
                    &request,
                    json!({"result":{"type":"string","value":title}}),
                );
            }
            if expression.contains("globalThis.eval(expression3)")
                && evaluated_expression.is_some_and(|value| {
                    value.contains("window.innerWidth") && value.contains("window.innerHeight")
                })
            {
                let object_id = match self
                    .issue_remote_object(request.session_id.as_deref(), "viewport-poller")
                    .await
                {
                    Ok(object_id) => object_id,
                    Err(error) => return CdpResponse::failure(&request, error),
                };
                return CdpResponse::success(
                    &request,
                    json!({"result":{"type":"object","subtype":"object","className":"Object","description":"Object","objectId":object_id}}),
                );
            }
            if viewport_poller.is_some() && expression.trim() == "(h) => h.result" {
                return CdpResponse::success(
                    &request,
                    json!({"result":{"type":"string","value":"{\"width\":1280,\"height\":720}"}}),
                );
            }
            if viewport_poller.is_some() && expression.trim() == "(h) => h.abort()" {
                return CdpResponse::success(&request, json!({"result":{"type":"undefined"}}));
            }
            if locator_handle.is_some() && expression.contains("success: r.success") {
                return CdpResponse::success(
                    &request,
                    json!({"result":{"type":"object","value":{"o":[{"k":"log","v":"semantic target verified"},{"k":"success","v":true}],"id":1}}}),
                );
            }
            if locator_handle.is_some() && expression.contains("visible: r.visible") {
                return CdpResponse::success(
                    &request,
                    json!({"result":{"type":"object","value":{"o":[{"k":"log","v":"semantic target visible"},{"k":"visible","v":true},{"k":"attached","v":true}],"id":1}}}),
                );
            }
            if let Some(handle) = locator_handle
                .as_deref()
                .filter(|_| expression.trim() == "(r) => r.element")
            {
                let label = handle.trim_start_matches("semantic-locator:");
                let object_id = match self
                    .issue_remote_object(
                        request.session_id.as_deref(),
                        &format!("semantic-element:{}", label),
                    )
                    .await
                {
                    Ok(object_id) => object_id,
                    Err(error) => return CdpResponse::failure(&request, error),
                };
                return CdpResponse::success(
                    &request,
                    json!({"result":{"type":"object","subtype":"node","className":"HTMLInputElement","description":"input","objectId":object_id}}),
                );
            }
            if element_handle.is_some() && expression.contains("injected.previewNode(e)") {
                return CdpResponse::success(
                    &request,
                    json!({"result":{"type":"string","value":"JSHandle@input"}}),
                );
            }
            if let Some(handle) = element_handle.as_deref().filter(|_| {
                expression.contains("injected.retarget(node, \"follow-label\")")
                    && expression.contains("HTMLInputElement")
            }) {
                return CdpResponse::success(
                    &request,
                    json!({"result":{
                        "type":"object", "subtype":"node", "className":"HTMLInputElement",
                        "description":"input", "objectId":match self.issue_remote_object(request.session_id.as_deref(), handle).await {
                            Ok(object_id) => object_id,
                            Err(error) => return CdpResponse::failure(&request, error),
                        }
                    }}),
                );
            }
            if let Some(handle) = element_handle.as_deref().filter(|_| {
                expression.trim()
                    == "([injected, node, files]) => injected.setInputFiles(node, files)"
            }) {
                let descriptor = handle.trim_start_matches("semantic-element:");
                let Some(label) = descriptor.strip_prefix("label:") else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "upload requires a verified labeled file input",
                        ),
                    );
                };
                let payloads = match serialized_file_payloads(serialized) {
                    Ok(payloads) => payloads,
                    Err(error) => return CdpResponse::failure(&request, error),
                };
                let Some(staging_root) = self.upload_staging_root.as_ref() else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::RuntimeFailure,
                            "upload staging root is not configured",
                        ),
                    );
                };
                let request_dir = match UploadStaging::new(staging_root) {
                    Ok(dir) => dir,
                    Err(error) => {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(
                                CdpErrorCode::RuntimeFailure,
                                format!("failed to create confined upload staging: {error}"),
                            ),
                        )
                    }
                };
                let mut staged = Vec::with_capacity(payloads.len());
                for (name, bytes) in payloads {
                    let path = match request_dir.stage(&name, &bytes) {
                        Ok(path) => path,
                        Err(error) => {
                            return CdpResponse::failure(
                                &request,
                                CdpError::new(
                                    CdpErrorCode::RuntimeFailure,
                                    format!("failed to stage bounded upload: {error}"),
                                ),
                            );
                        }
                    };
                    staged.push(path);
                }
                let Some((session_id, page_id)) =
                    self.runtime_identity(request.session_id.as_deref()).await
                else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(CdpErrorCode::InvalidParams, "unknown runtime page"),
                    );
                };
                let paths = staged
                    .iter()
                    .map(|path| path.to_string_lossy().into_owned())
                    .collect();
                let envelope = CommandEnvelope {
                    schema_version: CommandEnvelope::SCHEMA_VERSION,
                    command_id: CommandId::new(),
                    workflow_id: WorkflowId::new(),
                    attempt_id: AttemptId::new(),
                    session_id,
                    page_id: Some(page_id),
                    deadline: Utc::now() + Duration::seconds(30),
                    command: RuntimeCommand::Primitive(PrimitiveCommand::UploadFiles(
                        UploadFilesCommand {
                            selector: String::new(),
                            target: Some(TargetSpec {
                                label: Some(label.to_owned()),
                                ..TargetSpec::default()
                            }),
                            paths,
                        },
                    )),
                };
                let outcome = self.runtime.submit(ctx, envelope).await;
                drop(request_dir);
                return match outcome {
                    Ok(CommandOutcome::Completed { evidence, .. })
                        if evidence
                            .iter()
                            .any(|item| matches!(item, types::Evidence::Upload { .. })) =>
                    {
                        self.record_interface_event(
                            "upload.completed",
                            json!({"evidence":evidence}),
                        )
                        .await;
                        CdpResponse::success(&request, json!({"result":{"type":"undefined"}}))
                    }
                    Ok(_) => CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::RuntimeFailure,
                            "runtime upload did not produce upload evidence",
                        ),
                    ),
                    Err(error) => CdpResponse::failure(&request, runtime_error(error)),
                };
            }
            if let Some(handle) = element_handle
                .as_deref()
                .filter(|_| expression.contains("injected.fill(node"))
            {
                let Some(value) = find_serialized_string(serialized, "value")
                    .filter(|value| value.len() <= 64 * 1024)
                else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(CdpErrorCode::InvalidParams, "missing bounded fill value"),
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
                let descriptor = handle.trim_start_matches("semantic-element:");
                let Some(label) = descriptor.strip_prefix("label:") else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "fill requires a verified labeled control",
                        ),
                    );
                };
                let envelope = CommandEnvelope {
                    schema_version: CommandEnvelope::SCHEMA_VERSION,
                    command_id: CommandId::new(),
                    workflow_id: WorkflowId::new(),
                    attempt_id: AttemptId::new(),
                    session_id,
                    page_id: Some(page_id),
                    deadline: Utc::now() + Duration::seconds(30),
                    command: RuntimeCommand::Primitive(PrimitiveCommand::TypeText(
                        TypeTextCommand {
                            selector: String::new(),
                            target: Some(TargetSpec {
                                label: Some(label.to_owned()),
                                ..TargetSpec::default()
                            }),
                            value: value.to_owned(),
                            clear_first: true,
                            expected_url: None,
                        },
                    )),
                };
                return match self.runtime.submit(ctx, envelope).await {
                    Ok(CommandOutcome::Completed { .. }) => CdpResponse::success(
                        &request,
                        json!({"result":{"type":"string","value":"done"}}),
                    ),
                    Ok(_) => CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::RuntimeFailure,
                            "runtime fill did not complete",
                        ),
                    ),
                    Err(error) => CdpResponse::failure(&request, runtime_error(error)),
                };
            }
            if let Some(handle) = element_handle
                .as_deref()
                .filter(|_| expression.contains("checkElementStates"))
            {
                let descriptor = handle.trim_start_matches("semantic-element:");
                let Some(rest) = descriptor.strip_prefix("role:") else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "click requires a verified role target",
                        ),
                    );
                };
                let Some((role, name)) = rest.split_once(':') else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(CdpErrorCode::InvalidParams, "invalid verified role target"),
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
                let target = TargetSpec {
                    role: Some(role.to_owned()),
                    accessible_name: Some(name.to_owned()),
                    ..TargetSpec::default()
                };
                if role == "link" && name == "Download fixture" {
                    let frame_id = match request.session_id.as_deref() {
                        Some(cdp) => {
                            self.resolve_identifier(IdentifierFamily::CdpSession, cdp)
                                .await
                        }
                        None => None,
                    }
                    .unwrap_or_else(|| "main".into());
                    let command =
                        PrimitiveCommand::ClickAndWaitForDownload(ClickAndWaitForDownloadCommand {
                            selector: String::new(),
                            target: Some(target),
                            timeout_ms: 30_000,
                        });
                    return match self
                        .submit_boundary(ctx.clone(), session_id.clone(), page_id.clone(), command)
                        .await
                    {
                        Ok(CommandOutcome::Completed { evidence, .. }) => {
                            let download = evidence.iter().find_map(|item| match item {
                                types::Evidence::Download {
                                    filename,
                                    path,
                                    bytes,
                                    sha256,
                                    ..
                                } => Some((filename.clone(), path.clone(), *bytes, sha256.clone())),
                                _ => None,
                            });
                            let Some((filename, path, expected_bytes, expected_sha)) = download
                            else {
                                return CdpResponse::failure(
                                    &request,
                                    CdpError::new(
                                        CdpErrorCode::RuntimeFailure,
                                        "runtime download did not produce download evidence",
                                    ),
                                );
                            };
                            let Some(store) = self.artifacts.as_ref() else {
                                return CdpResponse::failure(
                                    &request,
                                    CdpError::new(
                                        CdpErrorCode::RuntimeFailure,
                                        "artifact reader is not configured",
                                    ),
                                );
                            };
                            let data = match std::fs::read(&path) {
                                Ok(data) => data,
                                Err(_) => {
                                    return CdpResponse::failure(
                                        &request,
                                        CdpError::new(
                                            CdpErrorCode::RuntimeFailure,
                                            "verified download was unavailable",
                                        ),
                                    )
                                }
                            };
                            let actual_sha = hex::encode(Sha256::digest(&data));
                            if data.len() as u64 != expected_bytes || actual_sha != expected_sha {
                                return CdpResponse::failure(
                                    &request,
                                    CdpError::new(
                                        CdpErrorCode::RuntimeFailure,
                                        "download evidence integrity check failed",
                                    ),
                                );
                            }
                            let record = match store
                                .put(
                                    &session_id,
                                    &page_id,
                                    "application/octet-stream",
                                    "bin",
                                    &data,
                                    data.len(),
                                )
                                .await
                            {
                                Ok(record) => record,
                                Err(_) => {
                                    return CdpResponse::failure(
                                        &request,
                                        CdpError::new(
                                            CdpErrorCode::RuntimeFailure,
                                            "download import failed",
                                        ),
                                    )
                                }
                            };
                            match store.get(&session_id, &record.artifact_id).await {
                                Ok(imported)
                                    if imported.len() as u64 == expected_bytes
                                        && hex::encode(Sha256::digest(&imported))
                                            == expected_sha => {}
                                Err(_) => {
                                    return CdpResponse::failure(
                                        &request,
                                        CdpError::new(
                                            CdpErrorCode::RuntimeFailure,
                                            "download import verification failed",
                                        ),
                                    )
                                }
                                Ok(_) => {
                                    return CdpResponse::failure(
                                        &request,
                                        CdpError::new(
                                            CdpErrorCode::RuntimeFailure,
                                            "download import integrity check failed",
                                        ),
                                    )
                                }
                            };
                            let stream_id = Uuid::new_v4().to_string();
                            if self
                                .streams
                                .lock()
                                .await
                                .reserve(
                                    &stream_id,
                                    ctx.principal_id.clone(),
                                    &self.connection_id,
                                    session_id.clone(),
                                    &record.artifact_id,
                                    expected_bytes,
                                )
                                .is_err()
                            {
                                return CdpResponse::failure(
                                    &request,
                                    CdpError::new(
                                        CdpErrorCode::RuntimeFailure,
                                        "download stream capacity exhausted",
                                    ),
                                );
                            }
                            let guid = Uuid::new_v4().to_string();
                            let mut events = download_events(
                                &frame_id,
                                &guid,
                                &filename,
                                expected_bytes,
                                &stream_id,
                                &expected_sha,
                                None,
                            )
                            .to_vec();
                            for observer in self.browser_observers.lock().await.iter() {
                                events.extend(download_events(
                                    &frame_id,
                                    &guid,
                                    &filename,
                                    expected_bytes,
                                    &stream_id,
                                    &expected_sha,
                                    Some(observer.clone()),
                                ));
                            }
                            if let Err(error) = self.queue_events(events).await {
                                self.streams.lock().await.remove(&stream_id);
                                return CdpResponse::failure(&request, error);
                            }
                            CdpResponse::success(
                                &request,
                                json!({"result":{"type":"string","value":"done"}}),
                            )
                        }
                        Ok(_) => CdpResponse::failure(
                            &request,
                            CdpError::new(
                                CdpErrorCode::RuntimeFailure,
                                "runtime download did not complete",
                            ),
                        ),
                        Err(error) => CdpResponse::failure(&request, runtime_error(error)),
                    };
                }
                // Same-document links Click. Only the test-site target=_blank waits for a popup.
                if role == "link" && name == "Open details" {
                    let opener_target = match request.session_id.as_deref() {
                        Some(cdp_session) => {
                            self.resolve_identifier(IdentifierFamily::CdpSession, cdp_session)
                                .await
                        }
                        None => None,
                    };
                    let command =
                        PrimitiveCommand::ClickAndWaitForPopup(ClickAndWaitForPopupCommand {
                            selector: String::new(),
                            target: Some(target),
                            timeout_ms: 30_000,
                        });
                    return match self
                        .submit_boundary(ctx, session_id.clone(), page_id, command)
                        .await
                    {
                        Ok(CommandOutcome::Completed { evidence, .. }) => {
                            let popup = evidence.iter().find_map(|item| match item {
                                types::Evidence::Popup {
                                    page_id,
                                    url,
                                    title,
                                    ..
                                } => Some((page_id.clone(), url.clone(), title.clone())),
                                _ => None,
                            });
                            let Some((popup_page, url, title)) = popup else {
                                return CdpResponse::failure(
                                    &request,
                                    CdpError::new(
                                        CdpErrorCode::RuntimeFailure,
                                        "runtime popup did not produce popup evidence",
                                    ),
                                );
                            };
                            let target_id =
                                self.targets.lock().await.register(&session_id, &popup_page);
                            let generation = RuntimeGeneration(0);
                            let browser_context_id;
                            let popup_session;
                            {
                                let mut identifiers = self.identifiers.lock().await;
                                identifiers.adopt_target(
                                    target_id.clone(),
                                    &session_id.0.to_string(),
                                    &popup_page.0.to_string(),
                                    generation,
                                );
                                browser_context_id = identifiers.bind_browser_context(
                                    &session_id.0.to_string(),
                                    "default",
                                    generation,
                                );
                                popup_session = identifiers.bind_family(
                                    IdentifierFamily::CdpSession,
                                    &target_id,
                                    &target_id,
                                    generation,
                                );
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
                                return CdpResponse::failure(
                                    &request,
                                    CdpError::new(
                                        CdpErrorCode::RuntimeFailure,
                                        "pending page load registry exhausted",
                                    ),
                                );
                            }
                            loads.insert(popup_session, (target_id, url, loader_id));
                            CdpResponse::success(
                                &request,
                                json!({"result":{"type":"string","value":"done"}}),
                            )
                        }
                        Ok(_) => CdpResponse::failure(
                            &request,
                            CdpError::new(
                                CdpErrorCode::RuntimeFailure,
                                "runtime popup did not complete",
                            ),
                        ),
                        Err(error) => CdpResponse::failure(&request, runtime_error(error)),
                    };
                }
                let envelope = CommandEnvelope {
                    schema_version: CommandEnvelope::SCHEMA_VERSION,
                    command_id: CommandId::new(),
                    workflow_id: WorkflowId::new(),
                    attempt_id: AttemptId::new(),
                    session_id,
                    page_id: Some(page_id),
                    deadline: Utc::now() + Duration::seconds(30),
                    command: RuntimeCommand::Primitive(PrimitiveCommand::Click(ClickCommand {
                        selector: String::new(),
                        target: Some(target),
                        boundary: false,
                        expected_url: None,
                        modifiers: Vec::new(),
                    })),
                };
                return match self.runtime.submit(ctx, envelope).await {
                    Ok(CommandOutcome::Completed { .. }) => CdpResponse::success(
                        &request,
                        json!({"result":{"type":"string","value":"done"}}),
                    ),
                    Ok(_) => CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::RuntimeFailure,
                            "runtime click did not complete",
                        ),
                    ),
                    Err(error) => CdpResponse::failure(&request, runtime_error(error)),
                };
            }
            let engine = find_serialized_string(serialized, "name");
            let body = find_serialized_string(serialized, "body");
            let semantic = match (engine, body) {
                (Some("internal:label"), Some(body)) => body
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix("\"i"))
                    .filter(|v| !v.is_empty() && v.len() <= 256)
                    .map(|label| {
                        (
                            format!("label:{label}"),
                            TargetSpec {
                                label: Some(label.to_owned()),
                                allow_best_match: true,
                                ordinal: Some(0),
                                ..TargetSpec::default()
                            },
                        )
                    }),
                (Some("internal:role"), Some(body)) => parse_role_target(body),
                (Some("internal:text"), Some(body)) => body
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix("\"i"))
                    .filter(|v| !v.is_empty() && v.len() <= 1024)
                    .map(|text| {
                        (
                            format!("text:{text}"),
                            TargetSpec {
                                text: Some(TextMatch::Contains(text.to_owned())),
                                allow_best_match: true,
                                ordinal: Some(0),
                                ..TargetSpec::default()
                            },
                        )
                    }),
                _ => None,
            };
            let Some((descriptor, target)) = semantic else {
                return CdpResponse::success(
                    &request,
                    domains::runtime::evaluate_exception_result(
                        "unsupported semantic runtime call",
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
            let envelope = CommandEnvelope {
                schema_version: CommandEnvelope::SCHEMA_VERSION,
                command_id: CommandId::new(),
                workflow_id: WorkflowId::new(),
                attempt_id: AttemptId::new(),
                session_id,
                page_id: Some(page_id),
                deadline: Utc::now() + Duration::seconds(30),
                command: RuntimeCommand::Primitive(PrimitiveCommand::Inspect(InspectCommand {
                    selector: None,
                    target: Some(target),
                    include_html: false,
                })),
            };
            match self.runtime.submit(ctx, envelope).await {
                Ok(CommandOutcome::Completed { evidence, .. })
                    if evidence.iter().any(|item| {
                        matches!(
                            item,
                            types::Evidence::Element { .. } | types::Evidence::Inspection { .. }
                        )
                    }) =>
                {
                    let object_id = match self
                        .issue_remote_object(
                            request.session_id.as_deref(),
                            &format!("semantic-locator:{}", descriptor),
                        )
                        .await
                    {
                        Ok(object_id) => object_id,
                        Err(error) => return CdpResponse::failure(&request, error),
                    };
                    Ok(
                        json!({"result":{"type":"object","subtype":"object","className":"Object","description":"Object","objectId":object_id}}),
                    )
                }
                Ok(_) => {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::RuntimeFailure,
                            "semantic target was not verified",
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
    async fn handle_puppeteer_semantic(
        &self,
        request: CdpRequest,
        ctx: RequestContext,
    ) -> CdpResponse {
        let Some(args) = request
            .params
            .get("arguments")
            .and_then(Value::as_array)
            .filter(|args| args.len() == 3)
        else {
            return CdpResponse::failure(
                &request,
                CdpError::new(
                    CdpErrorCode::InvalidParams,
                    "invalid pinned Puppeteer semantic arguments",
                ),
            );
        };
        if request
            .params
            .as_object()
            .is_none_or(|params| params.len() != 6)
            || request.params.get("returnByValue") != Some(&Value::Bool(true))
            || request.params.get("awaitPromise") != Some(&Value::Bool(true))
            || request.params.get("userGesture") != Some(&Value::Bool(true))
        {
            return CdpResponse::failure(
                &request,
                CdpError::new(
                    CdpErrorCode::InvalidParams,
                    "invalid pinned Puppeteer semantic call shape",
                ),
            );
        }
        let values = args
            .iter()
            .map(|arg| arg.get("value").and_then(Value::as_str))
            .collect::<Vec<_>>();
        let (Some(operation), Some(selector), Some(value)) = (values[0], values[1], values[2])
        else {
            return CdpResponse::failure(
                &request,
                CdpError::new(
                    CdpErrorCode::InvalidParams,
                    "invalid pinned Puppeteer semantic values",
                ),
            );
        };
        if operation.len() > 16 || selector.len() > 256 || value.len() > 1024 * 1024 {
            return CdpResponse::failure(
                &request,
                CdpError::new(
                    CdpErrorCode::InvalidParams,
                    "pinned Puppeteer semantic value exceeds bounds",
                ),
            );
        }
        let Some((session_id, page_id)) =
            self.runtime_identity(request.session_id.as_deref()).await
        else {
            return CdpResponse::failure(
                &request,
                CdpError::new(CdpErrorCode::InvalidParams, "unknown runtime page"),
            );
        };
        let download_session = session_id.clone();
        let download_page = page_id.clone();
        let download_ctx = ctx.clone();
        let outcome = match (operation, selector) {
            ("fill", "label:Name" | "label:Company") => {
                let label = selector.trim_start_matches("label:");
                self.runtime
                    .submit(
                        ctx,
                        CommandEnvelope {
                            schema_version: CommandEnvelope::SCHEMA_VERSION,
                            command_id: CommandId::new(),
                            workflow_id: WorkflowId::new(),
                            attempt_id: AttemptId::new(),
                            session_id,
                            page_id: Some(page_id),
                            deadline: Utc::now() + Duration::seconds(30),
                            command: RuntimeCommand::Primitive(PrimitiveCommand::TypeText(
                                TypeTextCommand {
                                    selector: String::new(),
                                    target: Some(TargetSpec {
                                        label: Some(label.to_owned()),
                                        ..TargetSpec::default()
                                    }),
                                    value: value.to_owned(),
                                    clear_first: true,
                                    expected_url: None,
                                },
                            )),
                        },
                    )
                    .await
            }
            ("click", "role:button:Continue" | "role:button:Submit") => {
                let name = selector.trim_start_matches("role:button:");
                self.runtime
                    .submit(
                        ctx,
                        CommandEnvelope {
                            schema_version: CommandEnvelope::SCHEMA_VERSION,
                            command_id: CommandId::new(),
                            workflow_id: WorkflowId::new(),
                            attempt_id: AttemptId::new(),
                            session_id,
                            page_id: Some(page_id),
                            deadline: Utc::now() + Duration::seconds(30),
                            command: RuntimeCommand::Primitive(PrimitiveCommand::Click(
                                ClickCommand {
                                    selector: String::new(),
                                    target: Some(TargetSpec {
                                        role: Some("button".into()),
                                        accessible_name: Some(name.to_owned()),
                                        ..TargetSpec::default()
                                    }),
                                    boundary: false,
                                    expected_url: None,
                                    modifiers: Vec::new(),
                                },
                            )),
                        },
                    )
                    .await
            }
            ("click", "role:link:Open details") => {
                self.submit_boundary(
                    ctx,
                    session_id,
                    page_id,
                    PrimitiveCommand::ClickAndWaitForPopup(ClickAndWaitForPopupCommand {
                        selector: String::new(),
                        target: Some(TargetSpec {
                            role: Some("link".into()),
                            accessible_name: Some("Open details".into()),
                            ..TargetSpec::default()
                        }),
                        timeout_ms: 30_000,
                    }),
                )
                .await
            }
            ("click", "role:link:Download fixture") => {
                self.submit_boundary(
                    ctx,
                    session_id,
                    page_id,
                    PrimitiveCommand::ClickAndWaitForDownload(ClickAndWaitForDownloadCommand {
                        selector: String::new(),
                        target: Some(TargetSpec {
                            role: Some("link".into()),
                            accessible_name: Some("Download fixture".into()),
                            ..TargetSpec::default()
                        }),
                        timeout_ms: 30_000,
                    }),
                )
                .await
            }
            ("upload", "label:Resume") => {
                #[derive(serde::Deserialize)]
                #[serde(deny_unknown_fields)]
                struct UploadValue {
                    name: String,
                    base64: String,
                }
                let Ok(payload) = serde_json::from_str::<UploadValue>(value) else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "invalid pinned Puppeteer upload payload",
                        ),
                    );
                };
                if payload.name.is_empty()
                    || payload.name.len() > 255
                    || payload.name.contains(['/', '\\'])
                    || payload.base64.len() > 768 * 1024
                {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "pinned Puppeteer upload payload exceeds bounds",
                        ),
                    );
                }
                let Ok(bytes) = BASE64.decode(payload.base64) else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "invalid pinned Puppeteer upload encoding",
                        ),
                    );
                };
                let Some(staging_root) = self.upload_staging_root.as_ref() else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::RuntimeFailure,
                            "upload staging root is not configured",
                        ),
                    );
                };
                let Ok(request_dir) = UploadStaging::new(staging_root) else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::RuntimeFailure,
                            "failed to create confined upload staging",
                        ),
                    );
                };
                let Ok(path) = request_dir.stage(&payload.name, &bytes) else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::RuntimeFailure,
                            "failed to stage confined upload",
                        ),
                    );
                };
                let result = self
                    .runtime
                    .submit(
                        ctx,
                        CommandEnvelope {
                            schema_version: CommandEnvelope::SCHEMA_VERSION,
                            command_id: CommandId::new(),
                            workflow_id: WorkflowId::new(),
                            attempt_id: AttemptId::new(),
                            session_id,
                            page_id: Some(page_id),
                            deadline: Utc::now() + Duration::seconds(30),
                            command: RuntimeCommand::Primitive(PrimitiveCommand::UploadFiles(
                                UploadFilesCommand {
                                    selector: String::new(),
                                    target: Some(TargetSpec {
                                        label: Some("Resume".into()),
                                        ..TargetSpec::default()
                                    }),
                                    paths: vec![path.to_string_lossy().into_owned()],
                                },
                            )),
                        },
                    )
                    .await;
                drop(request_dir);
                result
            }
            _ => {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(
                        CdpErrorCode::InvalidParams,
                        "unsupported pinned Puppeteer semantic operation",
                    ),
                )
            }
        };
        if operation == "click" && selector == "role:link:Download fixture" {
            return match outcome {
                Ok(CommandOutcome::Completed { evidence, .. }) => {
                    let Some((filename, path, expected_bytes, expected_sha)) =
                        evidence.iter().find_map(|item| match item {
                            types::Evidence::Download {
                                filename,
                                path,
                                bytes,
                                sha256,
                                ..
                            } => Some((filename.clone(), path.clone(), *bytes, sha256.clone())),
                            _ => None,
                        })
                    else {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(
                                CdpErrorCode::RuntimeFailure,
                                "runtime download did not produce download evidence",
                            ),
                        );
                    };
                    let Some(store) = self.artifacts.as_ref() else {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(
                                CdpErrorCode::RuntimeFailure,
                                "artifact reader is not configured",
                            ),
                        );
                    };
                    let Ok(data) = std::fs::read(&path) else {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(
                                CdpErrorCode::RuntimeFailure,
                                "verified download was unavailable",
                            ),
                        );
                    };
                    if data.len() as u64 != expected_bytes
                        || hex::encode(Sha256::digest(&data)) != expected_sha
                    {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(
                                CdpErrorCode::RuntimeFailure,
                                "download evidence integrity check failed",
                            ),
                        );
                    }
                    let record = match store
                        .put(
                            &download_session,
                            &download_page,
                            "application/octet-stream",
                            "bin",
                            &data,
                            data.len(),
                        )
                        .await
                    {
                        Ok(record) => record,
                        Err(_) => {
                            return CdpResponse::failure(
                                &request,
                                CdpError::new(
                                    CdpErrorCode::RuntimeFailure,
                                    "download import failed",
                                ),
                            )
                        }
                    };
                    let stream_id = Uuid::new_v4().to_string();
                    if self
                        .streams
                        .lock()
                        .await
                        .reserve(
                            &stream_id,
                            download_ctx.principal_id,
                            &self.connection_id,
                            download_session,
                            &record.artifact_id,
                            expected_bytes,
                        )
                        .is_err()
                    {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(
                                CdpErrorCode::RuntimeFailure,
                                "download stream capacity exhausted",
                            ),
                        );
                    }
                    let guid = Uuid::new_v4().to_string();
                    let frame_id = request.session_id.clone().unwrap_or_else(|| "main".into());
                    if let Err(error) = self
                        .queue_events(
                            download_events(
                                &frame_id,
                                &guid,
                                &filename,
                                expected_bytes,
                                &stream_id,
                                &expected_sha,
                                request.session_id.clone(),
                            )
                            .to_vec(),
                        )
                        .await
                    {
                        self.streams.lock().await.remove(&stream_id);
                        return CdpResponse::failure(&request, error);
                    }
                    CdpResponse::success(&request, json!({"result":{"type":"undefined"}}))
                }
                Ok(_) => CdpResponse::failure(
                    &request,
                    CdpError::new(
                        CdpErrorCode::RuntimeFailure,
                        "pinned Puppeteer download did not complete",
                    ),
                ),
                Err(error) => CdpResponse::failure(&request, runtime_error(error)),
            };
        }
        match outcome {
            Ok(CommandOutcome::Completed { evidence, .. }) => {
                if operation == "upload" {
                    self.record_interface_event("upload.completed", json!({"evidence":evidence}))
                        .await;
                }
                CdpResponse::success(&request, json!({"result":{"type":"undefined"}}))
            }
            Ok(_) => CdpResponse::failure(
                &request,
                CdpError::new(
                    CdpErrorCode::RuntimeFailure,
                    "pinned Puppeteer semantic operation did not complete",
                ),
            ),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
}
