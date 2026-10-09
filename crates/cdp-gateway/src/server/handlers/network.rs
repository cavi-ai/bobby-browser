//! Reserved network domain handlers.
use super::super::*;

impl CdpConnection {
    pub(super) async fn dispatch_network(
        &self,
        request: CdpRequest,
        _ctx: RequestContext,
        handler: Handler,
    ) -> CdpResponse {
        let result = match handler {
            Handler::NetworkSetUserAgent => {
                let Some(params) = request.params.as_object() else {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "user-agent override params must be an object",
                        ),
                    );
                };
                if params.len() != 1
                    || params.get("userAgent").and_then(Value::as_str)
                        != Some("AutomationRuntime/0.1")
                {
                    return CdpResponse::failure(
                        &request,
                        CdpError::new(
                            CdpErrorCode::InvalidParams,
                            "only the exact current runtime user-agent no-op is supported",
                        ),
                    );
                }
                Ok(json!({}))
            }
            _ => unreachable!("domain routing is exhaustive"),
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
}
