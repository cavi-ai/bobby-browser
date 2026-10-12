//! Firefox observations for the shared wait policy.
use super::{
    accessibility_tree_contains, driver_error, AccessibilitySnapshot, FirefoxCompanionWorker,
    COMPANION_SANDBOX, COMPOSED_QUERY, MAX_URL_BYTES,
};
use async_trait::async_trait;
use serde_json::{json, Value};
use types::{CommandError, ErrorCode, PageId, TargetSpec, WaitCondition};
use worker_pool::wait::{
    bound_element_state, is_page_scoped_text_target, observe_condition, queried_element_state,
    text_read_expression, ElementProbe, TextProbe, WaitDom, WaitObservation, WaitObserver,
};

fn composed_call(selector: &str) -> String {
    format!("{COMPOSED_QUERY}({selector})")
}

pub(super) struct FirefoxWaitObserver<'a> {
    pub(super) worker: &'a FirefoxCompanionWorker,
    pub(super) page_id: &'a PageId,
}

#[async_trait]
impl WaitObserver for FirefoxWaitObserver<'_> {
    async fn observe(&self, condition: &WaitCondition) -> Result<WaitObservation, CommandError> {
        observe_condition(self, condition).await
    }
}

#[async_trait]
impl WaitDom for FirefoxWaitObserver<'_> {
    async fn element(
        &self,
        target: &TargetSpec,
        state: types::ElementState,
    ) -> Result<ElementProbe, CommandError> {
        let worker = self.worker;
        let page_id = self.page_id;
        if !target.shadow_path.is_empty() {
            let top_context = worker.context(page_id).await?;
            let context = worker.resolve_input_context(&top_context, target).await?;
            return match worker.resolve_shadow_element(&context, target).await {
                Ok(shared_id) => {
                    let condition = bound_element_state(state);
                    let response = worker.transport.send("script.callFunction", json!({
                        "functionDeclaration": format!("function(el){{return Boolean({condition});}}"),
                        "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                        "arguments": [{"sharedId": shared_id}],
                        "awaitPromise": false,
                        "resultOwnership": "none",
                    })).await?;
                    Ok(ElementProbe::Satisfied(
                        response
                            .pointer("/result/value")
                            .and_then(Value::as_bool)
                            .unwrap_or(false),
                    ))
                }
                Err(error)
                    if matches!(
                        error.code,
                        ErrorCode::TargetNotFound | ErrorCode::ShadowRootUnavailable
                    ) =>
                {
                    Ok(ElementProbe::Missing)
                }
                Err(error) => Err(error),
            };
        }
        if state == types::ElementState::Visible
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
            let AccessibilitySnapshot { nodes, .. } = worker
                .observer
                .a11y_snapshot(&worker.current_lease(), page_id, 256, None, false)
                .await?;
            return Ok(ElementProbe::Satisfied(accessibility_tree_contains(
                &nodes, target,
            )));
        }
        let context = worker.context(page_id).await?;
        match worker
            .resolve_input_target(page_id, &context, "", Some(target))
            .await
        {
            Ok((context, selector)) => {
                let selector = serde_json::to_string(&selector).map_err(|error| {
                    driver_error(ErrorCode::InvalidRequest, error.to_string(), false)
                })?;
                let expression = queried_element_state(state, &composed_call(&selector));
                let response = worker
                    .transport
                    .send(
                        "script.evaluate",
                        json!({
                            "expression": expression,
                            "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                            "awaitPromise": false,
                            "resultOwnership": "none",
                        }),
                    )
                    .await?;
                Ok(ElementProbe::Satisfied(
                    response
                        .pointer("/result/value")
                        .and_then(Value::as_bool)
                        .unwrap_or(false),
                ))
            }
            Err(error) if error.code == ErrorCode::TargetNotFound => Ok(ElementProbe::Missing),
            Err(error) => Err(error),
        }
    }

    async fn text(&self, target: &TargetSpec, is_value: bool) -> Result<TextProbe, CommandError> {
        let worker = self.worker;
        let page_id = self.page_id;
        if !target.shadow_path.is_empty() {
            let top_context = worker.context(page_id).await?;
            let context = worker.resolve_input_context(&top_context, target).await?;
            return match worker
                .read_shadow_wait_values(&context, target, is_value)
                .await
            {
                Ok(values) => Ok(TextProbe::Values(values)),
                Err(error)
                    if matches!(
                        error.code,
                        ErrorCode::TargetNotFound | ErrorCode::ShadowRootUnavailable
                    ) =>
                {
                    Ok(TextProbe::Missing)
                }
                Err(error) => Err(error),
            };
        }
        if !is_value && is_page_scoped_text_target(target) {
            let context = worker.context(page_id).await?;
            let context = worker.resolve_input_context(&context, target).await?;
            let response = worker
                .transport
                .send(
                    "script.evaluate",
                    json!({
                        "expression": "document.body ? (document.body.innerText || '') : ''",
                        "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                        "awaitPromise": false,
                        "resultOwnership": "none",
                    }),
                )
                .await?;
            let value = response
                .pointer("/result/value")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned();
            return Ok(TextProbe::Values(vec![value]));
        }
        let context = worker.context(page_id).await?;
        match worker
            .resolve_input_target(page_id, &context, "", Some(target))
            .await
        {
            Ok((context, selector)) => {
                let selector = serde_json::to_string(&selector).map_err(|error| {
                    driver_error(ErrorCode::InvalidRequest, error.to_string(), false)
                })?;
                let read = text_read_expression(&composed_call(&selector), is_value);
                let response = worker
                    .transport
                    .send(
                        "script.evaluate",
                        json!({
                            "expression": read,
                            "target": {"context": context, "sandbox": COMPANION_SANDBOX},
                            "awaitPromise": false,
                            "resultOwnership": "none",
                        }),
                    )
                    .await?;
                Ok(
                    match response.pointer("/result/value").and_then(Value::as_str) {
                        Some(value) => TextProbe::Values(vec![value.to_owned()]),
                        None => TextProbe::Missing,
                    },
                )
            }
            Err(error) if error.code == ErrorCode::TargetNotFound => Ok(TextProbe::Missing),
            // The click already landed; a matcher-satisfying candidate
            // among several is still a match, not an ambiguity the
            // caller must narrow before the wait can even be evaluated.
            Err(error) if error.code == ErrorCode::TargetAmbiguous => {
                let selectors = worker
                    .resolve_ambiguous_wait_selectors(page_id, &context, target)
                    .await?;
                let mut values = Vec::new();
                for (selector_context, selector) in selectors {
                    let selector_json = serde_json::to_string(&selector).map_err(|error| {
                        driver_error(ErrorCode::InvalidRequest, error.to_string(), false)
                    })?;
                    let read = text_read_expression(&composed_call(&selector_json), is_value);
                    let response = match worker.transport.send("script.evaluate", json!({
                        "expression": read,
                        "target": {"context": selector_context, "sandbox": COMPANION_SANDBOX},
                        "awaitPromise": false,
                        "resultOwnership": "none",
                    })).await {
                        Ok(response) => response,
                        // A candidate that matched at collection time can
                        // detach before its value is read on this poll.
                        Err(error) => {
                            tracing::debug!(
                                selector = %selector,
                                error = %error.message,
                                "skipping ambiguous wait selector that failed to read"
                            );
                            continue;
                        }
                    };
                    if let Some(observed) =
                        response.pointer("/result/value").and_then(Value::as_str)
                    {
                        values.push(observed.to_owned());
                    }
                }
                Ok(TextProbe::Values(values))
            }
            Err(error) => Err(error),
        }
    }

    async fn url(&self) -> Result<String, CommandError> {
        let context = self.worker.context(self.page_id).await?;
        let response = self
            .worker
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
        Ok(url.to_owned())
    }

    async fn document_ready(&self) -> Result<String, CommandError> {
        let context = self.worker.context(self.page_id).await?;
        let response = self
            .worker
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
        Ok(response
            .pointer("/result/value")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned())
    }

    async fn network(
        &self,
        filters: &worker_pool::NetworkQuietFilters<'_>,
    ) -> Result<(usize, Vec<String>), CommandError> {
        let context = self.worker.context(self.page_id).await?;
        Ok(self
            .worker
            .network_quiet
            .lock()
            .await
            .snapshot(&context, filters))
    }
}
