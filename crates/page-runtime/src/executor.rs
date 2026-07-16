use chrono::Utc;
use thiserror::Error;
use types::{
    CommandClass, CommandEnvelope, CommandError, CommandOutcome, CommandPhase, ErrorCode,
    ErrorLayer, Evidence, InspectCommand, PrimitiveCommand,
};
use workflow_journal::{JournalError, JournalRecord};

use crate::PageRuntime;

#[derive(Debug, Error)]
pub enum ExecutorError {
    #[error("journal failed: {0}")]
    Journal(#[from] JournalError),
}

impl PageRuntime {
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
                Some(envelope.clone()),
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
        let execution = match &envelope.command {
            PrimitiveCommand::Navigate(command) => lease.worker().navigate(page_id, command).await,
            PrimitiveCommand::Inspect(command) => lease.worker().inspect(page_id, command).await,
            PrimitiveCommand::Click(command) => lease.worker().click(page_id, command).await,
            PrimitiveCommand::TypeText(command) => lease.worker().type_text(page_id, command).await,
        };
        let evidence = match execution {
            Ok(evidence) => evidence,
            Err(error) => {
                return self
                    .finish_failure(&envelope, classify_failure(&envelope, error))
                    .await;
            }
        };

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
                        Some(outcome.clone()),
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
                Some(outcome.clone()),
            ))
            .await
        {
            Ok(()) => outcome,
            Err(error) => journal_failure(envelope, error, true),
        }
    }
}

fn classify_failure(envelope: &CommandEnvelope, error: CommandError) -> CommandOutcome {
    if envelope.command.class() == CommandClass::Boundary {
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
    if may_have_executed && envelope.command.class() == CommandClass::Boundary {
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
