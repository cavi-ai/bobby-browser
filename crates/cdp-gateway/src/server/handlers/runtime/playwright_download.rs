//! Verified semantic operation behind reserved CDP admission.
use super::super::super::*;
impl CdpConnection {
    pub(super) async fn playwright_download(
        &self,
        request: &CdpRequest,
        ctx: RequestContext,
        session_id: SessionId,
        page_id: PageId,
        target: TargetSpec,
    ) -> CdpResponse {
        let frame_id = match request.session_id.as_deref() {
            Some(cdp) => {
                self.resolve_identifier(IdentifierFamily::CdpSession, cdp)
                    .await
            }
            None => None,
        }
        .unwrap_or_else(|| "main".into());
        let command = PrimitiveCommand::ClickAndWaitForDownload(ClickAndWaitForDownloadCommand {
            selector: String::new(),
            target: Some(target),
            timeout_ms: 30_000,
        });
        match self
            .submit_boundary(ctx.clone(), session_id.clone(), page_id.clone(), command)
            .await
        {
            Ok(CommandOutcome::Completed { evidence, .. }) => {
                let download = evidence.iter().find_map(|item| match item {
                    types::Evidence::Download {
                        filename,
                        path,
                        bytes,
                        sha256,
                        ..
                    } => Some((filename.clone(), path.clone(), *bytes, sha256.clone())),
                    _ => None,
                });
                let Some((filename, path, expected_bytes, expected_sha)) = download else {
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
                let data = match std::fs::read(&path) {
                    Ok(data) => data,
                    Err(_) => {
                        return CdpResponse::failure(
                            request,
                            CdpError::new(
                                CdpErrorCode::RuntimeFailure,
                                "verified download was unavailable",
                            ),
                        )
                    }
                };
                let actual_sha = hex::encode(Sha256::digest(&data));
                if data.len() as u64 != expected_bytes || actual_sha != expected_sha {
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
                        &session_id,
                        &page_id,
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
                match store.get(&session_id, &record.artifact_id).await {
                    Ok(imported)
                        if imported.len() as u64 == expected_bytes
                            && hex::encode(Sha256::digest(&imported)) == expected_sha => {}
                    Err(_) => {
                        return CdpResponse::failure(
                            request,
                            CdpError::new(
                                CdpErrorCode::RuntimeFailure,
                                "download import verification failed",
                            ),
                        )
                    }
                    Ok(_) => {
                        return CdpResponse::failure(
                            request,
                            CdpError::new(
                                CdpErrorCode::RuntimeFailure,
                                "download import integrity check failed",
                            ),
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
                        ctx.principal_id.clone(),
                        &self.connection_id,
                        session_id.clone(),
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
                let mut events = download_events(
                    &frame_id,
                    &guid,
                    &filename,
                    expected_bytes,
                    &stream_id,
                    &expected_sha,
                    None,
                )
                .to_vec();
                for observer in self.browser_observers.lock().await.iter() {
                    events.extend(download_events(
                        &frame_id,
                        &guid,
                        &filename,
                        expected_bytes,
                        &stream_id,
                        &expected_sha,
                        Some(observer.clone()),
                    ));
                }
                if let Err(error) = self.queue_events(events).await {
                    self.streams.lock().await.remove(&stream_id);
                    return CdpResponse::failure(request, error);
                }
                CdpResponse::success(request, json!({"result":{"type":"string","value":"done"}}))
            }
            Ok(_) => CdpResponse::failure(
                request,
                CdpError::new(
                    CdpErrorCode::RuntimeFailure,
                    "runtime download did not complete",
                ),
            ),
            Err(error) => CdpResponse::failure(request, runtime_error(error)),
        }
    }
}
