//! Verified semantic operation behind reserved CDP admission.
use super::super::super::*;
impl CdpConnection {
    pub(super) async fn playwright_locate(
        &self,
        request: &CdpRequest,
        ctx: RequestContext,
        serialized: &Value,
    ) -> CdpResponse {
        let result = {
            let engine = find_serialized_string(serialized, "name");
            let body = find_serialized_string(serialized, "body");
            let semantic = match (engine, body) {
                (Some("internal:label"), Some(body)) => body
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix("\"i"))
                    .filter(|v| !v.is_empty() && v.len() <= 256)
                    .map(|label| {
                        (
                            format!("label:{label}"),
                            TargetSpec {
                                label: Some(label.to_owned()),
                                allow_best_match: true,
                                ordinal: Some(0),
                                ..TargetSpec::default()
                            },
                        )
                    }),
                (Some("internal:role"), Some(body)) => parse_role_target(body),
                (Some("internal:text"), Some(body)) => body
                    .strip_prefix('"')
                    .and_then(|v| v.strip_suffix("\"i"))
                    .filter(|v| !v.is_empty() && v.len() <= 1024)
                    .map(|text| {
                        (
                            format!("text:{text}"),
                            TargetSpec {
                                text: Some(TextMatch::Contains(text.to_owned())),
                                allow_best_match: true,
                                ordinal: Some(0),
                                ..TargetSpec::default()
                            },
                        )
                    }),
                _ => None,
            };
            let Some((descriptor, target)) = semantic else {
                return CdpResponse::success(
                    request,
                    domains::runtime::evaluate_exception_result(
                        "unsupported semantic runtime call",
                    ),
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
            let envelope = CommandEnvelope {
                schema_version: CommandEnvelope::SCHEMA_VERSION,
                command_id: CommandId::new(),
                workflow_id: WorkflowId::new(),
                attempt_id: AttemptId::new(),
                session_id,
                page_id: Some(page_id),
                deadline: Utc::now() + Duration::seconds(30),
                command: RuntimeCommand::Primitive(PrimitiveCommand::Inspect(InspectCommand {
                    selector: None,
                    target: Some(target),
                    include_html: false,
                })),
            };
            match self.runtime.submit(ctx, envelope).await {
                Ok(CommandOutcome::Completed { evidence, .. })
                    if evidence.iter().any(|item| {
                        matches!(
                            item,
                            types::Evidence::Element { .. } | types::Evidence::Inspection { .. }
                        )
                    }) =>
                {
                    let object_id = match self
                        .issue_remote_object(
                            request.session_id.as_deref(),
                            &format!("semantic-locator:{}", descriptor),
                        )
                        .await
                    {
                        Ok(object_id) => object_id,
                        Err(error) => return CdpResponse::failure(request, error),
                    };
                    Ok(
                        json!({"result":{"type":"object","subtype":"object","className":"Object","description":"Object","objectId":object_id}}),
                    )
                }
                Ok(_) => {
                    return CdpResponse::failure(
                        request,
                        CdpError::new(
                            CdpErrorCode::RuntimeFailure,
                            "semantic target was not verified",
                        ),
                    )
                }
                Err(error) => Err(error),
            }
        };
        match result {
            Ok(value) => CdpResponse::success(request, value),
            Err(error) => CdpResponse::failure(request, runtime_error(error)),
        }
    }
}
