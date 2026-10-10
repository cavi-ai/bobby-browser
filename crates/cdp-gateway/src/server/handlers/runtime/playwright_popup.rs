//! Verified semantic operation behind reserved CDP admission.
use super::super::super::*;
impl CdpConnection {
    pub(super) async fn playwright_popup(
        &self,
        request: &CdpRequest,
        ctx: RequestContext,
        session_id: SessionId,
        page_id: PageId,
        target: TargetSpec,
    ) -> CdpResponse {
        let opener_target = match request.session_id.as_deref() {
            Some(cdp_session) => {
                self.resolve_identifier(IdentifierFamily::CdpSession, cdp_session)
                    .await
            }
            None => None,
        };
        let command = PrimitiveCommand::ClickAndWaitForPopup(ClickAndWaitForPopupCommand {
            selector: String::new(),
            target: Some(target),
            timeout_ms: 30_000,
        });
        match self
            .submit_boundary(ctx, session_id.clone(), page_id, command)
            .await
        {
            Ok(CommandOutcome::Completed { evidence, .. }) => {
                let popup = evidence.iter().find_map(|item| match item {
                    types::Evidence::Popup {
                        page_id,
                        url,
                        title,
                        ..
                    } => Some((page_id.clone(), url.clone(), title.clone())),
                    _ => None,
                });
                let Some((popup_page, url, title)) = popup else {
                    return CdpResponse::failure(
                        request,
                        CdpError::new(
                            CdpErrorCode::RuntimeFailure,
                            "runtime popup did not produce popup evidence",
                        ),
                    );
                };
                let target_id = self.targets.lock().await.register(&session_id, &popup_page);
                let generation = RuntimeGeneration(0);
                let browser_context_id;
                let popup_session;
                {
                    let mut identifiers = self.identifiers.lock().await;
                    identifiers.adopt_target(
                        target_id.clone(),
                        &session_id.0.to_string(),
                        &popup_page.0.to_string(),
                        generation,
                    );
                    browser_context_id = identifiers.bind_browser_context(
                        &session_id.0.to_string(),
                        "default",
                        generation,
                    );
                    popup_session = identifiers.bind_family(
                        IdentifierFamily::CdpSession,
                        &target_id,
                        &target_id,
                        generation,
                    );
                }
                if let Err(error) = self.queue_event(CdpEvent {
                                    method:"Target.attachedToTarget".into(),
                                    params:json!({"sessionId":popup_session,"targetInfo":{"targetId":target_id,"type":"page","title":title,"url":url,"attached":true,"canAccessOpener":true,"openerId":opener_target,"browserContextId":browser_context_id},"waitingForDebugger":false}),
                                    session_id:None,
                                }).await {
                                    return CdpResponse::failure(request, error);
                                }
                let loader_id = Uuid::new_v4().simple().to_string();
                let mut loads = self.pending_page_loads.lock().await;
                if loads.len() >= MAX_PENDING_PAGE_LOADS {
                    return CdpResponse::failure(
                        request,
                        CdpError::new(
                            CdpErrorCode::RuntimeFailure,
                            "pending page load registry exhausted",
                        ),
                    );
                }
                loads.insert(popup_session, (target_id, url, loader_id));
                CdpResponse::success(request, json!({"result":{"type":"string","value":"done"}}))
            }
            Ok(_) => CdpResponse::failure(
                request,
                CdpError::new(
                    CdpErrorCode::RuntimeFailure,
                    "runtime popup did not complete",
                ),
            ),
            Err(error) => CdpResponse::failure(request, runtime_error(error)),
        }
    }
}
