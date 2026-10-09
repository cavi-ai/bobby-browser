//! Fail-closed admission and durable pre-execution transitions.
use super::*;

impl PageRuntime {
    pub(super) async fn admit(
        &self,
        envelope: &CommandEnvelope,
    ) -> Result<
        (
            &Arc<dyn workflow_journal::CommandJournal>,
            &Arc<worker_pool::WorkerPool>,
        ),
        CommandOutcome,
    > {
        let failed = |error| CommandOutcome::Failed {
            command_id: envelope.command_id.clone(),
            error,
            evidence: Vec::new(),
        };
        self.validate(envelope).await.map_err(failed)?;
        let journal = self
            .journal
            .as_ref()
            .ok_or_else(|| failed(internal_error("command journal is not configured")))?;
        let workers = self
            .workers
            .as_ref()
            .ok_or_else(|| failed(internal_error("browser workers are not configured")))?;
        for phase in [
            CommandPhase::Accepted,
            CommandPhase::Prepared,
            CommandPhase::Executing,
        ] {
            let accepted_envelope =
                (phase == CommandPhase::Accepted).then(|| envelope.journal_safe());
            journal
                .append(record(envelope, phase, accepted_envelope, None))
                .await
                .map_err(|error| journal_failure(envelope, error, false))?;
            self.observe_durable_phase(phase).await;
        }
        Ok((journal, workers))
    }
}
