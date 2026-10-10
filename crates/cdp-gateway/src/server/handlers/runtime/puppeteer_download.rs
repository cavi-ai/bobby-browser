//! Import verified Puppeteer downloads and bind owned stream events.
use super::super::super::*;
impl CdpConnection {
    pub(super) async fn puppeteer_download(
        &self,
        request: &CdpRequest,
        download_ctx: RequestContext,
        download_session: SessionId,
        download_page: PageId,
        outcome: Result<CommandOutcome, InterfaceError>,
    ) -> CdpResponse {
        match outcome {
            Ok(CommandOutcome::Completed { evidence, .. }) => {
                let Some((filename, path, expected_bytes, expected_sha)) =
                    evidence.iter().find_map(|item| match item {
                        types::Evidence::Download {
                            filename,
                            path,
                            bytes,
                            sha256,
                            ..
                        } => Some((filename.clone(), path.clone(), *bytes, sha256.clone())),
                        _ => None,
                    })
                else {
                    return CdpResponse::failure(
                        request,
                        CdpError::new(
                            CdpErrorCode::RuntimeFailure,
                            "runtime download did not produce download evidence",
                        ),
                    );
                };
                let Some(store) = self.artifacts.as_ref() else {
                    return CdpResponse::failure(
                        request,
                        CdpError::new(
                            CdpErrorCode::RuntimeFailure,
                            "artifact reader is not configured",
                        ),
                    );
                };
                let Ok(data) = std::fs::read(&path) else {
                    return CdpResponse::failure(
                        request,
                        CdpError::new(
                            CdpErrorCode::RuntimeFailure,
                            "verified download was unavailable",
                        ),
                    );
                };
                if data.len() as u64 != expected_bytes
                    || hex::encode(Sha256::digest(&data)) != expected_sha
                {
                    return CdpResponse::failure(
                        request,
                        CdpError::new(
                            CdpErrorCode::RuntimeFailure,
                            "download evidence integrity check failed",
                        ),
                    );
                }
                let record = match store
                    .put(
                        &download_session,
                        &download_page,
                        "application/octet-stream",
                        "bin",
                        &data,
                        data.len(),
                    )
                    .await
                {
                    Ok(record) => record,
                    Err(_) => {
                        return CdpResponse::failure(
                            request,
                            CdpError::new(CdpErrorCode::RuntimeFailure, "download import failed"),
                        )
                    }
                };
                let stream_id = Uuid::new_v4().to_string();
                if self
                    .streams
                    .lock()
                    .await
                    .reserve(
                        &stream_id,
                        download_ctx.principal_id,
                        &self.connection_id,
                        download_session,
                        &record.artifact_id,
                        expected_bytes,
                    )
                    .is_err()
                {
                    return CdpResponse::failure(
                        request,
                        CdpError::new(
                            CdpErrorCode::RuntimeFailure,
                            "download stream capacity exhausted",
                        ),
                    );
                }
                let guid = Uuid::new_v4().to_string();
                let frame_id = request.session_id.clone().unwrap_or_else(|| "main".into());
                if let Err(error) = self
                    .queue_events(
                        download_events(
                            &frame_id,
                            &guid,
                            &filename,
                            expected_bytes,
                            &stream_id,
                            &expected_sha,
                            request.session_id.clone(),
                        )
                        .to_vec(),
                    )
                    .await
                {
                    self.streams.lock().await.remove(&stream_id);
                    return CdpResponse::failure(request, error);
                }
                CdpResponse::success(request, json!({"result":{"type":"undefined"}}))
            }
            Ok(_) => CdpResponse::failure(
                request,
                CdpError::new(
                    CdpErrorCode::RuntimeFailure,
                    "pinned Puppeteer download did not complete",
                ),
            ),
            Err(error) => CdpResponse::failure(request, runtime_error(error)),
        }
    }
}
