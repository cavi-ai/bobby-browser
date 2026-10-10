//! One bounded attempt followed by evidence-driven transport recovery.
use super::*;
use crate::adaptive::{AdaptiveExecution, AdaptiveFailure};
mod deadline;
mod revival;
mod transport;

type ExecutionResult =
    Result<(AdaptiveExecution, Option<worker_pool::WorkerLease>), CommandOutcome>;

struct ExecutionContext<'a> {
    runtime: &'a PageRuntime,
    envelope: &'a CommandEnvelope,
    gate: &'a SessionGate,
    workers: &'a Arc<worker_pool::WorkerPool>,
}

impl ExecutionContext<'_> {
    async fn finish(&self, failure: AdaptiveFailure) -> CommandOutcome {
        self.runtime
            .finish_failure(
                self.envelope,
                classify_failure(self.envelope, failure.error, failure.evidence),
            )
            .await
    }

    async fn retry(
        &self,
        lease: worker_pool::WorkerLease,
        page: types::PageState,
        evidence: Vec<Evidence>,
    ) -> ExecutionResult {
        self.runtime
            .adaptive
            .record_retry(observability::RetryClass::Transport);
        let mut execution = match self.attempt(&lease, Some(page)).await {
            Ok(execution) => execution,
            Err(failure) => return Err(self.finish(failure).await),
        };
        execution.evidence.extend(evidence);
        Ok((execution, Some(lease)))
    }
}

impl PageRuntime {
    pub(super) async fn run_with_deadline(
        &self,
        envelope: &CommandEnvelope,
        gate: &SessionGate,
        workers: &Arc<worker_pool::WorkerPool>,
        lease: worker_pool::WorkerLease,
        page_state: Option<types::PageState>,
    ) -> ExecutionResult {
        let context = ExecutionContext {
            runtime: self,
            envelope,
            gate,
            workers,
        };
        match context.attempt(&lease, page_state.clone()).await {
            Ok(execution) => Ok((execution, Some(lease))),
            Err(failure) => {
                let browser_died = worker_pool::is_dead_worker_error(&failure.error)
                    || (page_state.is_some()
                        && failure.error.message == "browser page is not open");
                match page_state.filter(|_| browser_died) {
                    Some(page) => context.recover(failure, lease, page).await,
                    None => Err(context.finish(failure).await),
                }
            }
        }
    }
}
