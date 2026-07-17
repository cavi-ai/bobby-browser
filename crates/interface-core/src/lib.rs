mod auth;
mod idempotency;

use async_trait::async_trait;
use chrono::Utc;
use types::{
    Capability, CommandEnvelope, CommandOutcome, CreateSessionRequest, ErrorLayer, Evidence,
    InterfaceError, InterfaceErrorCode, InterfaceOperation, OpenPageRequest, PageState,
    RecoveryDecision, RequestContext, RuntimeInfo, SessionState, WorkflowCheckpoint, WorkflowId,
};

pub use auth::{Authority, AuthorityStore, CapabilityHandle, IssuedToken};
pub use idempotency::{
    canonical_sha256, IdempotencyPermit, IdempotencyReservation, IdempotencyStore,
};

pub type InterfaceResult<T> = Result<T, InterfaceError>;

#[async_trait]
pub trait RuntimeInterface: Send + Sync {
    async fn runtime_info(&self, ctx: RequestContext) -> InterfaceResult<RuntimeInfo>;
    async fn list_sessions(&self, ctx: RequestContext) -> InterfaceResult<Vec<SessionState>>;
    async fn create_session(
        &self,
        ctx: RequestContext,
        req: CreateSessionRequest,
    ) -> InterfaceResult<SessionState>;
    async fn open_page(
        &self,
        ctx: RequestContext,
        req: OpenPageRequest,
    ) -> InterfaceResult<PageState>;
    async fn submit(
        &self,
        ctx: RequestContext,
        envelope: CommandEnvelope,
    ) -> InterfaceResult<CommandOutcome>;
    async fn checkpoint(
        &self,
        ctx: RequestContext,
        checkpoint: WorkflowCheckpoint,
        evidence: Vec<Evidence>,
    ) -> InterfaceResult<WorkflowCheckpoint>;
    async fn recover(
        &self,
        ctx: RequestContext,
        workflow: WorkflowId,
    ) -> InterfaceResult<RecoveryDecision>;
}

#[derive(Clone)]
pub struct AuthorizationGuard {
    authority: CapabilityHandle,
}

impl AuthorizationGuard {
    pub fn new(authority: CapabilityHandle) -> Self {
        Self { authority }
    }

    pub fn authorize(
        &self,
        ctx: &RequestContext,
        operation: InterfaceOperation,
    ) -> InterfaceResult<()> {
        let now = Utc::now();
        if ctx.validate_at(now).is_err() {
            return Err(interface_error(
                ctx,
                InterfaceErrorCode::DeadlineExceeded,
                "request deadline exceeded",
                None,
            ));
        }
        if self.authority.is_invalid_at(now) || ctx.principal_id != *self.authority.principal_id() {
            return Err(interface_error(
                ctx,
                InterfaceErrorCode::AuthenticationFailed,
                "authentication failed",
                None,
            ));
        }
        for capability in operation.required() {
            if !self.authority.allows(*capability) || !ctx.capabilities.contains(*capability) {
                return Err(interface_error(
                    ctx,
                    InterfaceErrorCode::MissingCapability,
                    "required capability is missing",
                    Some(*capability),
                ));
            }
        }
        Ok(())
    }
}

fn interface_error(
    ctx: &RequestContext,
    code: InterfaceErrorCode,
    message: &str,
    required_capability: Option<Capability>,
) -> InterfaceError {
    InterfaceError {
        code,
        layer: ErrorLayer::Interface,
        message: message.to_owned(),
        correlation_id: ctx.correlation_id.clone(),
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: false,
        required_capability,
    }
}
