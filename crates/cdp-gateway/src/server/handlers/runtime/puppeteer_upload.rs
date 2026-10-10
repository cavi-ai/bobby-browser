//! Pinned Puppeteer upload admission, staging and evidence.
use super::super::super::*;
impl CdpConnection {
    pub(super) async fn puppeteer_upload(
        &self,
        request: &CdpRequest,
        ctx: RequestContext,
        session_id: SessionId,
        page_id: PageId,
        value: &str,
    ) -> CdpResponse {
        #[derive(serde::Deserialize)]
        #[serde(deny_unknown_fields)]
        struct UploadValue {
            name: String,
            base64: String,
        }
        let Ok(payload) = serde_json::from_str::<UploadValue>(value) else {
            return CdpResponse::failure(
                request,
                CdpError::new(
                    CdpErrorCode::InvalidParams,
                    "invalid pinned Puppeteer upload payload",
                ),
            );
        };
        if payload.name.is_empty()
            || payload.name.len() > 255
            || payload.name.contains(['/', '\\'])
            || payload.base64.len() > 768 * 1024
        {
            return CdpResponse::failure(
                request,
                CdpError::new(
                    CdpErrorCode::InvalidParams,
                    "pinned Puppeteer upload payload exceeds bounds",
                ),
            );
        }
        let Ok(bytes) = BASE64.decode(payload.base64) else {
            return CdpResponse::failure(
                request,
                CdpError::new(
                    CdpErrorCode::InvalidParams,
                    "invalid pinned Puppeteer upload encoding",
                ),
            );
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
        let Ok(request_dir) = UploadStaging::new(staging_root) else {
            return CdpResponse::failure(
                request,
                CdpError::new(
                    CdpErrorCode::RuntimeFailure,
                    "failed to create confined upload staging",
                ),
            );
        };
        let Ok(path) = request_dir.stage(&payload.name, &bytes) else {
            return CdpResponse::failure(
                request,
                CdpError::new(
                    CdpErrorCode::RuntimeFailure,
                    "failed to stage confined upload",
                ),
            );
        };
        let result = self
            .runtime
            .submit(
                ctx,
                CommandEnvelope {
                    schema_version: CommandEnvelope::SCHEMA_VERSION,
                    command_id: CommandId::new(),
                    workflow_id: WorkflowId::new(),
                    attempt_id: AttemptId::new(),
                    session_id,
                    page_id: Some(page_id),
                    deadline: Utc::now() + Duration::seconds(30),
                    command: RuntimeCommand::Primitive(PrimitiveCommand::UploadFiles(
                        UploadFilesCommand {
                            selector: String::new(),
                            target: Some(TargetSpec {
                                label: Some("Resume".into()),
                                ..TargetSpec::default()
                            }),
                            paths: vec![path.to_string_lossy().into_owned()],
                        },
                    )),
                },
            )
            .await;
        drop(request_dir);
        match result {
            Ok(CommandOutcome::Completed { evidence, .. }) => {
                self.record_interface_event("upload.completed", json!({"evidence":evidence}))
                    .await;
                CdpResponse::success(request, json!({"result":{"type":"undefined"}}))
            }
            Ok(_) => CdpResponse::failure(
                request,
                CdpError::new(
                    CdpErrorCode::RuntimeFailure,
                    "pinned Puppeteer semantic operation did not complete",
                ),
            ),
            Err(error) => CdpResponse::failure(request, runtime_error(error)),
        }
    }
}
