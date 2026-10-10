mod admission;
mod context;
mod execution;
mod verification;

use std::sync::Arc;
use std::time::Duration as StdDuration;

use chrono::Utc;
use thiserror::Error;
use types::{
    CommandClass, CommandEnvelope, CommandError, CommandId, CommandOutcome, CommandPhase,
    ErrorCode, ErrorLayer, Evidence, InspectCommand, ListPagesCommand, PrimitiveCommand,
    RuntimeCommand, TextMatch, WaitCondition, WaitForCommand,
};
use workflow_journal::{JournalError, JournalRecord, PreparedResult};

use crate::{PageRuntime, SessionGate, VisionGate};

/// How long a typed Enter is given to start a navigation before the call
/// returns without one.
const ENTER_NAVIGATION_WINDOW: StdDuration = StdDuration::from_millis(1_000);

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
                return CommandOutcome::NeedsReconciliation {
                    command_id,
                    error: journal_error(error),
                    evidence: Vec::new(),
                }
            }
        };
        if scan.torn_tail || scan.incompatible_records > 0 {
            return CommandOutcome::NeedsReconciliation {
                command_id,
                error: internal_error(
                    "command history was damaged and archived; reconciliation is required",
                ),
                evidence: Vec::new(),
            };
        }
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
        let (journal, workers) = match self.admit(&envelope).await {
            Ok(resources) => resources,
            Err(outcome) => return outcome,
        };
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
        if let Err(error) =
            worker_pool::session_settings_or_default(lease.worker().session_settings())
                .set_fingerprint_enabled(gate.fingerprint)
                .await
        {
            return self
                .finish_failure(&envelope, classify_failure(&envelope, error, Vec::new()))
                .await;
        }
        if let Err(error) =
            worker_pool::session_settings_or_default(lease.worker().session_settings())
                .set_humanization_enabled(gate.humanize)
                .await
        {
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
        // Enter can move the page during the keypress itself, so the URL the
        // text is typed on is read before dispatch.
        let typed_on = match (&envelope.command, envelope.page_id.as_ref()) {
            (RuntimeCommand::Primitive(PrimitiveCommand::TypeText(command)), Some(page_id))
                if command.value.contains(['\n', '\r']) =>
            {
                let budget = (envelope.deadline - Utc::now())
                    .to_std()
                    .unwrap_or(StdDuration::ZERO);
                tokio::time::timeout(
                    budget,
                    worker_pool::observation_or_default(lease.worker().observation())
                        .inspect(page_id, &InspectCommand::default()),
                )
                .await
                .ok()
                .and_then(Result::ok)
                .and_then(|evidence| {
                    evidence.into_iter().find_map(|item| match item {
                        Evidence::Inspection { url, .. } => Some(url),
                        _ => None,
                    })
                })
            }
            _ => None,
        };
        let (mut execution, lease_slot) = match self
            .run_with_deadline(&envelope, &gate, workers, lease, page_state)
            .await
        {
            Ok(execution) => execution,
            Err(outcome) => return outcome,
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
            if let Err(error) = worker_pool::web_state_or_default(
                lease_slot
                    .as_ref()
                    .expect("lease survives to state commit")
                    .worker()
                    .web_state(),
            )
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
        let mut evidence = execution.evidence;
        self.record_execution_context(&envelope, &mut evidence)
            .await;

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
                typed_on,
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

/// True if `list_pages` evidence still lists `page_id`: the browser and the
/// page it was asked to act on are both intact, whatever transport error was
/// thrown getting to this check.
fn page_still_listed(evidence: &[Evidence], page_id: &types::PageId) -> bool {
    evidence.iter().any(|item| {
        matches!(
            item,
            Evidence::Pages { pages }
                if pages.iter().any(|entry| entry.page_id == *page_id)
        )
    })
}

/// Evidence that a closed-session-shaped error was a transient target/session
/// loss rather than a browser death: the original diagnostic message, kept
/// because the outer message is replaced with a generic re-resolve
/// instruction.
fn transient_target_loss_evidence(original_message: &str) -> Evidence {
    Evidence::Configuration {
        name: "transientTargetLoss".into(),
        value: original_message.into(),
    }
}

/// Evidence for the relaunch revive path: what the pre-revive probe
/// (`list_pages` on the current lease), the reattach attempt
/// (`worker_pool::reconnect_live_process`'s static reason), and the original
/// failing command each reported. The outer message is replaced with a
/// generic instruction to inspect and re-issue, so this is the only place
/// the actual "why" survives into `commands.jsonl`.
fn browser_revived_evidence(
    probe_error: Option<&str>,
    reattach_error: &str,
    original_message: &str,
) -> Evidence {
    let probe = match probe_error {
        Some(message) => format!("err: {message}"),
        None => "ok".into(),
    };
    Evidence::Configuration {
        name: "browserRevived".into(),
        value: format!(
            "probe: {probe} | reattach: {reattach_error} | original: {original_message}"
        ),
    }
}

fn classify_failure(
    envelope: &CommandEnvelope,
    mut error: CommandError,
    evidence: Vec<Evidence>,
) -> CommandOutcome {
    if error.message == worker_pool::FIREFOX_WORKER_CLOSED_MESSAGE {
        error.message = worker_pool::BROWSER_GONE_MESSAGE.into();
    }
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
            | ErrorCode::ExpectedStatePreSatisfied
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
/// Boundary never-retry reconciliation (which caused double-saves
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

fn internal_error(message: impl Into<String>) -> CommandError {
    CommandError {
        code: ErrorCode::Internal,
        message: message.into(),
        layer: ErrorLayer::Page,
        retryable: false,
    }
}
