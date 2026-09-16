//! The ACP stdio server: an editor drives the runtime as a fourth adapter.
//!
//! Wire scope: `initialize`, `session/new`, `session/prompt`,
//! `session/cancel`, and agent→client `session/update` plus
//! `session/request_permission`. A prompt is either a structured automation
//! request or a named context, checkpoint, or recovery operation. Freeform
//! natural language is rejected: there is no planner.
//!
//! Every run goes through the same `AuthenticatedRuntime` the other adapters
//! use, so capability, idempotency, evidence, and outcome semantics cannot
//! drift. The permission path is decided by [`crate::escalation`]: a click can
//! lift a session gate, never mint authority.

use std::collections::HashMap;
use std::future::Future;
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, CloseSessionRequest, CloseSessionResponse, ContentBlock,
    ContentChunk, InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse,
    PermissionOption, PermissionOptionKind, PromptCapabilities, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, SessionCapabilities,
    SessionCloseCapabilities, SessionNotification, SessionUpdate, StopReason, TextContent,
    ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Result as AcpResult, Stdio};
use async_trait::async_trait;
use chrono::{Duration, Utc};
use interface_core::{CapabilityHandle, InterfaceResult, RuntimeInterface};
use sdk_core::AuthenticatedRuntime;
use tokio::sync::Mutex;
use types::{
    AttemptId, Capability, ClosePageCommand, CommandEnvelope, CommandId, CommandOutcome,
    CreateSessionRequest, Evidence, IntentCommand, NavigateCommand, OpenPageRequest, PageId,
    PageState, RecoveryDecision, RuntimeCommand, SessionId, SessionState, WaitUntil,
    WorkflowCheckpoint, WorkflowId,
};

use crate::escalation::{decide, Escalation, EscalationRequest, SessionPolicyGates};

/// What a `session/prompt` text block must decode to.
#[derive(Debug, serde::Deserialize)]
#[serde(untagged)]
enum StructuredPrompt {
    Operation(PromptOperation),
    Automation(Box<AutomationPrompt>),
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct AutomationPrompt {
    #[serde(default)]
    url: Option<String>,
    intent: IntentCommand,
    #[serde(default)]
    workflow_id: Option<WorkflowId>,
}

#[derive(Debug, serde::Deserialize)]
#[serde(tag = "operation", deny_unknown_fields)]
enum PromptOperation {
    #[serde(rename = "contextAsk")]
    ContextAsk { description: String },
    #[serde(rename = "contextNeighbors")]
    ContextNeighbors { description: String },
    #[serde(rename = "contextSite")]
    ContextSite {
        #[serde(rename = "siteKey")]
        site_key: String,
    },
    #[serde(rename = "checkpointSave")]
    CheckpointSave {
        checkpoint: Box<WorkflowCheckpoint>,
        #[serde(rename = "evidenceRefs", default)]
        evidence_refs: Vec<CommandId>,
    },
    #[serde(rename = "recoveryStatus")]
    RecoveryStatus {
        #[serde(rename = "workflowId", default)]
        workflow_id: Option<WorkflowId>,
        #[serde(default)]
        limit: Option<usize>,
    },
    #[serde(rename = "workflowRecover")]
    WorkflowRecover {
        #[serde(rename = "workflowId")]
        workflow_id: WorkflowId,
    },
}

#[derive(Default)]
struct PromptTurn {
    state: std::sync::Mutex<PromptTurnState>,
    idle: tokio::sync::Notify,
}

#[derive(Default)]
struct PromptTurnState {
    active: bool,
    closed: bool,
    cancel: Option<tokio::sync::watch::Sender<bool>>,
}

struct PromptLease {
    turn: Arc<PromptTurn>,
    cancelled: tokio::sync::watch::Receiver<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TurnCancelled;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BeginTurnError {
    Active,
    Closed,
}

enum PromptStepError {
    Cancelled,
    Failed(agent_client_protocol::Error),
}

impl PromptTurn {
    fn begin(self: &Arc<Self>) -> Result<PromptLease, BeginTurnError> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => {
                let mut state = poisoned.into_inner();
                state.closed = true;
                if let Some(cancel) = &state.cancel {
                    let _ = cancel.send(true);
                }
                return Err(BeginTurnError::Closed);
            }
        };
        if state.closed {
            return Err(BeginTurnError::Closed);
        }
        if state.active {
            return Err(BeginTurnError::Active);
        }
        let (cancel, cancelled) = tokio::sync::watch::channel(false);
        state.active = true;
        state.cancel = Some(cancel);
        Ok(PromptLease {
            turn: Arc::clone(self),
            cancelled,
        })
    }

    fn cancel(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(cancel) = &state.cancel {
            let _ = cancel.send(true);
        }
        if state.closed {
            state.cancel = None;
        }
    }

    fn close(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.closed = true;
        if let Some(cancel) = &state.cancel {
            let _ = cancel.send(true);
        }
    }

    async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            if !self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .active
            {
                return;
            }
            notified.await;
        }
    }
}

impl PromptLease {
    fn is_cancelled(&self) -> bool {
        *self.cancelled.borrow()
    }

    async fn wait<F, T>(&mut self, future: F) -> Result<T, TurnCancelled>
    where
        F: Future<Output = T>,
    {
        if self.is_cancelled() {
            return Err(TurnCancelled);
        }
        tokio::select! {
            biased;
            changed = self.cancelled.changed() => {
                let _ = changed;
                Err(TurnCancelled)
            }
            value = future => Ok(value),
        }
    }
}

impl Drop for PromptLease {
    fn drop(&mut self) {
        let mut state = self
            .turn
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.active = false;
        state.cancel = None;
        drop(state);
        self.turn.idle.notify_waiters();
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PermissionDecision {
    Approved,
    Rejected,
    Cancelled,
}

fn permission_decision(outcome: &RequestPermissionOutcome) -> PermissionDecision {
    match outcome {
        RequestPermissionOutcome::Cancelled => PermissionDecision::Cancelled,
        RequestPermissionOutcome::Selected(selected)
            if selected.option_id.0.as_ref() == "allow" =>
        {
            PermissionDecision::Approved
        }
        RequestPermissionOutcome::Selected(_) => PermissionDecision::Rejected,
        _ => PermissionDecision::Rejected,
    }
}

fn permission_tool_call_id() -> String {
    format!("vision-escalation-{}", uuid::Uuid::new_v4())
}

struct AcpSession {
    runtime_session: SessionId,
    page: Option<PageId>,
    turn: Arc<PromptTurn>,
}

struct AcpCommandScope {
    runtime_session: SessionId,
    page: PageId,
    workflow_id: WorkflowId,
}

#[async_trait]
trait AcpRuntime: Send + Sync {
    async fn create_session(
        &self,
        ctx: types::RequestContext,
        request: CreateSessionRequest,
    ) -> InterfaceResult<SessionState>;
    async fn delete_session(
        &self,
        ctx: types::RequestContext,
        session: SessionId,
    ) -> InterfaceResult<()>;
    async fn open_page(
        &self,
        ctx: types::RequestContext,
        request: OpenPageRequest,
    ) -> InterfaceResult<PageState>;
    async fn submit(
        &self,
        ctx: types::RequestContext,
        envelope: CommandEnvelope,
    ) -> InterfaceResult<CommandOutcome>;
    async fn submit_with_one_shot_vision_consent(
        &self,
        ctx: types::RequestContext,
        envelope: CommandEnvelope,
    ) -> InterfaceResult<CommandOutcome>;
    async fn context_ask(
        &self,
        ctx: types::RequestContext,
        session: SessionId,
        page: PageId,
        description: String,
    ) -> InterfaceResult<Option<types::ContextAnswer>>;
    async fn context_neighbors(
        &self,
        ctx: types::RequestContext,
        session: SessionId,
        page: PageId,
        description: String,
    ) -> InterfaceResult<Option<types::ContextNeighbors>>;
    async fn context_site(
        &self,
        ctx: types::RequestContext,
        site_key: String,
    ) -> InterfaceResult<Option<types::ContextSiteView>>;
    async fn resolve_command_evidence(
        &self,
        ctx: types::RequestContext,
        command_ids: Vec<CommandId>,
    ) -> InterfaceResult<Vec<Evidence>>;
    async fn checkpoint(
        &self,
        ctx: types::RequestContext,
        checkpoint: WorkflowCheckpoint,
        evidence: Vec<Evidence>,
    ) -> InterfaceResult<WorkflowCheckpoint>;
    async fn recovery_status(
        &self,
        ctx: types::RequestContext,
        workflow: WorkflowId,
    ) -> InterfaceResult<types::RecoveryStatus>;
    async fn workflows_for_session(
        &self,
        ctx: types::RequestContext,
        session: SessionId,
        limit: usize,
    ) -> InterfaceResult<Vec<WorkflowId>>;
    async fn recover(
        &self,
        ctx: types::RequestContext,
        workflow: WorkflowId,
    ) -> InterfaceResult<RecoveryDecision>;
}

#[async_trait]
impl AcpRuntime for AuthenticatedRuntime {
    async fn create_session(
        &self,
        ctx: types::RequestContext,
        request: CreateSessionRequest,
    ) -> InterfaceResult<SessionState> {
        RuntimeInterface::create_session(self, ctx, request).await
    }

    async fn delete_session(
        &self,
        ctx: types::RequestContext,
        session: SessionId,
    ) -> InterfaceResult<()> {
        RuntimeInterface::delete_session(self, ctx, session).await
    }

    async fn open_page(
        &self,
        ctx: types::RequestContext,
        request: OpenPageRequest,
    ) -> InterfaceResult<PageState> {
        RuntimeInterface::open_page(self, ctx, request).await
    }

    async fn submit(
        &self,
        ctx: types::RequestContext,
        envelope: CommandEnvelope,
    ) -> InterfaceResult<CommandOutcome> {
        RuntimeInterface::submit(self, ctx, envelope).await
    }

    async fn submit_with_one_shot_vision_consent(
        &self,
        ctx: types::RequestContext,
        envelope: CommandEnvelope,
    ) -> InterfaceResult<CommandOutcome> {
        AuthenticatedRuntime::submit_with_one_shot_vision_consent(self, ctx, envelope).await
    }

    async fn context_ask(
        &self,
        ctx: types::RequestContext,
        session: SessionId,
        page: PageId,
        description: String,
    ) -> InterfaceResult<Option<types::ContextAnswer>> {
        RuntimeInterface::context_ask(self, ctx, session, page, description).await
    }

    async fn context_neighbors(
        &self,
        ctx: types::RequestContext,
        session: SessionId,
        page: PageId,
        description: String,
    ) -> InterfaceResult<Option<types::ContextNeighbors>> {
        RuntimeInterface::context_neighbors(self, ctx, session, page, description).await
    }

    async fn context_site(
        &self,
        ctx: types::RequestContext,
        site_key: String,
    ) -> InterfaceResult<Option<types::ContextSiteView>> {
        RuntimeInterface::context_site(self, ctx, site_key).await
    }

    async fn resolve_command_evidence(
        &self,
        ctx: types::RequestContext,
        command_ids: Vec<CommandId>,
    ) -> InterfaceResult<Vec<Evidence>> {
        RuntimeInterface::resolve_command_evidence(self, ctx, command_ids).await
    }

    async fn checkpoint(
        &self,
        ctx: types::RequestContext,
        checkpoint: WorkflowCheckpoint,
        evidence: Vec<Evidence>,
    ) -> InterfaceResult<WorkflowCheckpoint> {
        RuntimeInterface::checkpoint(self, ctx, checkpoint, evidence).await
    }

    async fn recovery_status(
        &self,
        ctx: types::RequestContext,
        workflow: WorkflowId,
    ) -> InterfaceResult<types::RecoveryStatus> {
        RuntimeInterface::recovery_status(self, ctx, workflow).await
    }

    async fn workflows_for_session(
        &self,
        ctx: types::RequestContext,
        session: SessionId,
        limit: usize,
    ) -> InterfaceResult<Vec<WorkflowId>> {
        RuntimeInterface::workflows_for_session(self, ctx, session, limit).await
    }

    async fn recover(
        &self,
        ctx: types::RequestContext,
        workflow: WorkflowId,
    ) -> InterfaceResult<RecoveryDecision> {
        RuntimeInterface::recover(self, ctx, workflow).await
    }
}

/// The running server: one `AuthenticatedRuntime` plus the ACP→runtime
/// session map. The runtime session id is reused as the ACP session id so an
/// editor-side handle names the same thing the other surfaces do.
#[derive(Clone)]
pub struct AcpServer {
    runtime: Arc<dyn AcpRuntime>,
    capability_handle: CapabilityHandle,
    principal_capabilities: Arc<Vec<Capability>>,
    sessions: Arc<Mutex<HashMap<String, AcpSession>>>,
    retired_sessions: Arc<Mutex<HashMap<String, AcpSession>>>,
    lifecycle: Arc<Mutex<()>>,
    shutting_down: Arc<std::sync::atomic::AtomicBool>,
}

impl AcpServer {
    /// Build a server from an [`AuthenticatedRuntime`] and the principal's
    /// capability set (used to decide permission prompts).
    pub fn new(
        runtime: Arc<AuthenticatedRuntime>,
        principal_capabilities: Vec<Capability>,
    ) -> Self {
        let capability_handle = runtime.capability_handle();
        Self::with_runtime(runtime, capability_handle, principal_capabilities)
    }

    fn with_runtime(
        runtime: Arc<dyn AcpRuntime>,
        capability_handle: CapabilityHandle,
        principal_capabilities: Vec<Capability>,
    ) -> Self {
        Self {
            runtime,
            capability_handle,
            principal_capabilities: Arc::new(principal_capabilities),
            sessions: Arc::new(Mutex::new(HashMap::new())),
            retired_sessions: Arc::new(Mutex::new(HashMap::new())),
            lifecycle: Arc::new(Mutex::new(())),
            shutting_down: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        }
    }

    fn ctx(&self) -> types::RequestContext {
        // The stdio gateway is single-principal: the startup credential the
        // process enrolled. Requests carry no idempotency key; each prompt
        // step mints its own command id.
        self.capability_handle
            .context(Utc::now() + Duration::minutes(5), None)
    }

    /// Serve stdin/stdout until the client disconnects.
    pub async fn serve(self) -> AcpResult<()> {
        let server = self.clone();
        let prompt_server = self.clone();
        let close_server = self.clone();
        let cancel_server = self.clone();
        let disconnect_server = self;
        let transport_result = Agent
            .builder()
            .name("bobby-browser")
            .on_receive_request(
                async move |initialize: InitializeRequest, responder, _connection| {
                    responder.respond(
                        InitializeResponse::new(initialize.protocol_version).agent_capabilities(
                            AgentCapabilities::new()
                                .prompt_capabilities(PromptCapabilities::new())
                                .session_capabilities(
                                    SessionCapabilities::new()
                                        .close(SessionCloseCapabilities::new()),
                                ),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_request: NewSessionRequest, responder, connection| {
                    let task_server = server.clone();
                    connection.spawn(async move {
                        match task_server.new_session().await {
                            Ok(response) => responder.respond(response),
                            Err(error) => responder.respond_with_error(error),
                        }
                    })?;
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: PromptRequest, responder, connection| {
                    let task_server = prompt_server.clone();
                    let task_connection = connection.clone();
                    connection.spawn(async move {
                        match task_server.prompt(request, &task_connection).await {
                            Ok(response) => responder.respond(response),
                            Err(error) => responder.respond_with_error(error),
                        }
                    })?;
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: CloseSessionRequest, responder, connection| {
                    let task_server = close_server.clone();
                    connection.spawn(async move {
                        match task_server
                            .close_session(&request.session_id.to_string())
                            .await
                        {
                            Ok(response) => responder.respond(response),
                            Err(error) => responder.respond_with_error(error),
                        }
                    })?;
                    Ok(())
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_notification(
                async move |notification: CancelNotification, _cx| {
                    cancel_server
                        .cancel(&notification.session_id.to_string())
                        .await;
                    Ok(())
                },
                agent_client_protocol::on_receive_notification!(),
            )
            .connect_to(Stdio::new())
            .await;
        let cleanup_result = disconnect_server.cleanup_sessions().await;
        transport_result?;
        cleanup_result
    }

    async fn new_session(&self) -> Result<NewSessionResponse, agent_client_protocol::Error> {
        let _lifecycle = self.lifecycle.lock().await;
        if self
            .shutting_down
            .load(std::sync::atomic::Ordering::Acquire)
        {
            return Err(invalid_request("gateway is shutting down"));
        }
        let session = self
            .runtime
            .create_session(
                self.ctx(),
                CreateSessionRequest {
                    profile: "acp".into(),
                    proxy: None,
                    execution_policy: Default::default(),
                    // ACP sessions run plain: no godmode ladder.
                    zigzagzig: false,
                },
            )
            .await
            .map_err(internal_error)?;
        let acp_session_id = session.id.0.to_string();
        self.sessions.lock().await.insert(
            acp_session_id.clone(),
            AcpSession {
                runtime_session: session.id,
                page: None,
                turn: Arc::new(PromptTurn::default()),
            },
        );
        Ok(NewSessionResponse::new(
            agent_client_protocol::schema::v1::SessionId::new(acp_session_id),
        ))
    }

    async fn cancel(&self, acp_session_id: &str) {
        if let Some(session) = self.sessions.lock().await.get(acp_session_id) {
            session.turn.cancel();
        }
    }

    async fn close_session(
        &self,
        acp_session_id: &str,
    ) -> Result<CloseSessionResponse, agent_client_protocol::Error> {
        let _lifecycle = self.lifecycle.lock().await;
        let session = if let Some(session) = self.sessions.lock().await.remove(acp_session_id) {
            session
        } else {
            self.retired_sessions
                .lock()
                .await
                .remove(acp_session_id)
                .ok_or_else(|| invalid_request("unknown session; it may already be closed"))?
        };
        session.turn.close();
        session.turn.wait_idle().await;
        if let Err(error) = self
            .runtime
            .delete_session(self.ctx(), session.runtime_session.clone())
            .await
        {
            self.retired_sessions
                .lock()
                .await
                .insert(acp_session_id.to_owned(), session);
            return Err(internal_error(error));
        }
        Ok(CloseSessionResponse::new())
    }

    async fn cleanup_sessions(&self) -> AcpResult<()> {
        let _lifecycle = self.lifecycle.lock().await;
        self.shutting_down
            .store(true, std::sync::atomic::Ordering::Release);
        let mut sessions: Vec<AcpSession> =
            self.sessions.lock().await.drain().map(|(_, v)| v).collect();
        sessions.extend(self.retired_sessions.lock().await.drain().map(|(_, v)| v));
        for session in &sessions {
            session.turn.close();
        }
        let mut cleanup_tasks = tokio::task::JoinSet::new();
        for session in sessions {
            let runtime = Arc::clone(&self.runtime);
            let ctx = self.ctx();
            cleanup_tasks.spawn(async move {
                session.turn.wait_idle().await;
                runtime.delete_session(ctx, session.runtime_session).await
            });
        }
        let mut first_error = None;
        while let Some(result) = cleanup_tasks.join_next().await {
            let error = match result {
                Ok(Ok(())) => None,
                Ok(Err(error)) => Some(internal_error(error)),
                Err(error) => Some(internal_error(error)),
            };
            if first_error.is_none() {
                first_error = error;
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    async fn prompt(
        &self,
        request: PromptRequest,
        connection: &ConnectionTo<Client>,
    ) -> Result<PromptResponse, agent_client_protocol::Error> {
        let acp_session_id = request.session_id.to_string();
        let structured = parse_prompt(&request.prompt)?;
        let (runtime_session, current_page, turn) = {
            let sessions = self.sessions.lock().await;
            let session = sessions
                .get(&acp_session_id)
                .ok_or_else(|| invalid_request("unknown session; call session/new first"))?;
            (
                session.runtime_session.clone(),
                session.page.clone(),
                Arc::clone(&session.turn),
            )
        };
        let mut prompt_turn = turn.begin().map_err(|error| match error {
            BeginTurnError::Active => {
                invalid_request("a prompt is already active for this session")
            }
            BeginTurnError::Closed => invalid_request("session is closed"),
        })?;

        let structured = match structured {
            StructuredPrompt::Operation(operation) => {
                return self
                    .prompt_operation(
                        connection,
                        &acp_session_id,
                        &runtime_session,
                        current_page.as_ref(),
                        operation,
                        &mut prompt_turn,
                    )
                    .await;
            }
            StructuredPrompt::Automation(structured) => *structured,
        };

        if let Some(url) = &structured.url {
            let page = match self
                .open_and_navigate(
                    &runtime_session,
                    current_page.as_ref(),
                    url,
                    &mut prompt_turn,
                )
                .await
            {
                Ok(page) => page,
                Err(PromptStepError::Cancelled) => {
                    return Ok(PromptResponse::new(StopReason::Cancelled));
                }
                Err(PromptStepError::Failed(error)) => return Err(error),
            };
            let mut sessions = self.sessions.lock().await;
            if let Some(session) = sessions.get_mut(&acp_session_id) {
                session.page = Some(page);
            }
            send_chunk(connection, &acp_session_id, format!("navigated to {url}"));
        }

        let page = self
            .sessions
            .lock()
            .await
            .get(&acp_session_id)
            .and_then(|session| session.page.clone());
        let Some(page) = page else {
            return Err(invalid_request(
                "no target page; include a url in the prompt or navigate first",
            ));
        };
        // Spawn + drain like open_page above: the intent may already be
        // dispatched and mutating the page, so a cancelled prompt must not
        // drop the submission mid-call and orphan its outcome.
        let runtime = Arc::clone(&self.runtime);
        let ctx = self.ctx();
        let submit_session = runtime_session.clone();
        let submit_page = page.clone();
        let intent = structured.intent.clone();
        let command = envelope(
            &submit_session,
            &submit_page,
            RuntimeCommand::Intent(intent),
            structured.workflow_id,
        );
        let workflow_id = command.workflow_id.clone();
        let attempt_id = command.attempt_id.clone();
        let mut submitted = tokio::spawn(async move { runtime.submit(ctx, command).await });
        let outcome = match prompt_turn.wait(&mut submitted).await {
            Ok(result) => result
                .map_err(internal_error)
                .and_then(|result| result.map_err(internal_error))?,
            Err(TurnCancelled) => {
                let _ = submitted.await;
                return Ok(PromptResponse::new(StopReason::Cancelled));
            }
        };
        report_outcome(
            connection,
            &acp_session_id,
            &runtime_session,
            &page,
            &workflow_id,
            &attempt_id,
            &outcome,
        )?;
        match &outcome {
            CommandOutcome::Completed { .. } => Ok(PromptResponse::new(StopReason::EndTurn)),
            CommandOutcome::Failed { error, .. }
                if error.code == types::ErrorCode::VisionAssistDenied =>
            {
                self.maybe_escalate(
                    connection,
                    &acp_session_id,
                    &AcpCommandScope {
                        runtime_session,
                        page,
                        workflow_id,
                    },
                    structured.intent,
                    &mut prompt_turn,
                )
                .await
            }
            CommandOutcome::Failed { .. } | CommandOutcome::NeedsReconciliation { .. } => {
                Ok(PromptResponse::new(StopReason::Refusal))
            }
            _ => Ok(PromptResponse::new(StopReason::EndTurn)),
        }
    }

    async fn prompt_operation(
        &self,
        connection: &ConnectionTo<Client>,
        acp_session_id: &str,
        runtime_session: &SessionId,
        page: Option<&PageId>,
        operation: PromptOperation,
        prompt_turn: &mut PromptLease,
    ) -> Result<PromptResponse, agent_client_protocol::Error> {
        const MAX_EVIDENCE_REFS: usize = 128;
        const MAX_RECOVERABLE_WORKFLOWS: usize = 32;

        let payload = async {
            let payload = match operation {
                PromptOperation::ContextAsk { description } => {
                    let page = page.ok_or_else(|| {
                        PromptStepError::Failed(invalid_request(
                            "contextAsk requires a page; navigate in an automation prompt first",
                        ))
                    })?;
                    let result = prompt_turn
                        .wait(self.runtime.context_ask(
                            self.ctx(),
                            runtime_session.clone(),
                            page.clone(),
                            description,
                        ))
                        .await
                        .map_err(|_| PromptStepError::Cancelled)?
                        .map_err(|error| PromptStepError::Failed(interface_error(error)))?;
                    serde_json::json!({"operation":"contextAsk", "result":result})
                }
                PromptOperation::ContextNeighbors { description } => {
                    let page = page.ok_or_else(|| {
                        PromptStepError::Failed(invalid_request(
                        "contextNeighbors requires a page; navigate in an automation prompt first",
                    ))
                    })?;
                    let result = prompt_turn
                        .wait(self.runtime.context_neighbors(
                            self.ctx(),
                            runtime_session.clone(),
                            page.clone(),
                            description,
                        ))
                        .await
                        .map_err(|_| PromptStepError::Cancelled)?
                        .map_err(|error| PromptStepError::Failed(interface_error(error)))?;
                    serde_json::json!({"operation":"contextNeighbors", "result":result})
                }
                PromptOperation::ContextSite { site_key } => {
                    let result = prompt_turn
                        .wait(self.runtime.context_site(self.ctx(), site_key))
                        .await
                        .map_err(|_| PromptStepError::Cancelled)?
                        .map_err(|error| PromptStepError::Failed(interface_error(error)))?;
                    serde_json::json!({"operation":"contextSite", "result":result})
                }
                PromptOperation::CheckpointSave {
                    checkpoint,
                    evidence_refs,
                } => {
                    if checkpoint.session_id != *runtime_session {
                        return Err(PromptStepError::Failed(invalid_request(
                            "checkpoint sessionId must match the ACP session",
                        )));
                    }
                    if evidence_refs.len() > MAX_EVIDENCE_REFS {
                        return Err(PromptStepError::Failed(invalid_request(
                            "evidenceRefs exceeds 128 entries",
                        )));
                    }
                    let evidence = prompt_turn
                        .wait(
                            self.runtime
                                .resolve_command_evidence(self.ctx(), evidence_refs),
                        )
                        .await
                        .map_err(|_| PromptStepError::Cancelled)?
                        .map_err(|error| PromptStepError::Failed(interface_error(error)))?;
                    let runtime = Arc::clone(&self.runtime);
                    let ctx = self.ctx();
                    let mut saving = tokio::spawn(async move {
                        runtime.checkpoint(ctx, *checkpoint, evidence).await
                    });
                    let result = match prompt_turn.wait(&mut saving).await {
                        Ok(result) => result
                            .map_err(|error| PromptStepError::Failed(internal_error(error)))?
                            .map_err(|error| PromptStepError::Failed(interface_error(error)))?,
                        Err(TurnCancelled) => {
                            let _ = saving.await;
                            return Err(PromptStepError::Cancelled);
                        }
                    };
                    serde_json::json!({"operation":"checkpointSave", "result":result})
                }
                PromptOperation::RecoveryStatus { workflow_id, limit } => {
                    let result = if let Some(workflow_id) = workflow_id {
                        serde_json::to_value(
                            prompt_turn
                                .wait(self.runtime.recovery_status(self.ctx(), workflow_id))
                                .await
                                .map_err(|_| PromptStepError::Cancelled)?
                                .map_err(|error| PromptStepError::Failed(interface_error(error)))?,
                        )
                        .map_err(|error| PromptStepError::Failed(internal_error(error)))?
                    } else {
                        let limit = limit.unwrap_or(MAX_RECOVERABLE_WORKFLOWS);
                        if !(1..=MAX_RECOVERABLE_WORKFLOWS).contains(&limit) {
                            return Err(PromptStepError::Failed(invalid_request(
                                "limit must be between 1 and 32",
                            )));
                        }
                        let workflows = prompt_turn
                            .wait(self.runtime.workflows_for_session(
                                self.ctx(),
                                runtime_session.clone(),
                                limit,
                            ))
                            .await
                            .map_err(|_| PromptStepError::Cancelled)?
                            .map_err(|error| PromptStepError::Failed(interface_error(error)))?;
                        serde_json::json!({
                            "sessionId":runtime_session,
                            "workflows":workflows,
                        })
                    };
                    serde_json::json!({"operation":"recoveryStatus", "result":result})
                }
                PromptOperation::WorkflowRecover { workflow_id } => {
                    let runtime = Arc::clone(&self.runtime);
                    let ctx = self.ctx();
                    let mut recovering =
                        tokio::spawn(async move { runtime.recover(ctx, workflow_id).await });
                    let result = match prompt_turn.wait(&mut recovering).await {
                        Ok(result) => result
                            .map_err(|error| PromptStepError::Failed(internal_error(error)))?
                            .map_err(|error| PromptStepError::Failed(interface_error(error)))?,
                        Err(TurnCancelled) => {
                            let _ = recovering.await;
                            return Err(PromptStepError::Cancelled);
                        }
                    };
                    serde_json::json!({"operation":"workflowRecover", "result":result})
                }
            };
            Ok::<_, PromptStepError>(payload)
        }
        .await;
        let payload = match payload {
            Ok(payload) => payload,
            Err(PromptStepError::Cancelled) => {
                return Ok(PromptResponse::new(StopReason::Cancelled));
            }
            Err(PromptStepError::Failed(error)) => return Err(error),
        };
        send_json_chunk(connection, acp_session_id, payload)?;
        Ok(PromptResponse::new(StopReason::EndTurn))
    }

    /// The only path that reaches a human. Whether to ask is
    /// [`crate::escalation`]'s decision; an approval applies only to the retry
    /// of this command on the existing page and never creates a reusable
    /// session.
    async fn maybe_escalate(
        &self,
        connection: &ConnectionTo<Client>,
        acp_session_id: &str,
        scope: &AcpCommandScope,
        intent: IntentCommand,
        prompt_turn: &mut PromptLease,
    ) -> Result<PromptResponse, agent_client_protocol::Error> {
        let gates = SessionPolicyGates {
            vision_assist: false,
        };
        match decide(
            EscalationRequest::with_vision(types::InterfaceOperation::SubmitCommand),
            &self.principal_capabilities,
            gates,
        ) {
            Escalation::Denied { missing } => {
                send_chunk(
                    connection,
                    acp_session_id,
                    format!(
                        "vision escalation denied: the principal does not hold {}",
                        missing.as_str()
                    ),
                );
                Ok(PromptResponse::new(StopReason::Refusal))
            }
            Escalation::AlreadyPermitted => Ok(PromptResponse::new(StopReason::Refusal)),
            Escalation::AskUser { capability } => {
                let permission = connection.send_request(RequestPermissionRequest::new(
                    agent_client_protocol::schema::v1::SessionId::new(acp_session_id),
                    ToolCallUpdate::new(
                        permission_tool_call_id(),
                        ToolCallUpdateFields::new().title(format!(
                            "Allow vision assist ({}) for this command?",
                            capability.as_str()
                        )),
                    ),
                    vec![
                        PermissionOption::new(
                            "allow",
                            "Allow once",
                            PermissionOptionKind::AllowOnce,
                        ),
                        PermissionOption::new("deny", "Deny", PermissionOptionKind::RejectOnce),
                    ],
                ));
                let outcome = match prompt_turn.wait(permission.block_task()).await {
                    Ok(result) => result.map_err(internal_error)?,
                    Err(TurnCancelled) => {
                        return Ok(PromptResponse::new(StopReason::Cancelled));
                    }
                };
                match permission_decision(&outcome.outcome) {
                    PermissionDecision::Approved => {}
                    PermissionDecision::Rejected => {
                        send_chunk(
                            connection,
                            acp_session_id,
                            "vision assist denied by user".to_string(),
                        );
                        return Ok(PromptResponse::new(StopReason::Refusal));
                    }
                    PermissionDecision::Cancelled => {
                        return Ok(PromptResponse::new(StopReason::Cancelled));
                    }
                }
                send_chunk(
                    connection,
                    acp_session_id,
                    "vision assist approved for this command; retrying on the current page"
                        .to_string(),
                );
                let (outcome, attempt_id) = match self
                    .retry_with_one_shot(
                        &scope.runtime_session,
                        &scope.page,
                        &scope.workflow_id,
                        intent,
                        prompt_turn,
                    )
                    .await
                {
                    Ok(outcome) => outcome,
                    Err(PromptStepError::Cancelled) => {
                        return Ok(PromptResponse::new(StopReason::Cancelled));
                    }
                    Err(PromptStepError::Failed(error)) => return Err(error),
                };
                report_outcome(
                    connection,
                    acp_session_id,
                    &scope.runtime_session,
                    &scope.page,
                    &scope.workflow_id,
                    &attempt_id,
                    &outcome,
                )?;
                Ok(PromptResponse::new(match &outcome {
                    CommandOutcome::Completed { .. } => StopReason::EndTurn,
                    _ => StopReason::Refusal,
                }))
            }
        }
    }

    async fn retry_with_one_shot(
        &self,
        runtime_session: &SessionId,
        page: &PageId,
        workflow_id: &WorkflowId,
        intent: IntentCommand,
        prompt_turn: &mut PromptLease,
    ) -> Result<(CommandOutcome, AttemptId), PromptStepError> {
        let command = envelope(
            runtime_session,
            page,
            RuntimeCommand::Intent(intent),
            Some(workflow_id.clone()),
        );
        let attempt_id = command.attempt_id.clone();
        let retry = self
            .runtime
            .submit_with_one_shot_vision_consent(self.ctx(), command);
        match prompt_turn.wait(retry).await {
            Ok(result) => result
                .map(|outcome| (outcome, attempt_id))
                .map_err(|error| PromptStepError::Failed(interface_error(error))),
            Err(TurnCancelled) => Err(PromptStepError::Cancelled),
        }
    }

    async fn open_and_navigate(
        &self,
        runtime_session: &SessionId,
        current_page: Option<&PageId>,
        url: &str,
        prompt_turn: &mut PromptLease,
    ) -> Result<PageId, PromptStepError> {
        let (page, newly_opened) = if let Some(page) = current_page {
            (page.clone(), false)
        } else {
            let runtime = Arc::clone(&self.runtime);
            let ctx = self.ctx();
            let request = OpenPageRequest {
                session_id: runtime_session.clone(),
            };
            // Keep ownership of the open operation after cancellation. Dropping
            // it can lose a page that the worker created concurrently with the
            // cancel signal, leaving no PageId available for cleanup.
            let mut opening = tokio::spawn(async move { runtime.open_page(ctx, request).await });
            let opened = match prompt_turn.wait(&mut opening).await {
                Ok(result) => result
                    .map_err(internal_error)
                    .and_then(|result| result.map_err(internal_error))
                    .map_err(PromptStepError::Failed)?,
                Err(TurnCancelled) => {
                    // Keep the prompt lease active until page creation reaches
                    // a result. A concurrent close therefore waits here, and a
                    // successfully created page is closed before its id can be
                    // lost.
                    if let Ok(Ok(page)) = opening.await {
                        self.close_page_best_effort(runtime_session, &page.id).await;
                    }
                    return Err(PromptStepError::Cancelled);
                }
            };
            (opened.id, true)
        };
        let navigation = prompt_turn
            .wait(self.runtime.submit(
                self.ctx(),
                envelope(
                    runtime_session,
                    &page,
                    RuntimeCommand::Primitive(types::PrimitiveCommand::Navigate(NavigateCommand {
                        url: url.into(),
                        wait_until: WaitUntil::Interactive,
                        timeout_ms: 30_000,
                    })),
                    None,
                ),
            ))
            .await;
        match navigation {
            Ok(Ok(CommandOutcome::Completed { .. })) => Ok(page),
            Ok(Ok(_)) => {
                if newly_opened {
                    self.close_page_best_effort(runtime_session, &page).await;
                }
                Err(PromptStepError::Failed(internal_error("navigation failed")))
            }
            Ok(Err(error)) => {
                if newly_opened {
                    self.close_page_best_effort(runtime_session, &page).await;
                }
                Err(PromptStepError::Failed(internal_error(error)))
            }
            Err(TurnCancelled) => {
                if newly_opened {
                    self.close_page_best_effort(runtime_session, &page).await;
                }
                Err(PromptStepError::Cancelled)
            }
        }
    }

    async fn close_page_best_effort(&self, runtime_session: &SessionId, page: &PageId) {
        let close = self.runtime.submit(
            self.ctx(),
            envelope(
                runtime_session,
                page,
                RuntimeCommand::Primitive(types::PrimitiveCommand::ClosePage(ClosePageCommand {
                    page_id: page.clone(),
                })),
                None,
            ),
        );
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), close).await;
    }
    // Used by the one-shot consent tests; the production prompt path spawns
    // its submission inline (cancel-drain) instead.
    #[allow(dead_code)]
    async fn submit_intent(
        &self,
        runtime_session: &SessionId,
        page: &PageId,
        intent: IntentCommand,
    ) -> Result<CommandOutcome, agent_client_protocol::Error> {
        self.runtime
            .submit(
                self.ctx(),
                envelope(runtime_session, page, RuntimeCommand::Intent(intent), None),
            )
            .await
            .map_err(interface_error)
    }
}

fn envelope(
    runtime_session: &SessionId,
    page: &PageId,
    command: RuntimeCommand,
    workflow_id: Option<WorkflowId>,
) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: workflow_id.unwrap_or_default(),
        attempt_id: AttemptId::new(),
        session_id: runtime_session.clone(),
        page_id: Some(page.clone()),
        deadline: Utc::now() + Duration::seconds(60),
        command,
    }
}

fn parse_prompt(blocks: &[ContentBlock]) -> Result<StructuredPrompt, agent_client_protocol::Error> {
    let text = blocks
        .iter()
        .find_map(|block| match block {
            ContentBlock::Text(text) => Some(text.text.clone()),
            _ => None,
        })
        .ok_or_else(|| invalid_request("prompt must contain a text block"))?;
    serde_json::from_str(&text).map_err(|error| {
        invalid_request(format!(
            "prompt text must be a structured automation request JSON \
             (example: {{\"url\":\"https://example.com\",\"intent\":{{\"kind\":\"locate\",\"input\":{{\"purpose\":\"the submit button\"}}}}}}): {error}"
        ))
    })
}

fn send_chunk(connection: &ConnectionTo<Client>, acp_session_id: &str, text: String) {
    let _ = connection.send_notification(SessionNotification::new(
        agent_client_protocol::schema::v1::SessionId::new(acp_session_id),
        SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(TextContent::new(
            text,
        )))),
    ));
}

fn send_json_chunk(
    connection: &ConnectionTo<Client>,
    acp_session_id: &str,
    payload: serde_json::Value,
) -> Result<(), agent_client_protocol::Error> {
    let text = serde_json::to_string(&payload).map_err(internal_error)?;
    send_chunk(connection, acp_session_id, text);
    Ok(())
}

fn report_outcome(
    connection: &ConnectionTo<Client>,
    acp_session_id: &str,
    runtime_session: &SessionId,
    page: &PageId,
    workflow_id: &WorkflowId,
    attempt_id: &AttemptId,
    outcome: &CommandOutcome,
) -> Result<(), agent_client_protocol::Error> {
    send_json_chunk(
        connection,
        acp_session_id,
        outcome_payload(runtime_session, page, workflow_id, attempt_id, outcome),
    )
}

fn outcome_payload(
    runtime_session: &SessionId,
    page: &PageId,
    workflow_id: &WorkflowId,
    attempt_id: &AttemptId,
    outcome: &CommandOutcome,
) -> serde_json::Value {
    serde_json::json!({
        "operation":"execute",
        "sessionId":runtime_session,
        "pageId":page,
        "workflowId":workflow_id,
        "attemptId":attempt_id,
        "outcome":outcome,
    })
}

fn invalid_request(message: impl Into<String>) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(serde_json::Value::String(message.into()))
}

fn internal_error(error: impl std::fmt::Debug) -> agent_client_protocol::Error {
    agent_client_protocol::Error::internal_error()
        .data(serde_json::Value::String(format!("{error:?}")))
}

fn interface_error(error: types::InterfaceError) -> agent_client_protocol::Error {
    let data = serde_json::to_value(&error)
        .unwrap_or_else(|_| serde_json::Value::String("runtime interface error".into()));
    agent_client_protocol::Error::internal_error().data(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    struct RecordingRuntime {
        created: std::sync::Mutex<Vec<SessionId>>,
        opened: std::sync::Mutex<Vec<PageState>>,
        deleted: std::sync::Mutex<Vec<SessionId>>,
        submitted: std::sync::Mutex<Vec<CommandEnvelope>>,
        one_shot: std::sync::Mutex<Vec<CommandEnvelope>>,
        block_create: std::sync::atomic::AtomicBool,
        create_started: tokio::sync::Semaphore,
        create_release: tokio::sync::Notify,
        block_open: std::sync::atomic::AtomicBool,
        open_started: tokio::sync::Semaphore,
        open_release: tokio::sync::Notify,
        block_delete: std::sync::atomic::AtomicBool,
        fail_next_delete: std::sync::atomic::AtomicBool,
        delete_started: tokio::sync::Semaphore,
        delete_release: tokio::sync::Notify,
    }

    impl Default for RecordingRuntime {
        fn default() -> Self {
            Self {
                created: std::sync::Mutex::new(Vec::new()),
                opened: std::sync::Mutex::new(Vec::new()),
                deleted: std::sync::Mutex::new(Vec::new()),
                submitted: std::sync::Mutex::new(Vec::new()),
                one_shot: std::sync::Mutex::new(Vec::new()),
                block_create: std::sync::atomic::AtomicBool::new(false),
                create_started: tokio::sync::Semaphore::new(0),
                create_release: tokio::sync::Notify::new(),
                block_open: std::sync::atomic::AtomicBool::new(false),
                open_started: tokio::sync::Semaphore::new(0),
                open_release: tokio::sync::Notify::new(),
                block_delete: std::sync::atomic::AtomicBool::new(false),
                fail_next_delete: std::sync::atomic::AtomicBool::new(false),
                delete_started: tokio::sync::Semaphore::new(0),
                delete_release: tokio::sync::Notify::new(),
            }
        }
    }

    impl RecordingRuntime {
        fn completed(envelope: &CommandEnvelope) -> CommandOutcome {
            CommandOutcome::Completed {
                command_id: envelope.command_id.clone(),
                evidence: Vec::new(),
            }
        }
    }

    #[async_trait]
    impl AcpRuntime for RecordingRuntime {
        async fn create_session(
            &self,
            _ctx: types::RequestContext,
            request: CreateSessionRequest,
        ) -> InterfaceResult<SessionState> {
            self.create_started.add_permits(1);
            if self.block_create.load(std::sync::atomic::Ordering::Acquire) {
                self.create_release.notified().await;
            }
            let now = Utc::now();
            let id = SessionId::new();
            self.created
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(id.clone());
            Ok(SessionState {
                id,
                profile: request.profile,
                proxy: request.proxy,
                page_ids: Vec::new(),
                created_at: now,
                last_used_at: now,
                execution_policy: request.execution_policy,
                zigzagzig: false,
            })
        }

        async fn delete_session(
            &self,
            _ctx: types::RequestContext,
            session: SessionId,
        ) -> InterfaceResult<()> {
            self.delete_started.add_permits(1);
            if self
                .fail_next_delete
                .swap(false, std::sync::atomic::Ordering::AcqRel)
            {
                return Err(types::InterfaceError {
                    code: types::InterfaceErrorCode::Internal,
                    layer: types::ErrorLayer::Interface,
                    message: "injected delete failure".into(),
                    correlation_id: types::CorrelationId::new(),
                    command_id: None,
                    retryable: true,
                    retry_after_ms: None,
                    reconciliation_required: false,
                    required_capability: None,
                });
            }
            if self.block_delete.load(std::sync::atomic::Ordering::Acquire) {
                self.delete_release.notified().await;
            }
            self.deleted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(session);
            Ok(())
        }

        async fn open_page(
            &self,
            _ctx: types::RequestContext,
            request: OpenPageRequest,
        ) -> InterfaceResult<PageState> {
            self.open_started.add_permits(1);
            if self.block_open.load(std::sync::atomic::Ordering::Acquire) {
                self.open_release.notified().await;
            }
            let page = PageState {
                id: PageId::new(),
                session_id: request.session_id,
                url: None,
                mode: types::PageMode::Interactive,
                ready_state: "interactive".into(),
                pending_requests: 0,
            };
            self.opened
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(page.clone());
            Ok(page)
        }

        async fn submit(
            &self,
            _ctx: types::RequestContext,
            envelope: CommandEnvelope,
        ) -> InterfaceResult<CommandOutcome> {
            let outcome = Self::completed(&envelope);
            self.submitted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(envelope);
            Ok(outcome)
        }

        async fn submit_with_one_shot_vision_consent(
            &self,
            _ctx: types::RequestContext,
            envelope: CommandEnvelope,
        ) -> InterfaceResult<CommandOutcome> {
            let outcome = Self::completed(&envelope);
            self.one_shot
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(envelope);
            Ok(outcome)
        }

        async fn context_ask(
            &self,
            _ctx: types::RequestContext,
            _session: SessionId,
            _page: PageId,
            _description: String,
        ) -> InterfaceResult<Option<types::ContextAnswer>> {
            Ok(None)
        }

        async fn context_neighbors(
            &self,
            _ctx: types::RequestContext,
            _session: SessionId,
            _page: PageId,
            _description: String,
        ) -> InterfaceResult<Option<types::ContextNeighbors>> {
            Ok(None)
        }

        async fn context_site(
            &self,
            _ctx: types::RequestContext,
            _site_key: String,
        ) -> InterfaceResult<Option<types::ContextSiteView>> {
            Ok(None)
        }

        async fn resolve_command_evidence(
            &self,
            _ctx: types::RequestContext,
            _command_ids: Vec<CommandId>,
        ) -> InterfaceResult<Vec<Evidence>> {
            Ok(Vec::new())
        }

        async fn checkpoint(
            &self,
            _ctx: types::RequestContext,
            checkpoint: WorkflowCheckpoint,
            _evidence: Vec<Evidence>,
        ) -> InterfaceResult<WorkflowCheckpoint> {
            Ok(checkpoint)
        }

        async fn recovery_status(
            &self,
            ctx: types::RequestContext,
            _workflow: WorkflowId,
        ) -> InterfaceResult<types::RecoveryStatus> {
            Err(test_interface_error(ctx, "recovery status unavailable"))
        }

        async fn workflows_for_session(
            &self,
            _ctx: types::RequestContext,
            _session: SessionId,
            _limit: usize,
        ) -> InterfaceResult<Vec<WorkflowId>> {
            Ok(Vec::new())
        }

        async fn recover(
            &self,
            ctx: types::RequestContext,
            _workflow: WorkflowId,
        ) -> InterfaceResult<RecoveryDecision> {
            Err(test_interface_error(ctx, "recovery unavailable"))
        }
    }

    fn test_interface_error(ctx: types::RequestContext, message: &str) -> types::InterfaceError {
        types::InterfaceError {
            code: types::InterfaceErrorCode::UnsupportedOperation,
            layer: types::ErrorLayer::Interface,
            message: message.into(),
            correlation_id: ctx.correlation_id,
            command_id: None,
            retryable: false,
            retry_after_ms: None,
            reconciliation_required: false,
            required_capability: None,
        }
    }

    async fn recording_server(runtime: Arc<RecordingRuntime>) -> AcpServer {
        use interface_core::AuthorityStore;

        let authority = AuthorityStore::in_memory();
        let token = authority
            .issue(
                types::PrincipalId::from_uuid(uuid::Uuid::new_v4()),
                [
                    Capability::SessionRead,
                    Capability::SessionWrite,
                    Capability::PageWrite,
                    Capability::BrowserMutate,
                    Capability::IntentExecute,
                    Capability::VisionAssist,
                ],
                Utc::now() + Duration::minutes(5),
            )
            .await
            .unwrap()
            .expose_once();
        let handle = authority.verify(&token).await.unwrap();
        AcpServer::with_runtime(
            runtime,
            handle,
            vec![
                Capability::SessionRead,
                Capability::SessionWrite,
                Capability::PageWrite,
                Capability::BrowserMutate,
                Capability::IntentExecute,
                Capability::VisionAssist,
            ],
        )
    }

    async fn only_session(server: &AcpServer) -> (String, SessionId, Arc<PromptTurn>) {
        let sessions = server.sessions.lock().await;
        let (id, session) = sessions.iter().next().expect("one recorded session");
        (
            id.clone(),
            session.runtime_session.clone(),
            Arc::clone(&session.turn),
        )
    }

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text(TextContent::new(text))
    }

    #[test]
    fn a_structured_prompt_decodes_url_and_intent() {
        let parsed = parse_prompt(&[text_block(
            r#"{"url":"https://example.com","intent":{"kind":"locate","input":{"purpose":"the submit button"}}}"#,
        )])
        .expect("structured prompt decodes");
        let StructuredPrompt::Automation(parsed) = parsed else {
            panic!("expected automation prompt");
        };
        let parsed = *parsed;
        assert_eq!(parsed.url.as_deref(), Some("https://example.com"));
        assert!(matches!(parsed.intent, IntentCommand::Locate(_)));
    }

    #[test]
    fn an_automation_prompt_preserves_a_supplied_workflow_id() {
        let workflow_id = WorkflowId::new();
        let parsed = parse_prompt(&[text_block(
            &serde_json::json!({
                "workflowId": workflow_id,
                "intent": {"kind":"locate", "input":{"purpose":"the submit button"}}
            })
            .to_string(),
        )])
        .expect("automation prompt decodes");
        let StructuredPrompt::Automation(parsed) = parsed else {
            panic!("expected automation prompt");
        };
        let parsed = *parsed;
        assert_eq!(parsed.workflow_id, Some(workflow_id));
    }

    #[test]
    fn recovery_status_prompt_decodes_session_discovery() {
        let parsed = parse_prompt(&[text_block(r#"{"operation":"recoveryStatus","limit":8}"#)])
            .expect("recovery prompt decodes");
        assert!(matches!(
            parsed,
            StructuredPrompt::Operation(PromptOperation::RecoveryStatus {
                workflow_id: None,
                limit: Some(8),
            })
        ));
    }

    #[test]
    fn command_payload_preserves_workflow_and_evidence() {
        let session_id = SessionId::new();
        let page_id = PageId::new();
        let workflow_id = WorkflowId::new();
        let attempt_id = AttemptId::new();
        let command_id = CommandId::new();
        let outcome = CommandOutcome::Completed {
            command_id: command_id.clone(),
            evidence: vec![Evidence::Navigation {
                url: "https://example.test/complete".into(),
                title: "Complete".into(),
            }],
        };
        let payload = outcome_payload(&session_id, &page_id, &workflow_id, &attempt_id, &outcome);
        assert_eq!(
            payload["sessionId"],
            serde_json::to_value(session_id).unwrap()
        );
        assert_eq!(payload["pageId"], serde_json::to_value(page_id).unwrap());
        assert_eq!(
            payload["workflowId"],
            serde_json::to_value(workflow_id).unwrap()
        );
        assert_eq!(
            payload["attemptId"],
            serde_json::to_value(attempt_id).unwrap()
        );
        assert_eq!(
            payload["outcome"]["commandId"],
            serde_json::to_value(command_id).unwrap()
        );
        assert_eq!(
            payload["outcome"]["evidence"][0]["url"],
            "https://example.test/complete"
        );
    }

    #[test]
    fn a_prompt_without_url_still_decodes() {
        let parsed = parse_prompt(&[text_block(
            r#"{"intent":{"kind":"extract","input":{"purpose":"the price","fields":[]}}}"#,
        )]);
        assert!(parsed.is_ok(), "url must be optional: {parsed:?}");
    }

    #[test]
    fn freeform_text_is_rejected() {
        let error = parse_prompt(&[text_block("click the submit button for me")])
            .expect_err("freeform text must not decode");
        let detail = error
            .data
            .as_ref()
            .and_then(|value| value.as_str())
            .unwrap_or(error.message.as_str());
        assert!(
            detail.contains("structured automation request"),
            "parse error should name the expected shape: {detail}"
        );
        assert!(
            detail.contains("example:") || detail.contains("https://example.com"),
            "parse error should include a concrete example: {detail}"
        );
    }

    #[test]
    fn a_prompt_without_intent_is_rejected() {
        assert!(parse_prompt(&[text_block(r#"{"url":"https://example.com"}"#)]).is_err());
    }

    #[test]
    fn a_prompt_without_a_text_block_is_rejected() {
        assert!(parse_prompt(&[]).is_err());
    }

    #[test]
    fn cancelled_permission_is_cancelled_not_refused() {
        assert_eq!(
            permission_decision(&RequestPermissionOutcome::Cancelled),
            PermissionDecision::Cancelled,
        );
    }

    #[test]
    fn permission_tool_call_ids_are_unique() {
        assert_ne!(permission_tool_call_id(), permission_tool_call_id());
    }

    #[tokio::test]
    async fn cancelling_an_active_turn_interrupts_its_wait() {
        let turn = Arc::new(PromptTurn::default());
        let mut lease = turn.begin().expect("first turn starts");

        turn.cancel();

        let result = lease.wait(std::future::pending::<()>()).await;
        assert_eq!(result, Err(TurnCancelled));
    }

    #[test]
    fn a_second_simultaneous_turn_is_rejected() {
        let turn = Arc::new(PromptTurn::default());
        let _first = turn.begin().expect("first turn starts");

        assert!(matches!(turn.begin(), Err(BeginTurnError::Active)));
    }

    #[test]
    fn closing_a_turn_prevents_later_prompts() {
        let turn = Arc::new(PromptTurn::default());

        turn.close();

        assert!(matches!(turn.begin(), Err(BeginTurnError::Closed)));
    }

    #[tokio::test]
    async fn dropping_the_prompt_lease_releases_idle_waiters() {
        let turn = Arc::new(PromptTurn::default());
        let lease = turn.begin().expect("turn starts");
        let waiter = tokio::spawn({
            let turn = Arc::clone(&turn);
            async move { turn.wait_idle().await }
        });
        tokio::task::yield_now().await;

        drop(lease);

        tokio::time::timeout(std::time::Duration::from_secs(1), waiter)
            .await
            .expect("idle waiter is notified")
            .expect("idle waiter task completes");
    }

    #[tokio::test]
    async fn cancelling_page_creation_closes_the_page_before_releasing_the_turn() {
        let runtime = Arc::new(RecordingRuntime::default());
        runtime
            .block_open
            .store(true, std::sync::atomic::Ordering::Release);
        let server = recording_server(Arc::clone(&runtime)).await;
        server.new_session().await.unwrap();
        let (_, runtime_session, turn) = only_session(&server).await;
        let task_server = server.clone();
        let task_session = runtime_session.clone();
        let task_turn = Arc::clone(&turn);
        let opening = tokio::spawn(async move {
            let mut lease = task_turn.begin().expect("prompt starts");
            task_server
                .open_and_navigate(&task_session, None, "https://example.test", &mut lease)
                .await
        });

        runtime.open_started.acquire().await.unwrap().forget();
        turn.cancel();
        assert!(
            !opening.is_finished(),
            "cancellation must retain the turn until page creation can be cleaned up"
        );
        runtime.open_release.notify_waiters();
        assert!(matches!(
            opening.await.unwrap(),
            Err(PromptStepError::Cancelled)
        ));

        let opened = runtime
            .opened
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone();
        let submitted = runtime
            .submitted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(opened.len(), 1);
        assert!(submitted.iter().any(|envelope| {
            matches!(
                &envelope.command,
                RuntimeCommand::Primitive(types::PrimitiveCommand::ClosePage(command))
                    if command.page_id == opened[0].id
            )
        }));
    }

    #[tokio::test]
    async fn later_navigation_reuses_the_existing_page() {
        let runtime = Arc::new(RecordingRuntime::default());
        let server = recording_server(Arc::clone(&runtime)).await;
        server.new_session().await.unwrap();
        let (_, runtime_session, turn) = only_session(&server).await;

        let first_page = {
            let mut lease = turn.begin().expect("first prompt starts");
            server
                .open_and_navigate(&runtime_session, None, "https://first.example", &mut lease)
                .await
                .ok()
                .expect("first navigation succeeds")
        };
        let second_page = {
            let mut lease = turn.begin().expect("second prompt starts");
            server
                .open_and_navigate(
                    &runtime_session,
                    Some(&first_page),
                    "https://second.example",
                    &mut lease,
                )
                .await
                .ok()
                .expect("second navigation succeeds")
        };

        assert_eq!(second_page, first_page);
        assert_eq!(
            runtime
                .opened
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1,
            "an explicit later URL must navigate the existing page"
        );
    }

    #[tokio::test]
    async fn one_shot_retry_uses_the_original_session_and_page_only_once() {
        let runtime = Arc::new(RecordingRuntime::default());
        let server = recording_server(Arc::clone(&runtime)).await;
        server.new_session().await.unwrap();
        let (_, runtime_session, turn) = only_session(&server).await;
        let page = PageId::new();
        let workflow_id = WorkflowId::new();
        let intent = IntentCommand::Locate(types::LocateIntent {
            purpose: "missing control".into(),
            hints: types::IntentHints::default(),
        });
        {
            let mut lease = turn.begin().expect("prompt starts");
            server
                .retry_with_one_shot(
                    &runtime_session,
                    &page,
                    &workflow_id,
                    intent.clone(),
                    &mut lease,
                )
                .await
                .ok()
                .expect("one-shot retry dispatches");
        }
        server
            .submit_intent(&runtime_session, &page, intent)
            .await
            .expect("later ordinary submission dispatches");

        let one_shot = runtime
            .one_shot
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(one_shot.len(), 1);
        assert_eq!(one_shot[0].session_id, runtime_session);
        assert_eq!(one_shot[0].page_id.as_ref(), Some(&page));
        assert_eq!(one_shot[0].workflow_id, workflow_id);
        assert_eq!(
            runtime
                .submitted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1,
            "the next command must return to ordinary submission"
        );
        assert_eq!(
            runtime
                .created
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1,
            "approval must not create a reusable escalated session"
        );
    }

    #[tokio::test]
    async fn close_atomically_claims_the_session_before_runtime_deletion() {
        let runtime = Arc::new(RecordingRuntime::default());
        runtime
            .block_delete
            .store(true, std::sync::atomic::Ordering::Release);
        let server = recording_server(Arc::clone(&runtime)).await;
        server.new_session().await.unwrap();
        let (session_id, _, _) = only_session(&server).await;
        let close_server = server.clone();
        let closing = tokio::spawn(async move { close_server.close_session(&session_id).await });

        runtime.delete_started.acquire().await.unwrap().forget();
        assert!(server.sessions.lock().await.is_empty());
        let cleanup_server = server.clone();
        let cleanup = tokio::spawn(async move { cleanup_server.cleanup_sessions().await });
        tokio::task::yield_now().await;
        assert!(
            runtime.delete_started.try_acquire().is_err(),
            "disconnect cleanup must not issue a second delete for the claimed session"
        );
        runtime.delete_release.notify_waiters();
        closing.await.unwrap().unwrap();
        cleanup.await.unwrap().unwrap();
        assert_eq!(
            runtime
                .deleted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_failed_close_is_retired_from_prompts_and_can_retry_deletion() {
        let runtime = Arc::new(RecordingRuntime::default());
        runtime
            .fail_next_delete
            .store(true, std::sync::atomic::Ordering::Release);
        let server = recording_server(Arc::clone(&runtime)).await;
        server.new_session().await.unwrap();
        let (session_id, _, turn) = only_session(&server).await;

        assert!(server.close_session(&session_id).await.is_err());
        assert!(server.sessions.lock().await.is_empty());
        assert!(server
            .retired_sessions
            .lock()
            .await
            .contains_key(&session_id));
        assert!(matches!(turn.begin(), Err(BeginTurnError::Closed)));

        server.close_session(&session_id).await.unwrap();
        assert!(server.retired_sessions.lock().await.is_empty());
        assert_eq!(
            runtime
                .deleted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn disconnect_signals_every_turn_and_cleans_idle_sessions_concurrently() {
        let runtime = Arc::new(RecordingRuntime::default());
        let server = recording_server(Arc::clone(&runtime)).await;
        server.new_session().await.unwrap();
        server.new_session().await.unwrap();
        let turns: Vec<Arc<PromptTurn>> = server
            .sessions
            .lock()
            .await
            .values()
            .map(|session| Arc::clone(&session.turn))
            .collect();
        let active = turns[0].begin().expect("one prompt stays active");
        let cleanup_server = server.clone();
        let cleanup = tokio::spawn(async move { cleanup_server.cleanup_sessions().await });

        runtime.delete_started.acquire().await.unwrap().forget();
        assert!(matches!(turns[0].begin(), Err(BeginTurnError::Closed)));
        assert!(matches!(turns[1].begin(), Err(BeginTurnError::Closed)));
        assert_eq!(
            runtime
                .deleted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1,
            "the idle session must be deleted while another turn drains"
        );

        drop(active);
        cleanup.await.unwrap().unwrap();
        assert_eq!(
            runtime
                .deleted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn disconnect_waits_for_in_progress_session_creation_then_cleans_it() {
        let runtime = Arc::new(RecordingRuntime::default());
        runtime
            .block_create
            .store(true, std::sync::atomic::Ordering::Release);
        let server = recording_server(Arc::clone(&runtime)).await;
        let create_server = server.clone();
        let creating = tokio::spawn(async move { create_server.new_session().await });
        runtime.create_started.acquire().await.unwrap().forget();
        let cleanup_server = server.clone();
        let cleanup = tokio::spawn(async move { cleanup_server.cleanup_sessions().await });
        tokio::task::yield_now().await;
        assert!(!cleanup.is_finished());

        runtime.create_release.notify_waiters();
        creating.await.unwrap().unwrap();
        cleanup.await.unwrap().unwrap();
        assert!(server.sessions.lock().await.is_empty());
        assert_eq!(
            runtime
                .deleted
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .len(),
            1
        );
        assert!(server.new_session().await.is_err());
    }
}
