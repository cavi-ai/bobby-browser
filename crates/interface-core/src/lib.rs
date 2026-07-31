mod artifacts;
mod auth;
mod events;
mod idempotency;
mod session_ownership;

use async_trait::async_trait;
use chrono::Utc;
use types::{
    Capability, CommandEnvelope, CommandOutcome, CreateSessionRequest, ErrorLayer, Evidence,
    InterfaceError, InterfaceErrorCode, InterfaceOperation, OpenPageRequest, PageState,
    RecoveryDecision, RequestContext, RuntimeInfo, SessionId, SessionState, WorkflowCheckpoint,
    WorkflowId,
};

pub use artifacts::{
    ArtifactBoundaryTestObserver, ArtifactContent, ArtifactOwnershipLimits,
    ArtifactPersistenceTestAction, ArtifactReader, ArtifactReaderInitError, ArtifactReference,
};
pub use auth::{Authority, AuthorityStore, CapabilityHandle, IssuedToken};
pub use events::{
    Event, EventBatch, EventGap, EventGapReason, EventStore, MAX_EVENT_PAYLOAD_BYTES,
    MAX_EVENT_PAYLOAD_NODES,
};
pub use idempotency::{
    canonical_sha256, IdempotencyPermit, IdempotencyReservation, IdempotencyStore, RetainedOutcome,
    SessionCheckpointOutcome,
};
pub use session_ownership::{
    SessionOwnershipAuthority, SessionOwnershipRecordError, SessionOwnershipRecorder,
    SessionOwnershipRegistry, SessionOwnershipReservation,
};

pub type InterfaceResult<T> = Result<T, InterfaceError>;

#[async_trait]
pub trait RuntimeInterface: Send + Sync {
    async fn runtime_info(&self, ctx: RequestContext) -> InterfaceResult<RuntimeInfo>;
    async fn list_sessions(&self, ctx: RequestContext) -> InterfaceResult<Vec<SessionState>>;
    async fn delete_session(&self, ctx: RequestContext, session: SessionId) -> InterfaceResult<()>;
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
        self.validate(ctx)?;
        for capability in operation.required() {
            if !self.authority.allows(*capability) || !ctx.capabilities.contains(*capability) {
                tracing::warn!(capability = ?capability, "authz.capability_denied");
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

    /// Requires a single capability beyond whatever `InterfaceOperation` was already
    /// authorized. Used at chokepoints (e.g. `AuthenticatedRuntime::submit`) where a
    /// coarse operation-level capability (`browser:mutate`) is not sufficient on its own
    /// to authorize a privileged primitive nested inside the request (file upload/download).
    /// Mirrors the per-capability check in `authorize`: both the live authority and the
    /// request context must independently carry the capability.
    pub fn require_capability(
        &self,
        ctx: &RequestContext,
        capability: Capability,
    ) -> InterfaceResult<()> {
        if !self.authority.allows(capability) || !ctx.capabilities.contains(capability) {
            tracing::warn!(capability = ?capability, "authz.capability_denied");
            return Err(interface_error(
                ctx,
                InterfaceErrorCode::MissingCapability,
                "required capability is missing",
                Some(capability),
            ));
        }
        Ok(())
    }

    /// Validates the live credential and request identity without selecting an operation.
    /// Enumeration boundaries use this before filtering entries by capability.
    pub fn validate(&self, ctx: &RequestContext) -> InterfaceResult<()> {
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
        Ok(())
    }

    pub fn capability_handle(&self) -> CapabilityHandle {
        self.authority.clone()
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
