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
    "intent_solve_challenge",
    "intent_detect_challenge",
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
                let evidence_detail = input.evidence_detail.unwrap_or(EvidenceDetail::Compact);
                let field_names = input
                    .fields
                    .iter()
                    .map(|field| field.name.clone())
                    .collect::<Vec<_>>();
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
                self.submit_envelope(context, envelope)
                    .await
                    .map(|outcome| {
                        project_complete_form_outcome(outcome, evidence_detail, &field_names)
                    })
            }
            "intent_submit_and_verify" => {
                let input: IntentSubmitAndVerifyArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                // Boundary-once guard: a completed Boundary submit for this
                // workflow means its effect is on record, and another submit
                // would double-apply it. The failure that started the
                // fix-and-resubmit flow does NOT record here (only completed
                // outcomes do), so the legitimate rejected-then-corrected
                // flow stays open. `reSubmit` is the explicit escape hatch.
                if let Some(workflow_id) = &input.workflow_id {
                    if !input.re_submit.unwrap_or(false) {
                        if let Some(prior) = self.prior_boundary_execution(workflow_id).await {
                            return self
                                .boundary_already_executed_response(id, workflow_id, &prior)
                                .await;
                        }
                    }
                }
                let boundary_workflow_id = input.workflow_id.clone();
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
                let boundary_command_id = envelope.command_id.clone();
                // Keyed on the caller-supplied workflow id only: an omitted
                // workflowId resolves to the envelope default, which must
                // never become a collision point for unrelated ad-hoc
                // submits. No threaded workflow, no ledger protection (and
                // no guard) -- documented in the failure taxonomy.
                let result = if input.auto_checkpoint.unwrap_or(true) {
                    self.submit_envelope_with_auto_checkpoint(context, envelope)
                        .await
                } else {
                    self.submit_envelope(context, envelope).await
                };
                // Record the completed-or-possibly-landed Boundary submit so
                // a second one is refused. Semantics by outcome:
                // - completed: effect landed.
                // - needsReconciliation: effect may have landed; the caller
                //   must reconcile, never replay.
                // - failed + verificationFailed: the click landed but the
                //   expected state was not proven. Re-running would
                //   double-apply.
                // Everything else (resolution failures, actFailed,
                // retryableFailure, policyDenied) means nothing landed, so
                // the fix-and-resubmit flow stays open.
                if let Some(workflow_id) = boundary_workflow_id {
                    let landed = match &result {
                        Ok(value) => {
                            let status = value.get("status").and_then(Value::as_str);
                            let code = value
                                .get("error")
                                .and_then(|error| error.get("code"))
                                .and_then(Value::as_str);
                            matches!(status, Some("completed") | Some("needsReconciliation"))
                                || (matches!(status, Some("failed"))
                                    && code == Some("verificationFailed"))
                        }
                        Err(_) => false,
                    };
                    if landed {
                        self.record_boundary_execution(&workflow_id, &boundary_command_id)
                            .await;
                    }
                }
                result
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
            "intent_solve_challenge" => {
                let input: IntentSolveChallengeArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::SolveChallenge(types::SolveChallengeIntent {
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
            "intent_detect_challenge" => {
                let input: IntentDetectChallengeArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::DetectChallenge(types::DetectChallengeIntent {
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
            _ => unreachable!("dispatch_intents received a tool it does not own"),
        };
        self.finish_tool(id, result).await
    }
}

fn project_complete_form_outcome(
    mut outcome: Value,
    detail: EvidenceDetail,
    field_names: &[String],
) -> Value {
    if detail == EvidenceDetail::Full || outcome["status"] != "completed" {
        return outcome;
    }

    const MAX_SUMMARY_NAMES: usize = 8;
    let shown = field_names
        .iter()
        .take(MAX_SUMMARY_NAMES)
        .map(String::as_str)
        .collect::<Vec<_>>()
        .join(", ");
    let remainder = field_names.len().saturating_sub(MAX_SUMMARY_NAMES);
    let suffix = if remainder == 0 {
        String::new()
    } else {
        format!(", +{remainder} more")
    };
    let value = format!("filled {} fields: {shown}{suffix}", field_names.len());
    let revealed_controls = outcome["evidence"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|item| {
            item["kind"] == "controlAction"
                && item["action"]["revealedControls"]
                    .as_array()
                    .is_some_and(|controls| !controls.is_empty())
        })
        .cloned()
        .collect::<Vec<_>>();
    let mut evidence = vec![json!({
        "kind": "configuration",
        "name": "completeForm",
        "value": value,
    })];
    evidence.extend(revealed_controls);
    outcome["evidence"] = Value::Array(evidence);
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compact_complete_form_outcome_keeps_result_identity_and_summarizes_evidence() {
        let raw = json!({
            "status": "completed",
            "commandId": "018f0000-0000-7000-8000-000000000001",
            "evidence": [
                {"kind":"configuration","name":"completeFormField","value":"email"},
                {"kind":"resolution","target":{},"fingerprint":{},"candidates":[],"bestMatchAuthorized":false},
                {"kind":"controlAction","action":{}},
                {"kind":"intentExecution","record":{}}
            ]
        });

        let compact = project_complete_form_outcome(
            raw,
            EvidenceDetail::Compact,
            &["email".into(), "country".into()],
        );

        assert_eq!(compact["status"], "completed");
        assert_eq!(compact["commandId"], "018f0000-0000-7000-8000-000000000001");
        assert_eq!(
            compact["evidence"],
            json!([{
                "kind":"configuration",
                "name":"completeForm",
                "value":"filled 2 fields: email, country"
            }])
        );
    }

    #[test]
    fn compact_complete_form_outcome_preserves_revealed_conditional_controls() {
        let raw = json!({
            "status": "completed",
            "commandId": "018f0000-0000-7000-8000-000000000004",
            "evidence": [
                {
                    "kind":"controlAction",
                    "action":{
                        "operation":"selectOne",
                        "target":{"role":"combobox","accessibleName":"Plan","ordinal":null,"framePath":[],"shadowPath":[]},
                        "state":{"kind":"selection","values":["business"]},
                        "validity":{"willValidate":true,"valid":true,"flags":[],"message":null,"describedBy":[]},
                        "nodeReplaced":false,
                        "revealedControls":[{
                            "controlKind":"text",
                            "accessibleName":"Company name",
                            "target":{"role":"textbox","accessibleName":"Company name","ordinal":null,"framePath":[],"shadowPath":[]}
                        }]
                    }
                }
            ]
        });

        let compact = project_complete_form_outcome(raw, EvidenceDetail::Compact, &["plan".into()]);

        assert_eq!(compact["evidence"].as_array().expect("evidence").len(), 2);
        assert_eq!(compact["evidence"][0]["name"], "completeForm");
        assert_eq!(compact["evidence"][1]["kind"], "controlAction");
        assert_eq!(
            compact["evidence"][1]["action"]["revealedControls"][0]["accessibleName"],
            "Company name"
        );
    }

    #[test]
    fn compact_complete_form_outcome_preserves_failure_evidence_for_repair() {
        let raw = json!({
            "status": "failed",
            "commandId": "018f0000-0000-7000-8000-000000000002",
            "error": {"code":"targetNotFound"},
            "evidence": [{"kind":"configuration","name":"completeFormField","value":"email"}]
        });

        assert_eq!(
            project_complete_form_outcome(raw.clone(), EvidenceDetail::Compact, &["email".into()]),
            raw
        );
    }

    #[test]
    fn full_complete_form_outcome_preserves_all_success_evidence() {
        let raw = json!({
            "status": "completed",
            "commandId": "018f0000-0000-7000-8000-000000000003",
            "evidence": [{"kind":"intentExecution","record":{}}]
        });

        assert_eq!(
            project_complete_form_outcome(raw.clone(), EvidenceDetail::Full, &["email".into()]),
            raw
        );
    }
}
