//! Individually callable reserved CDP method handlers.
mod playwright;
mod playwright_click;
mod playwright_download;
mod playwright_fill;
mod playwright_locate;
mod playwright_popup;
mod playwright_upload;
mod puppeteer;
mod puppeteer_download;
mod puppeteer_upload;
use super::super::*;

impl CdpConnection {
    pub(super) async fn handle_runtime_enable(
        &self,
        request: CdpRequest,
        _ctx: RequestContext,
    ) -> CdpResponse {
        let result = {
            if !request
                .params
                .as_object()
                .is_some_and(serde_json::Map::is_empty)
            {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(
                        CdpErrorCode::InvalidParams,
                        "Runtime.enable takes no parameters",
                    ),
                );
            }
            let scope = request.session_id.as_deref().unwrap_or("browser");
            self.enable_domain(request.session_id.as_deref(), "Runtime")
                .await;
            let frame_id = if let Some(session_id) = request.session_id.as_deref() {
                match self
                    .resolve_identifier(IdentifierFamily::CdpSession, session_id)
                    .await
                {
                    Some(target_id) => target_id,
                    None => {
                        return CdpResponse::failure(
                            &request,
                            CdpError::new(CdpErrorCode::InvalidParams, "unknown CDP session"),
                        )
                    }
                }
            } else {
                self.bind_identifier(IdentifierFamily::Frame, scope, "main", RuntimeGeneration(0))
                    .await
            };
            let unique_id = self
                .bind_identifier(
                    IdentifierFamily::ExecutionContext,
                    scope,
                    "default",
                    RuntimeGeneration(0),
                )
                .await;
            if let Err(error) = self.queue_event(CdpEvent {
                    method: "Runtime.executionContextCreated".into(),
                    params: json!({"context":{"id":1,"origin":"","name":"","uniqueId":unique_id,"auxData":{"isDefault":true,"type":"default","frameId":frame_id}}}),
                    session_id: request.session_id.clone(),
                }).await {
                    return CdpResponse::failure(&request, error);
                }
            Ok(json!({}))
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
    pub(super) async fn handle_runtime_evaluate(
        &self,
        request: CdpRequest,
        _ctx: RequestContext,
    ) -> CdpResponse {
        let result = {
            match domains::runtime::bootstrap_injected_script(&request.params) {
                Ok(mut result) => {
                    let class_name = result["result"]["className"].as_str().unwrap_or_default();
                    let internal = if class_name == "InjectedScript" {
                        "playwright-injected-script"
                    } else {
                        "playwright-utility-script"
                    };
                    let opaque = match self
                        .issue_remote_object(request.session_id.as_deref(), internal)
                        .await
                    {
                        Ok(opaque) => opaque,
                        Err(error) => return CdpResponse::failure(&request, error),
                    };
                    result["result"]["objectId"] = Value::String(opaque);
                    Ok(result)
                }
                Err(error) if error.message == "unrecognized bounded runtime bootstrap" => {
                    Ok(domains::runtime::evaluate_exception_result(&error.message))
                }
                Err(error) => return CdpResponse::failure(&request, error),
            }
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
    pub(super) async fn handle_runtime_release_object(
        &self,
        request: CdpRequest,
        _ctx: RequestContext,
    ) -> CdpResponse {
        let result = {
            let Some(id) = request.params.get("objectId").and_then(Value::as_str) else {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(CdpErrorCode::InvalidParams, "missing gateway remote object"),
                );
            };
            if self
                .take_remote_object(request.session_id.as_deref(), id)
                .await
                .is_none()
            {
                return CdpResponse::failure(
                    &request,
                    CdpError::new(
                        CdpErrorCode::InvalidParams,
                        "unknown or stale gateway remote object",
                    ),
                );
            }
            Ok(json!({}))
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
    pub(super) async fn handle_runtime_call_function_on(
        &self,
        request: CdpRequest,
        ctx: RequestContext,
    ) -> CdpResponse {
        const PUPPETEER_TRANSLATOR: &str = "(operation, selector, value) => globalThis.__automationRuntimePuppeteer(operation, selector, value)";
        if request
            .params
            .get("functionDeclaration")
            .and_then(Value::as_str)
            == Some(PUPPETEER_TRANSLATOR)
        {
            self.handle_puppeteer_semantic(request, ctx).await
        } else {
            self.handle_playwright_semantic(request, ctx).await
        }
    }
}
