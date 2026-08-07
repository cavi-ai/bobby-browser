//! Purpose-driven intents.
//!
//! Split out of the single 991-line `dispatch_named_tool` match. Arm
//! bodies are unchanged: each still returns the final response
//! directly on a malformed argument, so the early returns keep the
//! same meaning they had inside one function.

use super::*;

pub(super) const TOOLS: &[&str] = &[
    "intent_locate",
    "intent_fill",
    "intent_complete_form",
    "intent_submit_and_verify",
    "intent_wait_for_state",
    "intent_follow",
    "intent_dismiss_obstruction",
    "intent_extract",
];

impl Server {
    pub(super) async fn dispatch_intents(
        &self,
        id: Value,
        call: ToolCall,
        mut context: types::RequestContext,
    ) -> Value {
        let result = match call.name.as_str() {
            "intent_locate" => {
                let input: IntentLocateArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::Locate(types::LocateIntent {
                    purpose: input.purpose,
                    hints: input.hints.unwrap_or_default(),
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "intent_fill" => {
                let input: IntentFillArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::Fill(types::FillIntent {
                    purpose: input.purpose,
                    hints: input.hints.unwrap_or_default(),
                    value: input.value,
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "intent_complete_form" => {
                let input: IntentCompleteFormArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::CompleteForm(types::CompleteFormIntent {
                    purpose: input.purpose,
                    fields: input.fields,
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "intent_submit_and_verify" => {
                let input: IntentSubmitAndVerifyArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::SubmitAndVerify(types::SubmitAndVerifyIntent {
                    purpose: input.purpose,
                    hints: input.hints.unwrap_or_default(),
                    expected_state: input.expected_state,
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                if input.auto_checkpoint.unwrap_or(true) {
                    self.submit_envelope_with_auto_checkpoint(context, envelope)
                        .await
                } else {
                    self.submit_envelope(context, envelope).await
                }
            }
            "intent_wait_for_state" => {
                let input: IntentWaitForStateArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::WaitForState(types::WaitForStateIntent {
                    condition: input.condition,
                    timeout_ms: input.timeout_ms,
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "intent_follow" => {
                let input: IntentFollowArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::Follow(types::FollowIntent {
                    purpose: input.purpose,
                    hints: input.hints.unwrap_or_default(),
                    expected_destination: input.expected_destination,
                    boundary: input.boundary.unwrap_or(false),
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                if input.auto_checkpoint.unwrap_or(true) {
                    self.submit_envelope_with_auto_checkpoint(context, envelope)
                        .await
                } else {
                    self.submit_envelope(context, envelope).await
                }
            }
            "intent_dismiss_obstruction" => {
                let input: IntentDismissObstructionArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent =
                    types::IntentCommand::DismissObstruction(types::DismissObstructionIntent {
                        purpose: input.purpose,
                        hints: input.hints.unwrap_or_default(),
                        timeout_ms: input
                            .timeout_ms
                            .unwrap_or(types::DEFAULT_DISMISS_OBSTRUCTION_TIMEOUT_MS),
                    });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "intent_extract" => {
                let input: IntentExtractArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::Extract(types::ExtractIntent {
                    purpose: input.purpose,
                    fields: input.fields,
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            _ => unreachable!("dispatch_intents received a tool it does not own"),
        };
        self.finish_tool(id, result).await
    }
}
