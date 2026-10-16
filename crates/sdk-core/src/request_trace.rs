//! Request accounting is separate from execution evidence and replay authority.
use std::time::Instant;
use tracing::Span;
use types::{CommandEnvelope, CommandOutcome, InterfaceError, RequestContext};

pub(crate) struct RequestTrace {
    pub(crate) span: Span,
    started: Instant,
    runtime_invoked: bool,
    replayed: bool,
    finished: bool,
}

impl RequestTrace {
    pub(crate) fn new(
        ctx: &RequestContext,
        envelope: &CommandEnvelope,
        mode: &'static str,
    ) -> Self {
        Self {
            span: tracing::info_span!("command_request", correlation_id = %ctx.correlation_id.as_uuid(),
                command_id = %envelope.command_id.0, workflow_id = %envelope.workflow_id.0,
                attempt_id = %envelope.attempt_id.0, session_id = %envelope.session_id.0,
                request_mode = mode, keyed = ctx.idempotency_key.is_some(),
                runtime_version = env!("CARGO_PKG_VERSION")),
            started: Instant::now(),
            runtime_invoked: false,
            replayed: false,
            finished: false,
        }
    }

    pub(crate) fn runtime_invoked(&mut self) {
        self.runtime_invoked = true;
    }
    pub(crate) fn replayed(&mut self) {
        self.replayed = true;
    }

    pub(crate) fn finish(&mut self, result: Result<&CommandOutcome, &InterfaceError>) {
        let outcome_command_id = result.as_ref().ok().map(|outcome| match outcome {
            CommandOutcome::Completed { command_id, .. }
            | CommandOutcome::RetryableFailure { command_id, .. }
            | CommandOutcome::NeedsReconciliation { command_id, .. }
            | CommandOutcome::PolicyDenied { command_id, .. }
            | CommandOutcome::ResourceExhausted { command_id, .. }
            | CommandOutcome::Restarted { command_id, .. }
            | CommandOutcome::Failed { command_id, .. } => command_id.0.to_string(),
        });
        let (outcome, error_code) = match result {
            Ok(outcome) => (
                crate::command_trace::outcome_name(outcome),
                "none".to_owned(),
            ),
            Err(error) => ("interfaceError", format!("{:?}", error.code)),
        };
        self.finished = true;
        self.emit(
            false,
            outcome,
            &error_code,
            outcome_command_id.as_deref().unwrap_or("none"),
        );
    }

    fn emit(&self, interrupted: bool, outcome: &str, error_code: &str, outcome_command_id: &str) {
        let disposition = if interrupted {
            "interrupted"
        } else if self.replayed {
            "replayed"
        } else if self.runtime_invoked {
            "runtimeInvoked"
        } else {
            "rejected"
        };
        tracing::info!(parent: &self.span, disposition, outcome, error_code, outcome_command_id,
            runtime_invoked = self.runtime_invoked, replayed = self.replayed, interrupted,
            elapsed_ms = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            "command.request.finished");
    }
}

impl Drop for RequestTrace {
    fn drop(&mut self) {
        if !self.finished {
            self.emit(true, "notReturned", "none", "none");
        }
    }
}
