use std::time::Instant;
use tracing::Span;
use types::{CommandClass, CommandEnvelope, CommandOutcome, RuntimeCommand};

/// Diagnostic correlation only: a missing completion never authorizes a retry.
pub(crate) struct CommandTrace {
    pub(crate) span: Span,
    started: Instant,
    finished: bool,
}

impl CommandTrace {
    pub(crate) fn new(envelope: &CommandEnvelope) -> Self {
        let family = match envelope.command {
            RuntimeCommand::Primitive(_) => "primitive",
            RuntimeCommand::Intent(_) => "intent",
        };
        let class = match envelope.command.class() {
            CommandClass::Replayable => "replayable",
            CommandClass::Reconciliable => "reconciliable",
            CommandClass::Boundary => "boundary",
        };
        Self {
            span: tracing::info_span!("command", command_id = %envelope.command_id.0,
                workflow_id = %envelope.workflow_id.0, attempt_id = %envelope.attempt_id.0,
                session_id = %envelope.session_id.0, family, class,
                runtime_version = env!("CARGO_PKG_VERSION")),
            started: Instant::now(),
            finished: false,
        }
    }

    pub(crate) fn finish(&mut self, outcome: &CommandOutcome) {
        self.finished = true;
        self.emit(outcome_name(outcome));
    }

    fn emit(&self, outcome: &'static str) {
        tracing::info!(parent: &self.span, outcome,
            elapsed_ms = self.started.elapsed().as_millis().min(u64::MAX as u128) as u64,
            "command.finished");
    }
}

impl Drop for CommandTrace {
    fn drop(&mut self) {
        if !self.finished {
            self.emit("interrupted");
        }
    }
}

fn outcome_name(outcome: &CommandOutcome) -> &'static str {
    match outcome {
        CommandOutcome::Completed { .. } => "completed",
        CommandOutcome::RetryableFailure { .. } => "retryableFailure",
        CommandOutcome::NeedsReconciliation { .. } => "needsReconciliation",
        CommandOutcome::PolicyDenied { .. } => "policyDenied",
        CommandOutcome::ResourceExhausted { .. } => "resourceExhausted",
        CommandOutcome::Restarted { .. } => "restarted",
        CommandOutcome::Failed { .. } => "failed",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use observability::test_support::CaptureSink;

    #[test]
    fn dropped_trace_reports_interrupted_without_a_success_claim() {
        let sink = CaptureSink::install();
        drop(CommandTrace {
            span: tracing::info_span!("command"),
            started: Instant::now(),
            finished: false,
        });
        let events = sink.events();
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["fields"]["outcome"], "interrupted");
    }

    #[test]
    fn every_outcome_keeps_its_wire_status_and_hides_error_payloads() {
        let sink = CaptureSink::install();
        for status in [
            "completed",
            "retryableFailure",
            "needsReconciliation",
            "policyDenied",
            "resourceExhausted",
            "restarted",
            "failed",
        ] {
            let outcome: CommandOutcome = serde_json::from_value(serde_json::json!({
                "status": status, "commandId": uuid::Uuid::new_v4(), "evidence": [],
                "error": {"code": "internal", "layer": "workflow", "message": "PRIVATE_ERROR", "retryable": false},
                "retryAfterMs": 1, "priorAttemptId": uuid::Uuid::new_v4(), "attemptId": uuid::Uuid::new_v4(), "reason": "PRIVATE_REASON"
            })).unwrap();
            assert_eq!(outcome_name(&outcome), status);
            let mut trace = CommandTrace {
                span: tracing::info_span!("command"),
                started: Instant::now(),
                finished: false,
            };
            trace.finish(&outcome);
        }
        let events = sink.events();
        assert_eq!(events.len(), 7);
        assert!(!serde_json::to_string(&events).unwrap().contains("PRIVATE_"));
    }
}
