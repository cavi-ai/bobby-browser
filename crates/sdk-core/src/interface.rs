use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use chrono::Utc;
use interface_core::{
    canonical_sha256, AuthorizationGuard, CapabilityHandle, IdempotencyReservation,
    IdempotencyStore, InterfaceResult, RuntimeInterface, SessionOwnershipRecorder,
};
use types::{
    CommandEnvelope, CommandOutcome, CreateSessionRequest, ErrorLayer, Evidence, InterfaceError,
    InterfaceErrorCode, InterfaceOperation, OpenPageRequest, PageState, RecoveryDecision,
    RequestContext, RuntimeError, RuntimeInfo, SessionState, WorkflowCheckpoint, WorkflowId,
};

use crate::RuntimeService;

#[derive(Clone)]
pub struct AuthenticatedRuntime {
    inner: RuntimeService,
    authorization: AuthorizationGuard,
    idempotency: IdempotencyStore,
    submit_dispatches: Arc<AtomicUsize>,
    create_session_dispatches: Arc<AtomicUsize>,
    session_ownership: Option<SessionOwnershipRecorder>,
}

impl AuthenticatedRuntime {
    pub fn new(inner: RuntimeService, authority: CapabilityHandle) -> Self {
        Self::with_idempotency(inner, authority, IdempotencyStore::default())
    }

    pub fn with_idempotency(
        inner: RuntimeService,
        authority: CapabilityHandle,
        idempotency: IdempotencyStore,
    ) -> Self {
        Self {
            inner,
            authorization: AuthorizationGuard::new(authority),
            idempotency,
            submit_dispatches: Arc::new(AtomicUsize::new(0)),
            create_session_dispatches: Arc::new(AtomicUsize::new(0)),
            session_ownership: None,
        }
    }

    pub fn with_session_ownership(
        inner: RuntimeService,
        authority: CapabilityHandle,
        session_ownership: SessionOwnershipRecorder,
    ) -> Self {
        Self {
            inner,
            authorization: AuthorizationGuard::new(authority),
            idempotency: IdempotencyStore::default(),
            submit_dispatches: Arc::new(AtomicUsize::new(0)),
            create_session_dispatches: Arc::new(AtomicUsize::new(0)),
            session_ownership: Some(session_ownership),
        }
    }

    pub fn submit_dispatch_count(&self) -> usize {
        self.submit_dispatches.load(Ordering::Acquire)
    }

    pub fn create_session_dispatch_count(&self) -> usize {
        self.create_session_dispatches.load(Ordering::Acquire)
    }
}

#[async_trait]
impl RuntimeInterface for AuthenticatedRuntime {
    async fn runtime_info(&self, ctx: RequestContext) -> InterfaceResult<RuntimeInfo> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::RuntimeInfo)?;
        Ok(self.inner.runtime_info().await)
    }

    async fn list_sessions(&self, ctx: RequestContext) -> InterfaceResult<Vec<SessionState>> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::ReadSession)?;
        Ok(self.inner.list_sessions().await)
    }

    async fn create_session(
        &self,
        ctx: RequestContext,
        req: CreateSessionRequest,
    ) -> InterfaceResult<SessionState> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::CreateSession)?;
        let ownership_reservation = self
            .session_ownership
            .as_ref()
            .map(|ownership| ownership.reserve(ctx.principal_id.clone()))
            .transpose()
            .map_err(|_| {
                error_with(
                    &ctx,
                    InterfaceErrorCode::ResourceExhausted,
                    "session ownership capacity exhausted",
                )
            })?;
        self.create_session_dispatches
            .fetch_add(1, Ordering::AcqRel);
        let session = match self.inner.create_session(req).await {
            Ok(session) => session,
            Err(error) => {
                drop(ownership_reservation);
                return Err(map_runtime_error(&ctx, error));
            }
        };
        if let Some(reservation) = ownership_reservation {
            if reservation.finalize(session.id.clone()).is_err() {
                let _ = self.inner.sessions.delete(&session.id).await;
                return Err(error_with(
                    &ctx,
                    InterfaceErrorCode::ResourceExhausted,
                    "session ownership finalization failed",
                ));
            }
        }
        Ok(session)
    }

    async fn open_page(
        &self,
        ctx: RequestContext,
        req: OpenPageRequest,
    ) -> InterfaceResult<PageState> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::OpenPage)?;
        self.inner
            .open_page(req)
            .await
            .map_err(|error| map_runtime_error(&ctx, error))
    }

    async fn submit(
        &self,
        ctx: RequestContext,
        envelope: CommandEnvelope,
    ) -> InterfaceResult<CommandOutcome> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::SubmitCommand)?;
        let Some(key) = ctx.idempotency_key.clone() else {
            return Ok(self.inner.submit(envelope).await);
        };
        let digest = canonical_sha256(&envelope)?;
        let reservation = self
            .idempotency
            .reserve(
                ctx.principal_id.clone(),
                key,
                InterfaceOperation::SubmitCommand,
                digest,
                Utc::now(),
                ctx.deadline,
                ctx.correlation_id.clone(),
            )
            .await?;
        match reservation {
            IdempotencyReservation::Replay(outcome) => {
                self.authorization
                    .authorize(&ctx, InterfaceOperation::SubmitCommand)?;
                Ok(outcome)
            }
            IdempotencyReservation::Acquired(permit) => {
                if let Err(error) = self
                    .authorization
                    .authorize(&ctx, InterfaceOperation::SubmitCommand)
                {
                    self.idempotency.abandon(permit).await;
                    return Err(error);
                }
                if let Err(error) = self
                    .authorization
                    .authorize(&ctx, InterfaceOperation::SubmitCommand)
                {
                    self.idempotency.abandon(permit).await;
                    return Err(error);
                }
                self.submit_dispatches.fetch_add(1, Ordering::AcqRel);
                let outcome = self.inner.submit(envelope).await;
                self.idempotency
                    .finish(permit, outcome.clone(), Utc::now())
                    .await?;
                Ok(outcome)
            }
        }
    }

    async fn checkpoint(
        &self,
        ctx: RequestContext,
        checkpoint: WorkflowCheckpoint,
        evidence: Vec<Evidence>,
    ) -> InterfaceResult<WorkflowCheckpoint> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::CreateCheckpoint)?;
        self.inner
            .checkpoint(checkpoint, evidence)
            .await
            .map_err(|_| internal_error(&ctx))
    }

    async fn recover(
        &self,
        ctx: RequestContext,
        workflow: WorkflowId,
    ) -> InterfaceResult<RecoveryDecision> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::RecoverWorkflow)?;
        self.inner
            .recover(&workflow)
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
