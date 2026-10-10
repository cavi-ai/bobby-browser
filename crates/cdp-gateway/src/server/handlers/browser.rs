//! Individually callable reserved CDP method handlers.
use super::super::*;

impl CdpConnection {
    pub(super) async fn handle_browser_get_version(
        &self,
        request: CdpRequest,
        ctx: RequestContext,
    ) -> CdpResponse {
        let result = self.runtime.runtime_info(ctx).await.map(|info| json!({
                "protocolVersion": "1.3", "product": "AutomationRuntime/0.1", "revision": info.version,
                "userAgent": "AutomationRuntime/0.1", "jsVersion": "unknown"
            }));
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
    pub(super) async fn handle_browser_set_download_behavior(
        &self,
        request: CdpRequest,
        _ctx: RequestContext,
    ) -> CdpResponse {
        let result = {
            let behavior =
                match domains::browser::validate_download_behavior(request.params.clone()) {
                    Ok(behavior) => behavior,
                    Err(error) => return CdpResponse::failure(&request, error),
                };
            *self.download_events_enabled.lock().await =
                behavior.events_enabled && behavior.behavior != "deny";
            Ok(json!({}))
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
}
