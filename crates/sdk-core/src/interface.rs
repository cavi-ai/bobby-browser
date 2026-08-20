use std::{
    collections::HashSet,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use chrono::Utc;
use interface_core::{
    canonical_sha256, command_identity_sha256, AuthorizationGuard, CapabilityHandle,
    IdempotencyReservation, IdempotencyStore, InterfaceResult, RuntimeInterface,
    SessionCheckpointOutcome, SessionOwnershipRecorder,
};
use types::{
    Capability, CommandEnvelope, CommandId, CommandOutcome, ControlAction, CreateSessionRequest,
    ErrorLayer, Evidence, IntentCommand, InterfaceError, InterfaceErrorCode, InterfaceOperation,
    OpenPageRequest, PageState, PrimitiveCommand, RecoveryDecision, RequestContext, RuntimeCommand,
    RuntimeError, RuntimeInfo, SessionId, SessionState, WorkflowCheckpoint, WorkflowId,
};

use crate::RuntimeService;

/// Capability-checked, idempotent wrapper around [`RuntimeService`].
///
/// Implements [`interface_core::RuntimeInterface`] for HTTP, MCP, CDP, and ACP.
/// Construct with [`Self::new`] and pass to adapter crates; never expose
/// [`RuntimeService`] directly on a public surface.
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
    pub fn operational_metrics(&self) -> observability::OperationalMetrics {
        self.inner.operational_metrics()
    }

    /// Wrap `inner` with the principal's live capability handle.
    pub fn new(inner: RuntimeService, authority: CapabilityHandle) -> Self {
        Self::with_idempotency(inner, authority, IdempotencyStore::default())
    }

    /// Wrap `inner` with capability checks and a shared idempotency store.
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

    /// Wrap `inner` and record session ownership for multi-tenant isolation.
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
        // `executionPolicy` flags that change what the browser presents to the page
        // are double-gated like `vision:assist`: the session opt-in is not enough,
        // the principal must also hold the matching capability. Must run before the
        // session exists so a denied principal cannot materialize a flagged session.
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
        // Identity is what the command does, not which attempt is doing it:
        // `command_id`, `attempt_id`, and `deadline` are minted per attempt, so
        // digesting the whole envelope meant a retry never matched its own
        // first try. Consent is part of the identity because it changes what
        // the same command may do.
        let digest = command_identity_sha256(
            envelope.schema_version,
            &envelope.session_id,
            &envelope.page_id,
            &envelope.command,
            one_shot_vision_consent,
        )?;
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
        // Read the session's pages before deleting it: afterwards there is no record
        // of which pages it owned and the context graph would retain their structure
        // for the life of the process.
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
        let mut pages = pages.into_iter().collect::<HashSet<_>>();
        pages.extend(self.inner.pages.remove_session_pages(&session).await);
        self.inner
            .pages
            .context()
            .forget_all(&pages.into_iter().collect::<Vec<_>>());
        // Session close is the flush point for durable context promotion;
        // flush failures stay session-only and never fail the close.
        if let Some(promotion) = self.inner.pages.context_promotion() {
            promotion.flush().await;
        }
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
        if let Some(answer) = self.inner.pages.context().ask(&page, &description) {
            self.inner
                .operational_metrics()
                .record_context_lookup(observability::ContextLookupOutcome::Hit);
            return Ok(Some(answer));
        }
        // Hot miss: a durable-profile runtime answers from the persisted
        // context graph (cold start); any other runtime behaves as before.
        let Some(promotion) = self.inner.pages.context_promotion() else {
            self.inner
                .operational_metrics()
                .record_context_lookup(observability::ContextLookupOutcome::Miss);
            return Ok(None);
        };
        let url = self
            .inner
            .pages
            .get(&page)
            .await
            .ok()
            .and_then(|page| page.url);
        let answer = promotion.ask(url.as_deref(), &description).await;
        self.inner
            .operational_metrics()
            .record_context_lookup(if answer.is_some() {
                observability::ContextLookupOutcome::Hit
            } else {
                observability::ContextLookupOutcome::Miss
            });
        Ok(answer)
    }

    async fn authorize_operation(
        &self,
        ctx: RequestContext,
        operation: InterfaceOperation,
    ) -> InterfaceResult<()> {
        self.authorization.authorize(&ctx, operation)
    }

    async fn context_site(
        &self,
        ctx: RequestContext,
        site_key: String,
    ) -> InterfaceResult<Option<types::ContextSiteView>> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::ReadContext)?;
        let Some(promotion) = self.inner.pages.context_promotion() else {
            return Ok(None);
        };
        Ok(promotion.site_view(&site_key).await)
    }

    async fn context_neighbors(
        &self,
        ctx: RequestContext,
        session: SessionId,
        page: types::PageId,
        description: String,
    ) -> InterfaceResult<Option<types::ContextNeighbors>> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::ReadContext)?;
        self.require_owned_session(&ctx, &session)?;
        let Some(promotion) = self.inner.pages.context_promotion() else {
            return Ok(None);
        };
        let url = self
            .inner
            .pages
            .get(&page)
            .await
            .ok()
            .and_then(|page| page.url);
        Ok(promotion.neighbors(url.as_deref(), &description).await)
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

    async fn submit_with_auto_checkpoint(
        &self,
        ctx: RequestContext,
        envelope: CommandEnvelope,
    ) -> InterfaceResult<(CommandOutcome, types::CheckpointId)> {
        // Both gates the manual sequence passes: writing a checkpoint and
        // running the command. Sugar must not widen authority.
        self.authorization
            .authorize(&ctx, InterfaceOperation::CreateCheckpoint)?;
        self.authorize_submit(&ctx, &envelope.command, false)?;
        self.require_owned_session(&ctx, &envelope.session_id)?;
        let vision_capability_ok = self
            .authorization
            .capability_handle()
            .capabilities()
            .contains(Capability::VisionAssist)
            && ctx.capabilities.contains(Capability::VisionAssist);
        self.submit_dispatches.fetch_add(1, Ordering::AcqRel);
        self.inner
            .submit_with_auto_checkpoint(envelope, vision_capability_ok, false)
            .await
            .map_err(|error| map_runtime_error(&ctx, error))
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

    /// Evidence the runtime recorded for already-run commands, resolved by command
    /// id. Used by the MCP surface's `checkpoint_save`; `checkpoint` above remains
    /// the HTTP surface's raw-evidence path.
    ///
    /// SECURITY: the journal these ids resolve against is shared across every
    /// authenticated principal, so a command id is not proof of ownership. Each
    /// command's owning session is read from the runtime's own journal
    /// (`PageRuntime::command_session`), never the caller, and checked against this
    /// principal before any evidence is resolved; a command this principal does not
    /// own is rejected with the same opaque "not found" used elsewhere here.
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

    async fn workflows_for_session(
        &self,
        ctx: RequestContext,
        session: types::SessionId,
        limit: usize,
    ) -> InterfaceResult<Vec<WorkflowId>> {
        self.authorization
            .authorize(&ctx, InterfaceOperation::ReadCheckpoint)?;
        // A checkpoint records its session but no principal, so ownership is
        // enforced here against the same registry the single-workflow path
        // uses. A session the caller does not own answers as absence, matching
        // `recovery_status`.
        self.require_owned_session(&ctx, &session)?;
        self.inner
            .workflows_for_session(&session, limit)
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
/// `InterfaceOperation::SubmitCommand`) to submit this command.
///
/// The match must stay exhaustive by variant: a new command that falls through
/// would silently inherit `browser:mutate` as sufficient authorization.
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
            if matches!(fill.value, ControlAction::SetFiles { .. }) =>
        {
            vec![Capability::IntentExecute, Capability::FileUpload]
        }
        RuntimeCommand::Intent(IntentCommand::CompleteForm(form))
            if form
                .fields
                .iter()
                .any(|field| matches!(field.value, ControlAction::SetFiles { .. })) =>
        {
            vec![Capability::IntentExecute, Capability::FileUpload]
        }
        RuntimeCommand::Intent(_) => vec![Capability::IntentExecute],
    }
}

fn map_runtime_error(ctx: &RequestContext, error: RuntimeError) -> InterfaceError {
    // The canonical code classifies the failure; the message must carry the
    // runtime's detail so operators see the actual cause (e.g. "paired
    // profile has no browser target discovery") instead of a generic label.
    let (code, message) = match &error {
        RuntimeError::NotFound(detail) => (
            InterfaceErrorCode::NotFound,
            format!("runtime resource was not found: {detail}"),
        ),
        RuntimeError::InvalidRequest(detail) => (
            InterfaceErrorCode::InvalidRequest,
            format!("runtime request is invalid: {detail}"),
        ),
        // No prefix: the MCP gateway recognizes this message by its leading
        // "browser launch failed:" marker, and a wrapper would hide it.
        RuntimeError::EngineUnreachable(detail) => {
            (InterfaceErrorCode::EngineUnreachable, detail.clone())
        }
        RuntimeError::Internal(detail) => (
            InterfaceErrorCode::Internal,
            format!("runtime operation failed: {detail}"),
        ),
    };
    error_with(ctx, code, &message)
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
