//! Retire a dead worker once, restoring session flags and the last page URL.
use super::*;

impl ExecutionContext<'_> {
    pub(super) async fn revive(
        &self,
        failure: AdaptiveFailure,
        lease: worker_pool::WorkerLease,
        page: types::PageState,
        probe_error: Option<String>,
        reattach_error: String,
    ) -> ExecutionResult {
        let failed_worker_id = lease.worker_id();
        drop(lease);
        let _ = self
            .workers
            .invalidate_session_if_worker(&self.envelope.session_id, &failed_worker_id)
            .await;
        let revived = match self.workers.lease(self.envelope.session_id.clone()).await {
            Ok(revived) => revived,
            Err(_) => return Err(self.finish(failure).await),
        };
        let _ = worker_pool::session_settings_or_default(revived.worker().session_settings())
            .set_fingerprint_enabled(self.gate.fingerprint)
            .await;
        let _ = worker_pool::session_settings_or_default(revived.worker().session_settings())
            .set_humanization_enabled(self.gate.humanize)
            .await;
        if worker_pool::tabs_or_default(revived.worker().tabs())
            .open_page(page.id.clone())
            .await
            .is_err()
        {
            return Err(self.finish(failure).await);
        }
        if let Some(url) = &page.url {
            let _ = worker_pool::navigation_or_default(revived.worker().navigation())
                .navigate(
                    &page.id,
                    &types::NavigateCommand {
                        url: url.clone(),
                        wait_until: types::WaitUntil::Interactive,
                        timeout_ms: 15_000,
                    },
                )
                .await;
        }
        if self.envelope.command.class() == types::CommandClass::Replayable {
            return self.retry(revived, page, Vec::new()).await;
        }
        let mut evidence = failure.evidence;
        evidence.push(browser_revived_evidence(
            probe_error.as_deref(),
            &reattach_error,
            &failure.error.message,
        ));
        Err(self.finish(AdaptiveFailure {
            error: CommandError {
                code: ErrorCode::TargetDetached,
                message: "the browser transport was lost and could not be reattached; a fresh browser was launched and the page reloaded to its last URL -- inspect current state and re-issue the command".into(),
                layer: ErrorLayer::Driver,
                retryable: false,
            },
            evidence,
        }).await)
    }
}
