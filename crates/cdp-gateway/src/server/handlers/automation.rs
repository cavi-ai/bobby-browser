//! Reserved automation domain handlers.
use super::super::*;

impl CdpConnection {
    pub(super) async fn dispatch_automation(
        &self,
        request: CdpRequest,
        ctx: RequestContext,
        handler: Handler,
    ) -> CdpResponse {
        let result = match handler {
            Handler::AutomationCheckpointSave => {
                if !request
                    .params
                    .as_object()
                    .is_some_and(serde_json::Map::is_empty)
                {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "Automation.checkpointSave takes no parameters",
                        ),
                    );
                }
                let Some((session_id, page_id)) = self
                    .automation_runtime_identity(request.session_id.as_deref(), ctx.clone())
                    .await
                else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "checkpoint requires a runtime page session",
                        ),
                    );
                };
                {
                    let mut state = self.automation_boundary.lock().await;
                    if state.as_ref().is_some_and(|pending| {
                        pending.phase == AutomationBoundaryPhase::Pending
                            && pending.expires_at > Utc::now()
                    }) {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(
                                CdpErrorCode::RuntimeFailure,
                                "a browser boundary is already reserved",
                            ),
                        );
                    }
                    *state = None;
                }
                let workflow_id = WorkflowId::new();
                let attempt_id = AttemptId::new();
                let inspect_id = CommandId::new();
                let command_id = CommandId::new();
                let evidence = match self
                    .runtime
                    .submit(
                        ctx.clone(),
                        CommandEnvelope {
                            schema_version: CommandEnvelope::SCHEMA_VERSION,
                            command_id: inspect_id.clone(),
                            workflow_id: workflow_id.clone(),
                            attempt_id: attempt_id.clone(),
                            session_id: session_id.clone(),
                            page_id: Some(page_id.clone()),
                            deadline: Utc::now() + Duration::seconds(30),
                            command: RuntimeCommand::Primitive(PrimitiveCommand::Inspect(
                                InspectCommand::default(),
                            )),
                        },
                    )
                    .await
                {
                    Ok(CommandOutcome::Completed { evidence, .. }) => evidence,
                    Ok(_) => {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(
                                CdpErrorCode::RuntimeFailure,
                                "checkpoint inspection did not complete",
                            ),
                        )
                    }
                    Err(error) => return CdpResponse::failure(&request, runtime_error(error)),
                };
                let Some((url, title)) = evidence.iter().find_map(|item| {
                    if let types::Evidence::Inspection { url, title, .. } = item {
                        Some((url.clone(), title.clone()))
                    } else {
                        None
                    }
                }) else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::RuntimeFailure,
                            "checkpoint inspection lacked verified state",
                        ),
                    );
                };
                let checkpoint_id = CheckpointId::new();
                let checkpoint = WorkflowCheckpoint {
                    schema_version: 1,
                    checkpoint_id: checkpoint_id.clone(),
                    workflow_id: workflow_id.clone(),
                    attempt_id: attempt_id.clone(),
                    session_id: session_id.clone(),
                    page_id: page_id.clone(),
                    restart_url: url.clone(),
                    current_url: url.clone(),
                    cursor: Some(inspect_id),
                    boundary_command_id: Some(command_id.clone()),
                    recovery_class: CommandClass::Boundary,
                    invariants: vec![
                        CheckpointInvariant::Url { value: url },
                        CheckpointInvariant::Title { value: title },
                    ],
                    replayable_inputs: vec![],
                    evidence: evidence.clone(),
                    recovery_history: vec![],
                    recovery_receipts: vec![],
                    created_at: Utc::now(),
                };
                match self.runtime.checkpoint(ctx, checkpoint, evidence).await {
                    Ok(saved) => {
                        *self.automation_boundary.lock().await = Some(AutomationBoundary {
                            workflow_id: workflow_id.clone(),
                            attempt_id,
                            command_id: command_id.clone(),
                            checkpoint_id: checkpoint_id.clone(),
                            session_id,
                            page_id,
                            expires_at: Utc::now() + Duration::seconds(30),
                            phase: AutomationBoundaryPhase::Pending,
                        });
                        self.record_interface_event(
                            "checkpoint.saved",
                            serde_json::to_value(&saved).unwrap_or(Value::Null),
                        )
                        .await;
                        Ok(
                            json!({"checkpointId":checkpoint_id,"workflowId":workflow_id,"boundaryCommandId":command_id,"boundary":"boundary"}),
                        )
                    }
                    Err(error) => Err(error),
                }
            }
            Handler::AutomationRecoveryInspect => {
                if !request
                    .params
                    .as_object()
                    .is_some_and(serde_json::Map::is_empty)
                {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "Automation.recoveryInspect takes no parameters",
                        ),
                    );
                }
                let Some(boundary) = self
                    .automation_boundary
                    .lock()
                    .await
                    .clone()
                    .filter(|state| state.phase == AutomationBoundaryPhase::Consumed)
                else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(CdpErrorCode::RuntimeFailure, "no consumed browser boundary"),
                    );
                };
                match self
                    .runtime
                    .recover(ctx, boundary.workflow_id.clone())
                    .await
                {
                    Ok(decision) => {
                        let (status, replayed, observed_checkpoint) = match decision {
                            RecoveryDecision::Resumed { checkpoint_id, .. } => {
                                ("resumed", false, checkpoint_id)
                            }
                            RecoveryDecision::NeedsReconciliation { checkpoint_id, .. } => {
                                ("needsReconciliation", false, checkpoint_id)
                            }
                            RecoveryDecision::Restarted { checkpoint_id, .. } => {
                                ("restarted", true, checkpoint_id)
                            }
                        };
                        if observed_checkpoint != boundary.checkpoint_id {
                            return CdpResponse::failure(
                                &request,
                                CdpError::new(
                                    CdpErrorCode::RuntimeFailure,
                                    "recovery checkpoint lineage changed",
                                ),
                            );
                        }
                        let response = json!({"status":status,"checkpointId":observed_checkpoint,"workflowId":boundary.workflow_id,"boundaryCommandId":boundary.command_id,"boundary":"boundary","replayed":replayed});
                        if let Some(state) = self.automation_boundary.lock().await.as_mut() {
                            state.phase = AutomationBoundaryPhase::Recovered;
                        }
                        self.record_interface_event("recovery.inspected", response.clone())
                            .await;
                        Ok(response)
                    }
                    Err(error) => Err(error),
                }
            }
            Handler::AutomationEventsRead => {
                let Some(params) = request.params.as_object() else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "Automation.eventsRead requires object parameters",
                        ),
                    );
                };
                let cursor = params.get("cursor").and_then(Value::as_u64).unwrap_or(0);
                if params.keys().any(|key| key != "cursor") {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "Automation.eventsRead accepts only cursor",
                        ),
                    );
                }
                // No read receipt is recorded: journaling every poll would
                // flood the bounded event store with the client's own reads
                // and evict the real events it is trying to fetch.
                match self
                    .interface_events
                    .read_after_for(self.handle.principal_id(), types::EventCursor(cursor), 64)
                    .await
                {
                    Ok(batch) => Ok(serde_json::to_value(batch).unwrap_or(Value::Null)),
                    Err(_) => {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(
                                CdpErrorCode::RuntimeFailure,
                                "browser interface event history gap",
                            ),
                        )
                    }
                }
            }
            Handler::AutomationProtocolInventory => {
                if !request
                    .params
                    .as_object()
                    .is_some_and(serde_json::Map::is_empty)
                {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "Automation.protocolInventory takes no parameters",
                        ),
                    );
                }
                Ok(json!({
                    "methods":self.observed_methods.lock().await.iter().cloned().collect::<Vec<_>>(),
                    "events":self.observed_events.lock().await.iter().cloned().collect::<Vec<_>>(),
                }))
            }
            _ => unreachable!("domain routing is exhaustive"),
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
}
