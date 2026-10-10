//! Firefox observations for the shared wait policy.
use super::{
    accessibility_tree_contains, driver_error, FirefoxCompanionWorker, COMPANION_SANDBOX,
    COMPOSED_QUERY, MAX_URL_BYTES,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use types::{CommandError, ErrorCode, PageId, WaitCondition};
use worker_pool::wait::{
    is_page_scoped_text_target, WaitObservation, WaitObserver, ELEMENT_VISIBILITY_SCRIPT,
};

fn visibility_predicate(hidden: bool) -> String {
    let negate = if hidden { "!" } else { "" };
    format!("el.isConnected&&{negate}(()=>{{{ELEMENT_VISIBILITY_SCRIPT}}})()")
}

pub(super) struct FirefoxWaitObserver<'a> {
    pub(super) worker: &'a FirefoxCompanionWorker,
    pub(super) page_id: &'a PageId,
}

#[async_trait]
impl WaitObserver for FirefoxWaitObserver<'_> {
    async fn observe(&self, condition: &WaitCondition) -> Result<WaitObservation, CommandError> {
        self.worker.observe_wait(self.page_id, condition).await
    }
}

impl FirefoxCompanionWorker {
    async fn observe_wait(
        &self,
        page_id: &PageId,
        condition: &WaitCondition,
    ) -> Result<WaitObservation, CommandError> {
        match condition {
            WaitCondition::Url { .. } => {
                let context = self.context(page_id).await?;
                let response = self
                    .transport
                    .send(
                        "script.evaluate",
                        json!({
                            "expression": "globalThis.location.href",
                            "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                            "awaitPromise": false,
                            "resultOwnership": "none",
                        }),
                    )
                    .await?;
                let url = response
                    .pointer("/result/value")
                    .and_then(Value::as_str)
                    .ok_or_else(|| {
                        driver_error(
                            ErrorCode::BrowserCommandFailed,
                            "Firefox URL wait returned no location",
                            false,
                        )
                    })?;
                if url.len() > MAX_URL_BYTES * 4 {
                    return Err(driver_error(
                        ErrorCode::BrowserCommandFailed,
                        "Firefox URL wait exceeded its bound",
                        false,
                    ));
                }
                Ok(WaitObservation::Url(url.to_owned()))
            }
            WaitCondition::Element { target, state } => {
                let (satisfied, _): (bool, Option<String>) = {
                    if !target.shadow_path.is_empty() {
                        let top_context = self.context(page_id).await?;
                        let context = self.resolve_input_context(&top_context, target).await?;
                        match self.resolve_shadow_element(&context, target).await {
                            Ok(shared_id) => {
                                let condition = match state {
                                    types::ElementState::Attached => "el.isConnected".into(),
                                    types::ElementState::Visible => visibility_predicate(false),
                                    types::ElementState::Detached => "!el.isConnected".into(),
                                    types::ElementState::Enabled => "el.isConnected&&!el.matches(':disabled,[aria-disabled=\"true\"]')".into(),
                                    types::ElementState::Disabled => "el.isConnected&&el.matches(':disabled,[aria-disabled=\"true\"]')".into(),
                                    types::ElementState::Hidden => visibility_predicate(true),
                                };
                                let response = self.transport.send("script.callFunction", json!({
                                    "functionDeclaration": format!("function(el){{return Boolean({condition});}}"),
                                    "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                                    "arguments": [{"sharedId": shared_id}],
                                    "awaitPromise": false,
                                    "resultOwnership": "none",
                                })).await?;
                                (
                                    response
                                        .pointer("/result/value")
                                        .and_then(Value::as_bool)
                                        .unwrap_or(false),
                                    None,
                                )
                            }
                            Err(error)
                                if matches!(
                                    error.code,
                                    ErrorCode::TargetNotFound | ErrorCode::ShadowRootUnavailable
                                ) =>
                            {
                                (matches!(state, types::ElementState::Detached), None)
                            }
                            Err(error) => return Err(error),
                        }
                    } else if *state == types::ElementState::Visible
                        && target.css.is_none()
                        && target.test_id.is_none()
                        && target.frame_path.is_empty()
                        && target.shadow_path.is_empty()
                        && target.attributes.is_empty()
                        && target.ordinal.is_none()
                        && target.accessible_name.is_some()
                        && matches!(
                            target.role.as_deref(),
                            Some(
                                "navigation"
                                    | "main"
                                    | "region"
                                    | "heading"
                                    | "banner"
                                    | "complementary"
                                    | "contentinfo"
                                    | "form"
                                    | "dialog"
                                    | "alert"
                                    | "status"
                            )
                        )
                    {
                        let (nodes, _) = self
                            .observer
                            .a11y_snapshot(&self.current_lease(), page_id, 256, None, false)
                            .await?;
                        (accessibility_tree_contains(&nodes, target), None)
                    } else {
                        let context = self.context(page_id).await?;
                        let resolved = self
                            .resolve_input_target(page_id, &context, "", Some(target))
                            .await;
                        match resolved {
                            Ok((context, selector)) => {
                                let selector =
                                    serde_json::to_string(&selector).map_err(|error| {
                                        driver_error(
                                            ErrorCode::InvalidRequest,
                                            error.to_string(),
                                            false,
                                        )
                                    })?;
                                let expression = match state {
                                types::ElementState::Attached => {
                                    format!("Boolean({COMPOSED_QUERY}({selector}))")
                                }
                                types::ElementState::Visible | types::ElementState::Hidden => {
                                    let predicate = visibility_predicate(*state == types::ElementState::Hidden);
                                    format!("(()=>{{const el={COMPOSED_QUERY}({selector});return Boolean(el)&&({predicate});}})()")
                                }
                                types::ElementState::Detached => {
                                    format!("!{COMPOSED_QUERY}({selector})")
                                }
                                types::ElementState::Enabled => format!("!{COMPOSED_QUERY}({selector})?.matches(':disabled,[aria-disabled=\"true\"]')"),
                                types::ElementState::Disabled => format!("Boolean({COMPOSED_QUERY}({selector})?.matches(':disabled,[aria-disabled=\"true\"]'))"),
                            };
                                let response = self.transport.send("script.evaluate", json!({
                                "expression": expression,
                                "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                                "awaitPromise": false,
                                "resultOwnership": "none",
                            })).await?;
                                let satisfied = response
                                    .pointer("/result/value")
                                    .and_then(Value::as_bool)
                                    .unwrap_or(false);
                                (satisfied, None)
                            }
                            Err(error) if error.code == ErrorCode::TargetNotFound => {
                                (matches!(state, types::ElementState::Detached), None)
                            }
                            Err(error) => return Err(error),
                        }
                    }
                };
                Ok(WaitObservation::Element(satisfied))
            }
            WaitCondition::Text { target, .. } | WaitCondition::Value { target, .. } => {
                let is_value = matches!(condition, WaitCondition::Value { .. });
                if !is_value && is_page_scoped_text_target(target) {
                    let context = self.context(page_id).await?;
                    let context = self.resolve_input_context(&context, target).await?;
                    let response = self.transport.send("script.evaluate", json!({
                            "expression": "document.body ? (document.body.innerText || '') : ''",
                            "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                            "awaitPromise": false,
                            "resultOwnership": "none",
                        })).await?;
                    let value = response
                        .pointer("/result/value")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_owned();
                    Ok(WaitObservation::Text(vec![value]))
                } else {
                    let context = self.context(page_id).await?;
                    let resolved = self
                        .resolve_input_target(page_id, &context, "", Some(target))
                        .await;
                    match resolved {
                        Ok((context, selector)) => {
                            let selector = serde_json::to_string(&selector).map_err(|error| {
                                driver_error(ErrorCode::InvalidRequest, error.to_string(), false)
                            })?;
                            let read = if is_value {
                                format!("{COMPOSED_QUERY}({selector})?.value ?? ''")
                            } else {
                                format!("{COMPOSED_QUERY}({selector})?.innerText ?? ''")
                            };
                            let response = self.transport.send("script.evaluate", json!({
                                "expression": read,
                                "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                                "awaitPromise": false,
                                "resultOwnership": "none",
                            })).await?;
                            let value = response
                                .pointer("/result/value")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_owned();
                            Ok(WaitObservation::Text(vec![value]))
                        }
                        Err(error) if error.code == ErrorCode::TargetNotFound => {
                            Ok(WaitObservation::Pending)
                        }
                        // The click already landed; a matcher-satisfying
                        // candidate among several is still a match, not
                        // an ambiguity the caller must narrow before the
                        // wait can even be evaluated.
                        Err(error) if error.code == ErrorCode::TargetAmbiguous => {
                            let selectors = self
                                .resolve_ambiguous_wait_selectors(page_id, &context, target)
                                .await?;
                            let mut values = Vec::new();
                            for (selector_context, selector) in selectors {
                                let selector_json =
                                    serde_json::to_string(&selector).map_err(|error| {
                                        driver_error(
                                            ErrorCode::InvalidRequest,
                                            error.to_string(),
                                            false,
                                        )
                                    })?;
                                let read = if is_value {
                                    format!("{COMPOSED_QUERY}({selector_json})?.value ?? ''")
                                } else {
                                    format!("{COMPOSED_QUERY}({selector_json})?.innerText ?? ''")
                                };
                                let response = match self.transport.send("script.evaluate", json!({
                                        "expression": read,
                                        "target": {"context": selector_context, "sandbox": COMPANION_SANDBOX},
                                        "awaitPromise": false,
                                        "resultOwnership": "none",
                                    })).await {
                                        Ok(response) => response,
                                        // A candidate that matched at
                                        // collection time can detach (or the
                                        // page can re-render it away) before
                                        // its value is read on this same
                                        // poll. That is "this candidate did
                                        // not match on this poll", not a
                                        // wait failure: skip it and let the
                                        // remaining selectors — or the next
                                        // poll, if every selector here
                                        // failed — decide.
                                        Err(error) => {
                                            tracing::debug!(
                                                selector = %selector,
                                                error = %error.message,
                                                "skipping ambiguous wait selector that failed to read"
                                            );
                                            continue;
                                        }
                                    };
                                let observed = response
                                    .pointer("/result/value")
                                    .and_then(Value::as_str)
                                    .unwrap_or_default()
                                    .to_owned();
                                values.push(observed);
                            }
                            Ok(WaitObservation::Text(values))
                        }
                        Err(error) => Err(error),
                    }
                }
            }
            WaitCondition::Document { .. } => {
                let context = self.context(page_id).await?;
                let response = self
                    .transport
                    .send(
                        "script.evaluate",
                        json!({
                            "expression": "document.readyState",
                            "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                            "awaitPromise": false,
                            "resultOwnership": "none",
                        }),
                    )
                    .await?;
                let state = response
                    .pointer("/result/value")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned();
                Ok(WaitObservation::Document(state))
            }

            WaitCondition::NetworkQuiet {
                ignore_url_substrings,
                ignore_resource_types,
                ignore_long_lived,
                ..
            } => {
                let context = self.context(page_id).await?;
                let filters = worker_pool::NetworkQuietFilters {
                    ignore_url_substrings,
                    ignore_resource_types,
                    ignore_long_lived: *ignore_long_lived,
                };
                let (in_flight, excluded) =
                    self.network_quiet.lock().await.snapshot(&context, &filters);
                Ok(WaitObservation::Network {
                    in_flight,
                    excluded_classes: excluded,
                })
            }
        }
    }
}
