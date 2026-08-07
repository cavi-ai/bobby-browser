//! Checkpoints, recovery, jobs, and events.
//!
//! Split out of the single 991-line `dispatch_named_tool` match. Arm
//! bodies are unchanged: each still returns the final response
//! directly on a malformed argument, so the early returns keep the
//! same meaning they had inside one function.

use super::*;

pub(super) const TOOLS: &[&str] = &[
    "checkpoint_save",
    "workflow_recover",
    "job_submit",
    "job_status",
    "job_cancel",
    "recovery_status",
    "events_read",
];

impl Server {
    pub(super) async fn dispatch_workflow(
        &self,
        id: Value,
        call: ToolCall,
        context: types::RequestContext,
    ) -> Value {
        let result = match call.name.as_str() {
            "checkpoint_save" => {
                let input: CheckpointSaveArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                // The caller names commands, not evidence: each id is resolved
                // against the runtime's journal before the checkpoint is
                // persisted. An id with no journal record, no terminal outcome,
                // or a different owner is rejected rather than silently
                // contributing nothing.
                match self
                    .runtime
                    .resolve_command_evidence(context.clone(), input.evidence_refs)
                    .await
                {
                    Ok(evidence) => self
                        .runtime
                        .checkpoint(context, input.checkpoint, evidence)
                        .await
                        .and_then(to_json),
                    Err(interface_error) => Err(interface_error),
                }
            }
            "workflow_recover" => {
                let input: WorkflowRecoverArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                self.runtime
                    .recover(context, input.workflow_id)
                    .await
                    .and_then(to_json)
            }
            "job_submit" => {
                let Some(jobs) = self.jobs.as_ref() else {
                    return error(id, METHOD_NOT_FOUND, "Method not found", None);
                };
                let input: JobSubmitArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let priority = match input.priority.as_deref() {
                    None => crate::jobs::JobPriorityWire::Normal,
                    Some(raw) => match crate::jobs::JobPriorityWire::parse(raw) {
                        Some(priority) => priority,
                        None => return invalid_params_reason(id, "malformedArguments"),
                    },
                };
                match jobs
                    .submit(
                        &context.principal_id,
                        crate::jobs::JobSubmission {
                            name: input.name,
                            payload: input.payload.unwrap_or(Value::Null),
                            priority,
                            max_retries: input.max_retries.unwrap_or(3),
                            timeout_ms: input.timeout_ms,
                            correlation_id: Some(context.correlation_id.as_uuid().to_string()),
                        },
                    )
                    .await
                {
                    Ok(outcome) => Ok(outcome.to_value()),
                    Err(error) => return job_port_error_response(id, error),
                }
            }
            "job_status" => {
                let Some(jobs) = self.jobs.as_ref() else {
                    return error(id, METHOD_NOT_FOUND, "Method not found", None);
                };
                let input: JobIdArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                match jobs.status(&context.principal_id, &input.job_id).await {
                    Ok(status) => Ok(status.to_value()),
                    Err(error) => return job_port_error_response(id, error),
                }
            }
            "job_cancel" => {
                let Some(jobs) = self.jobs.as_ref() else {
                    return error(id, METHOD_NOT_FOUND, "Method not found", None);
                };
                let input: JobIdArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                match jobs.cancel(&context.principal_id, &input.job_id).await {
                    Ok(()) => Ok(json!({ "cancelled": true, "jobId": input.job_id })),
                    Err(error) => return job_port_error_response(id, error),
                }
            }
            "recovery_status" => {
                let input: RecoveryStatusArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                // Exactly one key. Both would be two different questions in one
                // call, neither is not a question at all; the JSON Schema
                // subset this validator implements cannot say so, and a silent
                // precedence rule would answer a question the caller did not
                // ask.
                match (input.workflow_id, input.session_id) {
                    (Some(workflow_id), None) => self
                        .runtime
                        .recovery_status(context, workflow_id)
                        .await
                        .and_then(to_json),
                    (None, Some(session_id)) => {
                        let limit = input
                            .limit
                            .unwrap_or(MAX_RECOVERABLE_WORKFLOWS)
                            .clamp(1, MAX_RECOVERABLE_WORKFLOWS);
                        self.runtime
                            .workflows_for_session(context, session_id.clone(), limit)
                            .await
                            .map(|workflows| {
                                json!({"sessionId": session_id, "workflows": workflows})
                            })
                    }
                    _ => {
                        return invalid_params_reason(id, "exactlyOneOfWorkflowIdOrSessionId");
                    }
                }
            }
            "events_read" => {
                let input: EventsReadArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                if input.limit == 0 || input.limit > MAX_EVENT_LIMIT {
                    return invalid_params_reason(id, "malformedArguments");
                }
                if let Err(interface_error) = self
                    .authorization
                    .authorize(&context, types::InterfaceOperation::SubscribeEvents)
                {
                    return interface_error_response(id, interface_error);
                }
                let remaining = match (context.deadline - Utc::now()).to_std() {
                    Ok(remaining) => remaining,
                    Err(_) => return invalid_params_reason(id, "malformedArguments"),
                };
                match tokio::time::timeout(
                    remaining,
                    self.events.read_after_for(
                        &context.principal_id,
                        input.cursor.into(),
                        input.limit,
                    ),
                )
                .await
                {
                    Ok(Ok(batch)) => to_json(batch),
                    Ok(Err(gap)) => {
                        return error(
                            id,
                            INTERFACE_ERROR,
                            "Runtime interface error",
                            Some(json!({"eventGap": gap})),
                        )
                    }
                    Err(_) => {
                        return error(
                            id,
                            INTERFACE_ERROR,
                            "Runtime interface error",
                            Some(json!({
                                "interfaceError": {
                                    "code":"deadlineExceeded",
                                    "layer":"interface",
                                    "message":"runtime interface request failed",
                                    "correlationId":context.correlation_id,
                                    "commandId":null,
                                    "retryable":false,
                                    "retryAfterMs":null,
                                    "reconciliationRequired":false,
                                    "requiredCapability":null
                                }
                            })),
                        )
                    }
                }
            }
            _ => unreachable!("dispatch_workflow received a tool it does not own"),
        };
        self.finish_tool(id, result).await
    }
}
