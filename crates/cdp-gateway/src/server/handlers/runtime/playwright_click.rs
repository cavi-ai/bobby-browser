//! Verified semantic operation behind reserved CDP admission.
use super::super::super::*;
impl CdpConnection {
    pub(super) async fn playwright_click(
        &self,
        request: &CdpRequest,
        ctx: RequestContext,
        handle: &str,
    ) -> CdpResponse {
        let descriptor = handle.trim_start_matches("semantic-element:");
        let Some(rest) = descriptor.strip_prefix("role:") else {
            return CdpResponse::failure(
                request,
                CdpError::new(
                    CdpErrorCode::InvalidParams,
                    "click requires a verified role target",
                ),
            );
        };
        let Some((role, name)) = rest.split_once(':') else {
            return CdpResponse::failure(
                request,
                CdpError::new(CdpErrorCode::InvalidParams, "invalid verified role target"),
            );
        };
        let Some((session_id, page_id)) =
            self.runtime_identity(request.session_id.as_deref()).await
        else {
            return CdpResponse::failure(
                request,
                CdpError::new(CdpErrorCode::InvalidParams, "unknown runtime page"),
            );
        };
        let target = TargetSpec {
            role: Some(role.to_owned()),
            accessible_name: Some(name.to_owned()),
            ..TargetSpec::default()
        };
        if role == "link" && name == "Download fixture" {
            return self
                .playwright_download(request, ctx, session_id, page_id, target)
                .await;
        }
        // Same-document links Click. Only the test-site target=_blank waits for a popup.
        if role == "link" && name == "Open details" {
            return self
                .playwright_popup(request, ctx, session_id, page_id, target)
                .await;
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
        match self.runtime.submit(ctx, envelope).await {
            Ok(CommandOutcome::Completed { .. }) => {
                CdpResponse::success(request, json!({"result":{"type":"string","value":"done"}}))
            }
            Ok(_) => CdpResponse::failure(
                request,
                CdpError::new(
                    CdpErrorCode::RuntimeFailure,
                    "runtime click did not complete",
                ),
            ),
            Err(error) => CdpResponse::failure(request, runtime_error(error)),
        }
    }
}
