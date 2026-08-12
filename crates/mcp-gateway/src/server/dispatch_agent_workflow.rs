//! Retained agent-workflow lifecycle orchestration.
//!
//! The detached supervisor owns every fallible future after a setup slot is
//! reserved. The request handler receives ownership only of a setup-ready
//! reservation; cleanup remains armed until publication and supervisor disarm
//! are one registry-mutex decision.

use super::*;
use crate::workflow_handles::{WorkflowBinding, WorkflowHandleReservation};
use std::future::Future;
use tokio::sync::oneshot;

pub(super) const TOOLS: &[&str] = &["workflow_start", "workflow_observe"];

const CLEANUP_DEADLINE_SECONDS: i64 = 30;
const DEFAULT_WORKFLOW_OBSERVE_MAX_NODES: u32 = 256;
const DEFAULT_WORKFLOW_OBSERVE_MAX_CONTROLS: u32 = 128;

#[derive(Clone, Debug)]
struct CleanupResult {
    page_closed: bool,
    session_deleted: bool,
    error_code: Option<String>,
}

impl CleanupResult {
    fn conservative(code: &str) -> Self {
        Self {
            page_closed: false,
            session_deleted: false,
            error_code: Some(code.to_owned()),
        }
    }

    fn succeeded(&self) -> bool {
        self.session_deleted
    }
}

struct SetupReady {
    reservation: WorkflowHandleReservation,
    session: types::SessionState,
    page: types::PageState,
    navigation_outcome: Option<types::CommandOutcome>,
    disposition: oneshot::Sender<SupervisorDisposition>,
}

enum SetupMessage {
    Ready(SetupReady),
    Failed(types::InterfaceError),
    Terminal {
        failure: WorkflowStartFailure,
        response_ack: oneshot::Sender<()>,
    },
}

enum SupervisorDisposition {
    Cleanup {
        reply: oneshot::Sender<CleanupReply>,
    },
    CommitPending {
        published: oneshot::Receiver<()>,
        cleanup_reply: oneshot::Sender<CleanupReply>,
    },
}

struct CleanupReply {
    result: CleanupResult,
    response_ack: oneshot::Sender<()>,
}

struct CleanupResponseGuard {
    result: CleanupResult,
    response_ack: Option<oneshot::Sender<()>>,
}

impl CleanupResponseGuard {
    fn supervised(reply: CleanupReply) -> Self {
        Self {
            result: reply.result,
            response_ack: Some(reply.response_ack),
        }
    }

    fn conservative(code: &str) -> Self {
        Self {
            result: CleanupResult::conservative(code),
            response_ack: None,
        }
    }

    fn result(&self) -> &CleanupResult {
        &self.result
    }

    async fn acknowledge_after(mut self, response: impl Future<Output = Value>) -> Value {
        let response = response.await;
        if let Some(response_ack) = self.response_ack.take() {
            let _ = response_ack.send(());
        }
        response
    }
}

#[derive(Clone)]
struct WorkflowStartFailure {
    reason: &'static str,
    session: types::SessionState,
    page: Option<types::PageState>,
    workflow_id: types::WorkflowId,
    navigation_outcome: Option<types::CommandOutcome>,
    cleanup: CleanupResult,
    /// The underlying error that drove the failure (`code: message`), so an
    /// opaque `pageOpenFailed` never reaches an agent without its cause.
    detail: Option<String>,
}

impl Server {
    pub(super) async fn dispatch_agent_workflow(
        &self,
        id: Value,
        call: ToolCall,
        context: types::RequestContext,
    ) -> Value {
        match call.name.as_str() {
            "workflow_start" => {
                self.dispatch_workflow_start(id, call.arguments, context)
                    .await
            }
            "workflow_observe" => {
                self.dispatch_workflow_observe(id, call.arguments, context)
                    .await
            }
            _ => unreachable!("dispatch_agent_workflow received a tool it does not own"),
        }
    }

    async fn dispatch_workflow_observe(
        &self,
        id: Value,
        arguments: Value,
        context: types::RequestContext,
    ) -> Value {
        let input: WorkflowObserveArgs = match bounded_parse(arguments) {
            Ok(input) => input,
            Err(()) => return invalid_params_reason(id, "malformedArguments"),
        };
        if !input.goal_within_scalar_bound() {
            return invalid_params_reason(id, "malformedArguments");
        }

        let handle = input.workflow_handle;
        let binding = match self.workflow_handles.resolve(&handle) {
            Ok(binding) => binding,
            Err(
                WorkflowHandleError::CapacityExhausted
                | WorkflowHandleError::GenerationChanged
                | WorkflowHandleError::SupervisorLost
                | WorkflowHandleError::Unknown
                | WorkflowHandleError::BindingConflict
                | WorkflowHandleError::Malformed,
            ) => return invalid_params_reason(id, "unknownWorkflowHandle"),
        };
        let max_nodes = input
            .max_nodes
            .unwrap_or(DEFAULT_WORKFLOW_OBSERVE_MAX_NODES);
        let max_controls = input
            .max_controls
            .unwrap_or(DEFAULT_WORKFLOW_OBSERVE_MAX_CONTROLS);

        if input.include_forms {
            if let Err(error) = self
                .authorization
                .require_capability(&context, types::Capability::PageRead)
            {
                return interface_error_response(id, error);
            }
        }

        if let Some(goal) = input.goal.filter(|goal| !goal.is_empty()) {
            if context.capabilities.contains(types::Capability::PageRead) {
                let answer = match self
                    .runtime
                    .context_ask(
                        context.clone(),
                        binding.session_id.clone(),
                        binding.page_id.clone(),
                        goal,
                    )
                    .await
                {
                    Ok(answer) => answer,
                    Err(error) => return interface_error_response(id, error),
                };
                if let Some(answer) = answer {
                    let form_snapshot = if input.include_forms {
                        match self
                            .runtime
                            .form_snapshot(
                                context,
                                binding.session_id.clone(),
                                binding.page_id.clone(),
                                Some(max_controls),
                            )
                            .await
                        {
                            Ok(snapshot) => Some(snapshot),
                            Err(error) => return interface_error_response(id, error),
                        }
                    } else {
                        None
                    };
                    return self
                        .workflow_observe_success(
                            id,
                            json!({
                                "status":"completed",
                                "source":"retained",
                                "workflowHandle":handle,
                                "sessionId":binding.session_id,
                                "pageId":binding.page_id,
                                "workflowId":binding.workflow_id,
                                "retainedAnswer":answer,
                                "observationOutcome":Value::Null,
                                "formSnapshot":form_snapshot,
                            }),
                        )
                        .await;
                }
            }
        }

        let (submit_context, envelope) = primitive_envelope(
            context.clone(),
            binding.session_id.clone(),
            Some(binding.page_id.clone()),
            Some(binding.workflow_id.clone()),
            types::PrimitiveCommand::AccessibilitySnapshot(types::AccessibilitySnapshotCommand {
                max_nodes: Some(max_nodes),
                target: input.target,
            }),
        );
        let observation_outcome = match self.submit_envelope(submit_context, envelope).await {
            Ok(outcome) => outcome,
            Err(error) => return interface_error_response(id, error),
        };
        let Some(status) = observation_outcome
            .get("status")
            .and_then(Value::as_str)
            .map(str::to_owned)
        else {
            return interface_error_response(id, workflow_internal_error(&context));
        };
        let form_snapshot = if status == "completed" && input.include_forms {
            match self
                .runtime
                .form_snapshot(
                    context,
                    binding.session_id.clone(),
                    binding.page_id.clone(),
                    Some(max_controls),
                )
                .await
            {
                Ok(snapshot) => Some(snapshot),
                Err(error) => return interface_error_response(id, error),
            }
        } else {
            None
        };

        self.workflow_observe_success(
            id,
            json!({
                "status":status,
                "source":"live",
                "workflowHandle":handle,
                "sessionId":binding.session_id,
                "pageId":binding.page_id,
                "workflowId":binding.workflow_id,
                "retainedAnswer":Value::Null,
                "observationOutcome":observation_outcome,
                "formSnapshot":form_snapshot,
            }),
        )
        .await
    }

    async fn workflow_observe_success(&self, id: Value, value: Value) -> Value {
        let is_error = value.get("status").and_then(Value::as_str) != Some("completed");
        let mut response = self.tool_success(id, value).await;
        if let Some(result) = response.get_mut("result").and_then(Value::as_object_mut) {
            result.insert("isError".to_owned(), json!(is_error));
        }
        response
    }

    async fn dispatch_workflow_start(
        &self,
        id: Value,
        arguments: Value,
        context: types::RequestContext,
    ) -> Value {
        let input: WorkflowStartArgs = match bounded_parse(arguments) {
            Ok(input) => input,
            Err(()) => return invalid_params_reason(id, "malformedArguments"),
        };
        if input.profile.is_empty()
            || input.profile.len() > 128
            || input.proxy.as_ref().is_some_and(|proxy| proxy.len() > 2048)
        {
            return invalid_params_reason(id, "malformedArguments");
        }
        if input.url.is_some() {
            if let Err(error) = self
                .authorization
                .require_capability(&context, types::Capability::BrowserMutate)
            {
                return interface_error_response(id, error);
            }
        }

        let sessions = match self.runtime.list_sessions(context.clone()).await {
            Ok(sessions) => sessions,
            Err(error) => return interface_error_response(id, error),
        };
        self.workflow_handles.reconcile_sessions(&sessions);

        let reservation = match self.workflow_handles.reserve() {
            Ok(reservation) => reservation,
            Err(WorkflowHandleError::CapacityExhausted) => {
                return interface_error_response(id, workflow_capacity_error(&context))
            }
            Err(_) => return interface_error_response(id, workflow_internal_error(&context)),
        };
        let workflow_id = types::WorkflowId::new();
        let handle = reservation.handle().to_owned();
        let runtime = Arc::clone(&self.runtime);
        let capability_handle = self.handle.clone();
        let supervisor_runtime = Arc::clone(&runtime);
        let supervisor_handle = capability_handle.clone();
        let supervisor_workflow_id = workflow_id.clone();
        let correlation_id = context.correlation_id.clone();
        let handler_correlation_id = correlation_id.clone();
        let create_request = types::CreateSessionRequest {
            profile: input.profile,
            proxy: input.proxy,
            execution_policy: input.execution_policy,
        };
        let (setup_sender, setup_receiver) = oneshot::channel();

        tokio::spawn(async move {
            supervise_start(
                supervisor_runtime,
                supervisor_handle,
                context,
                reservation,
                create_request,
                input.url,
                supervisor_workflow_id,
                correlation_id,
                setup_sender,
            )
            .await;
        });

        let ready = match setup_receiver.await {
            Ok(SetupMessage::Ready(ready)) => ready,
            Ok(SetupMessage::Failed(error)) => return interface_error_response(id, error),
            Ok(SetupMessage::Terminal {
                failure,
                response_ack,
            }) => {
                let response = self.workflow_start_failure_response(id, failure).await;
                let _ = response_ack.send(());
                return response;
            }
            Err(_) => {
                return interface_error_response(
                    id,
                    workflow_internal_error_with_correlation(handler_correlation_id),
                )
            }
        };
        let SetupReady {
            reservation,
            session,
            page,
            navigation_outcome,
            disposition,
        } = ready;

        if !reservation.generation_is_current() {
            let cleanup = request_supervisor_cleanup(
                disposition,
                Arc::clone(&runtime),
                capability_handle.clone(),
                session.id.clone(),
                Some(page.id.clone()),
                workflow_id.clone(),
                handler_correlation_id.clone(),
            )
            .await;
            let failure = WorkflowStartFailure {
                reason: "workflowGenerationChanged",
                session,
                page: Some(page),
                workflow_id,
                navigation_outcome,
                cleanup: cleanup.result().clone(),
                detail: None,
            };
            return cleanup
                .acknowledge_after(self.workflow_start_failure_response(id, failure))
                .await;
        }

        let generation = reservation.generation();
        let success_value = json!({
            "status":"completed",
            "workflowHandle":handle,
            "sessionId":session.id,
            "pageId":page.id,
            "workflowId":workflow_id,
            "session":session,
            "page":page,
            "navigationOutcome":navigation_outcome
        });
        let success_response = self.tool_success(id.clone(), success_value).await;
        let supervisor_lost_response = self
            .workflow_start_failure_response(
                id.clone(),
                WorkflowStartFailure {
                    reason: "workflowSupervisorLost",
                    session: session.clone(),
                    page: Some(page.clone()),
                    workflow_id: workflow_id.clone(),
                    navigation_outcome: navigation_outcome.clone(),
                    cleanup: CleanupResult::conservative("workflowSupervisorLost"),
                    detail: None,
                },
            )
            .await;

        let (published_sender, published_receiver) = oneshot::channel();
        let (cleanup_reply, cleanup_receiver) = oneshot::channel();
        if disposition
            .send(SupervisorDisposition::CommitPending {
                published: published_receiver,
                cleanup_reply,
            })
            .is_err()
        {
            spawn_fallback_cleanup(
                runtime,
                capability_handle,
                session.id,
                Some(page.id),
                workflow_id,
                handler_correlation_id.clone(),
            );
            return supervisor_lost_response;
        }

        let binding = WorkflowBinding {
            generation,
            session_id: session.id.clone(),
            page_id: page.id.clone(),
            workflow_id: workflow_id.clone(),
        };
        match reservation.publish_with_supervisor(binding, published_sender) {
            Ok(()) => success_response,
            Err(WorkflowHandleError::GenerationChanged) => {
                let cleanup = receive_supervisor_cleanup(
                    cleanup_receiver,
                    Arc::clone(&runtime),
                    capability_handle.clone(),
                    session.id.clone(),
                    Some(page.id.clone()),
                    workflow_id.clone(),
                    handler_correlation_id.clone(),
                )
                .await;
                let failure = WorkflowStartFailure {
                    reason: "workflowGenerationChanged",
                    session,
                    page: Some(page),
                    workflow_id,
                    navigation_outcome,
                    cleanup: cleanup.result().clone(),
                    detail: None,
                };
                cleanup
                    .acknowledge_after(self.workflow_start_failure_response(id, failure))
                    .await
            }
            Err(WorkflowHandleError::SupervisorLost) => {
                spawn_fallback_cleanup(
                    runtime,
                    capability_handle,
                    session.id,
                    Some(page.id),
                    workflow_id,
                    handler_correlation_id,
                );
                supervisor_lost_response
            }
            Err(
                WorkflowHandleError::BindingConflict
                | WorkflowHandleError::Unknown
                | WorkflowHandleError::Malformed
                | WorkflowHandleError::CapacityExhausted,
            ) => {
                let cleanup = receive_supervisor_cleanup(
                    cleanup_receiver,
                    Arc::clone(&runtime),
                    capability_handle.clone(),
                    session.id.clone(),
                    Some(page.id.clone()),
                    workflow_id.clone(),
                    handler_correlation_id.clone(),
                )
                .await;
                let failure = WorkflowStartFailure {
                    reason: "workflowSupervisorLost",
                    session,
                    page: Some(page),
                    workflow_id,
                    navigation_outcome,
                    cleanup: cleanup.result().clone(),
                    detail: None,
                };
                cleanup
                    .acknowledge_after(self.workflow_start_failure_response(id, failure))
                    .await
            }
        }
    }

    async fn workflow_start_failure_response(
        &self,
        id: Value,
        failure: WorkflowStartFailure,
    ) -> Value {
        let page_id = failure.page.as_ref().map(|page| page.id.clone());
        self.tool_success(
            id,
            json!({
                "status":"failed",
                "workflowHandle":Value::Null,
                "sessionId":failure.session.id,
                "workflowId":failure.workflow_id,
                "session":failure.session,
                "pageId":page_id,
                "page":failure.page,
                "navigationOutcome":failure.navigation_outcome,
                "reason":failure.reason,
                "detail":failure.detail,
                "pageClosed":failure.cleanup.page_closed,
                "sessionDeleted":failure.cleanup.session_deleted,
                "cleanupErrorCode":failure.cleanup.error_code
            }),
        )
        .await
    }
}

#[allow(clippy::too_many_arguments)]
async fn supervise_start(
    runtime: Arc<dyn RuntimeInterface>,
    handle: CapabilityHandle,
    context: types::RequestContext,
    reservation: WorkflowHandleReservation,
    create_request: types::CreateSessionRequest,
    url: Option<String>,
    workflow_id: types::WorkflowId,
    correlation_id: types::CorrelationId,
    setup_sender: oneshot::Sender<SetupMessage>,
) {
    let session = match runtime
        .create_session(context.clone(), create_request)
        .await
    {
        Ok(session) => session,
        Err(error) => {
            let _ = setup_sender.send(SetupMessage::Failed(error));
            return;
        }
    };

    if setup_sender.is_closed() {
        let cleanup =
            cleanup_workflow(&runtime, &handle, session.id.clone(), None, workflow_id).await;
        warn_cancel_cleanup(&cleanup, &correlation_id, &session.id, None);
        return;
    }

    let page = match runtime
        .open_page(
            context.clone(),
            types::OpenPageRequest {
                session_id: session.id.clone(),
            },
        )
        .await
    {
        Ok(page) => page,
        Err(error) => {
            let detail = format!(
                "{}: {}",
                serde_json::to_value(error.code)
                    .ok()
                    .and_then(|code| code.as_str().map(str::to_owned))
                    .unwrap_or_else(|| format!("{:?}", error.code)),
                error.message
            );
            let cleanup = cleanup_workflow(
                &runtime,
                &handle,
                session.id.clone(),
                None,
                workflow_id.clone(),
            )
            .await;
            let failure = WorkflowStartFailure {
                reason: "pageOpenFailed",
                session: session.clone(),
                page: None,
                workflow_id,
                navigation_outcome: None,
                cleanup: cleanup.clone(),
                detail: Some(detail),
            };
            deliver_terminal(setup_sender, failure, &correlation_id).await;
            return;
        }
    };

    if !reservation.generation_is_current() {
        finish_generation_change(
            runtime,
            handle,
            reservation,
            setup_sender,
            session,
            page,
            workflow_id,
            None,
            correlation_id,
        )
        .await;
        return;
    }

    let navigation_outcome = if let Some(url) = url {
        let (navigation_context, envelope) = primitive_envelope(
            context,
            session.id.clone(),
            Some(page.id.clone()),
            Some(workflow_id.clone()),
            types::PrimitiveCommand::Navigate(types::NavigateCommand {
                url,
                wait_until: types::WaitUntil::Interactive,
                timeout_ms: DEFAULT_COMMAND_TIMEOUT_MS,
            }),
        );
        let outcome = runtime
            .submit(navigation_context, envelope.clone())
            .await
            .unwrap_or_else(|error| interface_failure_outcome(envelope.command_id, error));
        if !reservation.generation_is_current() {
            finish_generation_change(
                runtime,
                handle,
                reservation,
                setup_sender,
                session,
                page,
                workflow_id,
                Some(outcome),
                correlation_id,
            )
            .await;
            return;
        }
        if !matches!(outcome, types::CommandOutcome::Completed { .. }) {
            let cleanup = cleanup_workflow(
                &runtime,
                &handle,
                session.id.clone(),
                Some(page.id.clone()),
                workflow_id.clone(),
            )
            .await;
            let failure = WorkflowStartFailure {
                reason: "navigationFailed",
                session: session.clone(),
                page: Some(page.clone()),
                workflow_id,
                navigation_outcome: Some(outcome),
                cleanup: cleanup.clone(),
                detail: None,
            };
            deliver_terminal(setup_sender, failure, &correlation_id).await;
            return;
        }
        Some(outcome)
    } else {
        None
    };

    if !reservation.generation_is_current() {
        finish_generation_change(
            runtime,
            handle,
            reservation,
            setup_sender,
            session,
            page,
            workflow_id,
            navigation_outcome,
            correlation_id,
        )
        .await;
        return;
    }

    let (disposition, disposition_receiver) = oneshot::channel();
    let ready = SetupReady {
        reservation,
        session: session.clone(),
        page: page.clone(),
        navigation_outcome,
        disposition,
    };
    if let Err(ready) = setup_sender.send(SetupMessage::Ready(ready)) {
        let SetupMessage::Ready(ready) = ready else {
            unreachable!("sent ready variant")
        };
        let _reservation = ready.reservation;
        drop(ready.disposition);
        let cleanup = cleanup_workflow(
            &runtime,
            &handle,
            session.id.clone(),
            Some(page.id.clone()),
            workflow_id,
        )
        .await;
        warn_cancel_cleanup(&cleanup, &correlation_id, &session.id, Some(&page.id));
        return;
    }

    match disposition_receiver.await {
        Ok(SupervisorDisposition::Cleanup { reply }) => {
            let cleanup = cleanup_workflow(
                &runtime,
                &handle,
                session.id.clone(),
                Some(page.id.clone()),
                workflow_id,
            )
            .await;
            deliver_cleanup_reply(reply, cleanup, &correlation_id, &session.id, Some(&page.id))
                .await;
        }
        Ok(SupervisorDisposition::CommitPending {
            published,
            cleanup_reply,
        }) => {
            if published.await.is_err() {
                let cleanup = cleanup_workflow(
                    &runtime,
                    &handle,
                    session.id.clone(),
                    Some(page.id.clone()),
                    workflow_id,
                )
                .await;
                deliver_cleanup_reply(
                    cleanup_reply,
                    cleanup,
                    &correlation_id,
                    &session.id,
                    Some(&page.id),
                )
                .await;
            }
        }
        Err(_) => {
            let cleanup = cleanup_workflow(
                &runtime,
                &handle,
                session.id.clone(),
                Some(page.id.clone()),
                workflow_id,
            )
            .await;
            warn_cancel_cleanup(&cleanup, &correlation_id, &session.id, Some(&page.id));
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn finish_generation_change(
    runtime: Arc<dyn RuntimeInterface>,
    handle: CapabilityHandle,
    reservation: WorkflowHandleReservation,
    setup_sender: oneshot::Sender<SetupMessage>,
    session: types::SessionState,
    page: types::PageState,
    workflow_id: types::WorkflowId,
    navigation_outcome: Option<types::CommandOutcome>,
    correlation_id: types::CorrelationId,
) {
    let _reservation = reservation;
    let cleanup = cleanup_workflow(
        &runtime,
        &handle,
        session.id.clone(),
        Some(page.id.clone()),
        workflow_id.clone(),
    )
    .await;
    let failure = WorkflowStartFailure {
        reason: "workflowGenerationChanged",
        session: session.clone(),
        page: Some(page.clone()),
        workflow_id,
        navigation_outcome,
        cleanup: cleanup.clone(),
        detail: None,
    };
    deliver_terminal(setup_sender, failure, &correlation_id).await;
}

async fn deliver_terminal(
    setup_sender: oneshot::Sender<SetupMessage>,
    failure: WorkflowStartFailure,
    correlation_id: &types::CorrelationId,
) {
    let cleanup = failure.cleanup.clone();
    let session_id = failure.session.id.clone();
    let page_id = failure.page.as_ref().map(|page| page.id.clone());
    let (response_ack, acknowledged) = oneshot::channel();
    if setup_sender
        .send(SetupMessage::Terminal {
            failure,
            response_ack,
        })
        .is_err()
    {
        warn_cancel_cleanup(&cleanup, correlation_id, &session_id, page_id.as_ref());
        return;
    }
    if acknowledged.await.is_err() {
        warn_cancel_cleanup(&cleanup, correlation_id, &session_id, page_id.as_ref());
    }
}

async fn deliver_cleanup_reply(
    reply: oneshot::Sender<CleanupReply>,
    cleanup: CleanupResult,
    correlation_id: &types::CorrelationId,
    session_id: &types::SessionId,
    page_id: Option<&types::PageId>,
) {
    let (response_ack, acknowledged) = oneshot::channel();
    if reply
        .send(CleanupReply {
            result: cleanup.clone(),
            response_ack,
        })
        .is_err()
    {
        warn_cancel_cleanup(&cleanup, correlation_id, session_id, page_id);
        return;
    }
    if acknowledged.await.is_err() {
        warn_cancel_cleanup(&cleanup, correlation_id, session_id, page_id);
    }
}

async fn request_supervisor_cleanup(
    disposition: oneshot::Sender<SupervisorDisposition>,
    runtime: Arc<dyn RuntimeInterface>,
    handle: CapabilityHandle,
    session_id: types::SessionId,
    page_id: Option<types::PageId>,
    workflow_id: types::WorkflowId,
    correlation_id: types::CorrelationId,
) -> CleanupResponseGuard {
    let (reply, receiver) = oneshot::channel();
    if disposition
        .send(SupervisorDisposition::Cleanup { reply })
        .is_err()
    {
        spawn_fallback_cleanup(
            runtime,
            handle,
            session_id,
            page_id,
            workflow_id,
            correlation_id,
        );
        return CleanupResponseGuard::conservative("workflowSupervisorLost");
    }
    receive_supervisor_cleanup(
        receiver,
        runtime,
        handle,
        session_id,
        page_id,
        workflow_id,
        correlation_id,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn receive_supervisor_cleanup(
    receiver: oneshot::Receiver<CleanupReply>,
    runtime: Arc<dyn RuntimeInterface>,
    handle: CapabilityHandle,
    session_id: types::SessionId,
    page_id: Option<types::PageId>,
    workflow_id: types::WorkflowId,
    correlation_id: types::CorrelationId,
) -> CleanupResponseGuard {
    match receiver.await {
        Ok(reply) => CleanupResponseGuard::supervised(reply),
        Err(_) => {
            spawn_fallback_cleanup(
                runtime,
                handle,
                session_id,
                page_id,
                workflow_id,
                correlation_id,
            );
            CleanupResponseGuard::conservative("workflowSupervisorLost")
        }
    }
}

async fn cleanup_workflow(
    runtime: &Arc<dyn RuntimeInterface>,
    handle: &CapabilityHandle,
    session_id: types::SessionId,
    page_id: Option<types::PageId>,
    workflow_id: types::WorkflowId,
) -> CleanupResult {
    let mut result = CleanupResult {
        page_closed: false,
        session_deleted: false,
        error_code: None,
    };

    if let Some(page_id) = page_id {
        let context = handle.context(
            Utc::now() + Duration::seconds(CLEANUP_DEADLINE_SECONDS),
            None,
        );
        let (context, envelope) = primitive_envelope(
            context,
            session_id.clone(),
            Some(page_id.clone()),
            Some(workflow_id),
            types::PrimitiveCommand::ClosePage(types::ClosePageCommand { page_id }),
        );
        match runtime.submit(context, envelope).await {
            Ok(types::CommandOutcome::Completed { .. }) => result.page_closed = true,
            Ok(outcome) => result.error_code = outcome_error_code(&outcome),
            Err(error) => result.error_code = Some(interface_error_code(&error)),
        }
    }

    let context = handle.context(
        Utc::now() + Duration::seconds(CLEANUP_DEADLINE_SECONDS),
        None,
    );
    match runtime.delete_session(context, session_id).await {
        Ok(()) => result.session_deleted = true,
        Err(error) => {
            if result.error_code.is_none() {
                result.error_code = Some(interface_error_code(&error));
            }
        }
    }
    result
}

fn spawn_fallback_cleanup(
    runtime: Arc<dyn RuntimeInterface>,
    handle: CapabilityHandle,
    session_id: types::SessionId,
    page_id: Option<types::PageId>,
    workflow_id: types::WorkflowId,
    correlation_id: types::CorrelationId,
) {
    tokio::spawn(async move {
        let cleanup = cleanup_workflow(
            &runtime,
            &handle,
            session_id.clone(),
            page_id.clone(),
            workflow_id,
        )
        .await;
        warn_cancel_cleanup(&cleanup, &correlation_id, &session_id, page_id.as_ref());
    });
}

fn warn_cancel_cleanup(
    cleanup: &CleanupResult,
    correlation_id: &types::CorrelationId,
    session_id: &types::SessionId,
    page_id: Option<&types::PageId>,
) {
    if cleanup.succeeded() {
        return;
    }
    tracing::warn!(
        correlation_id = %correlation_id.as_uuid(),
        session_id = %session_id.0,
        page_id = page_id.map(|id| id.0.to_string()).as_deref().unwrap_or("none"),
        cleanup_error_code = cleanup.error_code.as_deref().unwrap_or("unknown"),
        "workflow_start detached cleanup failed"
    );
}

fn interface_failure_outcome(
    command_id: types::CommandId,
    error: types::InterfaceError,
) -> types::CommandOutcome {
    types::CommandOutcome::Failed {
        command_id,
        error: types::CommandError {
            code: types::ErrorCode::Internal,
            message: "runtime interface request failed".into(),
            layer: types::ErrorLayer::Interface,
            retryable: error.retryable,
        },
        evidence: Vec::new(),
    }
}

fn outcome_error_code(outcome: &types::CommandOutcome) -> Option<String> {
    let value = serde_json::to_value(outcome).ok()?;
    value.get("error")?.get("code")?.as_str().map(str::to_owned)
}

fn interface_error_code(error: &types::InterfaceError) -> String {
    serde_json::to_value(error.code)
        .ok()
        .and_then(|value| value.as_str().map(str::to_owned))
        .unwrap_or_else(|| "internal".into())
}

fn workflow_capacity_error(context: &types::RequestContext) -> types::InterfaceError {
    types::InterfaceError {
        code: types::InterfaceErrorCode::ResourceExhausted,
        layer: types::ErrorLayer::Interface,
        message: "runtime interface request failed".into(),
        correlation_id: context.correlation_id.clone(),
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: true,
        required_capability: None,
    }
}

fn workflow_internal_error(context: &types::RequestContext) -> types::InterfaceError {
    types::InterfaceError {
        code: types::InterfaceErrorCode::Internal,
        layer: types::ErrorLayer::Interface,
        message: "runtime interface request failed".into(),
        correlation_id: context.correlation_id.clone(),
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: true,
        required_capability: None,
    }
}

fn workflow_internal_error_with_correlation(
    correlation_id: types::CorrelationId,
) -> types::InterfaceError {
    types::InterfaceError {
        code: types::InterfaceErrorCode::Internal,
        layer: types::ErrorLayer::Interface,
        message: "runtime interface request failed".into(),
        correlation_id,
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: true,
        required_capability: None,
    }
}

#[cfg(test)]
mod tests {
    use std::{
        collections::BTreeMap,
        sync::{Arc as StdArc, Mutex as StdMutex},
    };

    use super::*;
    use interface_core::AuthorityStore;
    use tokio::sync::Mutex;
    use tracing::{
        field::{Field, Visit},
        span::{Attributes, Id, Record},
        Event, Metadata, Subscriber,
    };

    #[derive(Debug)]
    struct RecordedWarning {
        level: tracing::Level,
        fields: BTreeMap<String, String>,
    }

    struct WarningSubscriber {
        events: StdArc<StdMutex<Vec<RecordedWarning>>>,
    }

    #[derive(Default)]
    struct WarningVisitor {
        fields: BTreeMap<String, String>,
    }

    impl Visit for WarningVisitor {
        fn record_debug(&mut self, field: &Field, value: &dyn std::fmt::Debug) {
            self.fields
                .insert(field.name().to_owned(), format!("{value:?}"));
        }
    }

    impl Subscriber for WarningSubscriber {
        fn enabled(&self, metadata: &Metadata<'_>) -> bool {
            *metadata.level() <= tracing::Level::WARN
        }

        fn new_span(&self, _: &Attributes<'_>) -> Id {
            Id::from_u64(1)
        }

        fn record(&self, _: &Id, _: &Record<'_>) {}

        fn record_follows_from(&self, _: &Id, _: &Id) {}

        fn event(&self, event: &Event<'_>) {
            let mut visitor = WarningVisitor::default();
            event.record(&mut visitor);
            self.events
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .push(RecordedWarning {
                    level: *event.metadata().level(),
                    fields: visitor.fields,
                });
        }

        fn enter(&self, _: &Id) {}

        fn exit(&self, _: &Id) {}
    }

    struct ExpiredRequestRuntime {
        session: types::SessionState,
        page: Option<types::PageState>,
        delete_contexts: Mutex<Vec<types::RequestContext>>,
        fail_delete: bool,
    }

    fn unused<T>() -> T {
        panic!("unused RuntimeInterface method in workflow cleanup test")
    }

    #[async_trait::async_trait]
    impl RuntimeInterface for ExpiredRequestRuntime {
        async fn runtime_info(
            &self,
            _: types::RequestContext,
        ) -> interface_core::InterfaceResult<types::RuntimeInfo> {
            unused()
        }

        async fn list_sessions(
            &self,
            _: types::RequestContext,
        ) -> interface_core::InterfaceResult<Vec<types::SessionState>> {
            Ok(vec![self.session.clone()])
        }

        async fn delete_session(
            &self,
            context: types::RequestContext,
            _: types::SessionId,
        ) -> interface_core::InterfaceResult<()> {
            self.delete_contexts.lock().await.push(context);
            if self.fail_delete {
                return Err(types::InterfaceError {
                    code: types::InterfaceErrorCode::Internal,
                    layer: types::ErrorLayer::Interface,
                    message: "injected cleanup failure with secret-profile and secret-url".into(),
                    correlation_id: types::CorrelationId::new(),
                    command_id: None,
                    retryable: false,
                    retry_after_ms: None,
                    reconciliation_required: true,
                    required_capability: None,
                });
            }
            Ok(())
        }

        async fn create_session(
            &self,
            _: types::RequestContext,
            _: types::CreateSessionRequest,
        ) -> interface_core::InterfaceResult<types::SessionState> {
            Ok(self.session.clone())
        }

        async fn open_page(
            &self,
            context: types::RequestContext,
            _: types::OpenPageRequest,
        ) -> interface_core::InterfaceResult<types::PageState> {
            if let Some(page) = &self.page {
                return Ok(page.clone());
            }
            Err(types::InterfaceError {
                code: types::InterfaceErrorCode::Internal,
                layer: types::ErrorLayer::Interface,
                message: "injected page-open failure".into(),
                correlation_id: context.correlation_id,
                command_id: None,
                retryable: false,
                retry_after_ms: None,
                reconciliation_required: false,
                required_capability: None,
            })
        }

        async fn submit(
            &self,
            _: types::RequestContext,
            envelope: types::CommandEnvelope,
        ) -> interface_core::InterfaceResult<types::CommandOutcome> {
            Ok(types::CommandOutcome::Completed {
                command_id: envelope.command_id,
                evidence: Vec::new(),
            })
        }

        async fn checkpoint(
            &self,
            _: types::RequestContext,
            _: types::WorkflowCheckpoint,
            _: Vec<types::Evidence>,
        ) -> interface_core::InterfaceResult<types::WorkflowCheckpoint> {
            unused()
        }

        async fn resolve_command_evidence(
            &self,
            _: types::RequestContext,
            _: Vec<types::CommandId>,
        ) -> interface_core::InterfaceResult<Vec<types::Evidence>> {
            unused()
        }

        async fn recover(
            &self,
            _: types::RequestContext,
            _: types::WorkflowId,
        ) -> interface_core::InterfaceResult<types::RecoveryDecision> {
            unused()
        }

        async fn recovery_status(
            &self,
            _: types::RequestContext,
            _: types::WorkflowId,
        ) -> interface_core::InterfaceResult<types::RecoveryStatus> {
            unused()
        }

        async fn submit_with_auto_checkpoint(
            &self,
            _: types::RequestContext,
            _: types::CommandEnvelope,
        ) -> interface_core::InterfaceResult<(types::CommandOutcome, types::CheckpointId)> {
            unused()
        }

        async fn workflows_for_session(
            &self,
            _: types::RequestContext,
            _: types::SessionId,
            _: usize,
        ) -> interface_core::InterfaceResult<Vec<types::WorkflowId>> {
            unused()
        }
    }

    #[tokio::test]
    async fn cleanup_after_expired_request_uses_fresh_bounded_context_from_same_handle() {
        let authority = AuthorityStore::with_capacity(1);
        let token = authority
            .issue(
                types::PrincipalId::from_uuid(uuid::Uuid::from_u128(700)),
                [
                    types::Capability::SessionRead,
                    types::Capability::SessionWrite,
                    types::Capability::PageWrite,
                ],
                Utc::now() + Duration::hours(1),
            )
            .await
            .unwrap();
        let handle = authority.verify(&token.expose_once()).await.unwrap();
        let original = handle.context(Utc::now() - Duration::seconds(1), None);
        let original_correlation = original.correlation_id.clone();
        let now = Utc::now();
        let session = types::SessionState {
            id: types::SessionId::new(),
            profile: "expired-request".into(),
            proxy: None,
            page_ids: Vec::new(),
            created_at: now,
            last_used_at: now,
            execution_policy: types::ExecutionPolicy::default(),
        };
        let runtime = Arc::new(ExpiredRequestRuntime {
            session,
            page: None,
            delete_contexts: Mutex::new(Vec::new()),
            fail_delete: false,
        });
        let registry = Arc::new(crate::workflow_handles::WorkflowHandles::new());
        let reservation = registry.reserve().unwrap();
        let (sender, receiver) = oneshot::channel();

        let supervisor = tokio::spawn(supervise_start(
            runtime.clone(),
            handle.clone(),
            original.clone(),
            reservation,
            types::CreateSessionRequest {
                profile: "expired-request".into(),
                proxy: None,
                execution_policy: types::ExecutionPolicy::default(),
            },
            None,
            types::WorkflowId::new(),
            original.correlation_id,
            sender,
        ));

        let SetupMessage::Terminal {
            failure,
            response_ack,
        } = receiver.await.unwrap()
        else {
            panic!("page-open failure returns a stable terminal setup result")
        };
        assert!(failure.cleanup.session_deleted);
        response_ack.send(()).unwrap();
        supervisor.await.unwrap();
        let contexts = runtime.delete_contexts.lock().await;
        assert_eq!(contexts.len(), 1);
        let cleanup = &contexts[0];
        assert!(cleanup.deadline > Utc::now() + Duration::seconds(25));
        assert!(cleanup.deadline <= Utc::now() + Duration::seconds(31));
        assert_eq!(cleanup.principal_id, *handle.principal_id());
        assert_ne!(cleanup.correlation_id, original_correlation);
        for capability in [
            types::Capability::SessionRead,
            types::Capability::SessionWrite,
            types::Capability::PageWrite,
        ] {
            assert!(cleanup.capabilities.contains(capability));
        }
    }

    #[test]
    fn cancellation_cleanup_warning_is_single_bounded_and_sanitized() {
        let events = StdArc::new(StdMutex::new(Vec::new()));
        let subscriber = WarningSubscriber {
            events: StdArc::clone(&events),
        };
        let correlation_id = types::CorrelationId::new();
        let session_id = types::SessionId(uuid::Uuid::from_u128(701));
        let page_id = types::PageId(uuid::Uuid::from_u128(702));

        tracing::subscriber::with_default(subscriber, || {
            warn_cancel_cleanup(
                &CleanupResult {
                    page_closed: true,
                    session_deleted: false,
                    error_code: Some("internal".into()),
                },
                &correlation_id,
                &session_id,
                Some(&page_id),
            );
            warn_cancel_cleanup(
                &CleanupResult {
                    page_closed: false,
                    session_deleted: true,
                    error_code: None,
                },
                &correlation_id,
                &session_id,
                Some(&page_id),
            );
        });

        let events = events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(events.len(), 1, "one failed cleanup emits one warning");
        let warning = &events[0];
        assert_eq!(warning.level, tracing::Level::WARN);
        assert_eq!(
            warning
                .fields
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            vec![
                "cleanup_error_code",
                "correlation_id",
                "message",
                "page_id",
                "session_id",
            ]
        );
        let rendered = warning
            .fields
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(rendered.contains(&correlation_id.as_uuid().to_string()));
        assert!(rendered.contains(&session_id.0.to_string()));
        assert!(rendered.contains(&page_id.0.to_string()));
        assert!(rendered.contains("internal"));
        assert!(rendered.contains("workflow_start detached cleanup failed"));
        for forbidden in ["profile", "proxy", "url", "page_content"] {
            assert!(!rendered.contains(forbidden), "warning leaked {forbidden}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_during_terminal_response_construction_emits_one_cleanup_warning() {
        let events = StdArc::new(StdMutex::new(Vec::new()));
        let subscriber = WarningSubscriber {
            events: StdArc::clone(&events),
        };
        let dispatch = tracing::Dispatch::new(subscriber);
        let _guard = tracing::dispatcher::set_default(&dispatch);

        let authority = AuthorityStore::with_capacity(1);
        let token = authority
            .issue(
                types::PrincipalId::from_uuid(uuid::Uuid::from_u128(703)),
                types::Capability::ALL.iter().copied(),
                Utc::now() + Duration::hours(1),
            )
            .await
            .unwrap();
        let handle = authority.verify(&token.expose_once()).await.unwrap();
        let now = Utc::now();
        let session = types::SessionState {
            id: types::SessionId(uuid::Uuid::from_u128(704)),
            profile: "terminal-response-cancellation".into(),
            proxy: None,
            page_ids: Vec::new(),
            created_at: now,
            last_used_at: now,
            execution_policy: types::ExecutionPolicy::default(),
        };
        let runtime = Arc::new(ExpiredRequestRuntime {
            session,
            page: None,
            delete_contexts: Mutex::new(Vec::new()),
            fail_delete: true,
        });
        let gate = Arc::new(crate::resources::ArtifactListTestGate::default());
        let server = Arc::new(Server::for_interface(
            runtime.clone(),
            handle,
            EventStore::new(16),
            crate::ArtifactResources::default().with_list_test_gate(Arc::clone(&gate)),
        ));
        server
            .handle_message(json!({
                "jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{"protocolVersion":"2025-11-25","capabilities":{},
                          "clientInfo":{"name":"terminal-ack-test","version":"1"}}
            }))
            .await;
        server
            .handle_message(json!({
                "jsonrpc":"2.0","method":"notifications/initialized","params":{}
            }))
            .await;

        let request_server = Arc::clone(&server);
        let request = tokio::spawn(async move {
            request_server
                .handle_message(json!({
                    "jsonrpc":"2.0","id":9,"method":"tools/call",
                    "params":{"name":"workflow_start","arguments":{"profile":"ack-race"}}
                }))
                .await
                .expect("workflow_start answered")
        });
        tokio::time::timeout(
            Duration::seconds(5).to_std().unwrap(),
            gate.wait_until_entered(),
        )
        .await
        .expect("terminal response entered artifact enumeration");

        server
            .handle_message(json!({
                "jsonrpc":"2.0","method":"notifications/cancelled",
                "params":{"requestId":9}
            }))
            .await;
        let response = tokio::time::timeout(Duration::seconds(5).to_std().unwrap(), request)
            .await
            .expect("cancelled terminal response completed")
            .unwrap();
        gate.release();
        assert_eq!(
            response["error"]["message"], "Request cancelled",
            "{response}"
        );

        tokio::time::timeout(Duration::seconds(5).to_std().unwrap(), async {
            loop {
                if !events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("supervisor emitted its cancellation cleanup warning");
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        let delete_context_count = runtime.delete_contexts.lock().await.len();
        let events = events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(
            events.len(),
            1,
            "terminal handoff emits exactly one warning"
        );
        assert_eq!(events[0].level, tracing::Level::WARN);
        let rendered = events[0]
            .fields
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(rendered.contains("internal"));
        assert!(rendered.contains("workflow_start detached cleanup failed"));
        assert!(!rendered.contains("secret-profile"));
        assert!(!rendered.contains("secret-url"));
        assert_eq!(delete_context_count, 1);
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_after_direct_cleanup_reply_emits_one_sanitized_warning() {
        let events = StdArc::new(StdMutex::new(Vec::new()));
        let dispatch = tracing::Dispatch::new(WarningSubscriber {
            events: StdArc::clone(&events),
        });
        let _guard = tracing::dispatcher::set_default(&dispatch);

        let authority = AuthorityStore::with_capacity(1);
        let token = authority
            .issue(
                types::PrincipalId::from_uuid(uuid::Uuid::from_u128(705)),
                types::Capability::ALL.iter().copied(),
                Utc::now() + Duration::hours(1),
            )
            .await
            .unwrap();
        let handle = authority.verify(&token.expose_once()).await.unwrap();
        let now = Utc::now();
        let session = types::SessionState {
            id: types::SessionId(uuid::Uuid::from_u128(706)),
            profile: "direct-cleanup-reply".into(),
            proxy: None,
            page_ids: Vec::new(),
            created_at: now,
            last_used_at: now,
            execution_policy: types::ExecutionPolicy::default(),
        };
        let page = types::PageState {
            id: types::PageId(uuid::Uuid::from_u128(707)),
            session_id: session.id.clone(),
            url: None,
            mode: types::PageMode::Document,
            ready_state: "created".into(),
            pending_requests: 0,
        };
        let runtime = Arc::new(ExpiredRequestRuntime {
            session: session.clone(),
            page: Some(page.clone()),
            delete_contexts: Mutex::new(Vec::new()),
            fail_delete: true,
        });
        let registry = Arc::new(crate::workflow_handles::WorkflowHandles::new());
        let reservation = registry.reserve().unwrap();
        let uncommitted_handle = reservation.handle().to_owned();
        let workflow_id = types::WorkflowId::new();
        let correlation_id = types::CorrelationId::new();
        let (setup_sender, setup_receiver) = oneshot::channel();
        let supervisor = tokio::spawn(supervise_start(
            runtime.clone(),
            handle.clone(),
            handle.context(Utc::now() + Duration::minutes(1), None),
            reservation,
            types::CreateSessionRequest {
                profile: "direct-cleanup-reply".into(),
                proxy: None,
                execution_policy: types::ExecutionPolicy::default(),
            },
            None,
            workflow_id.clone(),
            correlation_id.clone(),
            setup_sender,
        ));
        let SetupMessage::Ready(ready) = setup_receiver.await.unwrap() else {
            panic!("successful setup reaches SetupReady")
        };
        let SetupReady {
            reservation,
            session,
            page,
            navigation_outcome,
            disposition,
        } = ready;
        let cleanup = request_supervisor_cleanup(
            disposition,
            runtime.clone(),
            handle.clone(),
            session.id.clone(),
            Some(page.id.clone()),
            workflow_id.clone(),
            correlation_id,
        )
        .await;
        assert!(!cleanup.result().session_deleted);

        let gate = Arc::new(crate::resources::ArtifactListTestGate::default());
        let server = Arc::new(Server::for_interface(
            runtime.clone(),
            handle,
            EventStore::new(16),
            crate::ArtifactResources::default().with_list_test_gate(Arc::clone(&gate)),
        ));
        let response_server = Arc::clone(&server);
        let response = tokio::spawn(async move {
            let failure = WorkflowStartFailure {
                reason: "workflowGenerationChanged",
                session,
                page: Some(page),
                workflow_id,
                navigation_outcome,
                cleanup: cleanup.result().clone(),
                detail: None,
            };
            cleanup
                .acknowledge_after(
                    response_server.workflow_start_failure_response(json!(11), failure),
                )
                .await
        });
        tokio::time::timeout(
            Duration::seconds(5).to_std().unwrap(),
            gate.wait_until_entered(),
        )
        .await
        .expect("post-ready failure response entered resource enumeration");
        response.abort();
        assert!(response.await.unwrap_err().is_cancelled());
        gate.release();
        drop(reservation);

        tokio::time::timeout(Duration::seconds(5).to_std().unwrap(), async {
            loop {
                if !events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cleanup-reply owner emitted a cancellation warning");
        supervisor.await.unwrap();
        let delete_context_count = runtime.delete_contexts.lock().await.len();
        let events = events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(events.len(), 1, "one dropped response emits one warning");
        let rendered = events[0]
            .fields
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(rendered.contains("internal"));
        assert!(rendered.contains("workflow_start detached cleanup failed"));
        assert!(!rendered.contains("secret-profile"));
        assert!(!rendered.contains("secret-url"));
        assert_eq!(delete_context_count, 1);
        assert_eq!(
            registry.resolve(&uncommitted_handle),
            Err(WorkflowHandleError::Unknown)
        );
    }

    #[tokio::test(flavor = "current_thread")]
    async fn cancellation_after_publication_generation_cleanup_reply_warns_once() {
        let events = StdArc::new(StdMutex::new(Vec::new()));
        let dispatch = tracing::Dispatch::new(WarningSubscriber {
            events: StdArc::clone(&events),
        });
        let _guard = tracing::dispatcher::set_default(&dispatch);

        let authority = AuthorityStore::with_capacity(1);
        let token = authority
            .issue(
                types::PrincipalId::from_uuid(uuid::Uuid::from_u128(708)),
                types::Capability::ALL.iter().copied(),
                Utc::now() + Duration::hours(1),
            )
            .await
            .unwrap();
        let handle = authority.verify(&token.expose_once()).await.unwrap();
        let now = Utc::now();
        let session = types::SessionState {
            id: types::SessionId(uuid::Uuid::from_u128(709)),
            profile: "publication-generation-reply".into(),
            proxy: None,
            page_ids: Vec::new(),
            created_at: now,
            last_used_at: now,
            execution_policy: types::ExecutionPolicy::default(),
        };
        let page = types::PageState {
            id: types::PageId(uuid::Uuid::from_u128(710)),
            session_id: session.id.clone(),
            url: None,
            mode: types::PageMode::Document,
            ready_state: "created".into(),
            pending_requests: 0,
        };
        let runtime = Arc::new(ExpiredRequestRuntime {
            session,
            page: Some(page),
            delete_contexts: Mutex::new(Vec::new()),
            fail_delete: true,
        });
        let gate = Arc::new(crate::resources::ArtifactListTestGate::default());
        let server = Arc::new(Server::for_interface(
            runtime.clone(),
            handle,
            EventStore::new(16),
            crate::ArtifactResources::default().with_list_test_gate(Arc::clone(&gate)),
        ));
        server
            .handle_message(json!({
                "jsonrpc":"2.0","id":1,"method":"initialize",
                "params":{"protocolVersion":"2025-11-25","capabilities":{},
                          "clientInfo":{"name":"publication-ack-test","version":"1"}}
            }))
            .await;
        server
            .handle_message(json!({
                "jsonrpc":"2.0","method":"notifications/initialized","params":{}
            }))
            .await;

        let request_server = Arc::clone(&server);
        let request = tokio::spawn(async move {
            request_server
                .handle_message(json!({
                    "jsonrpc":"2.0","id":12,"method":"tools/call",
                    "params":{"name":"workflow_start","arguments":{"profile":"publish-race"}}
                }))
                .await
                .expect("workflow_start answered")
        });
        tokio::time::timeout(
            Duration::seconds(5).to_std().unwrap(),
            gate.wait_until_entered(),
        )
        .await
        .expect("success response entered resource enumeration");
        server
            .handle_message(json!({
                "jsonrpc":"2.0","id":2,"method":"initialize",
                "params":{"protocolVersion":"2025-11-25","capabilities":{},
                          "clientInfo":{"name":"publication-reset","version":"1"}}
            }))
            .await;
        server
            .handle_message(json!({
                "jsonrpc":"2.0","method":"notifications/initialized","params":{}
            }))
            .await;
        gate.release();

        tokio::time::timeout(
            Duration::seconds(5).to_std().unwrap(),
            gate.wait_until_entered(),
        )
        .await
        .expect("supervisor-lost fallback entered resource enumeration");
        gate.release();
        tokio::time::timeout(
            Duration::seconds(5).to_std().unwrap(),
            gate.wait_until_entered(),
        )
        .await
        .expect("publication generation failure entered resource enumeration");

        server
            .handle_message(json!({
                "jsonrpc":"2.0","method":"notifications/cancelled",
                "params":{"requestId":12}
            }))
            .await;
        let response = tokio::time::timeout(Duration::seconds(5).to_std().unwrap(), request)
            .await
            .expect("cancelled publication-generation response completed")
            .unwrap();
        gate.release();
        assert_eq!(
            response["error"]["message"], "Request cancelled",
            "{response}"
        );
        assert!(
            response["result"].is_null(),
            "no handle response was delivered"
        );

        tokio::time::timeout(Duration::seconds(5).to_std().unwrap(), async {
            loop {
                if !events
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("publication cleanup owner emitted a cancellation warning");
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        let delete_context_count = runtime.delete_contexts.lock().await.len();
        let events = events
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert_eq!(events.len(), 1, "one dropped response emits one warning");
        let rendered = events[0]
            .fields
            .values()
            .cloned()
            .collect::<Vec<_>>()
            .join(" ");
        assert!(rendered.contains("internal"));
        assert!(rendered.contains("workflow_start detached cleanup failed"));
        assert!(!rendered.contains("secret-profile"));
        assert!(!rendered.contains("secret-url"));
        assert_eq!(delete_context_count, 1);
    }
}
