//! Pinned Playwright utility-call admission and routing.
use super::super::super::*;
impl CdpConnection {
    pub(super) async fn handle_playwright_semantic(
        &self,
        request: CdpRequest,
        ctx: RequestContext,
    ) -> CdpResponse {
        let Some(utility_id) = request.params.get("objectId").and_then(Value::as_str) else {
            return CdpResponse::failure(
                &request,
                CdpError::new(CdpErrorCode::InvalidParams, "missing gateway remote object"),
            );
        };
        let utility = self
            .resolve_remote_object(request.session_id.as_deref(), utility_id)
            .await;
        let valid_shape = request
            .params
            .get("functionDeclaration")
            .and_then(Value::as_str)
            == Some("(utilityScript, ...args) => utilityScript.evaluate(...args)")
            && utility.as_deref() == Some("playwright-utility-script")
            && request
                .params
                .get("arguments")
                .and_then(Value::as_array)
                .is_some_and(|args| args.len() <= 16);
        if !valid_shape {
            return CdpResponse::success(
                &request,
                domains::runtime::evaluate_exception_result("unrecognized semantic runtime call"),
            );
        }
        let serialized = &request.params["arguments"];
        let locator_handle = self
            .resolve_serialized_object(
                serialized,
                request.session_id.as_deref(),
                "semantic-locator:",
            )
            .await;
        let element_handle = self
            .resolve_serialized_object(
                serialized,
                request.session_id.as_deref(),
                "semantic-element:",
            )
            .await;
        let viewport_poller = self
            .resolve_serialized_object(serialized, request.session_id.as_deref(), "viewport-poller")
            .await;
        let expression = request.params["arguments"]
            .as_array()
            .and_then(|args| args.get(3))
            .and_then(|arg| arg.get("value"))
            .and_then(Value::as_str)
            .unwrap_or("");
        let evaluated_expression = find_serialized_string(serialized, "expression");
        if matches!(expression.trim(), "() => document.title" | "document.title") {
            let title = self
                .verified_page_title(request.session_id.as_deref())
                .await;
            return CdpResponse::success(
                &request,
                json!({"result":{"type":"string","value":title}}),
            );
        }
        if expression.contains("globalThis.eval(expression3)")
            && evaluated_expression.is_some_and(|value| {
                value.contains("window.innerWidth") && value.contains("window.innerHeight")
            })
        {
            let object_id = match self
                .issue_remote_object(request.session_id.as_deref(), "viewport-poller")
                .await
            {
                Ok(object_id) => object_id,
                Err(error) => return CdpResponse::failure(&request, error),
            };
            return CdpResponse::success(
                &request,
                json!({"result":{"type":"object","subtype":"object","className":"Object","description":"Object","objectId":object_id}}),
            );
        }
        if viewport_poller.is_some() && expression.trim() == "(h) => h.result" {
            return CdpResponse::success(
                &request,
                json!({"result":{"type":"string","value":"{\"width\":1280,\"height\":720}"}}),
            );
        }
        if viewport_poller.is_some() && expression.trim() == "(h) => h.abort()" {
            return CdpResponse::success(&request, json!({"result":{"type":"undefined"}}));
        }
        if locator_handle.is_some() && expression.contains("success: r.success") {
            return CdpResponse::success(
                &request,
                json!({"result":{"type":"object","value":{"o":[{"k":"log","v":"semantic target verified"},{"k":"success","v":true}],"id":1}}}),
            );
        }
        if locator_handle.is_some() && expression.contains("visible: r.visible") {
            return CdpResponse::success(
                &request,
                json!({"result":{"type":"object","value":{"o":[{"k":"log","v":"semantic target visible"},{"k":"visible","v":true},{"k":"attached","v":true}],"id":1}}}),
            );
        }
        if let Some(handle) = locator_handle
            .as_deref()
            .filter(|_| expression.trim() == "(r) => r.element")
        {
            let label = handle.trim_start_matches("semantic-locator:");
            let object_id = match self
                .issue_remote_object(
                    request.session_id.as_deref(),
                    &format!("semantic-element:{}", label),
                )
                .await
            {
                Ok(object_id) => object_id,
                Err(error) => return CdpResponse::failure(&request, error),
            };
            return CdpResponse::success(
                &request,
                json!({"result":{"type":"object","subtype":"node","className":"HTMLInputElement","description":"input","objectId":object_id}}),
            );
        }
        if element_handle.is_some() && expression.contains("injected.previewNode(e)") {
            return CdpResponse::success(
                &request,
                json!({"result":{"type":"string","value":"JSHandle@input"}}),
            );
        }
        if let Some(handle) = element_handle.as_deref().filter(|_| {
            expression.contains("injected.retarget(node, \"follow-label\")")
                && expression.contains("HTMLInputElement")
        }) {
            return CdpResponse::success(
                &request,
                json!({"result":{
                    "type":"object", "subtype":"node", "className":"HTMLInputElement",
                    "description":"input", "objectId":match self.issue_remote_object(request.session_id.as_deref(), handle).await {
                        Ok(object_id) => object_id,
                        Err(error) => return CdpResponse::failure(&request, error),
                    }
                }}),
            );
        }
        if let Some(handle) = element_handle.as_deref().filter(|_| {
            expression.trim() == "([injected, node, files]) => injected.setInputFiles(node, files)"
        }) {
            return self
                .playwright_upload(&request, ctx, handle, serialized)
                .await;
        }
        if let Some(handle) = element_handle
            .as_deref()
            .filter(|_| expression.contains("injected.fill(node"))
        {
            return self
                .playwright_fill(&request, ctx, handle, serialized)
                .await;
        }
        if let Some(handle) = element_handle
            .as_deref()
            .filter(|_| expression.contains("checkElementStates"))
        {
            return self.playwright_click(&request, ctx, handle).await;
        }
        self.playwright_locate(&request, ctx, serialized).await
    }
}
