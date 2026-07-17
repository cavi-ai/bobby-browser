use async_trait::async_trait;
use interface_core::{InterfaceResult, RuntimeInterface};
use types::{
    CommandEnvelope, CommandOutcome, CreateSessionRequest, ErrorLayer, Evidence, InterfaceError,
    InterfaceErrorCode, OpenPageRequest, PageState, RecoveryDecision, RequestContext, RuntimeError,
    RuntimeInfo, SessionState, WorkflowCheckpoint, WorkflowId,
};

use crate::RuntimeService;

#[async_trait]
impl RuntimeInterface for RuntimeService {
    async fn runtime_info(&self, _ctx: RequestContext) -> InterfaceResult<RuntimeInfo> {
        Ok(RuntimeService::runtime_info(self).await)
    }

    async fn list_sessions(&self, _ctx: RequestContext) -> InterfaceResult<Vec<SessionState>> {
        Ok(RuntimeService::list_sessions(self).await)
    }

    async fn create_session(
        &self,
        ctx: RequestContext,
        req: CreateSessionRequest,
    ) -> InterfaceResult<SessionState> {
        RuntimeService::create_session(self, req)
            .await
            .map_err(|error| map_runtime_error(&ctx, error))
    }

    async fn open_page(
        &self,
        ctx: RequestContext,
        req: OpenPageRequest,
    ) -> InterfaceResult<PageState> {
        RuntimeService::open_page(self, req)
            .await
            .map_err(|error| map_runtime_error(&ctx, error))
    }

    async fn submit(
        &self,
        _ctx: RequestContext,
        envelope: CommandEnvelope,
    ) -> InterfaceResult<CommandOutcome> {
        Ok(RuntimeService::submit(self, envelope).await)
    }

    async fn checkpoint(
        &self,
        ctx: RequestContext,
        checkpoint: WorkflowCheckpoint,
        evidence: Vec<Evidence>,
    ) -> InterfaceResult<WorkflowCheckpoint> {
        RuntimeService::checkpoint(self, checkpoint, evidence)
            .await
            .map_err(|_| internal_error(&ctx))
    }

    async fn recover(
        &self,
        ctx: RequestContext,
        workflow: WorkflowId,
    ) -> InterfaceResult<RecoveryDecision> {
        RuntimeService::recover(self, &workflow)
            .await
            .map_err(|_| internal_error(&ctx))
    }
}

fn map_runtime_error(ctx: &RequestContext, error: RuntimeError) -> InterfaceError {
    let (code, message) = match error {
        RuntimeError::NotFound(_) => (
            InterfaceErrorCode::NotFound,
            "runtime resource was not found",
        ),
        RuntimeError::InvalidRequest(_) => (
            InterfaceErrorCode::InvalidRequest,
            "runtime request is invalid",
        ),
        RuntimeError::Internal(_) => (InterfaceErrorCode::Internal, "runtime operation failed"),
    };
    error_with(ctx, code, message)
}

fn internal_error(ctx: &RequestContext) -> InterfaceError {
    error_with(
        ctx,
        InterfaceErrorCode::Internal,
        "runtime operation failed",
    )
}

fn error_with(ctx: &RequestContext, code: InterfaceErrorCode, message: &str) -> InterfaceError {
    InterfaceError {
        code,
        layer: ErrorLayer::Interface,
        message: message.to_owned(),
        correlation_id: ctx.correlation_id.clone(),
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: false,
        required_capability: None,
    }
}
