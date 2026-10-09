//! Reserved target domain handlers.
use super::super::*;

impl CdpConnection {
    pub(super) async fn dispatch_target(
        &self,
        request: CdpRequest,
        ctx: RequestContext,
        handler: Handler,
    ) -> CdpResponse {
        let result = match handler {
            Handler::TargetGetBrowserContexts => {
                if !request
                    .params
                    .as_object()
                    .is_some_and(serde_json::Map::is_empty)
                {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "Target.getBrowserContexts takes no parameters",
                        ),
                    );
                }
                Ok(json!({"browserContextIds": []}))
            }
            Handler::TargetCreateBrowserContext => {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(
                        CdpErrorCode::InvalidParams,
                        "Target.createBrowserContext is not supported; use the auto-session page",
                    ),
                );
            }
            Handler::TargetSetDiscoverTargets => {
                let Some(params) = request.params.as_object() else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "Target.setDiscoverTargets params must be an object",
                        ),
                    );
                };
                if params.len() > 2 || !params.get("discover").is_some_and(Value::is_boolean) {
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::InvalidParams, "Target.setDiscoverTargets requires a boolean discover and optional filter"));
                }
                let filters = if let Some(filter) = params.get("filter") {
                    let Some(filters) = filter.as_array() else {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(
                                CdpErrorCode::InvalidParams,
                                "Target.setDiscoverTargets filter must be an array",
                            ),
                        );
                    };
                    if filters.len() > 32 || filters.iter().any(|entry| !entry.is_object()) {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(
                                CdpErrorCode::InvalidParams,
                                "Target.setDiscoverTargets filter must contain at most 32 objects",
                            ),
                        );
                    }
                    match serde_json::from_value::<Vec<domains::target::TargetFilter>>(
                        filter.clone(),
                    ) {
                        Ok(filters) => filters,
                        Err(_) => {
                            return CdpResponse::failure(
                                &request,
                                CdpError::new(
                                    CdpErrorCode::InvalidParams,
                                    "invalid bounded discovery filter",
                                ),
                            )
                        }
                    }
                } else {
                    Vec::new()
                };
                let discover = params.get("discover") == Some(&Value::Bool(true));
                *self.discovery_filter.lock().await = discover.then_some(filters.clone());
                if discover {
                    let sessions = match self.runtime.list_sessions(ctx.clone()).await {
                        Ok(sessions) => sessions,
                        Err(error) => return CdpResponse::failure(&request, runtime_error(error)),
                    };
                    let infos = self.target_infos(&sessions).await;
                    for target_info in infos["targetInfos"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|info| {
                            info["type"]
                                .as_str()
                                .is_some_and(|kind| domains::target::filter_matches(&filters, kind))
                        })
                    {
                        if let Err(error) = self
                            .queue_event(CdpEvent {
                                method: "Target.targetCreated".into(),
                                params: json!({"targetInfo": target_info}),
                                session_id: None,
                            })
                            .await
                        {
                            return CdpResponse::failure(&request, error);
                        }
                    }
                }
                Ok(json!({}))
            }
            Handler::TargetCreateTarget => {
                let Some(params) = request.params.as_object() else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "Target.createTarget params must be an object",
                        ),
                    );
                };
                if params.len() != 1
                    || params.get("url").and_then(Value::as_str) != Some("about:blank")
                {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "Target.createTarget supports only a new about:blank runtime page",
                        ),
                    );
                }
                let sessions = match self.runtime.list_sessions(ctx.clone()).await {
                    Ok(sessions) => sessions,
                    Err(error) => return CdpResponse::failure(&request, runtime_error(error)),
                };
                let Some(session) = sessions.first() else {
                    // First call after a bare connect lands here: CDP attaches to
                    // runtime sessions, it does not create them. Say where they come
                    // from rather than leaving the client to guess.
                    return CdpResponse::failure(&request, CdpError::new(CdpErrorCode::RuntimeFailure, "no runtime session is available: CDP attaches to existing runtime sessions and cannot create one -- open a session and page first (POST /v1/sessions then POST /v1/pages, MCP session_create/page_open, or an SDK client), then reuse the page this connection already exposes"));
                };
                let page = match self
                    .runtime
                    .open_page(
                        ctx,
                        OpenPageRequest {
                            session_id: session.id.clone(),
                        },
                    )
                    .await
                {
                    Ok(page) => page,
                    Err(error) => return CdpResponse::failure(&request, runtime_error(error)),
                };
                let runtime_session = session.id.0.to_string();
                let runtime_page = page.id.0.to_string();
                let target_id = self
                    .bind_identifier(
                        IdentifierFamily::Target,
                        &runtime_session,
                        &runtime_page,
                        RuntimeGeneration(0),
                    )
                    .await;
                let tab_id = self
                    .bind_identifier(
                        IdentifierFamily::Target,
                        &runtime_session,
                        &format!("tab:{runtime_page}"),
                        RuntimeGeneration(0),
                    )
                    .await;
                let browser_context_id = self
                    .bind_identifier(
                        IdentifierFamily::BrowserContext,
                        &runtime_session,
                        "default",
                        RuntimeGeneration(0),
                    )
                    .await;
                let attach = self
                    .auto_attach
                    .lock()
                    .await
                    .clone()
                    .filter(|(_, filters)| domains::target::filter_matches(filters, "tab"));
                let tab_info = json!({"targetId":tab_id,"type":"tab","title":"Automation Runtime","url":"about:blank","attached":attach.is_some(),"canAccessOpener":false,"browserContextId":browser_context_id});
                let page_info = json!({"targetId":target_id,"type":"page","title":"Automation Runtime","url":"about:blank","attached":false,"canAccessOpener":false,"browserContextId":browser_context_id});
                let tab_session_id = self
                    .bind_identifier(
                        IdentifierFamily::CdpSession,
                        &runtime_session,
                        &tab_id,
                        RuntimeGeneration(0),
                    )
                    .await;
                let page_session_id = self
                    .bind_identifier(
                        IdentifierFamily::CdpSession,
                        &runtime_session,
                        &target_id,
                        RuntimeGeneration(0),
                    )
                    .await;
                let discovery = self.discovery_filter.lock().await.clone();
                let mut events = Vec::new();
                if discovery
                    .as_ref()
                    .is_some_and(|filters| domains::target::filter_matches(filters, "tab"))
                {
                    events.push(CdpEvent {
                        method: "Target.targetCreated".into(),
                        params: json!({"targetInfo":tab_info.clone()}),
                        session_id: None,
                    });
                }
                if discovery
                    .as_ref()
                    .is_some_and(|filters| domains::target::filter_matches(filters, "page"))
                {
                    events.push(CdpEvent {
                        method: "Target.targetCreated".into(),
                        params: json!({"targetInfo":page_info.clone()}),
                        session_id: None,
                    });
                }
                if let Some((waiting, _)) = attach {
                    let mut children = self.pending_tab_children.lock().await;
                    if children.len() >= MAX_PENDING_TAB_CHILDREN {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(
                                CdpErrorCode::RuntimeFailure,
                                "pending tab registry exhausted",
                            ),
                        );
                    }
                    children.insert(tab_session_id.clone(), (page_session_id, page_info));
                    events.push(CdpEvent { method:"Target.attachedToTarget".into(), params:json!({"sessionId":tab_session_id,"targetInfo":tab_info,"waitingForDebugger":waiting}), session_id:None });
                }
                if let Err(error) = self.queue_events(events).await {
                    return CdpResponse::failure(&request, error);
                }
                Ok(json!({"targetId": target_id}))
            }
            Handler::TargetGetTargets => match self.runtime.list_sessions(ctx).await {
                Ok(sessions) => Ok(self.target_infos(&sessions).await),
                Err(error) => Err(error),
            },
            Handler::TargetGetTargetInfo => {
                let Some(params) = request.params.as_object() else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "Target.getTargetInfo params must be an object",
                        ),
                    );
                };
                if params.is_empty() {
                    let target_id = self
                        .bind_identifier(
                            IdentifierFamily::Target,
                            "browser",
                            "browser",
                            RuntimeGeneration(0),
                        )
                        .await;
                    Ok(
                        json!({"targetInfo": {"targetId": target_id, "type": "browser", "title": "", "url": "", "attached": true, "canAccessOpener": false}}),
                    )
                } else if params.len() == 1 {
                    let Some(target_id) = params.get("targetId").and_then(Value::as_str) else {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(CdpErrorCode::InvalidParams, "invalid targetId"),
                        );
                    };
                    let Some(page) = self
                        .resolve_identifier(IdentifierFamily::Target, target_id)
                        .await
                    else {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(CdpErrorCode::InvalidParams, "unknown target"),
                        );
                    };
                    let (url, title) = match self
                        .runtime_session_for(IdentifierFamily::Target, target_id)
                        .await
                    {
                        Some(runtime_session) => {
                            self.targets.lock().await.location(&runtime_session, &page)
                        }
                        None => (None, None),
                    };
                    let url = url.unwrap_or_else(|| "about:blank".into());
                    let title = title.unwrap_or_else(|| url.clone());
                    Ok(
                        json!({"targetInfo": {"targetId": target_id, "type": "page", "title": title, "url": url, "attached": true, "canAccessOpener": false}}),
                    )
                } else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "Target.getTargetInfo accepts only targetId",
                        ),
                    );
                }
            }
            Handler::TargetAttachToBrowserTarget => {
                if !request
                    .params
                    .as_object()
                    .is_some_and(serde_json::Map::is_empty)
                {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "Target.attachToBrowserTarget takes no parameters",
                        ),
                    );
                }
                let session_id = Uuid::new_v4().to_string();
                let mut observers = self.browser_observers.lock().await;
                if observers.len() >= MAX_BROWSER_OBSERVERS {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(CdpErrorCode::RuntimeFailure, "observer registry exhausted"),
                    );
                }
                observers.insert(session_id.clone());
                Ok(json!({"sessionId": session_id}))
            }
            Handler::TargetDetachFromTarget => {
                let Some(session_id) = request.params.get("sessionId").and_then(Value::as_str)
                else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(CdpErrorCode::InvalidParams, "missing observer session"),
                    );
                };
                if !self.browser_observers.lock().await.remove(session_id) {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(CdpErrorCode::InvalidParams, "unknown observer session"),
                    );
                }
                Ok(json!({}))
            }
            Handler::TargetSetAutoAttach => {
                match domains::target::auto_attach(request.params.clone()) {
                    Ok(options) => {
                        if let Some(parent_session) = request.session_id.as_deref() {
                            if options.auto_attach
                                && domains::target::filter_matches(&options.filter, "page")
                            {
                                if let Some((session_id, mut target_info)) = self
                                    .pending_tab_children
                                    .lock()
                                    .await
                                    .remove(parent_session)
                                {
                                    target_info["attached"] = Value::Bool(true);
                                    if let Err(error) = self.queue_event(CdpEvent {
                                    method: "Target.attachedToTarget".into(),
                                    params: json!({"sessionId":session_id,"targetInfo":target_info,"waitingForDebugger":options.wait_for_debugger_on_start}),
                                    session_id: Some(parent_session.to_owned()),
                                }).await {
                                    return CdpResponse::failure(&request, error);
                                }
                                }
                            }
                            return CdpResponse::success(&request, json!({}));
                        }
                        *self.auto_attach.lock().await = options.auto_attach.then_some((
                            options.wait_for_debugger_on_start,
                            options.filter.clone(),
                        ));
                        match self.runtime.list_sessions(ctx).await {
                            Ok(sessions) => {
                                if options.auto_attach && request.session_id.is_none() {
                                    if let Err(error) = self
                                        .queue_attached_targets(
                                            &sessions,
                                            options.wait_for_debugger_on_start,
                                            &options.filter,
                                        )
                                        .await
                                    {
                                        return CdpResponse::failure(&request, error);
                                    }
                                }
                                Ok(json!({}))
                            }
                            Err(error) => Err(error),
                        }
                    }
                    Err(error) => return CdpResponse::failure(&request, error),
                }
            }
            _ => unreachable!("domain routing is exhaustive"),
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
}
