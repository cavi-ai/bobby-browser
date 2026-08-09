//! Page inspection, artifacts, cookies, and evaluation.
//!
//! Split out of the single 991-line `dispatch_named_tool` match. Arm
//! bodies are unchanged: each still returns the final response
//! directly on a malformed argument, so the early returns keep the
//! same meaning they had inside one function.

use super::*;

pub(super) const TOOLS: &[&str] = &[
    "page_list",
    "page_close",
    "page_activate",
    "a11y_snapshot",
    "context_ask",
    "context_neighbors",
    "form_snapshot",
    "control_action",
    "network_log",
    "emulate",
    "dialog",
    "pdf",
    "cookie_get",
    "cookie_set",
    "cookie_delete",
    "extract_structured",
    "download_url",
    "upload_files",
    "evaluate_javascript",
];

impl Server {
    pub(super) async fn dispatch_page_ops(
        &self,
        id: Value,
        call: ToolCall,
        context: types::RequestContext,
    ) -> Value {
        let result = match call.name.as_str() {
            "page_list" => {
                let input: PageListArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    None,
                    input.workflow_id,
                    types::PrimitiveCommand::ListPages(types::ListPagesCommand),
                );
                self.submit_envelope(context, envelope).await
            }
            "page_close" => {
                let input: PageCloseArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let session_id = input.session_id;
                let page_id = input.page_id;
                let (context, envelope) = primitive_envelope(
                    context,
                    session_id.clone(),
                    Some(page_id.clone()),
                    input.workflow_id,
                    types::PrimitiveCommand::ClosePage(types::ClosePageCommand {
                        page_id: page_id.clone(),
                    }),
                );
                let result = self.submit_envelope(context, envelope).await;
                if result
                    .as_ref()
                    .is_ok_and(|outcome| outcome["status"] == "completed")
                {
                    self.workflow_handles.remove_page(&session_id, &page_id);
                }
                result
            }
            "page_activate" => {
                let input: PageCloseArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let page_id = input.page_id;
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(page_id.clone()),
                    input.workflow_id,
                    types::PrimitiveCommand::ActivatePage(types::ActivatePageCommand { page_id }),
                );
                self.submit_envelope(context, envelope).await
            }
            "a11y_snapshot" => {
                let input: A11ySnapshotArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::AccessibilitySnapshot(
                        types::AccessibilitySnapshotCommand {
                            max_nodes: input.max_nodes,
                        },
                    ),
                );
                self.submit_envelope(context, envelope).await
            }
            "context_ask" => {
                let input: ContextAskArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                self.runtime
                    .context_ask(context, input.session_id, input.page_id, input.description)
                    .await
                    // `None` is an answer, not a failure: the context does not
                    // know and the repair is to snapshot. An error here would
                    // be indistinguishable from a broken call.
                    .and_then(|answer| to_json(json!({"answer": answer})))
            }
            "context_neighbors" => {
                let input: ContextNeighborsArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                self.runtime
                    .context_neighbors(context, input.session_id, input.page_id, input.description)
                    .await
                    // Like context_ask: `None` is an answer, not a failure.
                    .and_then(|neighbors| to_json(json!({"neighbors": neighbors})))
            }
            "form_snapshot" => {
                let input: FormSnapshotArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                if input
                    .max_controls
                    .is_some_and(|limit| !(1..=512).contains(&limit))
                {
                    return invalid_params_reason(id, "malformedArguments");
                }
                self.runtime
                    .form_snapshot(context, input.session_id, input.page_id, input.max_controls)
                    .await
                    .and_then(to_json)
            }
            "control_action" => {
                let input: ControlActionArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                if input.action.validate().is_err() {
                    return invalid_params_reason(id, "malformedArguments");
                }
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::ControlAction(types::ControlActionCommand {
                        target: input.target,
                        action: input.action,
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "network_log" => {
                let input: NetworkLogArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::NetworkLog(
                        types::NetworkLogCommand {
                            clear: input.clear.unwrap_or(true),
                        },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "emulate" => {
                let input: EmulateArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::Emulate(
                        types::EmulateCommand {
                            viewport: input.viewport,
                            geolocation: input.geolocation,
                            mobile: input.mobile,
                        },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "dialog" => {
                let input: DialogArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::HandleDialog(
                        types::HandleDialogCommand {
                            action: input.action,
                            timeout_ms: input.timeout_ms,
                        },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "pdf" => {
                let input: PdfArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::PrintToPdf(
                        types::PrintToPdfCommand {
                            landscape: input.landscape,
                            print_background: input.print_background.unwrap_or(true),
                            scale: input.scale,
                            page_ranges: input.page_ranges,
                        },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "cookie_get" => {
                let input: CookieGetArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::GetCookies(
                        types::GetCookiesCommand { urls: input.urls },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "cookie_set" => {
                let input: CookieSetArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::SetCookies(
                        types::SetCookiesCommand {
                            cookies: input.cookies,
                        },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "cookie_delete" => {
                let input: CookieDeleteArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::DeleteCookies(
                        types::DeleteCookiesCommand {
                            urls: input.urls,
                            names: input.names,
                        },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "extract_structured" => {
                let input: ExtractStructuredArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::ExtractStructured(types::ExtractStructuredCommand {
                        schema: input.schema,
                        purpose: input.purpose,
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "download_url" => {
                let input: DownloadUrlArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::DownloadUrl(types::DownloadUrlCommand {
                        url: input.url,
                        expected_content_type: input.expected_content_type,
                        max_bytes: input.max_bytes,
                        save_as: input.save_as,
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "upload_files" => {
                let input: UploadFilesArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::UploadFiles(types::UploadFilesCommand {
                        selector: input.selector.unwrap_or_default(),
                        target: input.target,
                        paths: input.paths,
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "evaluate_javascript" => {
                let input: EvaluateJavaScriptArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::EvaluateJavaScript(types::EvaluateJavaScriptCommand {
                        expression: input.expression,
                        timeout_ms: input.timeout_ms.unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS),
                        await_promise: input.await_promise.unwrap_or(false),
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            _ => unreachable!("dispatch_page_ops received a tool it does not own"),
        };
        self.finish_tool(id, result).await
    }
}
