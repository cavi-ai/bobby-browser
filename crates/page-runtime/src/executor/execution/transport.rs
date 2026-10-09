//! Probe before reconnecting; preserve a live process and never replay mutations.
use super::*;

impl ExecutionContext<'_> {
    pub(super) async fn recover(
        &self,
        failure: AdaptiveFailure,
        lease: worker_pool::WorkerLease,
        page: types::PageState,
    ) -> ExecutionResult {
        let probe = lease.worker().list_pages(&ListPagesCommand).await;
        let probe_error = probe.as_ref().err().map(|error| error.message.clone());
        if let Ok(evidence) = probe {
            if !page_still_listed(&evidence, &page.id) {
                return Err(self.finish(failure).await);
            }
            if self.envelope.command.class() == types::CommandClass::Replayable {
                return self.retry(lease, page, Vec::new()).await;
            }
            return Err(self.runtime.finish_failure(self.envelope, CommandOutcome::Failed {
                command_id: self.envelope.command_id.clone(),
                error: CommandError {
                    code: ErrorCode::TargetDetached,
                    message: "transient target loss; the browser and page are intact -- re-resolve the target and re-issue the command".into(),
                    layer: ErrorLayer::Driver,
                    retryable: true,
                },
                evidence: vec![transient_target_loss_evidence(&failure.error.message)],
            }).await);
        }
        match lease.worker().reconnect_live_process().await {
            Ok(_) => self.after_reattach(failure, lease, page).await,
            Err(error) => {
                self.revive(failure, lease, page, probe_error, error.message)
                    .await
            }
        }
    }

    async fn after_reattach(
        &self,
        failure: AdaptiveFailure,
        lease: worker_pool::WorkerLease,
        page: types::PageState,
    ) -> ExecutionResult {
        tracing::info!(
            session_id = %self.envelope.session_id.0,
            command_id = %self.envelope.command_id.0,
            "transport reset reattached to the live browser"
        );
        // A failed listing is not proof that the page closed.
        let page_open = match lease.worker().list_pages(&ListPagesCommand).await {
            Ok(evidence) => page_still_listed(&evidence, &page.id),
            Err(_) => true,
        };
        if !page_open {
            return Err(self.finish(failure).await);
        }
        if self.envelope.command.class() == types::CommandClass::Replayable {
            return self.retry(lease, page, vec![reattach_evidence()]).await;
        }
        let mut error = failure.error;
        error.message = format!(
            "{} (CDP transport reset; reattached to the live browser — page state preserved, inspect before re-issuing)",
            error.message
        );
        error.retryable = true;
        let mut evidence = failure.evidence;
        evidence.push(reattach_evidence());
        Err(self.finish(AdaptiveFailure { error, evidence }).await)
    }
}
