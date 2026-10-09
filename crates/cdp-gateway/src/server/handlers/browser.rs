//! Reserved browser domain handlers.
use super::super::*;

impl CdpConnection {
    pub(super) async fn dispatch_browser(
        &self,
        request: CdpRequest,
        ctx: RequestContext,
        handler: Handler,
    ) -> CdpResponse {
        let result = match handler {
            Handler::BrowserGetVersion => self.runtime.runtime_info(ctx).await.map(|info| json!({
                "protocolVersion": "1.3", "product": "AutomationRuntime/0.1", "revision": info.version,
                "userAgent": "AutomationRuntime/0.1", "jsVersion": "unknown"
            })),
            Handler::BrowserSetDownloadBehavior => {
                let behavior = match domains::browser::validate_download_behavior(request.params.clone()) {
                    Ok(behavior) => behavior,
                    Err(error) => return CdpResponse::failure(&request, error),
                };
                *self.download_events_enabled.lock().await = behavior.events_enabled
                    && behavior.behavior != "deny";
                Ok(json!({}))
            },
            _ => unreachable!("domain routing is exhaustive"),
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
}
