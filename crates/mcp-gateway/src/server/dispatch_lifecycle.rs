//! Session and page lifecycle.
//!
//! Split out of the single 991-line `dispatch_named_tool` match. Arm
//! bodies are unchanged: each still returns the final response
//! directly on a malformed argument, so the early returns keep the
//! same meaning they had inside one function.

use super::*;

pub(super) const TOOLS: &[&str] = &[
    "runtime_info",
    "session_list",
    "session_create",
    "session_close",
    "page_open",
];

impl Server {
    /// Creates a session and records it for this connection. Runs in a
    /// spawned task so a request future dropped mid-create still finishes the
    /// record-or-delete step instead of leaving an unrecorded session. When
    /// the connection is already closing the creator deletes the session.
    async fn create_recorded_session(
        &self,
        context: types::RequestContext,
        request: types::CreateSessionRequest,
    ) -> interface_core::InterfaceResult<types::SessionState> {
        let runtime = Arc::clone(&self.runtime);
        let handle = self.handle.clone();
        let sessions = Arc::clone(&self.sessions);
        let correlation_id = context.correlation_id.clone();
        let task = tokio::spawn(async move {
            let session = runtime.create_session(context, request).await?;
            if !sessions.record(&session.id) {
                connection_sessions::delete_session_quietly(&runtime, &handle, session.id).await;
                return Err(types::InterfaceError {
                    code: types::InterfaceErrorCode::Internal,
                    layer: types::ErrorLayer::Interface,
                    message: "connection closed".into(),
                    correlation_id,
                    command_id: None,
                    retryable: false,
                    retry_after_ms: None,
                    reconciliation_required: false,
                    required_capability: None,
                });
            }
            Ok(session)
        });
        match task.await {
            Ok(result) => result,
            Err(_) => Err(types::InterfaceError {
                code: types::InterfaceErrorCode::Internal,
                layer: types::ErrorLayer::Interface,
                message: "runtime interface request failed".into(),
                correlation_id: types::CorrelationId::new(),
                command_id: None,
                retryable: false,
                retry_after_ms: None,
                reconciliation_required: true,
                required_capability: None,
            }),
        }
    }

    pub(super) async fn dispatch_lifecycle(
        &self,
        id: Value,
        call: ToolCall,
        context: types::RequestContext,
    ) -> Value {
        let result = match call.name.as_str() {
            "runtime_info" => {
                if bounded_parse::<EmptyArgs>(call.arguments).is_err() {
                    return invalid_params_reason(id, "malformedArguments");
                }
                // Principal-scoped, so it is added here rather than on the
                // runtime-wide `RuntimeInfo`. Without it a caller cannot see
                // the credential expiry coming.
                let credential_expires_at = self.handle.expires_at();
                self.runtime
                    .runtime_info(context)
                    .await
                    .and_then(to_json)
                    .map(|mut value| {
                        if let Some(object) = value.as_object_mut() {
                            object.insert(
                                "credentialExpiresAt".to_owned(),
                                json!(credential_expires_at.to_rfc3339()),
                            );
                        }
                        value
                    })
            }
            "session_list" => {
                if bounded_parse::<EmptyArgs>(call.arguments).is_err() {
                    return invalid_params_reason(id, "malformedArguments");
                }
                self.runtime
                    .list_sessions(context)
                    .await
                    .and_then(to_json)
                    .map(|sessions| json!({"sessions": sessions}))
            }
            "session_create" => {
                let input: SessionCreateArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                if input.profile.is_empty()
                    || input.profile.len() > 128
                    || input.proxy.as_ref().is_some_and(|proxy| proxy.len() > 2048)
                {
                    return invalid_params_reason(id, "malformedArguments");
                }
                self.create_recorded_session(
                    context,
                    types::CreateSessionRequest {
                        profile: input.profile,
                        proxy: input.proxy,
                        execution_policy: input.execution_policy,
                        zigzagzig: input.zigzagzig,
                    },
                )
                .await
                .and_then(to_json)
            }
            "session_close" => {
                let input: SessionCloseArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let session_id = input.session_id;
                let result = self
                    .runtime
                    .delete_session(context, session_id.clone())
                    .await
                    .and_then(|()| to_json(json!({"closed": true})));
                if result.is_ok() {
                    self.workflow_handles.remove_session(&session_id);
                    self.sessions.forget(&session_id);
                }
                result
            }
            "page_open" => {
                let input: PageOpenArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                async {
                    if input.url.is_some() {
                        self.authorization
                            .require_capability(&context, types::Capability::BrowserMutate)?;
                    }
                    let session_id = input.session_id;
                    let mut page = self
                        .runtime
                        .open_page(
                            context.clone(),
                            types::OpenPageRequest {
                                session_id: session_id.clone(),
                            },
                        )
                        .await?;
                    let Some(url) = input.url else {
                        return to_json(page);
                    };
                    let page_id = page.id.clone();
                    let (navigation_context, envelope) = primitive_envelope(
                        context.clone(),
                        session_id.clone(),
                        Some(page_id.clone()),
                        None,
                        types::PrimitiveCommand::Navigate(types::NavigateCommand {
                            url,
                            wait_until: types::WaitUntil::Interactive,
                            timeout_ms: DEFAULT_COMMAND_TIMEOUT_MS,
                        }),
                    );
                    let navigation_outcome =
                        match self.runtime.submit(navigation_context, envelope).await {
                            Ok(outcome) => outcome,
                            Err(navigation_error) => {
                                let (cleanup_context, cleanup_envelope) = primitive_envelope(
                                    context,
                                    session_id,
                                    Some(page_id.clone()),
                                    None,
                                    types::PrimitiveCommand::ClosePage(types::ClosePageCommand {
                                        page_id,
                                    }),
                                );
                                let _ =
                                    self.runtime.submit(cleanup_context, cleanup_envelope).await;
                                return Err(navigation_error);
                            }
                        };
                    let navigation_completed =
                        matches!(navigation_outcome, types::CommandOutcome::Completed { .. });
                    if let types::CommandOutcome::Completed { evidence, .. } = &navigation_outcome {
                        if let Some(types::Evidence::Navigation { url, .. }) = evidence.first() {
                            page.url = Some(url.clone());
                            page.ready_state = "interactive".to_owned();
                        }
                    }
                    let mut value = to_json(page)?;
                    let object = value
                        .as_object_mut()
                        .expect("page state serializes as an object");
                    object.insert("navigationOutcome".to_owned(), to_json(navigation_outcome)?);
                    if !navigation_completed {
                        let (cleanup_context, cleanup_envelope) = primitive_envelope(
                            context,
                            session_id,
                            Some(page_id.clone()),
                            None,
                            types::PrimitiveCommand::ClosePage(types::ClosePageCommand { page_id }),
                        );
                        let cleanup_outcome = self
                            .runtime
                            .submit(cleanup_context, cleanup_envelope)
                            .await?;
                        let page_closed =
                            matches!(cleanup_outcome, types::CommandOutcome::Completed { .. });
                        object.insert("cleanupOutcome".to_owned(), to_json(cleanup_outcome)?);
                        object.insert("pageClosed".to_owned(), json!(page_closed));
                    }
                    Ok(value)
                }
                .await
            }
            _ => unreachable!("dispatch_lifecycle received a tool it does not own"),
        };
        self.finish_tool(id, result, None, call.name.as_str()).await
    }
}
