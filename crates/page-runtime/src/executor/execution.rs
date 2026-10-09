//! Deadline, reconnect, and revival ownership for a single execution.
use super::*;

impl PageRuntime {
    pub(super) async fn run_with_deadline(
        &self,
        envelope: &CommandEnvelope,
        gate: &SessionGate,
        workers: &Arc<worker_pool::WorkerPool>,
        lease: worker_pool::WorkerLease,
        page_state: Option<types::PageState>,
    ) -> Result<
        (
            crate::adaptive::AdaptiveExecution,
            Option<worker_pool::WorkerLease>,
        ),
        CommandOutcome,
    > {
        // The envelope deadline is a real bound, not an admission formality:
        // race the command against it so a hung browser call fails the
        // command (and only that command) instead of parking it forever.
        let remaining = (envelope.deadline - Utc::now())
            .to_std()
            .unwrap_or(StdDuration::ZERO);
        let mut lease_slot = Some(lease);
        let execution = match tokio::time::timeout(
            remaining,
            self.adaptive.execute(
                envelope,
                lease_slot.as_ref().expect("lease before first execution"),
                page_state.clone(),
                gate,
            ),
        )
        .await
        {
            Ok(Ok(execution)) => execution,
            Ok(Err(failure)) => {
                // A killed browser (SIGKILL under host pressure, reset CDP
                // or Firefox BiDi socket) otherwise wedges every later call
                // on the session at a permanent "browser page is not open".
                // Revive once: retire the dead worker, relaunch, reopen the
                // page at its last URL.
                // Replayable commands retry transparently; anything else fails
                // with the revival noted so the *next* call lands on a live
                // page. The runtime already validated the page id, so a
                // page_missing here means the worker lost it, not a bad call.
                let browser_died = worker_pool::is_dead_worker_error(&failure.error)
                    || (page_state.is_some()
                        && failure.error.message == "browser page is not open");
                let mut revived_execution = None;
                let mut reattached_execution = None;
                let mut probed_execution = None;
                // Carried past their own blocks (which end before the revive
                // branch below) so a browserRevived evidence item can name
                // what the probe and the reattach attempt each reported,
                // instead of the caller only learning that a revive happened.
                let mut probe_error_message: Option<String> = None;
                let mut reattach_error_message: Option<String> = None;
                if browser_died && page_state.is_some() {
                    let page = page_state.clone().expect("checked above");
                    // Probe the CURRENT lease before assuming the browser
                    // died: a closed-session-shaped CDP error can mean the
                    // browser process and the page are both still alive and
                    // only this command's own session/target was lost.
                    // `list_pages` runs `sync_untracked_pages`, exercising
                    // the transport and refreshing the page registry
                    // without tearing anything down, so a successful call
                    // here proves (or disproves) browser death outright,
                    // before any reattach or revive is attempted.
                    let probe = lease_slot
                        .as_ref()
                        .expect("lease before probe")
                        .worker()
                        .list_pages(&ListPagesCommand)
                        .await;
                    probe_error_message = probe.as_ref().err().map(|error| error.message.clone());
                    if let Ok(evidence) = probe {
                        if !page_still_listed(&evidence, &page.id) {
                            // The page itself closed; the browser never died
                            // and there is nothing to reattach or revive.
                            // Fail permanently with the original
                            // page-missing error.
                            return Err(self
                                .finish_failure(
                                    envelope,
                                    classify_failure(envelope, failure.error, failure.evidence),
                                )
                                .await);
                        }
                        if envelope.command.class() == types::CommandClass::Replayable {
                            self.adaptive
                                .record_retry(observability::RetryClass::Transport);
                            let remaining = (envelope.deadline - Utc::now())
                                .to_std()
                                .unwrap_or(StdDuration::ZERO);
                            let same_lease = lease_slot.as_ref().expect("lease before retry");
                            match tokio::time::timeout(
                                remaining,
                                self.adaptive
                                    .execute(envelope, same_lease, Some(page), gate),
                            )
                            .await
                            {
                                Ok(Ok(retry_execution)) => {
                                    probed_execution = Some(retry_execution);
                                }
                                Ok(Err(retry_failure)) => {
                                    return Err(self
                                        .finish_failure(
                                            envelope,
                                            classify_failure(
                                                envelope,
                                                retry_failure.error,
                                                retry_failure.evidence,
                                            ),
                                        )
                                        .await);
                                }
                                Err(_) => {
                                    return Err(self
                                        .finish_failure(
                                            envelope,
                                            classify_failure(
                                                envelope,
                                                CommandError {
                                                    code: ErrorCode::DeadlineExceeded,
                                                    message: "command did not finish before its envelope deadline".into(),
                                                    layer: ErrorLayer::Workflow,
                                                    retryable: true,
                                                },
                                                Vec::new(),
                                            ),
                                        )
                                        .await);
                                }
                            }
                        } else {
                            // Mutating command: recode as a retryable
                            // target-detached failure instead of a browser
                            // death -- the browser and page are intact, only
                            // this command's target/session was lost. The
                            // original diagnostic survives as evidence since
                            // the outer message no longer carries it, and as
                            // the TargetDetached code once the outer message
                            // is redacted for the durable journal.
                            return Err(self
                                .finish_failure(
                                    envelope,
                                    CommandOutcome::Failed {
                                        command_id: envelope.command_id.clone(),
                                        error: CommandError {
                                            code: ErrorCode::TargetDetached,
                                            message: "transient target loss; the browser and page are intact -- re-resolve the target and re-issue the command".into(),
                                            layer: ErrorLayer::Driver,
                                            retryable: true,
                                        },
                                        evidence: vec![transient_target_loss_evidence(
                                            &failure.error.message,
                                        )],
                                    },
                                )
                                .await);
                        }
                    }
                    // Probe failed outright: the transport really may be
                    // gone, so fall through to the existing reattach /
                    // revive handling below, unchanged.
                }
                if probed_execution.is_none() && browser_died && page_state.is_some() {
                    let page = page_state.clone().expect("checked above");
                    let lease = lease_slot.take().expect("lease before revive");
                    // Transport-only death first: if the browser process is
                    // still alive, reattach to it and keep every page — the
                    // relaunch path below destroys page state (typed values
                    // included) and reloads the URL.
                    let reattached = match lease.worker().reconnect_live_process().await {
                        Ok(_) => {
                            tracing::info!(
                                session_id = %envelope.session_id.0,
                                command_id = %envelope.command_id.0,
                                "transport reset reattached to the live browser"
                            );
                            Some(lease)
                        }
                        Err(error) => {
                            // Process really gone (or reconnect unsupported):
                            // fall through to the relaunch revive path with
                            // the lease returned for its existing take.
                            reattach_error_message = Some(error.message);
                            lease_slot = Some(lease);
                            None
                        }
                    };
                    if let Some(reattached_lease) = reattached {
                        // The transport came back, but that only proves the
                        // browser process is alive, not that this specific
                        // page still is. A popup the site closed between
                        // validation and dispatch reattaches trivially (the
                        // connection was never the problem) and would only
                        // fail again on the same missing target, so check
                        // the worker's own page listing before treating this
                        // as a transport story at all.
                        let page_open = match reattached_lease
                            .worker()
                            .list_pages(&ListPagesCommand)
                            .await
                        {
                            Ok(evidence) => page_still_listed(&evidence, &page.id),
                            // Listing is unsupported or failed for an
                            // unrelated reason: no evidence the page is
                            // gone, so fall through to the existing
                            // transport-reset handling instead of failing a
                            // command that cannot be proven to have lost its
                            // target.
                            Err(_) => true,
                        };
                        if !page_open {
                            // The page itself closed; the browser did not
                            // die and there is nothing to retry or reattach
                            // to. Fail permanently with the original
                            // page-missing error -- no reattach suffix, no
                            // evidence of a transport story that did not
                            // happen -- for Replayable and mutating commands
                            // alike. `reattached_lease` simply drops here
                            // (same as the mutating, page-present branch
                            // below): no invalidation, no relaunch, so the
                            // session's next command still lands on a live
                            // worker.
                            return Err(self
                                .finish_failure(
                                    envelope,
                                    classify_failure(envelope, failure.error, failure.evidence),
                                )
                                .await);
                        }
                        if envelope.command.class() == types::CommandClass::Replayable {
                            self.adaptive
                                .record_retry(observability::RetryClass::Transport);
                            let remaining = (envelope.deadline - Utc::now())
                                .to_std()
                                .unwrap_or(StdDuration::ZERO);
                            match tokio::time::timeout(
                                remaining,
                                self.adaptive.execute(
                                    envelope,
                                    &reattached_lease,
                                    Some(page),
                                    gate,
                                ),
                            )
                            .await
                            {
                                Ok(Ok(retry_execution)) => {
                                    let mut retry_execution = retry_execution;
                                    retry_execution.evidence.push(reattach_evidence());
                                    reattached_execution = Some(retry_execution);
                                    lease_slot = Some(reattached_lease);
                                }
                                Ok(Err(retry_failure)) => {
                                    return Err(self
                                        .finish_failure(
                                            envelope,
                                            classify_failure(
                                                envelope,
                                                retry_failure.error,
                                                retry_failure.evidence,
                                            ),
                                        )
                                        .await);
                                }
                                Err(_) => {
                                    return Err(self
                                        .finish_failure(
                                            envelope,
                                            classify_failure(
                                                envelope,
                                                CommandError {
                                                    code: ErrorCode::DeadlineExceeded,
                                                    message: "command did not finish before its envelope deadline".into(),
                                                    layer: ErrorLayer::Workflow,
                                                    retryable: true,
                                                },
                                                Vec::new(),
                                            ),
                                        )
                                        .await);
                                }
                            }
                        } else {
                            // Mutating command: the effect may or may not have
                            // landed, so the caller decides what to do — but the
                            // failure explains the reattach (page state survived,
                            // connection restored) instead of implying the page
                            // was lost. The next command lands on the same,
                            // still-live page.
                            let mut error = failure.error.clone();
                            error.message = format!(
                                "{} (CDP transport reset; reattached to the live browser — page state preserved, inspect before re-issuing)",
                                error.message
                            );
                            error.retryable = true;
                            return Err(self
                                .finish_failure(
                                    envelope,
                                    classify_failure(envelope, error, {
                                        let mut evidence = failure.evidence.clone();
                                        evidence.push(reattach_evidence());
                                        evidence
                                    }),
                                )
                                .await);
                        }
                    }
                }
                if probed_execution.is_none()
                    && reattached_execution.is_none()
                    && browser_died
                    && page_state.is_some()
                {
                    let page = page_state.clone().expect("checked above");
                    let lease = lease_slot.take().expect("lease before revive");
                    let failed_worker_id = lease.worker_id();
                    drop(lease);
                    let _ = workers
                        .invalidate_session_if_worker(&envelope.session_id, &failed_worker_id)
                        .await;
                    if let Ok(revived) = workers.lease(envelope.session_id.clone()).await {
                        let _ = revived
                            .worker()
                            .set_fingerprint_enabled(gate.fingerprint)
                            .await;
                        let _ = revived
                            .worker()
                            .set_humanization_enabled(gate.humanize)
                            .await;
                        if revived.worker().open_page(page.id.clone()).await.is_ok() {
                            if let Some(url) = &page.url {
                                let _ = revived
                                    .worker()
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
                            if envelope.command.class() == types::CommandClass::Replayable {
                                self.adaptive
                                    .record_retry(observability::RetryClass::Transport);
                                let remaining = (envelope.deadline - Utc::now())
                                    .to_std()
                                    .unwrap_or(StdDuration::ZERO);
                                match tokio::time::timeout(
                                    remaining,
                                    self.adaptive.execute(envelope, &revived, Some(page), gate),
                                )
                                .await
                                {
                                    Ok(Ok(retry_execution)) => {
                                        revived_execution = Some(retry_execution);
                                        lease_slot = Some(revived);
                                    }
                                    Ok(Err(retry_failure)) => {
                                        return Err(self
                                            .finish_failure(
                                                envelope,
                                                classify_failure(
                                                    envelope,
                                                    retry_failure.error,
                                                    retry_failure.evidence,
                                                ),
                                            )
                                            .await);
                                    }
                                    Err(_) => {
                                        return Err(self
                                            .finish_failure(
                                                envelope,
                                                classify_failure(
                                                    envelope,
                                                    CommandError {
                                                        code: ErrorCode::DeadlineExceeded,
                                                        message: "command did not finish before its envelope deadline".into(),
                                                        layer: ErrorLayer::Workflow,
                                                        retryable: true,
                                                    },
                                                    Vec::new(),
                                                ),
                                            )
                                        .await);
                                    }
                                }
                            } else {
                                let mut evidence = failure.evidence;
                                evidence.push(browser_revived_evidence(
                                    probe_error_message.as_deref(),
                                    reattach_error_message.as_deref().unwrap_or("not attempted"),
                                    &failure.error.message,
                                ));
                                return Err(self
                                    .finish_failure(
                                        envelope,
                                        classify_failure(
                                            envelope,
                                            CommandError {
                                                code: ErrorCode::TargetDetached,
                                                message: "the browser transport was lost and could not be reattached; a fresh browser was launched and the page reloaded to its last URL -- inspect current state and re-issue the command".into(),
                                                layer: ErrorLayer::Driver,
                                                retryable: false,
                                            },
                                            evidence,
                                        ),
                                    )
                                .await);
                            }
                        }
                    }
                }
                match probed_execution
                    .or(reattached_execution)
                    .or(revived_execution)
                {
                    Some(revived_execution) => revived_execution,
                    None => {
                        return Err(self
                            .finish_failure(
                                envelope,
                                classify_failure(envelope, failure.error, failure.evidence),
                            )
                            .await);
                    }
                }
            }
            Err(_) => {
                return Err(self
                    .finish_failure(
                        envelope,
                        classify_failure(
                            envelope,
                            CommandError {
                                code: ErrorCode::DeadlineExceeded,
                                message: "command did not finish before its envelope deadline"
                                    .into(),
                                layer: ErrorLayer::Workflow,
                                retryable: true,
                            },
                            Vec::new(),
                        ),
                    )
                    .await);
            }
        };
        Ok((execution, lease_slot))
    }
}
