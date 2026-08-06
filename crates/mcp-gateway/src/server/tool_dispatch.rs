//! Name-matched `tools/call` dispatch (kept out of `call_tool` preamble).

use super::*;

impl Server {
    pub(super) async fn dispatch_named_tool(
        &self,
        id: Value,
        call: ToolCall,
        mut context: types::RequestContext,
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
                self.runtime
                    .create_session(
                        context,
                        types::CreateSessionRequest {
                            profile: input.profile,
                            proxy: input.proxy,
                            execution_policy: input.execution_policy,
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
                self.runtime
                    .delete_session(context, input.session_id)
                    .await
                    .and_then(|()| to_json(json!({"closed": true})))
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
                    let page = self
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
                        self.runtime.submit(navigation_context, envelope).await?;
                    let navigation_completed =
                        matches!(navigation_outcome, types::CommandOutcome::Completed { .. });
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
            "command_execute" => {
                let input: CommandExecuteArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let now = Utc::now();
                if input.envelope.deadline <= now
                    || input.envelope.deadline
                        > now + Duration::milliseconds(MAX_COMMAND_DEADLINE_MS)
                {
                    return invalid_params_reason(id, "deadlineOutOfRange");
                }
                context.deadline = input.envelope.deadline;
                context.idempotency_key = match input.idempotency_key {
                    Some(key) => match types::IdempotencyKey::try_from(key) {
                        Ok(key) => Some(key),
                        Err(_) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                    },
                    None => None,
                };
                self.submit_envelope(context, input.envelope).await
            }
            "navigate" => {
                let input: NavigateArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::Navigate(types::NavigateCommand {
                        url: input.url,
                        wait_until: input.wait_until.unwrap_or(types::WaitUntil::Interactive),
                        timeout_ms: input.timeout_ms.unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS),
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "click" => {
                let input: ClickArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, mut envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::Click(types::ClickCommand {
                        selector: input.selector.unwrap_or_default(),
                        target: input.target,
                        boundary: input.boundary.unwrap_or(false),
                        expected_url: input.expected_url,
                    }),
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                if input.boundary.unwrap_or(false) && input.auto_checkpoint.unwrap_or(true) {
                    self.submit_envelope_with_auto_checkpoint(context, envelope)
                        .await
                } else {
                    self.submit_envelope(context, envelope).await
                }
            }
            "type_text" => {
                let input: TypeTextArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::TypeText(types::TypeTextCommand {
                        selector: input.selector.unwrap_or_default(),
                        target: input.target,
                        value: input.value,
                        clear_first: input.clear_first.unwrap_or(false),
                        expected_url: input.expected_url,
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "inspect" => {
                let input: InspectArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::Inspect(types::InspectCommand {
                        selector: input.selector,
                        target: input.target,
                        include_html: input.include_html.unwrap_or(false),
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "screenshot" => {
                let input: ScreenshotArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::CaptureScreenshot(types::CaptureScreenshotCommand {
                        mode: input.mode.unwrap_or(types::ScreenshotMode::Viewport),
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "wait_for" => {
                let input: WaitForArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::WaitFor(types::WaitForCommand {
                        condition: input.condition,
                        timeout_ms: input.timeout_ms,
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "intent_locate" => {
                let input: IntentLocateArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::Locate(types::LocateIntent {
                    purpose: input.purpose,
                    hints: input.hints.unwrap_or_default(),
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "intent_fill" => {
                let input: IntentFillArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::Fill(types::FillIntent {
                    purpose: input.purpose,
                    hints: input.hints.unwrap_or_default(),
                    value: input.value,
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "intent_complete_form" => {
                let input: IntentCompleteFormArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::CompleteForm(types::CompleteFormIntent {
                    purpose: input.purpose,
                    fields: input.fields,
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "intent_submit_and_verify" => {
                let input: IntentSubmitAndVerifyArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::SubmitAndVerify(types::SubmitAndVerifyIntent {
                    purpose: input.purpose,
                    hints: input.hints.unwrap_or_default(),
                    expected_state: input.expected_state,
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                if input.auto_checkpoint.unwrap_or(true) {
                    self.submit_envelope_with_auto_checkpoint(context, envelope)
                        .await
                } else {
                    self.submit_envelope(context, envelope).await
                }
            }
            "intent_wait_for_state" => {
                let input: IntentWaitForStateArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::WaitForState(types::WaitForStateIntent {
                    condition: input.condition,
                    timeout_ms: input.timeout_ms,
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "intent_follow" => {
                let input: IntentFollowArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::Follow(types::FollowIntent {
                    purpose: input.purpose,
                    hints: input.hints.unwrap_or_default(),
                    expected_destination: input.expected_destination,
                    boundary: input.boundary.unwrap_or(false),
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                if input.auto_checkpoint.unwrap_or(true) {
                    self.submit_envelope_with_auto_checkpoint(context, envelope)
                        .await
                } else {
                    self.submit_envelope(context, envelope).await
                }
            }
            "intent_dismiss_obstruction" => {
                let input: IntentDismissObstructionArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent =
                    types::IntentCommand::DismissObstruction(types::DismissObstructionIntent {
                        purpose: input.purpose,
                        hints: input.hints.unwrap_or_default(),
                        timeout_ms: input
                            .timeout_ms
                            .unwrap_or(types::DEFAULT_DISMISS_OBSTRUCTION_TIMEOUT_MS),
                    });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "intent_extract" => {
                let input: IntentExtractArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::Extract(types::ExtractIntent {
                    purpose: input.purpose,
                    fields: input.fields,
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "page_list" => {
                let input: PageListArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    None,
                    input.workflow_id,
                    types::PrimitiveCommand::ListPages(types::ListPagesCommand),
                );
                self.submit_envelope(context, envelope).await
            }
            "page_close" => {
                let input: FormSnapshotArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let page_id = input.page_id;
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(page_id.clone()),
                    input.workflow_id,
                    types::PrimitiveCommand::ClosePage(types::ClosePageCommand { page_id }),
                );
                self.submit_envelope(context, envelope).await
            }
            "page_activate" => {
                let input: PageCloseArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let page_id = input.page_id;
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(page_id.clone()),
                    input.workflow_id,
                    types::PrimitiveCommand::ActivatePage(types::ActivatePageCommand { page_id }),
                );
                self.submit_envelope(context, envelope).await
            }
            "a11y_snapshot" => {
                let input: A11ySnapshotArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::AccessibilitySnapshot(
                        types::AccessibilitySnapshotCommand {
                            max_nodes: input.max_nodes,
                        },
                    ),
                );
                self.submit_envelope(context, envelope).await
            }
            "context_ask" => {
                let input: ContextAskArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                self.runtime
                    .context_ask(context, input.session_id, input.page_id, input.description)
                    .await
                    // `None` is an answer, not a failure: the context does not
                    // know and the repair is to snapshot. An error here would
                    // be indistinguishable from a broken call.
                    .and_then(|answer| to_json(json!({"answer": answer})))
            }
            "context_neighbors" => {
                let input: ContextNeighborsArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                self.runtime
                    .context_neighbors(context, input.session_id, input.page_id, input.description)
                    .await
                    // Like context_ask: `None` is an answer, not a failure.
                    .and_then(|neighbors| to_json(json!({"neighbors": neighbors})))
            }
            "form_snapshot" => {
                let input: FormSnapshotArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                if input
                    .max_controls
                    .is_some_and(|limit| !(1..=512).contains(&limit))
                {
                    return invalid_params_reason(id, "malformedArguments");
                }
                self.runtime
                    .form_snapshot(context, input.session_id, input.page_id, input.max_controls)
                    .await
                    .and_then(to_json)
            }
            "control_action" => {
                let input: ControlActionArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                if input.action.validate().is_err() {
                    return invalid_params_reason(id, "malformedArguments");
                }
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::ControlAction(types::ControlActionCommand {
                        target: input.target,
                        action: input.action,
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "network_log" => {
                let input: NetworkLogArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    None,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::NetworkLog(
                        types::NetworkLogCommand {
                            clear: input.clear.unwrap_or(true),
                        },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "emulate" => {
                let input: EmulateArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    None,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::Emulate(
                        types::EmulateCommand {
                            viewport: input.viewport,
                            geolocation: input.geolocation,
                            mobile: input.mobile,
                        },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "dialog" => {
                let input: DialogArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    None,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::HandleDialog(
                        types::HandleDialogCommand {
                            action: input.action,
                            timeout_ms: input.timeout_ms,
                        },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "pdf" => {
                let input: PdfArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    None,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::PrintToPdf(
                        types::PrintToPdfCommand {
                            landscape: input.landscape,
                            print_background: input.print_background.unwrap_or(true),
                            scale: input.scale,
                            page_ranges: input.page_ranges,
                        },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "cookie_get" => {
                let input: CookieGetArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    None,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::GetCookies(
                        types::GetCookiesCommand { urls: input.urls },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "cookie_set" => {
                let input: CookieSetArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    None,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::SetCookies(
                        types::SetCookiesCommand {
                            cookies: input.cookies,
                        },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "cookie_delete" => {
                let input: CookieDeleteArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    None,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::DeleteCookies(
                        types::DeleteCookiesCommand {
                            urls: input.urls,
                            names: input.names,
                        },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "extract_structured" => {
                let input: ExtractStructuredArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::ExtractStructured(types::ExtractStructuredCommand {
                        schema: input.schema,
                        purpose: input.purpose,
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "download_url" => {
                let input: DownloadUrlArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::DownloadUrl(types::DownloadUrlCommand {
                        url: input.url,
                        expected_content_type: input.expected_content_type,
                        max_bytes: input.max_bytes,
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "upload_files" => {
                let input: UploadFilesArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::UploadFiles(types::UploadFilesCommand {
                        selector: input.selector.unwrap_or_default(),
                        target: input.target,
                        paths: input.paths,
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "evaluate_javascript" => {
                let input: EvaluateJavaScriptArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::EvaluateJavaScript(types::EvaluateJavaScriptCommand {
                        expression: input.expression,
                        timeout_ms: input.timeout_ms.unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS),
                        await_promise: input.await_promise.unwrap_or(false),
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "checkpoint_save" => {
                let input: CheckpointSaveArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                // The caller names commands, not evidence: each id is resolved
                // against the runtime's journal before the checkpoint is
                // persisted. An id with no journal record, no terminal outcome,
                // or a different owner is rejected rather than silently
                // contributing nothing.
                match self
                    .runtime
                    .resolve_command_evidence(context.clone(), input.evidence_refs)
                    .await
                {
                    Ok(evidence) => self
                        .runtime
                        .checkpoint(context, input.checkpoint, evidence)
                        .await
                        .and_then(to_json),
                    Err(interface_error) => Err(interface_error),
                }
            }
            "workflow_recover" => {
                let input: WorkflowRecoverArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                self.runtime
                    .recover(context, input.workflow_id)
                    .await
                    .and_then(to_json)
            }
            "job_submit" => {
                let Some(jobs) = self.jobs.as_ref() else {
                    return error(id, METHOD_NOT_FOUND, "Method not found", None);
                };
                let input: JobSubmitArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let priority = match input.priority.as_deref() {
                    None => crate::jobs::JobPriorityWire::Normal,
                    Some(raw) => match crate::jobs::JobPriorityWire::parse(raw) {
                        Some(priority) => priority,
                        None => return invalid_params_reason(id, "malformedArguments"),
                    },
                };
                match jobs
                    .submit(
                        &context.principal_id,
                        crate::jobs::JobSubmission {
                            name: input.name,
                            payload: input.payload.unwrap_or(Value::Null),
                            priority,
                            max_retries: input.max_retries.unwrap_or(3),
                            timeout_ms: input.timeout_ms,
                            correlation_id: Some(context.correlation_id.as_uuid().to_string()),
                        },
                    )
                    .await
                {
                    Ok(outcome) => Ok(outcome.to_value()),
                    Err(error) => return job_port_error_response(id, error),
                }
            }
            "job_status" => {
                let Some(jobs) = self.jobs.as_ref() else {
                    return error(id, METHOD_NOT_FOUND, "Method not found", None);
                };
                let input: JobIdArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                match jobs.status(&context.principal_id, &input.job_id).await {
                    Ok(status) => Ok(status.to_value()),
                    Err(error) => return job_port_error_response(id, error),
                }
            }
            "job_cancel" => {
                let Some(jobs) = self.jobs.as_ref() else {
                    return error(id, METHOD_NOT_FOUND, "Method not found", None);
                };
                let input: JobIdArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                match jobs.cancel(&context.principal_id, &input.job_id).await {
                    Ok(()) => Ok(json!({ "cancelled": true, "jobId": input.job_id })),
                    Err(error) => return job_port_error_response(id, error),
                }
            }
            "recovery_status" => {
                let input: RecoveryStatusArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                // Exactly one key. Both would be two different questions in one
                // call, neither is not a question at all; the JSON Schema
                // subset this validator implements cannot say so, and a silent
                // precedence rule would answer a question the caller did not
                // ask.
                match (input.workflow_id, input.session_id) {
                    (Some(workflow_id), None) => self
                        .runtime
                        .recovery_status(context, workflow_id)
                        .await
                        .and_then(to_json),
                    (None, Some(session_id)) => {
                        let limit = input
                            .limit
                            .unwrap_or(MAX_RECOVERABLE_WORKFLOWS)
                            .clamp(1, MAX_RECOVERABLE_WORKFLOWS);
                        self.runtime
                            .workflows_for_session(context, session_id.clone(), limit)
                            .await
                            .map(|workflows| {
                                json!({"sessionId": session_id, "workflows": workflows})
                            })
                    }
                    _ => {
                        return invalid_params_reason(id, "exactlyOneOfWorkflowIdOrSessionId");
                    }
                }
            }
            "events_read" => {
                let input: EventsReadArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                if input.limit == 0 || input.limit > MAX_EVENT_LIMIT {
                    return invalid_params_reason(id, "malformedArguments");
                }
                if let Err(interface_error) = self
                    .authorization
                    .authorize(&context, types::InterfaceOperation::SubscribeEvents)
                {
                    return interface_error_response(id, interface_error);
                }
                let remaining = match (context.deadline - Utc::now()).to_std() {
                    Ok(remaining) => remaining,
                    Err(_) => return invalid_params_reason(id, "malformedArguments"),
                };
                match tokio::time::timeout(
                    remaining,
                    self.events.read_after_for(
                        &context.principal_id,
                        input.cursor.into(),
                        input.limit,
                    ),
                )
                .await
                {
                    Ok(Ok(batch)) => to_json(batch),
                    Ok(Err(gap)) => {
                        return error(
                            id,
                            INTERFACE_ERROR,
                            "Runtime interface error",
                            Some(json!({"eventGap": gap})),
                        )
                    }
                    Err(_) => {
                        return error(
                            id,
                            INTERFACE_ERROR,
                            "Runtime interface error",
                            Some(json!({
                                "interfaceError": {
                                    "code":"deadlineExceeded",
                                    "layer":"interface",
                                    "message":"runtime interface request failed",
                                    "correlationId":context.correlation_id,
                                    "commandId":null,
                                    "retryable":false,
                                    "retryAfterMs":null,
                                    "reconciliationRequired":false,
                                    "requiredCapability":null
                                }
                            })),
                        )
                    }
                }
            }
            _ => unreachable!("availability checked above"),
        };

        match result {
            Ok(value) => self.tool_success(id, value).await,
            Err(interface_error) => interface_error_response(id, interface_error),
        }

    }
}
