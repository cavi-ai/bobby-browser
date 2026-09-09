//! Raw primitives and the envelope escape hatch.
//!
//! Split out of the single 991-line `dispatch_named_tool` match. Arm
//! bodies are unchanged: each still returns the final response
//! directly on a malformed argument, so the early returns keep the
//! same meaning they had inside one function.

use super::*;

pub(super) const TOOLS: &[&str] = &[
    "command_execute",
    "navigate",
    "click",
    "click_and_wait_for_popup",
    "type_text",
    "inspect",
    "screenshot",
    "wait_for",
];

impl Server {
    pub(super) async fn dispatch_primitives(
        &self,
        id: Value,
        call: ToolCall,
        mut context: types::RequestContext,
        handle: Option<&str>,
    ) -> Value {
        let result = match call.name.as_str() {
            "command_execute" => {
                let input: CommandExecuteArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let now = Utc::now();
                if input.envelope.deadline <= now
                    || input.envelope.deadline
                        > now + Duration::milliseconds(MAX_COMMAND_DEADLINE_MS)
                {
                    return invalid_params_reason(id, "deadlineOutOfRange");
                }
                context.deadline = input.envelope.deadline;
                context.idempotency_key = match input.idempotency_key {
                    Some(key) => match types::IdempotencyKey::try_from(key) {
                        Ok(key) => Some(key),
                        Err(_) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                    },
                    None => None,
                };
                self.submit_envelope(context, input.envelope, handle, call.name.as_str())
                    .await
            }
            "navigate" => {
                let input: NavigateArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::Navigate(types::NavigateCommand {
                        url: input.url,
                        wait_until: input.wait_until.unwrap_or(types::WaitUntil::Interactive),
                        timeout_ms: input.timeout_ms.unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS),
                    }),
                );
                self.submit_envelope(context, envelope, handle, call.name.as_str())
                    .await
            }
            "click" => {
                let input: ClickArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, mut envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::Click(types::ClickCommand {
                        selector: input.selector.unwrap_or_default(),
                        target: input.target,
                        boundary: input.boundary.unwrap_or(false),
                        expected_url: input.expected_url,
                        modifiers: input.modifiers,
                    }),
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                if input.boundary.unwrap_or(false) && input.auto_checkpoint.unwrap_or(true) {
                    self.submit_envelope_with_auto_checkpoint(context, envelope, handle)
                        .await
                } else {
                    self.submit_envelope(context, envelope, handle, call.name.as_str())
                        .await
                }
            }
            "click_and_wait_for_popup" => {
                let input: ClickAndWaitForPopupArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, mut envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::ClickAndWaitForPopup(
                        types::ClickAndWaitForPopupCommand {
                            selector: input.selector.unwrap_or_default(),
                            target: input.target,
                            timeout_ms: input.timeout_ms.unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS),
                        },
                    ),
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                let result = if input.auto_checkpoint.unwrap_or(true) {
                    self.submit_envelope_with_auto_checkpoint(context, envelope, handle)
                        .await
                } else {
                    self.submit_envelope(context, envelope, handle, call.name.as_str())
                        .await
                };
                // A successful follow through a handle rebinds it onto the
                // popup: later handle-resolved calls land there, and the
                // closed-page rule can return to this opener once it closes.
                // Raw-id calls (`handle: None`) are never touched.
                if let (Some(handle), Ok(value)) = (handle, &result) {
                    if let Some((opener_page_id, popup_page_id)) = completed_popup_evidence(value) {
                        self.workflow_handles
                            .rebind_popup(handle, popup_page_id, opener_page_id);
                    }
                }
                result
            }
            "type_text" => {
                let input: TypeTextArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::TypeText(types::TypeTextCommand {
                        selector: input.selector.unwrap_or_default(),
                        target: input.target,
                        value: input.value,
                        clear_first: input.clear_first.unwrap_or(false),
                        expected_url: input.expected_url,
                    }),
                );
                self.submit_envelope(context, envelope, handle, call.name.as_str())
                    .await
            }
            "inspect" => {
                let input: InspectArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::Inspect(types::InspectCommand {
                        selector: input.selector,
                        target: input.target,
                        include_html: input.include_html.unwrap_or(false),
                    }),
                );
                self.submit_envelope(context, envelope, handle, call.name.as_str())
                    .await
            }
            "screenshot" => {
                let input: ScreenshotArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::CaptureScreenshot(types::CaptureScreenshotCommand {
                        mode: input.mode.unwrap_or(types::ScreenshotMode::Viewport),
                    }),
                );
                self.submit_envelope(context, envelope, handle, call.name.as_str())
                    .await
            }
            "wait_for" => {
                let input: WaitForArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::WaitFor(types::WaitForCommand {
                        condition: input.condition,
                        timeout_ms: input.timeout_ms,
                    }),
                );
                self.submit_envelope(context, envelope, handle, call.name.as_str())
                    .await
            }
            _ => unreachable!("dispatch_primitives received a tool it does not own"),
        };
        self.finish_tool(id, result).await
    }
}

/// The `(openerPageId, pageId)` pair out of a completed
/// `click_and_wait_for_popup`'s `Evidence::Popup`. `None` for anything else
/// -- a failed call, or a completed one carrying no popup evidence (which
/// `submit_envelope`'s `verificationFailed` guard should have already ruled
/// out, but this stays defensive rather than assume that guard forever).
fn completed_popup_evidence(value: &Value) -> Option<(types::PageId, types::PageId)> {
    if value.get("status").and_then(Value::as_str) != Some("completed") {
        return None;
    }
    let popup = value
        .get("evidence")?
        .as_array()?
        .iter()
        .find(|item| item.get("kind").and_then(Value::as_str) == Some("popup"))?;
    let opener_page_id = serde_json::from_value(popup.get("openerPageId")?.clone()).ok()?;
    let popup_page_id = serde_json::from_value(popup.get("pageId")?.clone()).ok()?;
    Some((opener_page_id, popup_page_id))
}
