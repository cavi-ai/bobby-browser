//! Individually callable reserved CDP method handlers.
use super::super::*;

impl CdpConnection {
    pub(super) async fn handle_audits_enable(
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
                        "enable method takes no parameters",
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
    pub(super) async fn handle_log_enable(
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
                    CdpError::new(CdpErrorCode::InvalidParams, "method takes no parameters"),
                );
            }
            match self.registry.handler(&request.method) {
                Some(Handler::LogEnable) => {
                    self.enable_domain(request.session_id.as_deref(), "Log")
                        .await
                }
                Some(Handler::NetworkEnable) => {
                    self.enable_domain(request.session_id.as_deref(), "Network")
                        .await
                }
                _ => {}
            }
            Ok(json!({}))
        };
        match result {
            Ok(value) => CdpResponse::success(&request, value),
            Err(error) => CdpResponse::failure(&request, runtime_error(error)),
        }
    }
}
