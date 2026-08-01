use std::sync::Arc;

use chrono::Utc;
use thiserror::Error;
use types::{
    CommandClass, CommandEnvelope, CommandError, CommandId, CommandOutcome, CommandPhase,
    ErrorCode, ErrorLayer, Evidence, InspectCommand, PrimitiveCommand, RuntimeCommand,
};
use workflow_journal::{JournalError, JournalRecord, PreparedResult};

use crate::{PageRuntime, VisionGate};

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
            return outcome;
        }
        if let Some(prepared) = scan
            .records
            .iter()
            .rev()
            .find_map(|record| record.prepared_result.clone())
        {
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
        self.execute_with_vision_gate(envelope, VisionGate::default())
            .await
    }

    pub async fn execute_with_vision_gate(
        &self,
        envelope: CommandEnvelope,
        vision_gate: VisionGate,
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
        let mut execution = match self
            .adaptive
            .execute(&envelope, &lease, page_state, vision_gate)
            .await
        {
            Ok(execution) => execution,
            Err(failure) => {
                return self
                    .finish_failure(
                        &envelope,
                        classify_failure(&envelope, failure.error, failure.evidence),
                    )
                    .await;
            }
        };
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
                evidence: execution.evidence.clone(),
                artifact_id: artifact.as_ref().map(|record| record.artifact_id.clone()),
                artifact_sha256: artifact.as_ref().map(|record| record.sha256.clone()),
                artifact_bytes: artifact.as_ref().map(|record| record.bytes),
                artifact_staging_id: prepared
                    .artifact
                    .as_ref()
                    .and_then(|pending| pending.staging_id().map(str::to_owned)),
            };
            let journal = Arc::clone(journal);
            let prepared_record = prepared_record(&envelope, prepared_result);
            let pending = prepared.artifact.take();
            let observer = self.phase_observer.clone();
            let durable_prepare = tokio::spawn(async move {
                journal
                    .append(prepared_record)
                    .await
                    .map_err(journal_error)?;
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
                Ok::<(), CommandError>(())
            });
            match durable_prepare.await {
                Ok(Ok(())) => {}
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
            if let Err(error) = lease
                .worker()
                .commit_http_state(
                    envelope.page_id.as_ref().expect("validated page id"),
                    prepared.state_version,
                    prepared.state,
                )
                .await
            {
                return self
                    .finish_failure(
                        &envelope,
                        prepared_failure(&envelope, error, execution.evidence.clone()),
                    )
                    .await;
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
                self.remove_page(&command.page_id).await
            }
            RuntimeCommand::Primitive(_) | RuntimeCommand::Intent(_) => {}
        }

        if let Err(error) = journal
            .append(record(&envelope, CommandPhase::Verifying, None, None))
            .await
        {
            return journal_failure(&envelope, error, true);
        }
        self.observe_durable_phase(CommandPhase::Verifying).await;
        match self.verify(&envelope, &lease, evidence).await {
            Ok(evidence) => {
                if let RuntimeCommand::Primitive(PrimitiveCommand::Navigate(_)) = &envelope.command
                {
                    if let Some(Evidence::Navigation { url, .. }) = evidence.first() {
                        let _ = self
                            .set_url(
                                envelope.page_id.as_ref().expect("validated page id"),
                                url.clone(),
                                "interactive",
                            )
                            .await;
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
                    Ok(()) => outcome,
                    Err(error) => journal_failure(&envelope, error, true),
                }
            }
            Err(error) => {
                self.finish_failure(&envelope, classify_failure(&envelope, error, Vec::new()))
                    .await
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
        let page = self
            .get(page_id)
            .await
            .map_err(|_| validation_error("page does not exist"))?;
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
                let matches = verification.iter().any(|item| {
                    matches!(item, Evidence::Inspection { text, .. } if text == &command.value)
                });
                if matches {
                    let mut combined = evidence;
                    combined.extend(verification);
                    Ok(combined)
                } else {
                    Err(verification_error("typed value did not match page state"))
                }
            }
            PrimitiveCommand::Click(command) => {
                if let Some(expected_url) = &command.expected_url {
                    let verification = lease
                        .worker()
                        .inspect(
                            page_id.expect("validated page id"),
                            &InspectCommand::default(),
                        )
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
        let Some(journal) = &self.journal else {
            return outcome;
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
            Ok(()) => outcome,
            Err(error) => journal_failure(envelope, error, true),
        }
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
    } else if requires_reconciliation(envelope) {
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
