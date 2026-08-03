//! The ACP stdio server: an editor drives the runtime as a fourth adapter.
//!
//! Wire scope (Spec D, v1): `initialize`, `session/new`, `session/prompt`,
//! `session/cancel`, and agent→client `session/update` plus
//! `session/request_permission`. A prompt is a structured automation request
//! (an optional `url` plus one intent in the exact `types::IntentCommand`
//! wire shape), never freeform natural language — there is no planner.
//!
//! Every run goes through the same `AuthenticatedRuntime` the other three
//! adapters use, so capability, idempotency, evidence, and outcome semantics
//! cannot drift. The permission path is decided by [`crate::escalation`]:
//! a click can lift a session gate, never mint authority.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use agent_client_protocol::schema::v1::{
    AgentCapabilities, CancelNotification, ContentBlock, ContentChunk, InitializeRequest,
    InitializeResponse, NewSessionRequest, NewSessionResponse, PermissionOption,
    PermissionOptionKind, PromptCapabilities, PromptRequest, PromptResponse,
    RequestPermissionOutcome, RequestPermissionRequest, SessionNotification, SessionUpdate,
    StopReason, TextContent, ToolCallUpdate, ToolCallUpdateFields,
};
use agent_client_protocol::{Agent, Client, ConnectionTo, Result as AcpResult, Stdio};
use chrono::{Duration, Utc};
use interface_core::RuntimeInterface;
use sdk_core::AuthenticatedRuntime;
use tokio::sync::Mutex;
use types::{
    AttemptId, Capability, CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest,
    IntentCommand, NavigateCommand, OpenPageRequest, PageId, RuntimeCommand, SessionId, WaitUntil,
    WorkflowId,
};

use crate::escalation::{decide, Escalation, EscalationRequest, SessionPolicyGates};

/// What a `session/prompt` text block must decode to. `url` is the target
/// page (opened and navigated first); `intent` is one intent in the exact
/// shape `command_execute` accepts — no ACP-specific vocabulary to drift.
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StructuredPrompt {
    #[serde(default)]
    url: Option<String>,
    intent: IntentCommand,
}

struct AcpSession {
    runtime_session: SessionId,
    page: Option<PageId>,
    url: Option<String>,
    cancelled: Arc<AtomicBool>,
}

/// The running server: one `AuthenticatedRuntime` plus the ACP→runtime
/// session map. The runtime session id is reused as the ACP session id so an
/// editor-side handle names the same thing the other surfaces do.
#[derive(Clone)]
pub struct AcpServer {
    runtime: Arc<AuthenticatedRuntime>,
    principal_capabilities: Arc<Vec<Capability>>,
    sessions: Arc<Mutex<HashMap<String, AcpSession>>>,
}

impl AcpServer {
    pub fn new(
        runtime: Arc<AuthenticatedRuntime>,
        principal_capabilities: Vec<Capability>,
    ) -> Self {
        Self {
            runtime,
            principal_capabilities: Arc::new(principal_capabilities),
            sessions: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    fn ctx(&self) -> types::RequestContext {
        // The stdio gateway is single-principal: the startup credential the
        // process enrolled. Requests carry no idempotency key; each prompt
        // step mints its own command id.
        self.runtime
            .capability_handle()
            .context(Utc::now() + Duration::minutes(5), None)
    }

    /// Serve stdin/stdout until the client disconnects.
    pub async fn serve(self) -> AcpResult<()> {
        let server = self.clone();
        let prompt_server = self.clone();
        let cancel_server = self;
        Agent
            .builder()
            .name("bobby-browser")
            .on_receive_request(
                async move |initialize: InitializeRequest, responder, _connection| {
                    responder.respond(
                        InitializeResponse::new(initialize.protocol_version).agent_capabilities(
                            AgentCapabilities::new().prompt_capabilities(PromptCapabilities::new()),
                        ),
                    )
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |_request: NewSessionRequest, responder, _connection| match server
                    .new_session()
                    .await
                {
                    Ok(response) => responder.respond(response),
                    Err(error) => responder.respond_with_error(error),
                },
                agent_client_protocol::on_receive_request!(),
            )
            .on_receive_request(
                async move |request: PromptRequest, responder, connection| match prompt_server
                    .prompt(request, &connection)
                    .await
                {
                    Ok(response) => responder.respond(response),
                    Err(error) => responder.respond_with_error(error),
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
            .await
    }

    async fn new_session(&self) -> Result<NewSessionResponse, agent_client_protocol::Error> {
        let session = self
            .runtime
            .create_session(
                self.ctx(),
                CreateSessionRequest {
                    profile: "acp".into(),
                    proxy: None,
                    execution_policy: Default::default(),
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
                url: None,
                cancelled: Arc::new(AtomicBool::new(false)),
            },
        );
        Ok(NewSessionResponse::new(
            agent_client_protocol::schema::v1::SessionId::new(acp_session_id),
        ))
    }

    async fn cancel(&self, acp_session_id: &str) {
        if let Some(session) = self.sessions.lock().await.get(acp_session_id) {
            session.cancelled.store(true, Ordering::SeqCst);
        }
    }

    async fn prompt(
        &self,
        request: PromptRequest,
        connection: &ConnectionTo<Client>,
    ) -> Result<PromptResponse, agent_client_protocol::Error> {
        let acp_session_id = request.session_id.to_string();
        let structured = parse_prompt(&request.prompt)?;
        let (runtime_session, page_url, cancelled) = {
            let sessions = self.sessions.lock().await;
            let session = sessions
                .get(&acp_session_id)
                .ok_or_else(|| invalid_request("unknown session; call session/new first"))?;
            (
                session.runtime_session.clone(),
                session.url.clone(),
                Arc::clone(&session.cancelled),
            )
        };
        cancelled.store(false, Ordering::SeqCst);

        let url = structured.url.clone().or(page_url);
        if let Some(url) = &structured.url {
            if cancelled.load(Ordering::SeqCst) {
                return Ok(PromptResponse::new(StopReason::Cancelled));
            }
            let page = self
                .open_and_navigate(&runtime_session, url)
                .await
                .map_err(internal_error)?;
            let mut sessions = self.sessions.lock().await;
            if let Some(session) = sessions.get_mut(&acp_session_id) {
                session.page = Some(page);
                session.url = Some(url.clone());
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
        if cancelled.load(Ordering::SeqCst) {
            return Ok(PromptResponse::new(StopReason::Cancelled));
        }

        let outcome = self
            .submit_intent(&runtime_session, &page, structured.intent.clone())
            .await?;
        report_outcome(connection, &acp_session_id, &outcome);
        match &outcome {
            CommandOutcome::Completed { .. } => Ok(PromptResponse::new(StopReason::EndTurn)),
            CommandOutcome::Failed { error, .. }
                if error.code == types::ErrorCode::VisionAssistDenied =>
            {
                self.maybe_escalate(
                    connection,
                    &acp_session_id,
                    &runtime_session,
                    url.as_deref(),
                    structured.intent,
                )
                .await
            }
            CommandOutcome::Failed { .. } | CommandOutcome::NeedsReconciliation { .. } => {
                Ok(PromptResponse::new(StopReason::Refusal))
            }
            _ => Ok(PromptResponse::new(StopReason::EndTurn)),
        }
    }

    /// The only path that reaches a human. Vision escalation was denied by
    /// the session gate; whether to ask at all is the escalation module's
    /// decision, and an approval lifts the gate by creating a session that
    /// carries the same page under `visionAssist: true` — authority the
    /// principal already held, never new authority.
    async fn maybe_escalate(
        &self,
        connection: &ConnectionTo<Client>,
        acp_session_id: &str,
        _runtime_session: &SessionId,
        url: Option<&str>,
        intent: IntentCommand,
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
                let outcome = connection
                    .send_request(RequestPermissionRequest::new(
                        agent_client_protocol::schema::v1::SessionId::new(acp_session_id),
                        ToolCallUpdate::new(
                            "vision-escalation",
                            ToolCallUpdateFields::new().title(format!(
                                "Allow vision assist ({}) for this session?",
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
                    ))
                    .block_task()
                    .await
                    .map_err(internal_error)?;
                let approved = matches!(
                    outcome.outcome,
                    RequestPermissionOutcome::Selected(ref selected)
                        if selected.option_id.0.as_ref() == "allow"
                );
                if !approved {
                    send_chunk(
                        connection,
                        acp_session_id,
                        "vision assist denied by user".to_string(),
                    );
                    return Ok(PromptResponse::new(StopReason::Refusal));
                }
                let Some(url) = url else {
                    return Ok(PromptResponse::new(StopReason::Refusal));
                };
                let escalated = self
                    .runtime
                    .create_session(
                        self.ctx(),
                        CreateSessionRequest {
                            profile: "acp".into(),
                            proxy: None,
                            execution_policy: types::ExecutionPolicy {
                                vision_assist: true,
                                ..Default::default()
                            },
                        },
                    )
                    .await
                    .map_err(internal_error)?;
                let page = self
                    .open_and_navigate(&escalated.id, url)
                    .await
                    .map_err(internal_error)?;
                let acp_escalated_id = escalated.id.0.to_string();
                self.sessions.lock().await.insert(
                    acp_escalated_id.clone(),
                    AcpSession {
                        runtime_session: escalated.id,
                        page: Some(page.clone()),
                        url: Some(url.to_owned()),
                        cancelled: Arc::new(AtomicBool::new(false)),
                    },
                );
                send_chunk(
                    connection,
                    acp_session_id,
                    format!("vision assist approved; rerunning in session {acp_escalated_id}"),
                );
                let outcome = self
                    .submit_intent(
                        &escalated_session(self, &acp_escalated_id).await,
                        &page,
                        intent,
                    )
                    .await?;
                report_outcome(connection, acp_session_id, &outcome);
                Ok(PromptResponse::new(match &outcome {
                    CommandOutcome::Completed { .. } => StopReason::EndTurn,
                    _ => StopReason::Refusal,
                }))
            }
        }
    }

    async fn open_and_navigate(
        &self,
        runtime_session: &SessionId,
        url: &str,
    ) -> Result<PageId, agent_client_protocol::Error> {
        let page = self
            .runtime
            .open_page(
                self.ctx(),
                OpenPageRequest {
                    session_id: runtime_session.clone(),
                },
            )
            .await
            .map_err(internal_error)?;
        self.runtime
            .submit(
                self.ctx(),
                envelope(
                    runtime_session,
                    &page.id,
                    RuntimeCommand::Primitive(types::PrimitiveCommand::Navigate(NavigateCommand {
                        url: url.into(),
                        wait_until: WaitUntil::Interactive,
                        timeout_ms: 30_000,
                    })),
                ),
            )
            .await
            .map_err(internal_error)?
            .completed_or(|| internal_error("navigation failed"))?;
        Ok(page.id)
    }

    async fn submit_intent(
        &self,
        runtime_session: &SessionId,
        page: &PageId,
        intent: IntentCommand,
    ) -> Result<CommandOutcome, agent_client_protocol::Error> {
        self.runtime
            .submit(
                self.ctx(),
                envelope(runtime_session, page, RuntimeCommand::Intent(intent)),
            )
            .await
            .map_err(internal_error)
    }
}

async fn escalated_session(server: &AcpServer, acp_id: &str) -> SessionId {
    server
        .sessions
        .lock()
        .await
        .get(acp_id)
        .map(|session| session.runtime_session.clone())
        .expect("escalated session was just inserted")
}

fn envelope(
    runtime_session: &SessionId,
    page: &PageId,
    command: RuntimeCommand,
) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
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
            "prompt text must be a structured automation request JSON ({{\"url\"?: string, \"intent\": {{\"kind\": .., \"input\": ..}}}}): {error}"
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

fn report_outcome(
    connection: &ConnectionTo<Client>,
    acp_session_id: &str,
    outcome: &CommandOutcome,
) {
    let line = match outcome {
        CommandOutcome::Completed { evidence, .. } => {
            format!("completed ({} evidence record(s))", evidence.len())
        }
        CommandOutcome::Failed { error, .. } => {
            format!("failed: {:?} — {}", error.code, error.message)
        }
        CommandOutcome::NeedsReconciliation { error, .. } => {
            format!("needs reconciliation: {:?} — {}", error.code, error.message)
        }
        other => format!("outcome: {other:?}"),
    };
    send_chunk(connection, acp_session_id, line);
}

trait CompletedOr {
    fn completed_or(
        self,
        error: impl FnOnce() -> agent_client_protocol::Error,
    ) -> Result<(), agent_client_protocol::Error>;
}

impl CompletedOr for CommandOutcome {
    fn completed_or(
        self,
        error: impl FnOnce() -> agent_client_protocol::Error,
    ) -> Result<(), agent_client_protocol::Error> {
        match self {
            CommandOutcome::Completed { .. } => Ok(()),
            _ => Err(error()),
        }
    }
}

fn invalid_request(message: impl Into<String>) -> agent_client_protocol::Error {
    agent_client_protocol::Error::invalid_params().data(serde_json::Value::String(message.into()))
}

fn internal_error(error: impl std::fmt::Debug) -> agent_client_protocol::Error {
    agent_client_protocol::Error::internal_error()
        .data(serde_json::Value::String(format!("{error:?}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_block(text: &str) -> ContentBlock {
        ContentBlock::Text(TextContent::new(text))
    }

    #[test]
    fn a_structured_prompt_decodes_url_and_intent() {
        let parsed = parse_prompt(&[text_block(
            r#"{"url":"https://example.com","intent":{"kind":"locate","input":{"purpose":"the submit button"}}}"#,
        )])
        .expect("structured prompt decodes");
        assert_eq!(parsed.url.as_deref(), Some("https://example.com"));
        assert!(matches!(parsed.intent, IntentCommand::Locate(_)));
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
        assert!(error.message.contains("structured automation request") || error.data.is_some());
    }

    #[test]
    fn a_prompt_without_intent_is_rejected() {
        assert!(parse_prompt(&[text_block(r#"{"url":"https://example.com"}"#)]).is_err());
    }

    #[test]
    fn a_prompt_without_a_text_block_is_rejected() {
        assert!(parse_prompt(&[]).is_err());
    }
}
