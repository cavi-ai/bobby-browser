//! Verified semantic operation behind reserved CDP admission.
use super::super::super::*;
impl CdpConnection {
    pub(super) async fn playwright_fill(
        &self,
        request: &CdpRequest,
        ctx: RequestContext,
        handle: &str,
        serialized: &Value,
    ) -> CdpResponse {
        let Some(value) =
            find_serialized_string(serialized, "value").filter(|value| value.len() <= 64 * 1024)
        else {
            return CdpResponse::failure(
                request,
                CdpError::new(CdpErrorCode::InvalidParams, "missing bounded fill value"),
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
        let descriptor = handle.trim_start_matches("semantic-element:");
        let Some(label) = descriptor.strip_prefix("label:") else {
            return CdpResponse::failure(
                request,
                CdpError::new(
                    CdpErrorCode::InvalidParams,
                    "fill requires a verified labeled control",
                ),
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
            command: RuntimeCommand::Primitive(PrimitiveCommand::TypeText(TypeTextCommand {
                selector: String::new(),
                target: Some(TargetSpec {
                    label: Some(label.to_owned()),
                    ..TargetSpec::default()
                }),
                value: value.to_owned(),
                clear_first: true,
                expected_url: None,
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
                    "runtime fill did not complete",
                ),
            ),
            Err(error) => CdpResponse::failure(request, runtime_error(error)),
        }
    }
}
