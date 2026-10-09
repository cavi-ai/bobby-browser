//! Reserved emulation domain handlers.
use super::super::*;

impl CdpConnection {
    pub(super) async fn dispatch_emulation(
        &self,
        request: CdpRequest,
        ctx: RequestContext,
        handler: Handler,
    ) -> CdpResponse {
        let result = match handler {
            Handler::EmulationSetFocus => {
                let Some(enabled) = request.params.get("enabled").and_then(Value::as_bool) else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "invalid focus emulation configuration",
                        ),
                    );
                };
                if request
                    .params
                    .as_object()
                    .is_none_or(|params| params.len() != 1)
                {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "invalid focus emulation configuration",
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
                let envelope = CommandEnvelope {
                    schema_version: CommandEnvelope::SCHEMA_VERSION,
                    command_id: CommandId::new(),
                    workflow_id: WorkflowId::new(),
                    attempt_id: AttemptId::new(),
                    session_id,
                    page_id: Some(page_id),
                    deadline: Utc::now() + Duration::seconds(30),
                    command: RuntimeCommand::Primitive(PrimitiveCommand::SetFocusEmulation(
                        SetFocusEmulationCommand { enabled },
                    )),
                };
                match self.runtime.submit(ctx, envelope).await {
                    Ok(CommandOutcome::Completed { evidence, .. }) if evidence.iter().any(|item| matches!(item, types::Evidence::Configuration { name, value } if name == "focusEmulation" && value == &enabled.to_string())) => Ok(json!({})),
                    Ok(_) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "focus emulation produced no verified evidence")),
                    Err(error) => Err(error),
                }
            }
            Handler::EmulationSetMedia => {
                let Some(media) = request
                    .params
                    .get("media")
                    .and_then(Value::as_str)
                    .filter(|value| value.len() <= 32)
                else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "invalid media emulation configuration",
                        ),
                    );
                };
                let Some(items) = request
                    .params
                    .get("features")
                    .and_then(Value::as_array)
                    .filter(|items| items.len() <= 16)
                else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "invalid media emulation configuration",
                        ),
                    );
                };
                if request
                    .params
                    .as_object()
                    .is_none_or(|params| params.len() != 2)
                {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "invalid media emulation configuration",
                        ),
                    );
                }
                let mut features = BTreeMap::new();
                for item in items {
                    let Some(name) = item
                        .get("name")
                        .and_then(Value::as_str)
                        .filter(|value| !value.is_empty() && value.len() <= 64)
                    else {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(CdpErrorCode::InvalidParams, "invalid media feature"),
                        );
                    };
                    let Some(value) = item
                        .get("value")
                        .and_then(Value::as_str)
                        .filter(|value| value.len() <= 64)
                    else {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(CdpErrorCode::InvalidParams, "invalid media feature"),
                        );
                    };
                    if item.as_object().is_none_or(|fields| fields.len() != 2)
                        || features.insert(name.to_owned(), value.to_owned()).is_some()
                    {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(CdpErrorCode::InvalidParams, "invalid media feature"),
                        );
                    }
                }
                let Some((session_id, page_id)) =
                    self.runtime_identity(request.session_id.as_deref()).await
                else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(CdpErrorCode::InvalidParams, "unknown runtime page"),
                    );
                };
                let command = SetEmulatedMediaCommand {
                    media: media.to_owned(),
                    features,
                };
                let expected =
                    serde_json::to_string(&command).expect("bounded media command serializes");
                let envelope = CommandEnvelope {
                    schema_version: CommandEnvelope::SCHEMA_VERSION,
                    command_id: CommandId::new(),
                    workflow_id: WorkflowId::new(),
                    attempt_id: AttemptId::new(),
                    session_id,
                    page_id: Some(page_id),
                    deadline: Utc::now() + Duration::seconds(30),
                    command: RuntimeCommand::Primitive(PrimitiveCommand::SetEmulatedMedia(command)),
                };
                match self.runtime.submit(ctx, envelope).await {
                    Ok(CommandOutcome::Completed { evidence, .. }) if evidence.iter().any(|item| matches!(item, types::Evidence::Configuration { name, value } if name == "emulatedMedia" && value == &expected)) => Ok(json!({})),
                    Ok(_) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "media emulation produced no verified evidence")),
                    Err(error) => Err(error),
                }
            }
            Handler::EmulationSetDeviceMetrics => {
                // Puppeteer applies its default viewport through this method on
                // every page it opens, so refusing it refuses the client. The
                // runtime's own emulation covers width, height, and the mobile
                // flag; a scale factor or orientation it cannot apply is
                // refused rather than silently ignored, which would leave the
                // client believing a viewport it never got.
                let params = request.params.as_object();
                let Some(params) = params.filter(|params| params.len() <= 8) else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "invalid device metrics configuration",
                        ),
                    );
                };
                if params.keys().any(|key| {
                    !matches!(
                        key.as_str(),
                        "width" | "height" | "deviceScaleFactor" | "mobile" | "screenOrientation"
                    )
                }) {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "unsupported device metrics field",
                        ),
                    );
                }
                let (Some(width), Some(height)) = (
                    params
                        .get("width")
                        .and_then(Value::as_u64)
                        .filter(|value| (1..=16384).contains(value)),
                    params
                        .get("height")
                        .and_then(Value::as_u64)
                        .filter(|value| (1..=16384).contains(value)),
                ) else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "viewport dimensions must be within 1..=16384",
                        ),
                    );
                };
                let mobile = params
                    .get("mobile")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if params
                    .get("deviceScaleFactor")
                    .and_then(Value::as_f64)
                    .is_some_and(|scale| scale != 1.0)
                {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "device scale factor emulation is unsupported; connect with deviceScaleFactor 1"));
                }
                if let Some(orientation) = params.get("screenOrientation") {
                    let angle = orientation
                        .get("angle")
                        .and_then(Value::as_i64)
                        .unwrap_or(0);
                    let kind = orientation
                        .get("type")
                        .and_then(Value::as_str)
                        .unwrap_or("portraitPrimary");
                    if angle != 0 || kind != "portraitPrimary" {
                        return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "screen orientation emulation is unsupported; only portraitPrimary at angle 0 is applied"));
                    }
                }
                let Some((session_id, page_id)) =
                    self.runtime_identity(request.session_id.as_deref()).await
                else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(CdpErrorCode::InvalidParams, "unknown runtime page"),
                    );
                };
                let viewport = types::ViewportSize {
                    width: width as u32,
                    height: height as u32,
                };
                let envelope = CommandEnvelope {
                    schema_version: CommandEnvelope::SCHEMA_VERSION,
                    command_id: CommandId::new(),
                    workflow_id: WorkflowId::new(),
                    attempt_id: AttemptId::new(),
                    session_id,
                    page_id: Some(page_id),
                    deadline: Utc::now() + Duration::seconds(30),
                    command: RuntimeCommand::Primitive(PrimitiveCommand::Emulate(
                        types::EmulateCommand {
                            viewport: Some(viewport),
                            geolocation: None,
                            mobile: Some(mobile),
                        },
                    )),
                };
                match self.runtime.submit(ctx, envelope).await {
                    Ok(CommandOutcome::Completed { evidence, .. }) if evidence.iter().any(|item| matches!(item, types::Evidence::Emulation { viewport: Some(applied), .. } if applied == &viewport)) => Ok(json!({})),
                    Ok(_) => return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "viewport emulation produced no verified evidence")),
                    Err(error) => Err(error),
                }
            }
            Handler::EmulationSetTouch => {
                // Puppeteer pairs this with the viewport above. The runtime has
                // no touch emulation, so `false` is answered truthfully as the
                // state that already holds, and `true` is refused instead of
                // being accepted as a no-op the client would trust.
                let Some(params) = request
                    .params
                    .as_object()
                    .filter(|params| params.len() <= 2)
                else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "invalid touch emulation configuration",
                        ),
                    );
                };
                if params
                    .keys()
                    .any(|key| !matches!(key.as_str(), "enabled" | "maxTouchPoints"))
                {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "unsupported touch emulation field",
                        ),
                    );
                }
                match params.get("enabled").and_then(Value::as_bool) {
                    Some(false) => Ok(json!({})),
                    Some(true) => {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(
                                CdpErrorCode::InvalidParams,
                                "touch emulation is unsupported; connect without hasTouch",
                            ),
                        )
                    }
                    None => {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(
                                CdpErrorCode::InvalidParams,
                                "invalid touch emulation configuration",
                            ),
                        )
                    }
                }
            }
            _ => unreachable!("domain routing is exhaustive"),
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
}
