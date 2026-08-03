use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use chrono::Utc;
use interface_core::{
    canonical_sha256, AuthorizationGuard, CapabilityHandle, IdempotencyReservation,
    IdempotencyStore, InterfaceResult, RuntimeInterface, SessionCheckpointOutcome,
    SessionOwnershipRecorder,
};
use types::{
    Capability, CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest, ErrorLayer,
    Evidence, FillValue, IntentCommand, InterfaceError, InterfaceErrorCode, InterfaceOperation,
    OpenPageRequest, PageState, PrimitiveCommand, RecoveryDecision, RequestContext, RuntimeCommand,
    RuntimeError, RuntimeInfo, SessionId, SessionState, WorkflowCheckpoint, WorkflowId,
};

use crate::RuntimeService;

#[derive(Clone)]
pub struct AuthenticatedRuntime {
    inner: RuntimeService,
    authorization: AuthorizationGuard,
    idempotency: IdempotencyStore,
    lifecycle_idempotency: IdempotencyStore<SessionCheckpointOutcome>,
    submit_dispatches: Arc<AtomicUsize>,
    create_session_dispatches: Arc<AtomicUsize>,
    checkpoint_dispatches: Arc<AtomicUsize>,
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
            lifecycle_idempotency: IdempotencyStore::default(),
            submit_dispatches: Arc::new(AtomicUsize::new(0)),
            create_session_dispatches: Arc::new(AtomicUsize::new(0)),
            checkpoint_dispatches: Arc::new(AtomicUsize::new(0)),
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
            lifecycle_idempotency: IdempotencyStore::default(),
            submit_dispatches: Arc::new(AtomicUsize::new(0)),
            create_session_dispatches: Arc::new(AtomicUsize::new(0)),
            checkpoint_dispatches: Arc::new(AtomicUsize::new(0)),
            session_ownership: Some(session_ownership),
        }
    }

    pub fn submit_dispatch_count(&self) -> usize {
        self.submit_dispatches.load(Ordering::Acquire)
    }

    pub fn create_session_dispatch_count(&self) -> usize {
        self.create_session_dispatches.load(Ordering::Acquire)
    }

    pub fn checkpoint_dispatch_count(&self) -> usize {
        self.checkpoint_dispatches.load(Ordering::Acquire)
    }

    pub fn capability_handle(&self) -> CapabilityHandle {
        self.authorization.capability_handle()
    }

    fn require_owned_session(
        &self,
        ctx: &RequestContext,
        session: &types::SessionId,
    ) -> InterfaceResult<()> {
        if self.session_ownership.as_ref().is_some_and(|ownership| {
            !ownership.owns_authenticated_session(&ctx.principal_id, session)
        }) {
            return Err(error_with(
                ctx,
                InterfaceErrorCode::NotFound,
                "runtime resource was not found",
            ));
        }
        Ok(())
    }

    async fn dispatch_create_session(
        &self,
        ctx: &RequestContext,
        req: CreateSessionRequest,
    ) -> InterfaceResult<SessionState> {
        // `executionPolicy` flags that change what the browser presents to the
        // page are privileged: the session opt-in alone is not enough, the
        // principal must also hold the matching capability (same double gate
        // as `vision:assist`). Checked before the session exists so a denied
        // principal cannot even materialize a flagged session.
        if req.execution_policy.fingerprint {
            self.authorization
                .require_capability(ctx, Capability::BrowserFingerprint)?;
        }
        if req.execution_policy.humanize {
            self.authorization
                .require_capability(ctx, Capability::BrowserHumanize)?;
        }
        let ownership_reservation = self
            .session_ownership
            .as_ref()
            .map(|ownership| ownership.reserve(ctx.principal_id.clone()))
            .transpose()
            .map_err(|_| {
                error_with(
                    ctx,
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
                return Err(map_runtime_error(ctx, error));
            }
        };
        if let Some(reservation) = ownership_reservation {
            if reservation.finalize(session.id.clone()).is_err() {
                let _ = self.inner.sessions.delete(&session.id).await;
                return Err(error_with(
                    ctx,
                    InterfaceErrorCode::ResourceExhausted,
                    "session ownership finalization failed",
                ));
            }
        }
        Ok(session)
    }

    async fn dispatch_checkpoint(
        &self,
        ctx: &RequestContext,
        checkpoint: WorkflowCheckpoint,
        evidence: Vec<Evidence>,
    ) -> InterfaceResult<WorkflowCheckpoint> {
        self.checkpoint_dispatches.fetch_add(1, Ordering::AcqRel);
        self.inner
            .checkpoint_with_evidence(checkpoint, evidence)
            .await
            .map_err(|_| internal_error(ctx))
    }

    async fn submit_authorized(
        &self,
        ctx: RequestContext,
        envelope: CommandEnvelope,
        one_shot_vision_consent: bool,
    ) -> InterfaceResult<CommandOutcome> {
        self.authorize_submit(&ctx, &envelope.command, one_shot_vision_consent)?;
        self.require_owned_session(&ctx, &envelope.session_id)?;
        // Vision assist is gated at escalation time inside IntentEngine, not upfront.
        // Thread whether this principal holds `vision:assist` so stuck intents can
        // enforce the capability half of the deny-by-default double gate.
        let vision_capability_ok = self
            .authorization
            .capability_handle()
            .capabilities()
            .contains(Capability::VisionAssist)
            && ctx.capabilities.contains(Capability::VisionAssist);
        let Some(key) = ctx.idempotency_key.clone() else {
            return Ok(self
                .inner
                .submit_with_vision_grant(envelope, vision_capability_ok, one_shot_vision_consent)
                .await);
        };
        // Consent changes what the exact same command may do. It is therefore
        // part of the idempotency identity: an ordinary denial must not replay
        // into an approved retry, and an approved result must not replay into
        // a later ordinary submission.
        let digest = if one_shot_vision_consent {
            canonical_sha256(&(&envelope, true))?
        } else {
            canonical_sha256(&envelope)?
        };
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
                self.authorize_submit(&ctx, &envelope.command, one_shot_vision_consent)?;
                self.require_owned_session(&ctx, &envelope.session_id)?;
                Ok(outcome)
            }
            IdempotencyReservation::Acquired(permit) => {
                if let Err(error) = self
                    .authorize_submit(&ctx, &envelope.command, one_shot_vision_consent)
                    .and_then(|()| self.require_owned_session(&ctx, &envelope.session_id))
                {
                    self.idempotency.abandon(permit).await;
                    return Err(error);
                }
                self.submit_dispatches.fetch_add(1, Ordering::AcqRel);
                let outcome = self
                    .inner
                    .submit_with_vision_grant(
                        envelope,
                        vision_capability_ok,
                        one_shot_vision_consent,
                    )
                    .await;
                self.idempotency
                    .finish(permit, outcome.clone(), Utc::now())
                    .await?;
                Ok(outcome)
            }
        }
    }

    fn authorize_submit(
        &self,
        ctx: &RequestContext,
        command: &RuntimeCommand,
        one_shot_vision_consent: bool,
    ) -> InterfaceResult<()> {
        self.authorization
            .authorize(ctx, InterfaceOperation::SubmitCommand)?;
        for capability in command_extra_capabilities(command) {
            self.authorization.require_capability(ctx, capability)?;
        }
        if one_shot_vision_consent {
            self.authorization
                .require_capability(ctx, Capability::VisionAssist)?;
        }
        Ok(())
    }

    /// Submit one intent with the session-policy half of the vision double
    /// gate open for this dispatch only. The authenticated principal must
    /// already hold `vision:assist`; stored session policy is never mutated.
    pub async fn submit_with_one_shot_vision_consent(
        &self,
        ctx: RequestContext,
        envelope: CommandEnvelope,
    ) -> InterfaceResult<CommandOutcome> {
        self.submit_authorized(ctx, envelope, true).await
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
        let sessions = self.inner.list_sessions().await;
        Ok(match &self.session_ownership {
            Some(ownership) => sessions
                .into_iter()
                .filter(|session| {
                    ownership.owns_authenticated_session(&ctx.principal_id, &session.id)
                })
                .collect(),
            None => sessions,
        })
    }

    async fn delete_session(&self, ctx: RequestContext, session: SessionId) -> InterfaceResult<()> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::DeleteSession)?;
        self.require_owned_session(&ctx, &session)?;
        // Read the session's pages before deleting it: afterwards there is no
        // record of which pages it owned, and the context graph would retain
        // their structure for the life of the process. Retention is bounded by
        // session lifetime, so the eviction happens here, at the one point both
        // facts are still available.
        let pages = self
            .inner
            .sessions
            .get(&session)
            .await
            .map(|state| state.page_ids)
            .unwrap_or_default();
        self.inner
            .sessions
            .delete(&session)
            .await
            .map_err(|error| map_runtime_error(&ctx, error))?;
        self.inner.pages.context().forget_all(&pages);
        if let Some(ownership) = &self.session_ownership {
            ownership.release_authenticated_session(&ctx.principal_id, &session);
        }
        Ok(())
    }

    async fn create_session(
        &self,
        ctx: RequestContext,
        req: CreateSessionRequest,
    ) -> InterfaceResult<SessionState> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::CreateSession)?;
        let Some(key) = ctx.idempotency_key.clone() else {
            return self.dispatch_create_session(&ctx, req).await;
        };
        let digest = canonical_sha256(&req)?;
        let reservation = self
            .lifecycle_idempotency
            .reserve(
                ctx.principal_id.clone(),
                key,
                InterfaceOperation::CreateSession,
                digest,
                Utc::now(),
                ctx.deadline,
                ctx.correlation_id.clone(),
            )
            .await?;
        match reservation {
            IdempotencyReservation::Replay(SessionCheckpointOutcome::Session(session)) => {
                Ok(session)
            }
            IdempotencyReservation::Replay(_) => Err(internal_error(&ctx)),
            IdempotencyReservation::Acquired(permit) => {
                match self.dispatch_create_session(&ctx, req).await {
                    Ok(session) => {
                        self.lifecycle_idempotency
                            .finish(
                                permit,
                                SessionCheckpointOutcome::Session(session.clone()),
                                Utc::now(),
                            )
                            .await?;
                        Ok(session)
                    }
                    Err(error) => {
                        self.lifecycle_idempotency.abandon(permit).await;
                        Err(error)
                    }
                }
            }
        }
    }

    async fn open_page(
        &self,
        ctx: RequestContext,
        req: OpenPageRequest,
    ) -> InterfaceResult<PageState> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::OpenPage)?;
        self.require_owned_session(&ctx, &req.session_id)?;
        self.inner
            .open_page(req)
            .await
            .map_err(|error| map_runtime_error(&ctx, error))
    }

    async fn context_ask(
        &self,
        ctx: RequestContext,
        session: SessionId,
        page: types::PageId,
        description: String,
    ) -> InterfaceResult<Option<types::ContextAnswer>> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::ReadPage)?;
        self.require_owned_session(&ctx, &session)?;
        Ok(self.inner.pages.context().ask(&page, &description))
    }

    async fn form_snapshot(
        &self,
        ctx: RequestContext,
        session: SessionId,
        page: types::PageId,
        max_controls: Option<u32>,
    ) -> InterfaceResult<types::FormSnapshot> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::ReadPage)?;
        self.require_owned_session(&ctx, &session)?;
        self.inner
            .form_snapshot(&session, &page, max_controls)
            .await
            .map_err(|error| map_runtime_error(&ctx, error))
    }

    async fn submit(
        &self,
        ctx: RequestContext,
        envelope: CommandEnvelope,
    ) -> InterfaceResult<CommandOutcome> {
        self.submit_authorized(ctx, envelope, false).await
    }

    async fn checkpoint(
        &self,
        ctx: RequestContext,
        checkpoint: WorkflowCheckpoint,
        evidence: Vec<Evidence>,
    ) -> InterfaceResult<WorkflowCheckpoint> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::CreateCheckpoint)?;
        self.require_owned_session(&ctx, &checkpoint.session_id)?;
        let Some(key) = ctx.idempotency_key.clone() else {
            return self.dispatch_checkpoint(&ctx, checkpoint, evidence).await;
        };
        let digest = canonical_sha256(&(&checkpoint, &evidence))?;
        let reservation = self
            .lifecycle_idempotency
            .reserve(
                ctx.principal_id.clone(),
                key,
                InterfaceOperation::CreateCheckpoint,
                digest,
                Utc::now(),
                ctx.deadline,
                ctx.correlation_id.clone(),
            )
            .await?;
        match reservation {
            IdempotencyReservation::Replay(SessionCheckpointOutcome::Checkpoint(checkpoint)) => {
                Ok(checkpoint)
            }
            IdempotencyReservation::Replay(_) => Err(internal_error(&ctx)),
            IdempotencyReservation::Acquired(permit) => {
                match self.dispatch_checkpoint(&ctx, checkpoint, evidence).await {
                    Ok(checkpoint) => {
                        self.lifecycle_idempotency
                            .finish(
                                permit,
                                SessionCheckpointOutcome::Checkpoint(checkpoint.clone()),
                                Utc::now(),
                            )
                            .await?;
                        Ok(checkpoint)
                    }
                    Err(error) => {
                        self.lifecycle_idempotency.abandon(permit).await;
                        Err(error)
                    }
                }
            }
        }
    }

    /// Evidence the runtime recorded for already-run commands, resolved by
    /// command id. Used by the MCP surface's `checkpoint_save`, which names
    /// commands rather than supplying `Evidence` directly — the raw-evidence
    /// path (`checkpoint`, above) remains the HTTP surface's unchanged
    /// contract.
    ///
    /// The journal these ids resolve against is shared across every
    /// authenticated principal (one `RuntimeService`, one `PageRuntime`, per
    /// `broker::bootstrap_listener_with`), so a command id is not itself
    /// proof of ownership — a UUIDv4 being hard to guess is exactly the
    /// assumption `require_owned_session` exists so nothing else has to rely
    /// on. Each referenced command's owning session (from the runtime's own
    /// journal, `PageRuntime::command_session`, never the caller's say-so) is
    /// checked against this principal before its evidence is resolved at
    /// all; a command this principal does not own is rejected with the same
    /// opaque "not found" every other cross-principal lookup in this file
    /// uses, not silently dropped.
    async fn resolve_command_evidence(
        &self,
        ctx: RequestContext,
        command_ids: Vec<CommandId>,
    ) -> InterfaceResult<Vec<Evidence>> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::CreateCheckpoint)?;
        let mut evidence = Vec::new();
        for command_id in command_ids {
            let session_id = self
                .inner
                .pages
                .command_session(&command_id)
                .await
                .map_err(|_| {
                    error_with(
                        &ctx,
                        InterfaceErrorCode::NotFound,
                        "runtime resource was not found",
                    )
                })?;
            self.require_owned_session(&ctx, &session_id)?;
            let items = self
                .inner
                .pages
                .evidence_for_command(command_id)
                .await
                .map_err(|_| {
                    error_with(
                        &ctx,
                        InterfaceErrorCode::NotFound,
                        "runtime resource was not found",
                    )
                })?;
            evidence.extend(items);
        }
        Ok(evidence)
    }

    async fn recovery_status(
        &self,
        ctx: RequestContext,
        workflow: WorkflowId,
    ) -> InterfaceResult<types::RecoveryStatus> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::ReadCheckpoint)?;
        let session_id = self
            .inner
            .recovery_session(&workflow)
            .await
            .map_err(|_| internal_error(&ctx))?;
        self.require_owned_session(&ctx, &session_id)?;
        self.inner
            .recovery_status(&workflow)
            .await
            .map_err(|error| match error {
                page_runtime::RecoveryError::SessionMismatch => error_with(
                    &ctx,
                    InterfaceErrorCode::NotFound,
                    "runtime resource was not found",
                ),
                _ => internal_error(&ctx),
            })
    }

    async fn recover(
        &self,
        ctx: RequestContext,
        workflow: WorkflowId,
    ) -> InterfaceResult<RecoveryDecision> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::RecoverWorkflow)?;
        let session_id = self
            .inner
            .recovery_session(&workflow)
            .await
            .map_err(|_| internal_error(&ctx))?;
        self.require_owned_session(&ctx, &session_id)?;
        self.inner
            .recover_for_session(&workflow, &session_id)
            .await
            .map_err(|error| match error {
                page_runtime::RecoveryError::SessionMismatch => error_with(
                    &ctx,
                    InterfaceErrorCode::NotFound,
                    "runtime resource was not found",
                ),
                _ => internal_error(&ctx),
            })
    }
}

/// Capabilities required beyond `Capability::BrowserMutate` (already enforced by
/// `InterfaceOperation::SubmitCommand`) to submit this command. `SubmitCommand`
/// authorizes the coarse "can mutate the browser" grant only; privileged primitives and
/// all intents need additional, explicit capabilities. The match is exhaustive by variant
/// so that adding a new command forces a deliberate decision here rather than silently
/// inheriting `browser:mutate` as sufficient authorization.
fn command_extra_capabilities(command: &RuntimeCommand) -> Vec<Capability> {
    match command {
        RuntimeCommand::Primitive(PrimitiveCommand::UploadFiles(_)) => {
            vec![Capability::FileUpload]
        }
        RuntimeCommand::Primitive(PrimitiveCommand::ControlAction(command))
            if matches!(command.action, types::ControlAction::SetFiles { .. }) =>
        {
            vec![Capability::FileUpload]
        }
        RuntimeCommand::Primitive(PrimitiveCommand::DownloadUrl(_))
        | RuntimeCommand::Primitive(PrimitiveCommand::ClickAndWaitForDownload(_)) => {
            vec![Capability::FileDownload]
        }
        RuntimeCommand::Primitive(PrimitiveCommand::EvaluateJavaScript(_)) => {
            vec![Capability::JavascriptEvaluate]
        }
        RuntimeCommand::Primitive(PrimitiveCommand::ExtractStructured(_)) => {
            vec![Capability::VisionAssist]
        }
        RuntimeCommand::Primitive(_) => vec![],
        RuntimeCommand::Intent(IntentCommand::Fill(fill))
            if matches!(fill.value, FillValue::Files { .. }) =>
        {
            vec![Capability::IntentExecute, Capability::FileUpload]
        }
        RuntimeCommand::Intent(IntentCommand::CompleteForm(form))
            if form
                .fields
                .iter()
                .any(|field| matches!(field.value, FillValue::Files { .. })) =>
        {
            vec![Capability::IntentExecute, Capability::FileUpload]
        }
        RuntimeCommand::Intent(_) => vec![Capability::IntentExecute],
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
