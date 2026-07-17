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
pub use idempotency::{canonical_sha256, IdempotencyStore};

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
pub struct AuthenticatedRuntime<R> {
    inner: R,
    authority: CapabilityHandle,
    idempotency: IdempotencyStore,
}

impl<R> AuthenticatedRuntime<R> {
    pub fn new(inner: R, authority: CapabilityHandle) -> Self {
        Self {
            inner,
            authority,
            idempotency: IdempotencyStore::default(),
        }
    }

    pub fn with_idempotency(
        inner: R,
        authority: CapabilityHandle,
        idempotency: IdempotencyStore,
    ) -> Self {
        Self {
            inner,
            authority,
            idempotency,
        }
    }

    pub fn into_inner(self) -> R {
        self.inner
    }

    fn authorize(
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

#[async_trait]
impl<R: RuntimeInterface> RuntimeInterface for AuthenticatedRuntime<R> {
    async fn runtime_info(&self, ctx: RequestContext) -> InterfaceResult<RuntimeInfo> {
        self.authorize(&ctx, InterfaceOperation::RuntimeInfo)?;
        self.inner.runtime_info(ctx).await
    }

    async fn list_sessions(&self, ctx: RequestContext) -> InterfaceResult<Vec<SessionState>> {
        self.authorize(&ctx, InterfaceOperation::ReadSession)?;
        self.inner.list_sessions(ctx).await
    }

    async fn create_session(
        &self,
        ctx: RequestContext,
        req: CreateSessionRequest,
    ) -> InterfaceResult<SessionState> {
        self.authorize(&ctx, InterfaceOperation::CreateSession)?;
        self.inner.create_session(ctx, req).await
    }

    async fn open_page(
        &self,
        ctx: RequestContext,
        req: OpenPageRequest,
    ) -> InterfaceResult<PageState> {
        self.authorize(&ctx, InterfaceOperation::OpenPage)?;
        self.inner.open_page(ctx, req).await
    }

    async fn submit(
        &self,
        ctx: RequestContext,
        envelope: CommandEnvelope,
    ) -> InterfaceResult<CommandOutcome> {
        self.authorize(&ctx, InterfaceOperation::SubmitCommand)?;
        let digest = canonical_sha256(&envelope)?;
        if let Some(key) = &ctx.idempotency_key {
            if let Some(outcome) = self.idempotency.lookup_outcome(
                &ctx.principal_id,
                key,
                InterfaceOperation::SubmitCommand,
                digest,
                Utc::now(),
                ctx.correlation_id.clone(),
            )? {
                return Ok(outcome);
            }
        }
        let principal_id = ctx.principal_id.clone();
        let idempotency_key = ctx.idempotency_key.clone();
        let outcome = self.inner.submit(ctx, envelope).await?;
        if let Some(key) = idempotency_key {
            self.idempotency.record_committed_outcome(
                principal_id,
                key,
                InterfaceOperation::SubmitCommand,
                digest,
                outcome.clone(),
                Utc::now(),
            )?;
        }
        Ok(outcome)
    }

    async fn checkpoint(
        &self,
        ctx: RequestContext,
        checkpoint: WorkflowCheckpoint,
        evidence: Vec<Evidence>,
    ) -> InterfaceResult<WorkflowCheckpoint> {
        self.authorize(&ctx, InterfaceOperation::CreateCheckpoint)?;
        self.inner.checkpoint(ctx, checkpoint, evidence).await
    }

    async fn recover(
        &self,
        ctx: RequestContext,
        workflow: WorkflowId,
    ) -> InterfaceResult<RecoveryDecision> {
        self.authorize(&ctx, InterfaceOperation::RecoverWorkflow)?;
        self.inner.recover(ctx, workflow).await
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
