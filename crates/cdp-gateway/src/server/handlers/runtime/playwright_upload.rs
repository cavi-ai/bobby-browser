//! Verified semantic operation behind reserved CDP admission.
use super::super::super::*;
impl CdpConnection {
    pub(super) async fn playwright_upload(
        &self,
        request: &CdpRequest,
        ctx: RequestContext,
        handle: &str,
        serialized: &Value,
    ) -> CdpResponse {
        let descriptor = handle.trim_start_matches("semantic-element:");
        let Some(label) = descriptor.strip_prefix("label:") else {
            return CdpResponse::failure(
                request,
                CdpError::new(
                    CdpErrorCode::InvalidParams,
                    "upload requires a verified labeled file input",
                ),
            );
        };
        let payloads = match serialized_file_payloads(serialized) {
            Ok(payloads) => payloads,
            Err(error) => return CdpResponse::failure(request, error),
        };
        let Some(staging_root) = self.upload_staging_root.as_ref() else {
            return CdpResponse::failure(
                request,
                CdpError::new(
                    CdpErrorCode::RuntimeFailure,
                    "upload staging root is not configured",
                ),
            );
        };
        let request_dir = match UploadStaging::new(staging_root) {
            Ok(dir) => dir,
            Err(error) => {
                return CdpResponse::failure(
                    request,
                    CdpError::new(
                        CdpErrorCode::RuntimeFailure,
                        format!("failed to create confined upload staging: {error}"),
                    ),
                )
            }
        };
        let mut staged = Vec::with_capacity(payloads.len());
        for (name, bytes) in payloads {
            let path = match request_dir.stage(&name, &bytes) {
                Ok(path) => path,
                Err(error) => {
                    return CdpResponse::failure(
                        request,
                        CdpError::new(
                            CdpErrorCode::RuntimeFailure,
                            format!("failed to stage bounded upload: {error}"),
                        ),
                    );
                }
            };
            staged.push(path);
        }
        let Some((session_id, page_id)) =
            self.runtime_identity(request.session_id.as_deref()).await
        else {
            return CdpResponse::failure(
                request,
                CdpError::new(CdpErrorCode::InvalidParams, "unknown runtime page"),
            );
        };
        let paths = staged
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect();
        let envelope = CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id,
            page_id: Some(page_id),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(PrimitiveCommand::UploadFiles(UploadFilesCommand {
                selector: String::new(),
                target: Some(TargetSpec {
                    label: Some(label.to_owned()),
                    ..TargetSpec::default()
                }),
                paths,
            })),
        };
        let outcome = self.runtime.submit(ctx, envelope).await;
        drop(request_dir);
        match outcome {
            Ok(CommandOutcome::Completed { evidence, .. })
                if evidence
                    .iter()
                    .any(|item| matches!(item, types::Evidence::Upload { .. })) =>
            {
                self.record_interface_event("upload.completed", json!({"evidence":evidence}))
                    .await;
                CdpResponse::success(request, json!({"result":{"type":"undefined"}}))
            }
            Ok(_) => CdpResponse::failure(
                request,
                CdpError::new(
                    CdpErrorCode::RuntimeFailure,
                    "runtime upload did not produce upload evidence",
                ),
            ),
            Err(error) => CdpResponse::failure(request, runtime_error(error)),
        }
    }
}
