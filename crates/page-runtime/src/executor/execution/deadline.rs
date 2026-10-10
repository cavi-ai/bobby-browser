//! Every initial and replayed attempt uses the same envelope deadline.
use super::*;

impl ExecutionContext<'_> {
    pub(super) async fn attempt(
        &self,
        lease: &worker_pool::WorkerLease,
        page: Option<types::PageState>,
    ) -> Result<AdaptiveExecution, AdaptiveFailure> {
        let remaining = (self.envelope.deadline - Utc::now())
            .to_std()
            .unwrap_or(StdDuration::ZERO);
        tokio::time::timeout(
            remaining,
            self.runtime
                .adaptive
                .execute(self.envelope, lease, page, self.gate),
        )
        .await
        .unwrap_or_else(|_| {
            Err(AdaptiveFailure {
                error: CommandError {
                    code: ErrorCode::DeadlineExceeded,
                    message: "command did not finish before its envelope deadline".into(),
                    layer: ErrorLayer::Workflow,
                    retryable: true,
                },
                evidence: Vec::new(),
            })
        })
    }
}
