use chrono::Utc;
use thiserror::Error;
use types::{
    CommandClass, CommandEnvelope, CommandError, CommandId, CommandOutcome, CommandPhase,
    ErrorCode, ErrorLayer, Evidence, InspectCommand, PrimitiveCommand,
};
use workflow_journal::{JournalError, JournalRecord, PreparedResult};

use crate::PageRuntime;

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
        CommandOutcome::RetryableFailure {
            command_id,
            error: internal_error("no durable prepared result exists"),
        }
    }

    pub async fn execute(&self, envelope: CommandEnvelope) -> CommandOutcome {
        let command_id = envelope.command_id.clone();
        if let Err(error) = self.validate(&envelope).await {
            return CommandOutcome::Failed { command_id, error };
        }
        let Some(journal) = &self.journal else {
            return CommandOutcome::Failed {
                command_id,
                error: internal_error("command journal is not configured"),
            };
        };
        let Some(workers) = &self.workers else {
            return CommandOutcome::Failed {
                command_id,
                error: internal_error("browser workers are not configured"),
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
        if let Err(error) = journal
            .append(record(&envelope, CommandPhase::Prepared, None, None))
            .await
        {
            return journal_failure(&envelope, error, false);
        }
        if let Err(error) = journal
            .append(record(&envelope, CommandPhase::Executing, None, None))
            .await
        {
            return journal_failure(&envelope, error, false);
        }

        let lease = match workers.lease(envelope.session_id.clone()).await {
            Ok(lease) => lease,
            Err(error) => {
                return self
                    .finish_failure(&envelope, classify_failure(&envelope, error))
                    .await;
            }
        };
        let page_id = envelope.page_id.as_ref().expect("validated page id");
        let page_state = match self.get(page_id).await {
            Ok(page) => page,
            Err(_) => {
                return self
                    .finish_failure(
                        &envelope,
                        classify_failure(
                            &envelope,
                            internal_error("page disappeared before dispatch"),
                        ),
                    )
                    .await;
            }
        };
        let mut execution = match self.adaptive.execute(&envelope, &lease, page_state).await {
            Ok(execution) => execution,
            Err(error) => {
                return self
                    .finish_failure(&envelope, classify_failure(&envelope, error))
                    .await;
            }
        };
        if let Some(mut prepared) = execution.prepared_http.take() {
            let artifact = prepared.artifact.take().map(|pending| pending.commit());
            let prepared_result = PreparedResult {
                command_id: envelope.command_id.clone(),
                attempt_id: envelope.attempt_id.clone(),
                state_version: prepared.state_version,
                state_delta: serde_json::to_value(&prepared.state)
                    .unwrap_or(serde_json::Value::Null),
                evidence: execution.evidence.clone(),
                artifact_id: artifact.as_ref().map(|record| record.artifact_id.clone()),
                artifact_sha256: artifact.as_ref().map(|record| record.sha256.clone()),
            };
            if let Err(error) = journal
                .append(prepared_record(&envelope, prepared_result))
                .await
            {
                return prepared_failure(&envelope, journal_error(error), execution.evidence);
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
            PrimitiveCommand::OpenPage(_) => {
                if let Some(Evidence::Page { page_id, url, .. }) = evidence.first() {
                    self.register_page_id(
                        envelope.session_id.clone(),
                        page_id.clone(),
                        url.clone(),
                    )
                    .await;
                }
            }
            PrimitiveCommand::ClickAndWaitForPopup(_) => {
                if let Some(Evidence::Popup { page_id, url, .. }) = evidence.first() {
                    self.register_page_id(
                        envelope.session_id.clone(),
                        page_id.clone(),
                        url.clone(),
                    )
                    .await;
                }
            }
            PrimitiveCommand::ClosePage(command) => self.remove_page(&command.page_id).await,
            _ => {}
        }

        if let Err(error) = journal
            .append(record(&envelope, CommandPhase::Verifying, None, None))
            .await
        {
            return journal_failure(&envelope, error, true);
        }
        match self.verify(&envelope, &lease, evidence).await {
            Ok(evidence) => {
                if let PrimitiveCommand::Navigate(_) = &envelope.command {
                    if let Some(Evidence::Navigation { url, .. }) = evidence.first() {
                        let _ = self.set_url(page_id, url.clone(), "interactive").await;
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
                self.finish_failure(&envelope, classify_failure(&envelope, error))
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
        if let PrimitiveCommand::Navigate(command) = &envelope.command {
            if !(command.url.starts_with("http://")
                || command.url.starts_with("https://")
                || command.url.starts_with("data:"))
            {
                return Err(validation_error("navigation URL scheme is not supported"));
            }
        }
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
        let page_id = envelope.page_id.as_ref().expect("validated page id");
        match &envelope.command {
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
                        page_id,
                        &InspectCommand {
                            selector: Some(command.selector.clone()),
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

fn classify_failure(envelope: &CommandEnvelope, error: CommandError) -> CommandOutcome {
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
            evidence: Vec::new(),
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
        || matches!(envelope.command, PrimitiveCommand::DownloadUrl(_))
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
