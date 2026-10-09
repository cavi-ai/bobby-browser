//! Pinned Puppeteer semantic-call admission and routing.
use super::super::super::*;
impl CdpConnection {
    pub(super) async fn handle_puppeteer_semantic(
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
                return self
                    .puppeteer_upload(&request, ctx, session_id, page_id, value)
                    .await;
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
            return self
                .puppeteer_download(
                    &request,
                    download_ctx,
                    download_session,
                    download_page,
                    outcome,
                )
                .await;
        }
        match outcome {
            Ok(CommandOutcome::Completed { .. }) => {
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
