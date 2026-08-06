//! Shared interface layer for every runtime adapter.
//!
//! Defines the authenticated [`RuntimeInterface`] trait, bearer-token
//! [`Authority`], capability [`AuthorizationGuard`], idempotency stores, event
//! fan-out, and session-ownership records. HTTP, MCP, CDP, and ACP all call
//! the same trait so capability and evidence semantics stay aligned.

mod artifacts;
mod auth;
mod events;
mod idempotency;
mod session_ownership;

use async_trait::async_trait;
use chrono::Utc;
use types::{
    Capability, CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest, ErrorLayer,
    Evidence, InterfaceError, InterfaceErrorCode, InterfaceOperation, OpenPageRequest, PageState,
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

/// Authenticated runtime operations every adapter implements.
///
/// Each method takes a [`RequestContext`] carrying the principal, capability
/// set, deadline, and optional idempotency key. Implementations must fail
/// closed on missing capabilities and expired deadlines.
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
    /// Where the retained page context says a described control is.
    ///
    /// `Ok(None)` is a real answer: the context does not know, and the caller
    /// should snapshot. Deliberately not an error, so callers can distinguish
    /// "no answer" from "call failed".
    async fn context_ask(
        &self,
        _ctx: RequestContext,
        _session: SessionId,
        _page: types::PageId,
        _description: String,
    ) -> InterfaceResult<Option<types::ContextAnswer>> {
        Err(InterfaceError {
            code: types::InterfaceErrorCode::UnsupportedOperation,
            layer: types::ErrorLayer::Interface,
            message: "context questions are not supported".into(),
            correlation_id: _ctx.correlation_id,
            command_id: None,
            retryable: false,
            retry_after_ms: None,
            reconciliation_required: false,
            required_capability: None,
        })
    }

    async fn form_snapshot(
        &self,
        _ctx: RequestContext,
        _session: SessionId,
        _page: types::PageId,
        _max_controls: Option<u32>,
    ) -> InterfaceResult<types::FormSnapshot> {
        Err(InterfaceError {
            code: types::InterfaceErrorCode::UnsupportedOperation,
            layer: types::ErrorLayer::Interface,
            message: "form snapshots are not supported".into(),
            correlation_id: types::CorrelationId::new(),
            command_id: None,
            retryable: false,
            retry_after_ms: None,
            reconciliation_required: false,
            required_capability: None,
        })
    }
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
    /// Evidence the runtime itself recorded for already-run commands, resolved
    /// by id rather than authored by the caller.
    ///
    /// The journal these ids resolve against is shared across every principal,
    /// so an implementation MUST verify each referenced command belongs to a
    /// session this principal owns before returning its evidence, the same
    /// guard `checkpoint` applies to `checkpoint.session_id` via
    /// `require_owned_session`. Reject an unowned command; skipping or
    /// substituting it leaks its existence through the response shape.
    async fn resolve_command_evidence(
        &self,
        ctx: RequestContext,
        command_ids: Vec<CommandId>,
    ) -> InterfaceResult<Vec<Evidence>>;
    async fn recover(
        &self,
        ctx: RequestContext,
        workflow: WorkflowId,
    ) -> InterfaceResult<RecoveryDecision>;
    async fn recovery_status(
        &self,
        ctx: RequestContext,
        workflow: WorkflowId,
    ) -> InterfaceResult<types::RecoveryStatus>;
}

/// Capability and identity checks for one authenticated principal.
#[derive(Clone)]
pub struct AuthorizationGuard {
    authority: CapabilityHandle,
}

impl AuthorizationGuard {
    /// Build a guard from a verified [`CapabilityHandle`].
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

    /// Requires a single capability beyond the already-authorized `InterfaceOperation`,
    /// at chokepoints (e.g. `AuthenticatedRuntime::submit`) where a coarse operation-level
    /// capability (`browser:mutate`) does not authorize a privileged primitive nested in
    /// the request (file upload/download). Both the live authority and the request context
    /// must independently carry the capability.
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
