use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::Utc;
use thiserror::Error;
use types::{
    CommandClass, CommandEnvelope, CommandError, CommandId, CommandOutcome, CommandPhase,
    ErrorCode, ErrorLayer, Evidence, InspectCommand, PrimitiveCommand, RuntimeCommand, TextMatch,
    WaitCondition, WaitForCommand,
};
use workflow_journal::{JournalError, JournalRecord, PreparedResult};

use crate::{PageRuntime, SessionGate, VisionGate};

#[derive(Debug, Error)]
pub enum ExecutorError {
    #[error("journal failed: {0}")]
    Journal(#[from] JournalError),
}

impl PageRuntime {
    pub async fn recover_command(&self, command_id: CommandId) -> CommandOutcome {
        let Some(journal) = &self.journal else {
            return CommandOutcome::Failed {
                command_id,
                error: internal_error("command journal is not configured"),
                evidence: Vec::new(),
            };
        };
        let scan = match journal.history(command_id.clone()).await {
            Ok(scan) => scan,
            Err(error) => {
                return CommandOutcome::RetryableFailure {
                    command_id,
                    error: journal_error(error),
                }
            }
        };
        if let Some(outcome) = scan
            .records
            .iter()
            .rev()
            .find_map(|record| record.outcome.clone())
        {
            if let Some(download) = scan
                .records
                .iter()
                .rev()
                .find_map(|record| record.prepared_result.as_ref())
                .and_then(|prepared| prepared.download.as_ref())
            {
                if let Err(error) = self.adaptive.cleanup_prepared_download(download) {
                    tracing::warn!(
                        error = ?error,
                        "terminal download staging recovery cleanup failed"
                    );
                }
            }
            return outcome;
        }
        if let Some(prepared) = scan
            .records
            .iter()
            .rev()
            .find_map(|record| record.prepared_result.clone())
        {
            if let Some(download) = prepared.download.as_ref() {
                let Some(sha256) = prepared.artifact_sha256.as_deref() else {
                    return CommandOutcome::NeedsReconciliation {
                        command_id,
                        error: internal_error("prepared download is missing its digest"),
                        evidence: prepared.evidence,
                    };
                };
                let Some(bytes) = prepared.artifact_bytes else {
                    return CommandOutcome::NeedsReconciliation {
                        command_id,
                        error: internal_error("prepared download is missing its byte count"),
                        evidence: prepared.evidence,
                    };
                };
                if let Err(error) = self
                    .adaptive
                    .finalize_prepared_download(download, sha256, bytes)
                {
                    return CommandOutcome::NeedsReconciliation {
                        command_id,
                        error,
                        evidence: prepared.evidence,
                    };
                }
            }
            if let (Some(artifact_id), Some(staging_id), Some(sha256), Some(bytes)) = (
                prepared.artifact_id.as_deref(),
                prepared.artifact_staging_id.as_deref(),
                prepared.artifact_sha256.as_deref(),
                prepared.artifact_bytes,
            ) {
                let session_id = scan
                    .records
                    .iter()
                    .find_map(|record| record.envelope.as_ref())
                    .map(|envelope| envelope.session_id.clone());
                if let Some(session_id) = session_id {
                    if let Err(error) = self.adaptive.finalize_prepared_artifact(
                        &session_id,
                        artifact_id,
                        staging_id,
                        sha256,
                        bytes,
                    ) {
                        return CommandOutcome::NeedsReconciliation {
                            command_id,
                            error,
                            evidence: prepared.evidence,
                        };
                    }
                }
            }
            return CommandOutcome::NeedsReconciliation {
                command_id,
                error: CommandError {
                    code: ErrorCode::HttpEquivalenceUnproven,
                    message: "durable prepared result requires deterministic finalization".into(),
                    layer: ErrorLayer::Workflow,
                    retryable: false,
                },
                evidence: prepared.evidence,
            };
        }
        let envelope = scan
            .records
            .iter()
            .find_map(|record| record.envelope.as_ref());
        let latest_phase = scan.records.last().map(|record| record.phase);
        if envelope.is_some_and(|envelope| envelope.command.class() != CommandClass::Replayable)
            && matches!(
                latest_phase,
                Some(CommandPhase::Executing | CommandPhase::Verifying)
            )
        {
            return CommandOutcome::NeedsReconciliation {
                command_id,
                error: CommandError {
                    code: ErrorCode::Internal,
                    message: "durable non-replayable command may have reached the browser".into(),
                    layer: ErrorLayer::Workflow,
                    retryable: false,
                },
                evidence: Vec::new(),
            };
        }
        CommandOutcome::RetryableFailure {
            command_id,
            error: internal_error("no durable prepared result exists"),
        }
    }

    pub async fn execute(&self, envelope: CommandEnvelope) -> CommandOutcome {
        self.execute_with_session_gate(envelope, SessionGate::default())
            .await
    }

    pub async fn execute_with_vision_gate(
        &self,
        envelope: CommandEnvelope,
        vision_gate: VisionGate,
    ) -> CommandOutcome {
        self.execute_with_session_gate(envelope, vision_gate.into())
            .await
    }

    pub async fn execute_with_session_gate(
        &self,
        envelope: CommandEnvelope,
        gate: SessionGate,
    ) -> CommandOutcome {
        let command_id = envelope.command_id.clone();
        if let Err(error) = self.validate(&envelope).await {
            return CommandOutcome::Failed {
                command_id,
                error,
                evidence: Vec::new(),
            };
        }
        let Some(journal) = &self.journal else {
            return CommandOutcome::Failed {
                command_id,
                error: internal_error("command journal is not configured"),
                evidence: Vec::new(),
            };
        };
        let Some(workers) = &self.workers else {
            return CommandOutcome::Failed {
                command_id,
                error: internal_error("browser workers are not configured"),
                evidence: Vec::new(),
            };
        };

        if let Err(error) = journal
            .append(record(
                &envelope,
                CommandPhase::Accepted,
                Some(envelope.journal_safe()),
                None,
            ))
            .await
        {
            return journal_failure(&envelope, error, false);
        }
        self.observe_durable_phase(CommandPhase::Accepted).await;
        if let Err(error) = journal
            .append(record(&envelope, CommandPhase::Prepared, None, None))
            .await
        {
            return journal_failure(&envelope, error, false);
        }
        self.observe_durable_phase(CommandPhase::Prepared).await;
        if let Err(error) = journal
            .append(record(&envelope, CommandPhase::Executing, None, None))
            .await
        {
            return journal_failure(&envelope, error, false);
        }
        self.observe_durable_phase(CommandPhase::Executing).await;

        let lease = match workers.lease(envelope.session_id.clone()).await {
            Ok(lease) => lease,
            Err(error) => {
                return self
                    .finish_failure(&envelope, classify_failure(&envelope, error, Vec::new()))
                    .await;
            }
        };
        // Apply the session's policy to the worker before it runs anything.
        // Workers are pooled and re-leased across sessions, so both flags must
        // be written on every lease or one session's opt-in leaks into the next.
        if let Err(error) = lease
            .worker()
            .set_fingerprint_enabled(gate.fingerprint)
            .await
        {
            return self
                .finish_failure(&envelope, classify_failure(&envelope, error, Vec::new()))
                .await;
        }
        if let Err(error) = lease.worker().set_humanization_enabled(gate.humanize).await {
            return self
                .finish_failure(&envelope, classify_failure(&envelope, error, Vec::new()))
                .await;
        }
        let page_state = match envelope.page_id.as_ref() {
            Some(page_id) => match self.get(page_id).await {
                Ok(page) => Some(page),
                Err(_) => {
                    return self
                        .finish_failure(
                            &envelope,
                            classify_failure(
                                &envelope,
                                internal_error("page disappeared before dispatch"),
                                Vec::new(),
                            ),
                        )
                        .await;
                }
            },
            None => None,
        };
        // The envelope deadline is a real bound, not an admission formality:
        // race the command against it so a hung browser call fails the
        // command (and only that command) instead of parking it forever.
        let remaining = (envelope.deadline - Utc::now())
            .to_std()
            .unwrap_or(StdDuration::ZERO);
        let mut lease_slot = Some(lease);
        let mut execution = match tokio::time::timeout(
            remaining,
            self.adaptive.execute(
                &envelope,
                lease_slot.as_ref().expect("lease before first execution"),
                page_state.clone(),
                &gate,
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
                if browser_died && page_state.is_some() {
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
                        Err(_) => {
                            // Process really gone (or reconnect unsupported):
                            // fall through to the relaunch revive path with
                            // the lease returned for its existing take.
                            lease_slot = Some(lease);
                            None
                        }
                    };
                    if let Some(reattached_lease) = reattached {
                        if envelope.command.class() == types::CommandClass::Replayable {
                            self.adaptive
                                .record_retry(observability::RetryClass::Transport);
                            let remaining = (envelope.deadline - Utc::now())
                                .to_std()
                                .unwrap_or(StdDuration::ZERO);
                            match tokio::time::timeout(
                                remaining,
                                self.adaptive.execute(
                                    &envelope,
                                    &reattached_lease,
                                    Some(page),
                                    &gate,
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
                                    return self
                                        .finish_failure(
                                            &envelope,
                                            classify_failure(
                                                &envelope,
                                                retry_failure.error,
                                                retry_failure.evidence,
                                            ),
                                        )
                                        .await;
                                }
                                Err(_) => {
                                    return self
                                        .finish_failure(
                                            &envelope,
                                            classify_failure(
                                                &envelope,
                                                CommandError {
                                                    code: ErrorCode::DeadlineExceeded,
                                                    message: "command did not finish before its envelope deadline".into(),
                                                    layer: ErrorLayer::Workflow,
                                                    retryable: true,
                                                },
                                                Vec::new(),
                                            ),
                                        )
                                        .await;
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
                            return self
                                .finish_failure(
                                    &envelope,
                                    classify_failure(&envelope, error, {
                                        let mut evidence = failure.evidence.clone();
                                        evidence.push(reattach_evidence());
                                        evidence
                                    }),
                                )
                                .await;
                        }
                    }
                }
                if reattached_execution.is_none() && browser_died && page_state.is_some() {
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
                                    self.adaptive
                                        .execute(&envelope, &revived, Some(page), &gate),
                                )
                                .await
                                {
                                    Ok(Ok(retry_execution)) => {
                                        revived_execution = Some(retry_execution);
                                        lease_slot = Some(revived);
                                    }
                                    Ok(Err(retry_failure)) => {
                                        return self
                                            .finish_failure(
                                                &envelope,
                                                classify_failure(
                                                    &envelope,
                                                    retry_failure.error,
                                                    retry_failure.evidence,
                                                ),
                                            )
                                            .await;
                                    }
                                    Err(_) => {
                                        return self
                                            .finish_failure(
                                                &envelope,
                                                classify_failure(
                                                    &envelope,
                                                    CommandError {
                                                        code: ErrorCode::DeadlineExceeded,
                                                        message: "command did not finish before its envelope deadline".into(),
                                                        layer: ErrorLayer::Workflow,
                                                        retryable: true,
                                                    },
                                                    Vec::new(),
                                                ),
                                            )
                                        .await;
                                    }
                                }
                            } else {
                                return self
                                    .finish_failure(
                                        &envelope,
                                        classify_failure(
                                            &envelope,
                                            CommandError {
                                                code: ErrorCode::TargetDetached,
                                                message: "the browser process was killed; a fresh browser was launched and the page reloaded to its last URL -- inspect current state and re-issue the command".into(),
                                                layer: ErrorLayer::Driver,
                                                retryable: false,
                                            },
                                            failure.evidence,
                                        ),
                                    )
                                .await;
                            }
                        }
                    }
                }
                match reattached_execution.or(revived_execution) {
                    Some(revived_execution) => revived_execution,
                    None => {
                        return self
                            .finish_failure(
                                &envelope,
                                classify_failure(&envelope, failure.error, failure.evidence),
                            )
                            .await;
                    }
                }
            }
            Err(_) => {
                return self
                    .finish_failure(
                        &envelope,
                        classify_failure(
                            &envelope,
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
                    .await;
            }
        };
        let mut committed_download = None;
        if let Some(mut prepared) = execution.prepared_http.take() {
            let artifact = prepared
                .artifact
                .as_ref()
                .map(|pending| pending.record().clone());
            let prepared_result = PreparedResult {
                command_id: envelope.command_id.clone(),
                attempt_id: envelope.attempt_id.clone(),
                state_version: prepared.state_version,
                state_delta: serde_json::Value::Null,
                // The durable record keeps the raw evidence minus the save_as
                // landing path: the journal must never carry it, and recovery
                // republishes from the download record, not the evidence.
                evidence: execution
                    .evidence
                    .iter()
                    .cloned()
                    .map(|mut item| {
                        if let Evidence::Download { saved_to, .. } = &mut item {
                            *saved_to = None;
                        }
                        item
                    })
                    .collect(),
                artifact_id: artifact.as_ref().map(|record| record.artifact_id.clone()),
                artifact_sha256: artifact.as_ref().map(|record| record.sha256.clone()),
                artifact_bytes: artifact.as_ref().map(|record| record.bytes),
                artifact_staging_id: prepared
                    .artifact
                    .as_ref()
                    .and_then(|pending| pending.staging_id().map(str::to_owned)),
                download: prepared
                    .download
                    .as_ref()
                    .map(|pending| pending.record().clone()),
            };
            let journal = Arc::clone(journal);
            let prepared_record = prepared_record(&envelope, prepared_result);
            let pending = prepared.artifact.take();
            let pending_download = prepared.download.take();
            let observer = self.phase_observer.clone();
            let durable_prepare = tokio::spawn(async move {
                if let Err(error) = journal.append(prepared_record).await {
                    if let Some(pending) = pending_download {
                        pending.discard();
                    }
                    return Err(journal_error(error));
                }
                if let Some(observer) = observer {
                    observer
                        .durable_phase_reached(CommandPhase::ResultPrepared)
                        .await;
                }
                if let Some(pending) = pending {
                    pending.commit().map_err(|error| {
                        internal_error(format!("prepared artifact publication failed: {error}"))
                    })?;
                }
                if let Some(pending) = pending_download {
                    return pending.commit().map(Some);
                }
                Ok(None)
            });
            match durable_prepare.await {
                Ok(Ok(download)) => committed_download = download,
                Ok(Err(error)) => {
                    return prepared_failure(&envelope, error, execution.evidence);
                }
                Err(error) => {
                    return prepared_failure(
                        &envelope,
                        internal_error(format!("prepared result task failed: {error}")),
                        execution.evidence,
                    );
                }
            }
            if let Err(error) = lease_slot
                .as_ref()
                .expect("lease survives to state commit")
                .worker()
                .commit_http_state(
                    envelope.page_id.as_ref().expect("validated page id"),
                    prepared.state_version,
                    prepared.state,
                )
                .await
            {
                let (outcome, terminal_durable) = self
                    .finish_failure_durable(
                        &envelope,
                        prepared_failure(&envelope, error, execution.evidence.clone()),
                    )
                    .await;
                if terminal_durable {
                    cleanup_committed_download(&mut committed_download);
                }
                return outcome;
            }
        }
        let evidence = execution.evidence;
        match &envelope.command {
            RuntimeCommand::Primitive(PrimitiveCommand::OpenPage(_)) => {
                if let Some(Evidence::Page { page_id, url, .. }) = evidence.first() {
                    self.register_page_id(
                        envelope.session_id.clone(),
                        page_id.clone(),
                        url.clone(),
                    )
                    .await;
                }
            }
            RuntimeCommand::Primitive(PrimitiveCommand::ClickAndWaitForPopup(_)) => {
                if let Some(Evidence::Popup { page_id, url, .. }) = evidence.first() {
                    self.register_page_id(
                        envelope.session_id.clone(),
                        page_id.clone(),
                        url.clone(),
                    )
                    .await;
                }
            }
            RuntimeCommand::Primitive(PrimitiveCommand::ClosePage(command)) => {
                self.context().forget(&command.page_id);
                self.remove_page(&command.page_id).await
            }
            RuntimeCommand::Primitive(_) | RuntimeCommand::Intent(_) => {}
        }

        // Ordering matters: invalidate first, record second. A replayable
        // snapshot does not invalidate, and its result is what the graph should
        // hold; any other command may have changed the page. `finish_failure`
        // invalidates for the same reason.
        if let Some(page_id) = envelope.page_id.as_ref() {
            // Recorded before invalidation: which command produced evidence
            // does not go stale when the page changes, so it outlives the
            // generation bump below.
            if !evidence.is_empty() {
                self.context().record_command(
                    page_id,
                    envelope.command_id.clone(),
                    crate::context::command_kind_name(&envelope.command),
                );
            }
            self.context().invalidate_for(page_id, &envelope.command);
            for item in &evidence {
                if let Evidence::AccessibilitySnapshot {
                    page_id: observed,
                    nodes,
                    truncated,
                } = item
                {
                    // A truncated snapshot is not the page: recording it would
                    // let the graph answer "not found" for a control that
                    // exists past the truncation point.
                    if !*truncated {
                        self.context().record(observed, nodes.clone());
                    }
                }
            }
        }

        if let Err(error) = journal
            .append(record(&envelope, CommandPhase::Verifying, None, None))
            .await
        {
            return journal_failure(&envelope, error, true);
        }
        self.observe_durable_phase(CommandPhase::Verifying).await;
        match self
            .verify(
                &envelope,
                lease_slot.as_ref().expect("lease survives to verify"),
                evidence,
            )
            .await
        {
            Ok(evidence) => {
                self.promote_outcome(&envelope, &evidence, true).await;
                if let RuntimeCommand::Primitive(PrimitiveCommand::Navigate(_)) = &envelope.command
                {
                    if let Some(Evidence::Navigation { url, .. }) = evidence.first() {
                        // The navigation itself succeeded, so the outcome is
                        // still completed -- but a registry write failure
                        // leaves page_list showing a stale URL, and that must
                        // be visible in logs rather than swallowed.
                        if let Err(error) = self
                            .set_url(
                                envelope.page_id.as_ref().expect("validated page id"),
                                url.clone(),
                                "interactive",
                            )
                            .await
                        {
                            tracing::warn!(
                                error = %error,
                                "page registry URL update failed after navigate"
                            );
                        }
                    }
                }
                let outcome = CommandOutcome::Completed {
                    command_id: command_id.clone(),
                    evidence,
                };
                match journal
                    .append(record(
                        &envelope,
                        CommandPhase::Completed,
                        None,
                        Some(outcome.journal_safe()),
                    ))
                    .await
                {
                    Ok(()) => {
                        cleanup_committed_download(&mut committed_download);
                        outcome
                    }
                    Err(error) => journal_failure(&envelope, error, true),
                }
            }
            Err(error) => {
                let (outcome, terminal_durable) = self
                    .finish_failure_durable(
                        &envelope,
                        classify_failure(&envelope, error, Vec::new()),
                    )
                    .await;
                if terminal_durable {
                    cleanup_committed_download(&mut committed_download);
                }
                outcome
            }
        }
    }

    async fn validate(&self, envelope: &CommandEnvelope) -> Result<(), CommandError> {
        if envelope.schema_version != CommandEnvelope::SCHEMA_VERSION {
            return Err(validation_error("unsupported command schema version"));
        }
        if envelope.deadline <= Utc::now() {
            return Err(CommandError {
                code: ErrorCode::DeadlineExceeded,
                message: "command deadline has elapsed".into(),
                layer: ErrorLayer::Workflow,
                retryable: false,
            });
        }
        if matches!(
            envelope.command,
            RuntimeCommand::Primitive(PrimitiveCommand::ListPages(_))
        ) {
            return Ok(());
        }
        let page_id = envelope
            .page_id
            .as_ref()
            .ok_or_else(|| validation_error("pageId is required for page commands"))?;
        let page = self.get(page_id).await.map_err(|_| missing_page_error())?;
        if page.session_id != envelope.session_id {
            return Err(validation_error("page does not belong to session"));
        }
        if let RuntimeCommand::Primitive(PrimitiveCommand::Navigate(command)) = &envelope.command {
            if !(command.url.starts_with("http://")
                || command.url.starts_with("https://")
                || command.url.starts_with("data:"))
            {
                return Err(validation_error("navigation URL scheme is not supported"));
            }
        }
        if let RuntimeCommand::Primitive(PrimitiveCommand::Click(command)) = &envelope.command {
            let has_duplicate = command
                .modifiers
                .iter()
                .enumerate()
                .any(|(index, modifier)| command.modifiers[..index].contains(modifier));
            if has_duplicate {
                return Err(validation_error("click modifiers must be unique"));
            }
        }
        // Boundary primitives (Click { boundary: true }, …) and Boundary intents
        // (SubmitAndVerify, or Follow when the caller sets boundary: true, via
        // IntentCommand::class) share this pre-act checkpoint gate.
        if envelope.command.class() == CommandClass::Boundary {
            if let Some(checkpoints) = &self.checkpoints {
                let checkpoint = checkpoints.load(&envelope.workflow_id).await.map_err(|_| {
                    validation_error("a verified pre-action checkpoint is required")
                })?;
                if checkpoint.attempt_id != envelope.attempt_id
                    || checkpoint.session_id != envelope.session_id
                    || checkpoint.page_id != *page_id
                    || checkpoint.recovery_class != CommandClass::Boundary
                    || checkpoint.boundary_command_id.as_ref() != Some(&envelope.command_id)
                {
                    return Err(validation_error(
                        "boundary checkpoint does not match the command context",
                    ));
                }
            }
        }
        Ok(())
    }

    async fn verify(
        &self,
        envelope: &CommandEnvelope,
        lease: &worker_pool::WorkerLease,
        evidence: Vec<Evidence>,
    ) -> Result<Vec<Evidence>, CommandError> {
        let page_id = envelope.page_id.as_ref();
        let RuntimeCommand::Primitive(command) = &envelope.command else {
            // IntentEngine owns verify for intent commands.
            if evidence
                .iter()
                .any(|item| matches!(item, Evidence::IntentExecution { .. }))
            {
                return Ok(evidence);
            }
            return Err(verification_error(
                "intent command returned no execution record",
            ));
        };
        match command {
            PrimitiveCommand::Navigate(_) => match evidence.first() {
                Some(Evidence::Navigation { url, .. }) if !url.is_empty() => Ok(evidence),
                _ => Err(verification_error("navigation returned no final URL")),
            },
            PrimitiveCommand::Inspect(_) => {
                if evidence.is_empty() {
                    Err(verification_error("inspection returned no evidence"))
                } else {
                    Ok(evidence)
                }
            }
            PrimitiveCommand::TypeText(command) => {
                let verification = lease
                    .worker()
                    .inspect(
                        page_id.expect("validated page id"),
                        &InspectCommand {
                            selector: (!command.selector.is_empty())
                                .then(|| command.selector.clone()),
                            target: command.target.clone(),
                            include_html: false,
                        },
                    )
                    .await?;
                let inspected = verification.iter().find_map(|item| match item {
                    Evidence::Inspection { text, .. } => Some(text.as_str()),
                    _ => None,
                });
                let observed = evidence.iter().find_map(|item| match item {
                    Evidence::Element { text, .. } => text.as_deref(),
                    _ => None,
                });
                let kind = evidence
                    .iter()
                    .find_map(|item| match item {
                        Evidence::Configuration { name, value } if name == "typedControlKind" => {
                            Some(value.as_str())
                        }
                        _ => None,
                    })
                    .unwrap_or("text");
                let matches = inspected.is_some_and(|inspected| {
                    typed_value_verified(
                        &command.value,
                        command.clear_first,
                        inspected,
                        observed,
                        kind,
                    )
                });
                if matches {
                    let mut combined = evidence;
                    combined.extend(verification);
                    Ok(combined)
                } else {
                    Err(verification_error("typed value did not match page state"))
                }
            }
            PrimitiveCommand::ControlAction(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::ControlAction { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "control action returned no typed post-action evidence",
                    ))
                }
            }
            PrimitiveCommand::Click(command) => {
                if let Some(expected_url) = &command.expected_url {
                    let page_id = page_id.expect("validated page id");
                    // Settle wait: its failure is tolerated because the
                    // inspect below is the real verification, but it must be
                    // logged -- a silently skipped wait reads as a fast,
                    // flaky expected-URL failure.
                    if let Err(error) = lease
                        .worker()
                        .wait_for(
                            page_id,
                            &WaitForCommand {
                                condition: WaitCondition::Url {
                                    matcher: TextMatch::Exact(expected_url.clone()),
                                },
                                timeout_ms: 5_000,
                            },
                        )
                        .await
                    {
                        tracing::warn!(
                            error = %error.message,
                            "expected-URL settle wait failed before click verification"
                        );
                    }
                    let verification = lease
                        .worker()
                        .inspect(page_id, &InspectCommand::default())
                        .await?;
                    let matches = verification.iter().any(|item| {
                        matches!(item, Evidence::Inspection { url, .. } if url == expected_url)
                    });
                    if !matches {
                        return Err(verification_error("click did not reach expected URL"));
                    }
                    let mut combined = evidence;
                    combined.extend(verification);
                    Ok(combined)
                } else if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Element { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("click returned no target evidence"))
                }
            }
            PrimitiveCommand::UploadFiles(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Upload { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("upload returned no file evidence"))
                }
            }
            PrimitiveCommand::AccessibilitySnapshot(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::AccessibilitySnapshot { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "accessibility snapshot returned no snapshot evidence",
                    ))
                }
            }
            PrimitiveCommand::NetworkLog(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::HarArtifact { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("network log returned no HAR artifact"))
                }
            }
            PrimitiveCommand::Emulate(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Emulation { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("emulate command returned no emulation evidence"))
                }
            }
            PrimitiveCommand::HandleDialog(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Dialog { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("dialog command returned no dialog evidence"))
                }
            }
            PrimitiveCommand::PrintToPdf(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::PdfArtifact { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("PDF command returned no PDF artifact"))
                }
            }
            PrimitiveCommand::GetCookies(_)
            | PrimitiveCommand::SetCookies(_)
            | PrimitiveCommand::DeleteCookies(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::CookieState { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("cookie command returned no cookie state"))
                }
            }
            PrimitiveCommand::ExtractStructured(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::StructuredExtraction { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "structured extraction returned no extraction evidence",
                    ))
                }
            }
            PrimitiveCommand::ActivatePage(_) => {
                if evidence.iter().any(|item| {
                    matches!(
                        item,
                        Evidence::Page { .. } | Evidence::BrowserExecution { .. }
                    )
                }) {
                    Ok(evidence)
                } else {
                    Err(verification_error("page activation returned no page evidence"))
                }
            }
            PrimitiveCommand::OpenPage(_) | PrimitiveCommand::ClosePage(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Page { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("page command returned no page evidence"))
                }
            }
            PrimitiveCommand::ListPages(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Pages { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("page listing returned no evidence"))
                }
            }
            PrimitiveCommand::ClickAndWaitForPopup(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Popup { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "popup command returned no popup evidence",
                    ))
                }
            }
            PrimitiveCommand::ClickAndWaitForDownload(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Download { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "download command returned no download evidence",
                    ))
                }
            }
            PrimitiveCommand::DownloadUrl(_) => {
                let download = evidence.iter().find_map(|item| match item {
                    Evidence::Download { bytes, sha256, .. } => Some((*bytes, sha256)),
                    _ => None,
                });
                let execution = evidence.iter().find_map(|item| match item {
                    Evidence::ExecutionPath { bytes, sha256, .. } => Some((*bytes, sha256)),
                    _ => None,
                });
                match (download, execution) {
                    (
                        Some((download_bytes, download_sha)),
                        Some((Some(exec_bytes), Some(exec_sha))),
                    ) if download_bytes == exec_bytes && download_sha == exec_sha => Ok(evidence),
                    _ => Err(verification_error(
                        "download lacks matching durable execution evidence",
                    )),
                }
            }
            PrimitiveCommand::WaitFor(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Wait { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("wait returned no condition evidence"))
                }
            }
            PrimitiveCommand::CaptureScreenshot(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Screenshot { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "screenshot returned no artifact evidence",
                    ))
                }
            }
            PrimitiveCommand::SetFocusEmulation(command) => {
                if evidence.iter().any(|item| matches!(item, Evidence::Configuration { name, value } if name == "focusEmulation" && value == &command.enabled.to_string())) {
                    Ok(evidence)
                } else {
                    Err(verification_error("focus emulation returned no matching configuration evidence"))
                }
            }
            PrimitiveCommand::SetEmulatedMedia(command) => {
                let expected = serde_json::to_string(command).map_err(|_| verification_error("media configuration serialization failed"))?;
                if evidence.iter().any(|item| matches!(item, Evidence::Configuration { name, value } if name == "emulatedMedia" && value == &expected)) {
                    Ok(evidence)
                } else {
                    Err(verification_error("media emulation returned no matching configuration evidence"))
                }
            }
            PrimitiveCommand::EvaluateJavaScript(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::JavaScriptResult { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "javascript evaluation returned no result evidence",
                    ))
                }
            }
        }
    }

    async fn finish_failure(
        &self,
        envelope: &CommandEnvelope,
        outcome: CommandOutcome,
    ) -> CommandOutcome {
        self.finish_failure_durable(envelope, outcome).await.0
    }

    async fn finish_failure_durable(
        &self,
        envelope: &CommandEnvelope,
        outcome: CommandOutcome,
    ) -> (CommandOutcome, bool) {
        let failure_fields = match &outcome {
            CommandOutcome::Failed { error, .. } => Some(("failed", error)),
            CommandOutcome::RetryableFailure { error, .. } => Some(("retryableFailure", error)),
            CommandOutcome::NeedsReconciliation { error, .. } => {
                Some(("needsReconciliation", error))
            }
            CommandOutcome::PolicyDenied { error, .. } => Some(("policyDenied", error)),
            _ => None,
        };
        if let Some((outcome_label, error)) = failure_fields {
            tracing::warn!(
                command = crate::context::command_kind_name(&envelope.command),
                session_id = %envelope.session_id.0,
                page_id = ?envelope.page_id.as_ref().map(|id| id.0),
                outcome = outcome_label,
                code = ?error.code,
                retryable = error.retryable,
                message = %error.message,
                "command failed"
            );
        }
        // A failed command may still have changed the page (a click that timed
        // out waiting for navigation may have navigated), so the context graph
        // forgets on any non-replayable failure.
        if let Some(page_id) = envelope.page_id.as_ref() {
            self.context().invalidate_for(page_id, &envelope.command);
        }
        let failure_evidence = match &outcome {
            CommandOutcome::Failed { evidence, .. }
            | CommandOutcome::NeedsReconciliation { evidence, .. } => evidence.clone(),
            _ => Vec::new(),
        };
        self.promote_outcome(envelope, &failure_evidence, false)
            .await;
        let Some(journal) = &self.journal else {
            return (outcome, false);
        };
        match journal
            .append(record(
                envelope,
                CommandPhase::Failed,
                None,
                Some(outcome.journal_safe()),
            ))
            .await
        {
            Ok(()) => (outcome, true),
            Err(error) => (journal_failure(envelope, error, true), false),
        }
    }

    /// Promotes a command's outcome into the durable context graph. No-op
    /// unless this runtime has a durable profile identity; never fails the
    /// command — promotion is write-behind and degrades to session-only.
    async fn promote_outcome(
        &self,
        envelope: &CommandEnvelope,
        evidence: &[Evidence],
        success: bool,
    ) {
        let Some(promotion) = &self.promotion else {
            return;
        };
        let Some(page_id) = envelope.page_id.as_ref() else {
            return;
        };
        let url = self.get(page_id).await.ok().and_then(|page| page.url);
        promotion
            .record_outcome(url.as_deref(), evidence, success)
            .await;
    }
}

/// Evidence that the CDP transport was reattached without losing the page.
fn reattach_evidence() -> Evidence {
    Evidence::Configuration {
        name: "cdpReattach".into(),
        value: "websocket reset with the browser process still alive; reattached to the \
                same process and page state is preserved"
            .into(),
    }
}

fn classify_failure(
    envelope: &CommandEnvelope,
    error: CommandError,
    evidence: Vec<Evidence>,
) -> CommandOutcome {
    if matches!(
        error.code,
        ErrorCode::NetworkPolicyDenied | ErrorCode::PolicyDenied
    ) {
        CommandOutcome::PolicyDenied {
            command_id: envelope.command_id.clone(),
            error,
        }
    } else if requires_reconciliation(envelope)
        && !is_pre_effect(&error)
        && !is_postcondition_failure(&error)
        && !is_transient_target_loss(&error)
    {
        CommandOutcome::NeedsReconciliation {
            command_id: envelope.command_id.clone(),
            error,
            evidence,
        }
    } else if error.retryable {
        CommandOutcome::RetryableFailure {
            command_id: envelope.command_id.clone(),
            error,
        }
    } else {
        CommandOutcome::Failed {
            command_id: envelope.command_id.clone(),
            error,
            evidence,
        }
    }
}

fn prepared_failure(
    envelope: &CommandEnvelope,
    error: CommandError,
    evidence: Vec<Evidence>,
) -> CommandOutcome {
    CommandOutcome::NeedsReconciliation {
        command_id: envelope.command_id.clone(),
        error,
        evidence,
    }
}

fn record(
    envelope: &CommandEnvelope,
    phase: CommandPhase,
    stored_envelope: Option<CommandEnvelope>,
    outcome: Option<CommandOutcome>,
) -> JournalRecord {
    JournalRecord {
        sequence: 0,
        recorded_at: Utc::now(),
        command_id: envelope.command_id.clone(),
        phase,
        envelope: stored_envelope,
        outcome,
        prepared_result: None,
    }
}

fn prepared_record(envelope: &CommandEnvelope, prepared_result: PreparedResult) -> JournalRecord {
    JournalRecord {
        sequence: 0,
        recorded_at: Utc::now(),
        command_id: envelope.command_id.clone(),
        phase: CommandPhase::ResultPrepared,
        envelope: None,
        outcome: None,
        prepared_result: Some(prepared_result),
    }
}

fn journal_failure(
    envelope: &CommandEnvelope,
    error: JournalError,
    may_have_executed: bool,
) -> CommandOutcome {
    let command_error = CommandError {
        code: ErrorCode::JournalFailed,
        message: error.to_string(),
        layer: ErrorLayer::Journal,
        retryable: true,
    };
    if may_have_executed && requires_reconciliation(envelope) {
        CommandOutcome::NeedsReconciliation {
            command_id: envelope.command_id.clone(),
            error: command_error,
            evidence: Vec::new(),
        }
    } else {
        CommandOutcome::RetryableFailure {
            command_id: envelope.command_id.clone(),
            error: command_error,
        }
    }
}

fn requires_reconciliation(envelope: &CommandEnvelope) -> bool {
    envelope.command.class() == CommandClass::Boundary
        || matches!(
            envelope.command,
            RuntimeCommand::Primitive(PrimitiveCommand::DownloadUrl(_))
        )
}

/// Errors raised before the command could reach the browser, or aborted
/// before any artifact/file landed: argument validation and target
/// resolution run before dispatch, and a response body over the configured
/// cap is dropped mid-stream with nothing written, so the side effect
/// provably never landed. Reporting `needsReconciliation` for these tells
/// the agent to stop and reconcile an effect that never happened.
fn is_pre_effect(error: &CommandError) -> bool {
    matches!(
        error.code,
        ErrorCode::InvalidRequest
            | ErrorCode::TargetNotFound
            | ErrorCode::TargetAmbiguous
            | ErrorCode::FrameNotFound
            | ErrorCode::ShadowRootUnavailable
            | ErrorCode::IntentCompileFailed
            | ErrorCode::IntentActionMismatch
            | ErrorCode::HttpResponseTooLarge
    )
}

/// Postcondition failures after a known act (click landed, wait/verify did
/// not). Keep these as plain `failed` so agents inspect and adjust rather
/// than entering the Boundary never-retry recovery path.
fn is_postcondition_failure(error: &CommandError) -> bool {
    matches!(
        error.code,
        ErrorCode::VerificationFailed | ErrorCode::WaitConditionTimedOut
    )
}

/// Transient page/target loss after an act: retryable re-list/reattach, not
/// Boundary never-retry reconciliation (which caused double-saves in gauntlet
/// when a tab died mid-submit).
fn is_transient_target_loss(error: &CommandError) -> bool {
    matches!(error.code, ErrorCode::TargetDetached)
}

fn cleanup_committed_download(committed: &mut Option<crate::adaptive::CommittedDownload>) {
    if let Some(committed) = committed.take() {
        if let Err(error) = committed.cleanup() {
            tracing::warn!(error = ?error, "completed download staging cleanup failed");
        }
    }
}

fn journal_error(error: JournalError) -> CommandError {
    CommandError {
        code: ErrorCode::JournalFailed,
        message: error.to_string(),
        layer: ErrorLayer::Journal,
        retryable: true,
    }
}

fn validation_error(message: impl Into<String>) -> CommandError {
    CommandError {
        code: ErrorCode::InvalidRequest,
        message: message.into(),
        layer: ErrorLayer::Workflow,
        retryable: false,
    }
}

fn missing_page_error() -> CommandError {
    CommandError {
        code: ErrorCode::NotFound,
        message: "runtime resource was not found".into(),
        layer: ErrorLayer::Workflow,
        retryable: false,
    }
}

fn verification_error(message: impl Into<String>) -> CommandError {
    CommandError {
        code: ErrorCode::VerificationFailed,
        message: message.into(),
        layer: ErrorLayer::Page,
        retryable: true,
    }
}

/// Whether a TypeText command's post-action state counts as verified.
///
/// `inspected` is the independent post-action read (`Inspection.text`);
/// `observed` is the worker's own post-action element read
/// (`Evidence::Element.text`, first one); `kind` is the worker's
/// `typedControlKind` Configuration value ("text" when the worker did not
/// report one). Exact match always passes; an append (`!clear_first`) passes
/// when the independent read ends with the typed value; a checkable passes
/// when the worker's checked-state read echoes the typed boolean; a select
/// passes when the independent read confirms the option value the worker set.
fn typed_value_verified(
    value: &str,
    clear_first: bool,
    inspected: &str,
    observed: Option<&str>,
    kind: &str,
) -> bool {
    if inspected == value {
        return true;
    }
    if !clear_first && inspected.ends_with(value) {
        return true;
    }
    if value.parse::<bool>().is_ok() && observed == Some(value) {
        return true;
    }
    if kind == "select"
        && observed.is_some_and(|observed| !observed.is_empty() && observed == inspected)
    {
        return true;
    }
    false
}

fn internal_error(message: impl Into<String>) -> CommandError {
    CommandError {
        code: ErrorCode::Internal,
        message: message.into(),
        layer: ErrorLayer::Page,
        retryable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::typed_value_verified;

    #[test]
    fn exact_match_passes() {
        assert!(typed_value_verified("Ada", true, "Ada", None, "text"));
    }

    #[test]
    fn exact_match_fails_on_mismatch() {
        assert!(!typed_value_verified(
            "ada@example.test",
            true,
            "not-the-typed-value",
            None,
            "text"
        ));
    }

    #[test]
    fn append_without_clear_passes_when_inspected_ends_with_value() {
        assert!(typed_value_verified("x", false, "prefilledx", None, "text"));
    }

    #[test]
    fn append_with_clear_first_does_not_use_suffix_rule() {
        assert!(!typed_value_verified("x", true, "prefilledx", None, "text"));
    }

    #[test]
    fn append_fails_when_inspected_does_not_end_with_value() {
        assert!(!typed_value_verified("x", false, "prefilled", None, "text"));
    }

    #[test]
    fn checkable_passes_when_observed_echoes_typed_boolean() {
        assert!(typed_value_verified(
            "true",
            true,
            "on",
            Some("true"),
            "checkable"
        ));
    }

    #[test]
    fn checkable_fails_when_observed_does_not_match() {
        assert!(!typed_value_verified(
            "true",
            true,
            "on",
            Some("false"),
            "checkable"
        ));
    }

    #[test]
    fn checkable_fails_when_value_is_not_boolean() {
        assert!(!typed_value_verified(
            "maybe",
            true,
            "on",
            Some("maybe"),
            "checkable"
        ));
    }

    #[test]
    fn select_passes_when_observed_confirms_inspected_option_value() {
        assert!(typed_value_verified(
            "Pro plan",
            true,
            "pro",
            Some("pro"),
            "select"
        ));
    }

    #[test]
    fn select_fails_when_observed_differs_from_inspected() {
        assert!(!typed_value_verified(
            "Pro plan",
            true,
            "pro",
            Some("basic"),
            "select"
        ));
    }

    #[test]
    fn select_fails_when_observed_is_empty() {
        assert!(!typed_value_verified(
            "Pro plan",
            true,
            "pro",
            Some(""),
            "select"
        ));
    }

    #[test]
    fn select_fails_when_observed_is_absent() {
        assert!(!typed_value_verified(
            "Pro plan", true, "pro", None, "select"
        ));
    }
}
