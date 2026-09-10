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
        handle: Option<&str>,
        defaulted_handle: Option<&str>,
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
                self.submit_envelope(context, envelope, handle, call.name.as_str())
                    .await
            }
            "intent_fill" => {
                let input: IntentFillArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let mut hints = input.hints.unwrap_or_default();
                let control_id = hints
                    .accessible_name
                    .as_deref()
                    .filter(|name| looks_like_control_id(name))
                    .map(str::to_owned);
                if let Some(control_id) = control_id {
                    if hints_locator_empty_besides_name(&hints) {
                        if let Ok(snapshot) = self
                            .runtime
                            .form_snapshot(
                                context.clone(),
                                input.session_id.clone(),
                                input.page_id.clone(),
                                None,
                            )
                            .await
                        {
                            if let Some(target) =
                                control_target_from_snapshot(&snapshot, &control_id)
                            {
                                apply_control_target(&mut hints, target);
                            }
                        }
                    }
                }
                let intent = types::IntentCommand::Fill(types::FillIntent {
                    purpose: input.purpose,
                    hints,
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
                self.submit_envelope(context, envelope, handle, call.name.as_str())
                    .await
            }
            "intent_complete_form" => {
                let mut input: IntentCompleteFormArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                // `intent_fill`'s top-level `hints` shape is only unambiguous
                // here when there is exactly one field to apply it to, and
                // that field carries no hints of its own to be overwritten.
                // Anything else (no single target, or a conflicting field
                // hint already set) is rejected rather than guessed at.
                if let Some(hints) = input.hints.take() {
                    match input.fields.as_mut_slice() {
                        [field] if hints_are_empty(&field.hints) => field.hints = hints,
                        _ => return invalid_params_reason(id, "hintsPerField"),
                    }
                }
                let evidence_detail = input.evidence_detail.unwrap_or(EvidenceDetail::Compact);
                let field_names = input
                    .fields
                    .iter()
                    .map(|field| field.name.clone())
                    .collect::<Vec<_>>();
                // A field addressed by a form-snapshot controlId (as its bare
                // `name`, with no other hints) needs the control's real
                // target -- otherwise the compiler falls back to
                // `accessible_name = name`, and nothing on the page is
                // accessibly named "control-1". One snapshot serves every
                // qualifying field in the call; zero when none qualify.
                if input.fields.iter().any(|field| {
                    hints_are_empty(&field.hints) && looks_like_control_id(&field.name)
                }) {
                    if let Ok(snapshot) = self
                        .runtime
                        .form_snapshot(
                            context.clone(),
                            input.session_id.clone(),
                            input.page_id.clone(),
                            None,
                        )
                        .await
                    {
                        for field in &mut input.fields {
                            if hints_are_empty(&field.hints) && looks_like_control_id(&field.name) {
                                if let Some(target) =
                                    control_target_from_snapshot(&snapshot, &field.name)
                                {
                                    apply_control_target(&mut field.hints, target);
                                }
                            }
                        }
                    }
                }
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
                self.submit_envelope(context, envelope, handle, call.name.as_str())
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
                // Boundary-once guard, per (workflow, control): a completed
                // Boundary submit for this control means its effect is on
                // record, and another submit against it would double-apply.
                // One workflow legitimately holds several Boundary submits
                // against different controls (a search submit, then a save),
                // so the ledger never keys on the workflow alone. Submits
                // whose hints do not name a role + accessibleName have no
                // key at all: they are neither guarded nor recorded (a
                // purpose-text key would false-positive on rephrasings, and
                // keying them on the workflow alone is the search-vs-save
                // false positive this guard shipped with). The failure that
                // started the fix-and-resubmit flow does NOT record, so the
                // legitimate rejected-then-corrected flow stays open.
                // `reSubmit` is the explicit escape hatch.
                let boundary_key = input
                    .workflow_id
                    .clone()
                    .zip(control_identity(&input.hints.clone().unwrap_or_default()))
                    .map(|(workflow_id, control)| (workflow_id, Some(control)));
                if let Some(key) = &boundary_key {
                    if !input.re_submit.unwrap_or(false) {
                        if let Some(prior) = self.prior_boundary_execution(key).await {
                            return self
                                .boundary_already_executed_response(id, &key.0, &prior)
                                .await;
                        }
                    }
                }
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
                    self.submit_envelope_with_auto_checkpoint(context, envelope, handle)
                        .await
                } else {
                    self.submit_envelope(context, envelope, handle, call.name.as_str())
                        .await
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
                if let Some(boundary_key) = &boundary_key {
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
                        self.record_boundary_execution(boundary_key, &boundary_command_id)
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
                self.submit_envelope(context, envelope, handle, call.name.as_str())
                    .await
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
                    self.submit_envelope_with_auto_checkpoint(context, envelope, handle)
                        .await
                } else {
                    self.submit_envelope(context, envelope, handle, call.name.as_str())
                        .await
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
                self.submit_envelope(context, envelope, handle, call.name.as_str())
                    .await
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
                self.submit_envelope(context, envelope, handle, call.name.as_str())
                    .await
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
                self.submit_envelope(context, envelope, handle, call.name.as_str())
                    .await
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
                self.submit_envelope(context, envelope, handle, call.name.as_str())
                    .await
            }
            _ => unreachable!("dispatch_intents received a tool it does not own"),
        };
        self.finish_tool(id, result, defaulted_handle).await
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

    /// One schema-valid dummy per top-level intent property name. Shared across
    /// tools because the same name (`hints`, `purpose`, ...) means the same shape
    /// everywhere it appears at the top level of an intent call.
    fn intent_property_dummy(property: &str) -> Value {
        match property {
            "sessionId" => json!("10000000-0000-4000-8000-000000000001"),
            "pageId" => json!("10000000-0000-4000-8000-000000000002"),
            "workflowId" => json!("10000000-0000-4000-8000-000000000003"),
            "commandId" => json!("10000000-0000-4000-8000-000000000004"),
            "attemptId" => json!("10000000-0000-4000-8000-000000000005"),
            "idempotencyKey" => json!("parity-guard-idempotency-key"),
            "purpose" => json!("parity guard dummy purpose"),
            // Valid against `IntentHints` (every field optional) and against the
            // challenge intents' `{region?, timeoutMs?}` hints -- both accept `{}`.
            "hints" => json!({}),
            // `FillValue`/`ControlAction`'s simplest variant.
            "value" => json!({"kind": "clear"}),
            // One valid `ExtractField`; `intent_complete_form`'s `fields` (excluded
            // below) additionally requires each item's `value`.
            "fields" => json!([{"name": "field", "purpose": "parity guard field"}]),
            "evidenceDetail" => json!("compact"),
            "expectedState" | "expectedDestination" => json!({
                "condition": {"kind": "document", "ready": "commit"},
                "timeoutMs": 1000
            }),
            "autoCheckpoint" | "boundary" | "reSubmit" => json!(true),
            "condition" => json!({"kind": "document", "ready": "commit"}),
            "timeoutMs" => json!(1000),
            other => panic!(
                "intent_property_dummy: no dummy registered for property {other:?}; \
                 add one so the parity guard can cover it"
            ),
        }
    }

    /// Every intent tool's input schema must advertise only what its own parser
    /// (the `intent_args!` struct behind `bounded_parse`, `deny_unknown_fields`)
    /// accepts. A property present in the schema but absent from the struct
    /// validates fine and then fails to parse with `malformedArguments` --
    /// `intent_extract` did exactly that (schema advertised `hints`,
    /// `IntentExtractArgs` had no such field; `ExtractIntent` has no top-level
    /// hints to receive it either).
    ///
    /// `intent_complete_form` is excluded: PR #462 (open) is already changing
    /// `IntentCompleteFormArgs` and its dispatch arm, so this guard leaves that
    /// tool to it rather than asserting either the current mismatch or #462's
    /// still-unmerged fix.
    #[test]
    fn intent_schema_properties_all_parse_into_the_tool_args_struct() {
        let covered = TOOLS
            .iter()
            .copied()
            .filter(|name| *name != "intent_complete_form");
        for name in covered {
            let schema = crate::schema::tool_schema(name);
            let properties = schema["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{name}: schema properties is not an object"));
            let mut arguments = serde_json::Map::new();
            for key in properties.keys() {
                arguments.insert(key.clone(), intent_property_dummy(key));
            }
            let arguments = Value::Object(arguments);

            validate_tool_arguments(name, &arguments).unwrap_or_else(|violation| {
                panic!("{name}: {arguments} failed schema validation: {violation:?}")
            });

            let parsed: Result<(), ()> = match name {
                "intent_locate" => bounded_parse::<IntentLocateArgs>(arguments.clone()).map(drop),
                "intent_fill" => bounded_parse::<IntentFillArgs>(arguments.clone()).map(drop),
                "intent_submit_and_verify" => {
                    bounded_parse::<IntentSubmitAndVerifyArgs>(arguments.clone()).map(drop)
                }
                "intent_wait_for_state" => {
                    bounded_parse::<IntentWaitForStateArgs>(arguments.clone()).map(drop)
                }
                "intent_follow" => bounded_parse::<IntentFollowArgs>(arguments.clone()).map(drop),
                "intent_dismiss_obstruction" => {
                    bounded_parse::<IntentDismissObstructionArgs>(arguments.clone()).map(drop)
                }
                "intent_extract" => bounded_parse::<IntentExtractArgs>(arguments.clone()).map(drop),
                "intent_solve_challenge" => {
                    bounded_parse::<IntentSolveChallengeArgs>(arguments.clone()).map(drop)
                }
                "intent_detect_challenge" => {
                    bounded_parse::<IntentDetectChallengeArgs>(arguments.clone()).map(drop)
                }
                other => {
                    unreachable!("intent tool {other} missing from the parity guard's own dispatch")
                }
            };
            parsed.unwrap_or_else(|()| {
                let advertised: Vec<_> = properties.keys().collect();
                panic!(
                    "{name}: schema advertises {advertised:?} but bounded_parse into its \
                     args struct rejected a call setting every one of them"
                )
            });
        }
    }
}
