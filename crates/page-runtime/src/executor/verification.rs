//! Evidence verification, after execution and context invalidation.
use super::*;

impl PageRuntime {
    pub(super) async fn verify(
        &self,
        envelope: &CommandEnvelope,
        lease: &worker_pool::WorkerLease,
        evidence: Vec<Evidence>,
        typed_on: Option<String>,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page_id = envelope.page_id.as_ref();
        let RuntimeCommand::Primitive(command) = &envelope.command else {
            // IntentEngine owns verify for intent commands.
            if evidence
                .iter()
                .any(|item| matches!(item, Evidence::IntentExecution { .. }))
            {
                return Ok(evidence);
            }
            return Err(verification_error(
                "intent command returned no execution record",
            ));
        };
        match command {
            PrimitiveCommand::Navigate(_) => match evidence.first() {
                Some(Evidence::Navigation { url, .. }) if !url.is_empty() => Ok(evidence),
                _ => Err(verification_error("navigation returned no final URL")),
            },
            PrimitiveCommand::Inspect(_) => {
                if evidence.is_empty() {
                    Err(verification_error("inspection returned no evidence"))
                } else {
                    Ok(evidence)
                }
            }
            PrimitiveCommand::TypeText(command) => {
                let observed = evidence.iter().find_map(|item| match item {
                    Evidence::Element { text, .. } => text.as_deref(),
                    _ => None,
                });
                let kind = evidence
                    .iter()
                    .find_map(|item| match item {
                        Evidence::Configuration { name, value } if name == "typedControlKind" => {
                            Some(value.as_str())
                        }
                        _ => None,
                    })
                    .unwrap_or("text");
                if let Some(verification) = lease
                    .worker()
                    .verify_framed_typed_value(
                        page_id.expect("validated page id"),
                        command,
                        observed,
                        kind,
                    )
                    .await?
                {
                    let mut combined = evidence;
                    combined.extend(verification);
                    return Ok(combined);
                }
                // Enter in the typed text submits the form. The control holds
                // the text without the line break, or no longer exists once
                // the page has navigated; neither is a value mismatch.
                let submitted_with_enter = command.value.contains(['\n', '\r']);
                let typed_text = if submitted_with_enter {
                    command.value.replace(['\n', '\r'], "")
                } else {
                    command.value.clone()
                };
                let page_id = page_id.expect("validated page id");
                let inspected_control = lease
                    .worker()
                    .inspect(
                        page_id,
                        &InspectCommand {
                            selector: (!command.selector.is_empty())
                                .then(|| command.selector.clone()),
                            target: command.target.clone(),
                            include_html: false,
                        },
                    )
                    .await;
                let verification = match inspected_control {
                    Ok(verification) => verification,
                    Err(error)
                        if submitted_with_enter
                            && matches!(
                                error.code,
                                ErrorCode::TargetNotFound | ErrorCode::TargetAmbiguous
                            ) =>
                    {
                        Vec::new()
                    }
                    Err(error) => return Err(error),
                };
                let inspected = verification.iter().find_map(|item| match item {
                    Evidence::Inspection { text, .. } => Some(text.as_str()),
                    _ => None,
                });
                let matches = if submitted_with_enter {
                    inspected.is_none_or(|inspected| {
                        typed_value_verified(
                            &typed_text,
                            command.clear_first,
                            inspected,
                            observed,
                            kind,
                        )
                    })
                } else {
                    inspected.is_some_and(|inspected| {
                        typed_value_verified(
                            &command.value,
                            command.clear_first,
                            inspected,
                            observed,
                            kind,
                        )
                    })
                };
                if matches {
                    let mut combined = evidence;
                    combined.extend(verification);
                    if submitted_with_enter {
                        // Report where the submit landed: the page the agent
                        // is on after the navigation, not the one it typed on.
                        // The signal is the URL moving off the page the field
                        // was typed on (read before the keypress when it could
                        // be), watched for a bounded window. A
                        // single-page app may already have pushed its URL and
                        // keeps rendering after it, so the reported URL and
                        // title are read once the document stops changing.
                        let page_inspect = InspectCommand {
                            selector: None,
                            target: None,
                            include_html: false,
                        };
                        let read_page = |evidence: Vec<Evidence>| {
                            evidence.into_iter().find_map(|item| match item {
                                Evidence::Inspection { url, title, .. } => Some((url, title)),
                                _ => None,
                            })
                        };
                        let typed_on = typed_on.or_else(|| {
                            combined.iter().find_map(|item| match item {
                                Evidence::Inspection { url, .. } => Some(url.clone()),
                                _ => None,
                            })
                        });
                        let window = tokio::time::Instant::now() + ENTER_NAVIGATION_WINDOW;
                        let mut landed: Option<(String, String)>;
                        let mut navigated: bool;
                        loop {
                            landed = lease
                                .worker()
                                .inspect(page_id, &page_inspect)
                                .await
                                .ok()
                                .and_then(read_page);
                            navigated = match (&landed, &typed_on) {
                                (Some((url, _)), Some(typed_on)) => url != typed_on,
                                (Some(_), None) => true,
                                (None, _) => false,
                            };
                            if navigated || tokio::time::Instant::now() >= window {
                                break;
                            }
                            tokio::task::yield_now().await;
                        }
                        if navigated {
                            let _ = lease
                                .worker()
                                .wait_for(
                                    page_id,
                                    &WaitForCommand {
                                        condition: WaitCondition::Document {
                                            ready: types::WaitUntil::Interactive,
                                        },
                                        timeout_ms: 5_000,
                                    },
                                )
                                .await;
                        }
                        let settle_budget = (envelope.deadline - Utc::now())
                            .to_std()
                            .unwrap_or_default()
                            .min(worker_pool::navigation_settle::NAVIGATION_SETTLE_CAP);
                        match lease
                            .worker()
                            .settle_page(page_id, settle_budget, None)
                            .await
                        {
                            Some(settled) => landed = Some(settled),
                            None if navigated => {
                                landed = lease
                                    .worker()
                                    .inspect(page_id, &page_inspect)
                                    .await
                                    .ok()
                                    .and_then(read_page)
                                    .or(landed);
                            }
                            None => {}
                        }
                        if let Some((url, title)) = landed {
                            // One call reports one page. The control read-back
                            // was taken before the submit settled, so it keeps
                            // its typed value and drops its page fields.
                            for item in &mut combined {
                                if let Evidence::Inspection {
                                    url: read_url,
                                    title: read_title,
                                    ..
                                } = item
                                {
                                    read_url.clear();
                                    read_title.clear();
                                }
                            }
                            combined.push(Evidence::Navigation { url, title });
                        }
                    }
                    Ok(combined)
                } else {
                    Err(verification_error("typed value did not match page state"))
                }
            }
            PrimitiveCommand::ControlAction(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::ControlAction { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "control action returned no typed post-action evidence",
                    ))
                }
            }
            PrimitiveCommand::Click(command) => {
                if let Some(expected_url) = &command.expected_url {
                    let page_id = page_id.expect("validated page id");
                    // Settle wait: its failure is tolerated because the
                    // inspect below is the real verification, but it must be
                    // logged -- a silently skipped wait reads as a fast,
                    // flaky expected-URL failure.
                    if let Err(error) = lease
                        .worker()
                        .wait_for(
                            page_id,
                            &WaitForCommand {
                                condition: WaitCondition::Url {
                                    matcher: TextMatch::Exact(expected_url.clone()),
                                },
                                timeout_ms: 5_000,
                            },
                        )
                        .await
                    {
                        tracing::warn!(
                            error = %error.message,
                            "expected-URL settle wait failed before click verification"
                        );
                    }
                    let verification = lease
                        .worker()
                        .inspect(page_id, &InspectCommand::default())
                        .await?;
                    let matches = verification.iter().any(|item| {
                        matches!(item, Evidence::Inspection { url, .. } if url == expected_url)
                    });
                    if !matches {
                        return Err(verification_error("click did not reach expected URL"));
                    }
                    let mut combined = evidence;
                    combined.extend(verification);
                    Ok(combined)
                } else if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Element { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("click returned no target evidence"))
                }
            }
            PrimitiveCommand::UploadFiles(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Upload { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("upload returned no file evidence"))
                }
            }
            PrimitiveCommand::UploadAndConfirm(_) => {
                let uploaded = evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Upload { .. }));
                let confirmed = evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Wait { .. }));
                if uploaded && confirmed {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "upload confirmation returned incomplete evidence",
                    ))
                }
            }
            PrimitiveCommand::AccessibilitySnapshot(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::AccessibilitySnapshot { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "accessibility snapshot returned no snapshot evidence",
                    ))
                }
            }
            PrimitiveCommand::NetworkLog(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::HarArtifact { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("network log returned no HAR artifact"))
                }
            }
            PrimitiveCommand::Emulate(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Emulation { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("emulate command returned no emulation evidence"))
                }
            }
            PrimitiveCommand::HandleDialog(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Dialog { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("dialog command returned no dialog evidence"))
                }
            }
            PrimitiveCommand::PrintToPdf(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::PdfArtifact { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("PDF command returned no PDF artifact"))
                }
            }
            PrimitiveCommand::GetCookies(_)
            | PrimitiveCommand::SetCookies(_)
            | PrimitiveCommand::DeleteCookies(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::CookieState { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("cookie command returned no cookie state"))
                }
            }
            PrimitiveCommand::ExtractStructured(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::StructuredExtraction { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "structured extraction returned no extraction evidence",
                    ))
                }
            }
            PrimitiveCommand::ActivatePage(_) => {
                if evidence.iter().any(|item| {
                    matches!(
                        item,
                        Evidence::Page { .. } | Evidence::BrowserExecution { .. }
                    )
                }) {
                    Ok(evidence)
                } else {
                    Err(verification_error("page activation returned no page evidence"))
                }
            }
            PrimitiveCommand::OpenPage(_) | PrimitiveCommand::ClosePage(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Page { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("page command returned no page evidence"))
                }
            }
            PrimitiveCommand::ListPages(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Pages { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("page listing returned no evidence"))
                }
            }
            PrimitiveCommand::ClickAndWaitForPopup(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Popup { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "popup command returned no popup evidence",
                    ))
                }
            }
            PrimitiveCommand::ClickAndWaitForDownload(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Download { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "download command returned no download evidence",
                    ))
                }
            }
            PrimitiveCommand::DownloadUrl(_) => {
                let download = evidence.iter().find_map(|item| match item {
                    Evidence::Download { bytes, sha256, .. } => Some((*bytes, sha256)),
                    _ => None,
                });
                let execution = evidence.iter().find_map(|item| match item {
                    Evidence::ExecutionPath { bytes, sha256, .. } => Some((*bytes, sha256)),
                    _ => None,
                });
                match (download, execution) {
                    (
                        Some((download_bytes, download_sha)),
                        Some((Some(exec_bytes), Some(exec_sha))),
                    ) if download_bytes == exec_bytes && download_sha == exec_sha => Ok(evidence),
                    _ => Err(verification_error(
                        "download lacks matching durable execution evidence",
                    )),
                }
            }
            PrimitiveCommand::WaitFor(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Wait { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("wait returned no condition evidence"))
                }
            }
            PrimitiveCommand::CaptureScreenshot(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Screenshot { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "screenshot returned no artifact evidence",
                    ))
                }
            }
            PrimitiveCommand::SetFocusEmulation(command) => {
                if evidence.iter().any(|item| matches!(item, Evidence::Configuration { name, value } if name == "focusEmulation" && value == &command.enabled.to_string())) {
                    Ok(evidence)
                } else {
                    Err(verification_error("focus emulation returned no matching configuration evidence"))
                }
            }
            PrimitiveCommand::SetEmulatedMedia(command) => {
                let expected = serde_json::to_string(command).map_err(|_| verification_error("media configuration serialization failed"))?;
                if evidence.iter().any(|item| matches!(item, Evidence::Configuration { name, value } if name == "emulatedMedia" && value == &expected)) {
                    Ok(evidence)
                } else {
                    Err(verification_error("media emulation returned no matching configuration evidence"))
                }
            }
            PrimitiveCommand::EvaluateJavaScript(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::JavaScriptResult { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "javascript evaluation returned no result evidence",
                    ))
                }
            }
        }
    }
}
