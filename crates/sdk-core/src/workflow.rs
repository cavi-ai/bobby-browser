//! Typed workflow setup and observation shared by Rust adapters.
//!
//! A transport owns publication and handles. Setup returns partial resources
//! on failure so that its cancellation supervisor can clean them up.

use std::{future::Future, sync::Arc};

use interface_core::RuntimeInterface;
use types::{
    AccessibilitySnapshotCommand, AttemptId, ClosePageCommand, CommandEnvelope, CommandId,
    CommandOutcome, CreateSessionRequest, InterfaceError, NavigateCommand, OpenPageRequest, PageId,
    PageState, PrimitiveCommand, RequestContext, RuntimeCommand, SessionId, SessionState,
    WaitUntil, WorkflowId,
};

#[derive(Debug)]
pub enum WorkflowSetupFailure {
    Session(InterfaceError),
    Cancelled {
        session: Box<SessionState>,
        page: Option<PageState>,
    },
    Page {
        session: Box<SessionState>,
        error: InterfaceError,
    },
}

#[derive(Debug)]
pub struct WorkflowSetup {
    pub session: SessionState,
    pub page: PageState,
}

#[derive(Debug)]
pub struct WorkflowObservation<T = CommandOutcome> {
    pub session_id: SessionId,
    pub page_id: PageId,
    pub workflow_id: WorkflowId,
    pub outcome: T,
    pub page_derived: bool,
}

pub struct WorkflowCleanup {
    pub page_close: Option<Result<CommandOutcome, InterfaceError>>,
    pub session_delete: Result<(), InterfaceError>,
}

#[derive(Clone)]
pub struct WorkflowService {
    runtime: Arc<dyn RuntimeInterface>,
}

impl WorkflowService {
    pub fn new(runtime: Arc<dyn RuntimeInterface>) -> Self {
        Self { runtime }
    }

    /// The caller supplies its cancellation/generation check. A detached
    /// supervisor must own this future until it handles every partial result.
    pub async fn prepare(
        &self,
        context: RequestContext,
        request: CreateSessionRequest,
        still_current: impl Fn() -> bool,
    ) -> Result<WorkflowSetup, WorkflowSetupFailure> {
        let mut session = self
            .runtime
            .create_session(context.clone(), request)
            .await
            .map_err(WorkflowSetupFailure::Session)?;
        if !still_current() {
            return Err(WorkflowSetupFailure::Cancelled {
                session: Box::new(session),
                page: None,
            });
        }
        let page = self
            .runtime
            .open_page(
                context,
                OpenPageRequest {
                    session_id: session.id.clone(),
                },
            )
            .await
            .map_err(|error| WorkflowSetupFailure::Page {
                session: Box::new(session.clone()),
                error,
            })?;
        if !still_current() {
            return Err(WorkflowSetupFailure::Cancelled {
                session: Box::new(session),
                page: Some(page),
            });
        }
        if !session.page_ids.contains(&page.id) {
            session.page_ids.push(page.id.clone());
        }
        Ok(WorkflowSetup { session, page })
    }

    pub fn navigation_envelope(
        context: &RequestContext,
        session_id: SessionId,
        page_id: PageId,
        workflow_id: WorkflowId,
        url: String,
        timeout_ms: u64,
    ) -> CommandEnvelope {
        Self::envelope(
            context,
            session_id,
            page_id,
            workflow_id,
            PrimitiveCommand::Navigate(NavigateCommand {
                url,
                wait_until: WaitUntil::Interactive,
                timeout_ms,
            }),
        )
    }

    /// Dispatch optional navigation while retaining the command identity even
    /// when the interface fails before returning an outcome.
    #[allow(clippy::too_many_arguments)]
    pub async fn navigate_optional(
        &self,
        context: RequestContext,
        session_id: SessionId,
        page_id: PageId,
        workflow_id: WorkflowId,
        url: Option<String>,
        timeout_ms: u64,
    ) -> Option<(CommandId, Result<CommandOutcome, InterfaceError>)> {
        let envelope =
            Self::navigation_envelope(&context, session_id, page_id, workflow_id, url?, timeout_ms);
        let command_id = envelope.command_id.clone();
        Some((command_id, self.runtime.submit(context, envelope).await))
    }

    pub fn observation_envelope(
        context: &RequestContext,
        session_id: SessionId,
        page_id: PageId,
        workflow_id: WorkflowId,
        max_nodes: u32,
        target: Option<types::TargetSpec>,
    ) -> CommandEnvelope {
        Self::envelope(
            context,
            session_id,
            page_id,
            workflow_id,
            PrimitiveCommand::AccessibilitySnapshot(AccessibilitySnapshotCommand {
                max_nodes: Some(max_nodes),
                target,
            }),
        )
    }

    fn envelope(
        context: &RequestContext,
        session_id: SessionId,
        page_id: PageId,
        workflow_id: WorkflowId,
        command: PrimitiveCommand,
    ) -> CommandEnvelope {
        CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id,
            attempt_id: AttemptId::new(),
            session_id,
            page_id: Some(page_id),
            deadline: context.deadline,
            command: RuntimeCommand::Primitive(command),
        }
    }

    /// Direct Rust consumers receive a typed observation. MCP can use the
    /// same envelope while retaining its own resource registration and handle
    /// fallback around dispatch.
    #[allow(clippy::too_many_arguments)]
    pub async fn observe_with<T, F, Fut>(
        &self,
        context: RequestContext,
        session_id: SessionId,
        page_id: PageId,
        workflow_id: WorkflowId,
        max_nodes: u32,
        target: Option<types::TargetSpec>,
        dispatch: F,
    ) -> Result<WorkflowObservation<T>, InterfaceError>
    where
        F: FnOnce(CommandEnvelope) -> Fut,
        Fut: Future<Output = Result<T, InterfaceError>>,
    {
        let envelope = Self::observation_envelope(
            &context,
            session_id.clone(),
            page_id.clone(),
            workflow_id.clone(),
            max_nodes,
            target,
        );
        let outcome = dispatch(envelope).await?;
        Ok(WorkflowObservation {
            session_id,
            page_id,
            workflow_id,
            outcome,
            page_derived: true,
        })
    }

    pub async fn observe_live(
        &self,
        context: RequestContext,
        session_id: SessionId,
        page_id: PageId,
        workflow_id: WorkflowId,
        max_nodes: u32,
        target: Option<types::TargetSpec>,
    ) -> Result<WorkflowObservation, InterfaceError> {
        let dispatch_context = context.clone();
        self.observe_with(
            context,
            session_id,
            page_id,
            workflow_id,
            max_nodes,
            target,
            |envelope| self.runtime.submit(dispatch_context, envelope),
        )
        .await
    }

    /// A post-action observation is an optional optimization. An observation
    /// failure must not turn a completed action into a failure.
    pub async fn post_action_with<T, E, U, F, Fut>(
        &self,
        result: Result<T, E>,
        completed: impl FnOnce(&T) -> bool,
        observe: F,
    ) -> Result<(T, Option<U>), E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<U, E>>,
    {
        let action = result?;
        let post_state = if completed(&action) {
            observe().await.ok()
        } else {
            None
        };
        Ok((action, post_state))
    }

    /// Clean partial setup with a fresh caller context for each operation.
    /// The caller's detached supervisor owns this future until it finishes.
    pub async fn cleanup(
        &self,
        context: impl Fn() -> RequestContext,
        session_id: SessionId,
        page_id: Option<PageId>,
        workflow_id: WorkflowId,
    ) -> WorkflowCleanup {
        let page_close = if let Some(page_id) = page_id {
            let request_context = context();
            let envelope = Self::envelope(
                &request_context,
                session_id.clone(),
                page_id.clone(),
                workflow_id,
                PrimitiveCommand::ClosePage(ClosePageCommand { page_id }),
            );
            Some(self.runtime.submit(request_context, envelope).await)
        } else {
            None
        };
        let session_delete = self.runtime.delete_session(context(), session_id).await;
        WorkflowCleanup {
            page_close,
            session_delete,
        }
    }
}
