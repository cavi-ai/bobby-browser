//! Text command verification and its local predicates.
use super::*;
pub(super) struct Text;
#[async_trait::async_trait]
impl CommandVerifier for Text {
    async fn verify(
        &self,
        context: VerificationContext<'_>,
        command: &PrimitiveCommand,
        evidence: Vec<Evidence>,
    ) -> Result<Vec<Evidence>, CommandError> {
        let envelope = context.envelope;
        let page_id = envelope.page_id.as_ref();
        let lease = context.lease;
        let typed_on = context.typed_on;
        match command {
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
                // A field the worker read before the Enter that submits it
                // shows the typed text; reading it again races the submit.
                let read_before_submit = submitted_with_enter
                    && observed.is_some_and(|observed| {
                        typed_value_verified(
                            &typed_text,
                            command.clear_first,
                            observed,
                            Some(observed),
                            kind,
                        )
                    });
                let inspected_control = if read_before_submit {
                    Ok(Vec::new())
                } else {
                    lease
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
                        .await
                };
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
                    Evidence::Inspection { text, url, .. } => Some((text.as_str(), url.as_str())),
                    _ => None,
                });
                let matches = if submitted_with_enter {
                    // A read-back from the page the submit landed on shows that
                    // page's field, which the site may fill with its own value.
                    inspected.is_none_or(|(inspected, read_on)| {
                        typed_on
                            .as_deref()
                            .is_some_and(|typed_on| read_on != typed_on)
                            || typed_value_verified(
                                &typed_text,
                                command.clear_first,
                                inspected,
                                observed,
                                kind,
                            )
                    })
                } else {
                    inspected.is_some_and(|(inspected, _)| {
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
            _ => Err(verification_error(
                "command was routed to the wrong verifier family",
            )),
        }
    }
}
/// Whether a TypeText command's post-action state counts as verified.
///
/// `inspected` is the independent post-action read (`Inspection.text`);
/// `observed` is the worker's own post-action element read
/// (`Evidence::Element.text`, first one); `kind` is the worker's
/// `typedControlKind` Configuration value ("text" when the worker did not
/// report one). Exact match always passes; an append (`!clear_first`) passes
/// when the independent read ends with the typed value; a checkable passes
/// when the worker's checked-state read echoes the typed boolean; a select
/// passes when the independent read confirms the option value the worker set.
fn typed_value_verified(
    value: &str,
    clear_first: bool,
    inspected: &str,
    observed: Option<&str>,
    kind: &str,
) -> bool {
    if inspected == value {
        return true;
    }
    if !clear_first && inspected.ends_with(value) {
        return true;
    }
    // A redacted read-back cannot show the value; the control's typed read-back must.
    if inspected == "[redacted]"
        && observed.is_some_and(|observed| {
            observed == value || (!clear_first && observed.ends_with(value))
        })
    {
        return true;
    }
    if value.parse::<bool>().is_ok() && observed == Some(value) {
        return true;
    }
    if kind == "select"
        && observed.is_some_and(|observed| !observed.is_empty() && observed == inspected)
    {
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::typed_value_verified;

    #[test]
    fn exact_match_passes() {
        assert!(typed_value_verified("Ada", true, "Ada", None, "text"));
    }

    #[test]
    fn exact_match_fails_on_mismatch() {
        assert!(!typed_value_verified(
            "ada@example.test",
            true,
            "not-the-typed-value",
            None,
            "text"
        ));
    }

    #[test]
    fn append_without_clear_passes_when_inspected_ends_with_value() {
        assert!(typed_value_verified("x", false, "prefilledx", None, "text"));
    }

    #[test]
    fn append_with_clear_first_does_not_use_suffix_rule() {
        assert!(!typed_value_verified("x", true, "prefilledx", None, "text"));
    }

    #[test]
    fn append_fails_when_inspected_does_not_end_with_value() {
        assert!(!typed_value_verified("x", false, "prefilled", None, "text"));
    }

    #[test]
    fn redacted_read_back_passes_when_the_typed_read_back_matches() {
        assert!(typed_value_verified(
            "pass-41",
            true,
            "[redacted]",
            Some("pass-41"),
            "text"
        ));
        assert!(typed_value_verified(
            "41",
            false,
            "[redacted]",
            Some("pass-41"),
            "text"
        ));
    }

    #[test]
    fn redacted_read_back_fails_without_a_matching_typed_read_back() {
        assert!(!typed_value_verified(
            "pass-41",
            true,
            "[redacted]",
            Some("pass-4"),
            "text"
        ));
        assert!(!typed_value_verified(
            "pass-41",
            true,
            "[redacted]",
            None,
            "text"
        ));
        assert!(!typed_value_verified(
            "true",
            true,
            "[redacted]",
            Some("[redacted]"),
            "checkable"
        ));
    }

    #[test]
    fn checkable_passes_when_observed_echoes_typed_boolean() {
        assert!(typed_value_verified(
            "true",
            true,
            "on",
            Some("true"),
            "checkable"
        ));
    }

    #[test]
    fn checkable_fails_when_observed_does_not_match() {
        assert!(!typed_value_verified(
            "true",
            true,
            "on",
            Some("false"),
            "checkable"
        ));
    }

    #[test]
    fn checkable_fails_when_value_is_not_boolean() {
        assert!(!typed_value_verified(
            "maybe",
            true,
            "on",
            Some("maybe"),
            "checkable"
        ));
    }

    #[test]
    fn select_passes_when_observed_confirms_inspected_option_value() {
        assert!(typed_value_verified(
            "Pro plan",
            true,
            "pro",
            Some("pro"),
            "select"
        ));
    }

    #[test]
    fn select_fails_when_observed_differs_from_inspected() {
        assert!(!typed_value_verified(
            "Pro plan",
            true,
            "pro",
            Some("basic"),
            "select"
        ));
    }

    #[test]
    fn select_fails_when_observed_is_empty() {
        assert!(!typed_value_verified(
            "Pro plan",
            true,
            "pro",
            Some(""),
            "select"
        ));
    }

    #[test]
    fn select_fails_when_observed_is_absent() {
        assert!(!typed_value_verified(
            "Pro plan", true, "pro", None, "select"
        ));
    }
}
