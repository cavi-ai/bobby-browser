use super::*;

pub(super) fn failure_from_outcome(outcome: &CommandOutcome, class: CommandClass) -> SkillFailure {
    match outcome {
        CommandOutcome::NeedsReconciliation { .. } if class == CommandClass::Boundary => {
            SkillFailure::EffectUncertain
        }
        CommandOutcome::NeedsReconciliation { .. } => SkillFailure::PostconditionFailed,
        CommandOutcome::RetryableFailure { error, .. }
        | CommandOutcome::PolicyDenied { error, .. }
        | CommandOutcome::ResourceExhausted { error, .. }
        | CommandOutcome::Failed { error, .. } => failure_from_error(error),
        CommandOutcome::Restarted { .. } => SkillFailure::CheckpointMismatch,
        CommandOutcome::Completed { .. } => SkillFailure::ConfigurationConflict,
    }
}

pub(super) fn failure_from_error(error: &CommandError) -> SkillFailure {
    match error.code {
        ErrorCode::DeadlineExceeded => SkillFailure::DeadlineExceeded,
        ErrorCode::TargetNotFound
        | ErrorCode::TargetAmbiguous
        | ErrorCode::FrameNotFound
        | ErrorCode::ShadowRootUnavailable
        | ErrorCode::TargetDetached => SkillFailure::TargetDrift,
        ErrorCode::BrowserLaunchFailed | ErrorCode::ResourceExhausted => {
            SkillFailure::EngineUnavailable
        }
        ErrorCode::VerificationFailed | ErrorCode::WaitConditionTimedOut => {
            SkillFailure::PostconditionFailed
        }
        ErrorCode::PolicyDenied | ErrorCode::NetworkPolicyDenied | ErrorCode::InvalidRequest => {
            SkillFailure::ConfigurationConflict
        }
        _ if error.retryable => SkillFailure::TargetDrift,
        _ => SkillFailure::PostconditionFailed,
    }
}

pub(super) fn tactic_record(
    decision: &SkillDecision,
    effect: SkillTacticEffect,
) -> SkillTacticEvidence {
    SkillTacticEvidence {
        tactic: decision.tactic,
        trigger: decision.trigger,
        effect,
        remaining_deadline_ms: decision.remaining_deadline_ms,
        tactic_budget_ms: decision.tactic_budget_ms,
    }
}

pub(super) fn evidence_refs<'a>(
    evidence: impl IntoIterator<Item = &'a SkillTacticEvidence>,
) -> Result<Vec<SkillEvidenceRef>, CommandError> {
    evidence
        .into_iter()
        .enumerate()
        .map(|(index, item)| {
            let bytes = serde_json::to_vec(item).map_err(skill_contract_error)?;
            SkillEvidenceRef::new(
                format!("skill-recovery-{index}-{:?}", item.tactic).to_ascii_lowercase(),
                format!("{:x}", Sha256::digest(bytes)),
            )
            .map_err(skill_contract_error)
        })
        .collect()
}

pub(super) fn append_tactic_evidence(
    mut outcome: CommandOutcome,
    tactic_evidence: &[SkillTacticEvidence],
) -> Result<CommandOutcome, CommandError> {
    let target = match &mut outcome {
        CommandOutcome::Completed { evidence, .. }
        | CommandOutcome::NeedsReconciliation { evidence, .. }
        | CommandOutcome::Restarted { evidence, .. } => Some(evidence),
        _ => None,
    };
    if let Some(target) = target {
        for tactic in tactic_evidence {
            target.push(Evidence::Configuration {
                name: "skillRecoveryTactic".into(),
                value: serde_json::to_string(tactic).map_err(skill_contract_error)?,
            });
        }
    }
    Ok(outcome)
}

pub(super) fn replace_skill_state(
    store: &SkillStateStore,
    next: &types::SkillSessionState,
) -> Result<(), SkillStateStoreError> {
    store.transition(&next.session_id, |state| {
        *state = next.clone();
        Ok(())
    })
}

pub(super) fn command_failure(
    envelope: &CommandEnvelope,
    error: CommandError,
    failure: SkillFailure,
) -> CommandOutcome {
    if failure == SkillFailure::EffectUncertain
        || envelope.command.class() == CommandClass::Boundary
    {
        CommandOutcome::NeedsReconciliation {
            command_id: envelope.command_id.clone(),
            error,
            evidence: Vec::new(),
        }
    } else {
        CommandOutcome::Failed {
            command_id: envelope.command_id.clone(),
            error,
            evidence: Vec::new(),
        }
    }
}

pub(super) fn skill_contract_error(error: impl std::fmt::Display) -> CommandError {
    recovery_error(
        ErrorCode::Internal,
        format!("skill evidence contract failed: {error}"),
        false,
    )
}

pub(super) fn skill_state_error(error: SkillStateStoreError) -> CommandError {
    recovery_error(
        ErrorCode::Internal,
        format!("skill state persistence failed: {error}"),
        false,
    )
}

pub(super) fn store_failure(_: SkillStateStoreError) -> SkillFailure {
    SkillFailure::ConfigurationConflict
}

pub(super) fn skill_failure_error(failure: SkillFailure) -> CommandError {
    let code = if failure == SkillFailure::DeadlineExceeded {
        ErrorCode::DeadlineExceeded
    } else {
        ErrorCode::Internal
    };
    recovery_error(code, format!("skill recovery failed: {failure:?}"), false)
}

pub(super) fn is_owned_terminal_error(error: &CommandError) -> bool {
    error.message.starts_with(OWNED_TERMINAL_PREFIX)
}

pub(super) fn is_outbox_pending_error(error: &CommandError) -> bool {
    error.message.starts_with(OUTBOX_PENDING_PREFIX)
}

pub(super) fn journal_error(error: workflow_journal::JournalError) -> CommandError {
    recovery_error(ErrorCode::JournalFailed, error.to_string(), true)
}

pub(super) fn deadline_error(message: impl Into<String>) -> CommandError {
    recovery_error(ErrorCode::DeadlineExceeded, message, false)
}

pub(super) fn recovery_error(
    code: ErrorCode,
    message: impl Into<String>,
    retryable: bool,
) -> CommandError {
    CommandError {
        code,
        message: message.into(),
        layer: ErrorLayer::Workflow,
        retryable,
    }
}

pub(super) fn recovery_identity(
    envelope: &CommandEnvelope,
) -> Result<RecoveryCommandIdentity, CommandError> {
    let identity = issued_command_identity(envelope).map_err(skill_failure_error)?;
    RecoveryCommandIdentity::new(
        identity.command_id,
        identity.workflow_id,
        identity.attempt_id,
        identity.session_id,
        identity.page_id,
        identity.command_class,
        identity.command_sha256,
    )
    .map_err(skill_contract_error)
}

fn recovery_receipt(
    envelope: &CommandEnvelope,
    issued: &SkillIssuedDecision,
    command_outcome: &CommandOutcome,
    skill_outcome: &SkillOutcome,
    tactic_evidence: &[SkillTacticEvidence],
) -> Result<RecoveryReceipt, CommandError> {
    let command_identity = issued_command_identity(envelope).map_err(skill_failure_error)?;
    if issued.command_identity.as_ref() != Some(&command_identity) {
        return Err(recovery_error(
            ErrorCode::InvalidRequest,
            "issued recovery identity changed before receipt persistence",
            false,
        ));
    }
    RecoveryReceipt::new(
        envelope.command_id.clone(),
        recovery_identity(envelope)?,
        RecoveryReceiptState::Unresolved,
        issued.reservation_id.clone(),
        issued.decision.clone(),
        command_outcome.journal_safe(),
        skill_outcome.clone(),
        tactic_evidence
            .iter()
            .map(|evidence| {
                Ok(Evidence::Configuration {
                    name: "skillRecoveryReceiptTactic".into(),
                    value: serde_json::to_string(evidence).map_err(skill_contract_error)?,
                })
            })
            .collect::<Result<Vec<_>, CommandError>>()?,
        Utc::now(),
    )
    .map_err(skill_contract_error)
}

fn execution_from_receipt(
    receipt: &RecoveryReceipt,
) -> Result<SkillRecoveryExecution, CommandError> {
    let tactic_evidence = receipt
        .tactic_evidence
        .iter()
        .filter_map(|evidence| match evidence {
            Evidence::Configuration { name, value } if name == "skillRecoveryReceiptTactic" => {
                Some(value)
            }
            _ => None,
        })
        .map(|value| serde_json::from_str(value).map_err(skill_contract_error))
        .collect::<Result<Vec<SkillTacticEvidence>, CommandError>>()?;
    Ok(SkillRecoveryExecution {
        command_outcome: receipt.command_outcome.clone(),
        skill_outcome: receipt.skill_outcome.clone(),
        tactic_evidence,
    })
}

impl SkillRecoveryCoordinator {
    pub(super) async fn commit_owned_execution(
        &self,
        decision: &SkillDecision,
        envelope: &CommandEnvelope,
        command_outcome: &CommandOutcome,
        skill_outcome: &SkillOutcome,
        tactic_evidence: &[SkillTacticEvidence],
    ) -> Result<SkillRecoveryExecution, CommandError> {
        let issued = self.issued_for_receipt(decision, envelope).await?;
        let mut receipt = recovery_receipt(
            envelope,
            &issued,
            command_outcome,
            skill_outcome,
            tactic_evidence,
        )?;
        self.recovery
            .persist_recovery_receipt(receipt.clone())
            .await
            .map_err(recovery_coordinator_error)?;
        self.recovery
            .transition_recovery_receipt(&receipt.identity, RecoveryReceiptState::PendingJournal)
            .await
            .map_err(recovery_coordinator_error)?;
        if let Err(error) = self.flush_recovery_receipt(envelope, &receipt).await {
            return Err(recovery_error(
                error.code,
                format!("{OUTBOX_PENDING_PREFIX}{}", error.message),
                error.retryable,
            ));
        }
        self.recovery
            .transition_recovery_receipt(&receipt.identity, RecoveryReceiptState::Committed)
            .await
            .map_err(recovery_coordinator_error)?;
        receipt.state = RecoveryReceiptState::Committed;
        self.settle_committed_receipt(&receipt).await?;
        execution_from_receipt(&receipt)
    }

    pub(super) async fn record_decision(
        &self,
        decision: &SkillDecision,
        outcome: &SkillOutcome,
    ) -> Result<(), CommandError> {
        let mut skills = self.skills.lock().await;
        let before = skills.clone();
        skills
            .record_outcome(decision, outcome, Utc::now())
            .map_err(skill_failure_error)?;
        let completed = skills.session_state().clone();
        if let Err(error) = replace_skill_state(&self.skill_state, &completed) {
            *skills = before;
            return Err(skill_state_error(error));
        }
        self.recovery
            .clear_skill_issuance(
                &before
                    .session_state()
                    .pending_issuance
                    .as_ref()
                    .and_then(|issued| issued.command_identity.as_ref())
                    .ok_or_else(|| {
                        recovery_error(
                            ErrorCode::Internal,
                            "issued decision lost command identity",
                            false,
                        )
                    })?
                    .workflow_id,
            )
            .await
            .map_err(recovery_coordinator_error)?;
        Ok(())
    }

    async fn issued_for_receipt(
        &self,
        decision: &SkillDecision,
        envelope: &CommandEnvelope,
    ) -> Result<SkillIssuedDecision, CommandError> {
        let issued = self
            .skills
            .lock()
            .await
            .session_state()
            .pending_issuance
            .clone()
            .ok_or_else(|| {
                recovery_error(
                    ErrorCode::InvalidRequest,
                    "owned recovery receipt requires its issued decision",
                    false,
                )
            })?;
        let identity = issued_command_identity(envelope).map_err(skill_failure_error)?;
        if issued.decision != *decision || issued.command_identity.as_ref() != Some(&identity) {
            return Err(recovery_error(
                ErrorCode::InvalidRequest,
                "owned recovery receipt does not match its issued decision",
                false,
            ));
        }
        Ok(issued)
    }

    async fn settle_committed_receipt(
        &self,
        receipt: &RecoveryReceipt,
    ) -> Result<(), CommandError> {
        let mut skills = self.skills.lock().await;
        let mut settled = skills.clone();
        settled
            .settle_committed_receipt(receipt)
            .map_err(skill_failure_error)?;
        self.skill_state
            .settle_committed_receipt(receipt)
            .map_err(skill_state_error)?;
        *skills = settled;
        self.recovery
            .clear_skill_issuance(&receipt.identity.workflow_id)
            .await
            .map_err(recovery_coordinator_error)?;
        Ok(())
    }

    pub(super) async fn finalize_owned_deadline(
        &self,
        decision: &SkillDecision,
        envelope: &CommandEnvelope,
    ) -> Result<(), CommandError> {
        let tactic_evidence = vec![tactic_record(
            decision,
            SkillTacticEffect::ReconciliationRequired,
        )];
        let failure = if envelope.command.class() == CommandClass::Boundary {
            SkillFailure::EffectUncertain
        } else {
            SkillFailure::DeadlineExceeded
        };
        let skill_outcome = SkillOutcome::failed(failure, evidence_refs(&tactic_evidence)?)
            .map_err(skill_contract_error)?;
        let error = if failure == SkillFailure::DeadlineExceeded {
            deadline_error("owned recovery stabilized after the command deadline")
        } else {
            recovery_error(
                ErrorCode::VerificationFailed,
                "owned boundary recovery stabilized with an uncertain effect",
                false,
            )
        };
        let outcome =
            append_tactic_evidence(command_failure(envelope, error, failure), &tactic_evidence)?;
        let issued = self.issued_for_receipt(decision, envelope).await?;
        let mut receipt = recovery_receipt(
            envelope,
            &issued,
            &outcome,
            &skill_outcome,
            &tactic_evidence,
        )?;
        self.recovery
            .persist_recovery_receipt(receipt.clone())
            .await
            .map_err(recovery_coordinator_error)?;
        self.recovery
            .transition_recovery_receipt(&receipt.identity, RecoveryReceiptState::PendingJournal)
            .await
            .map_err(recovery_coordinator_error)?;
        if let Err(error) = self.flush_recovery_receipt(envelope, &receipt).await {
            return Err(recovery_error(
                error.code,
                format!("{OUTBOX_PENDING_PREFIX}{}", error.message),
                error.retryable,
            ));
        }
        self.recovery
            .transition_recovery_receipt(&receipt.identity, RecoveryReceiptState::Committed)
            .await
            .map_err(recovery_coordinator_error)?;
        receipt.state = RecoveryReceiptState::Committed;
        self.settle_committed_receipt(&receipt)
            .await
            .map_err(|error| {
                recovery_error(
                    error.code,
                    format!("{OWNED_TERMINAL_PREFIX}{}", error.message),
                    error.retryable,
                )
            })
    }

    pub(super) async fn persist_unresolved_deadline(
        &self,
        decision: &SkillDecision,
        envelope: &CommandEnvelope,
    ) -> Result<(), CommandError> {
        let tactic_evidence = vec![tactic_record(
            decision,
            SkillTacticEffect::ReconciliationRequired,
        )];
        let failure = if envelope.command.class() == CommandClass::Boundary {
            SkillFailure::EffectUncertain
        } else {
            SkillFailure::DeadlineExceeded
        };
        let skill_outcome = SkillOutcome::failed(failure, evidence_refs(&tactic_evidence)?)
            .map_err(skill_contract_error)?;
        let error = if failure == SkillFailure::DeadlineExceeded {
            deadline_error("owned recovery did not stabilize within its finalization budget")
        } else {
            recovery_error(
                ErrorCode::VerificationFailed,
                "owned boundary recovery is still stabilizing with an uncertain effect",
                false,
            )
        };
        let outcome =
            append_tactic_evidence(command_failure(envelope, error, failure), &tactic_evidence)?;
        let issued = self.issued_for_receipt(decision, envelope).await?;
        let receipt = recovery_receipt(
            envelope,
            &issued,
            &outcome,
            &skill_outcome,
            &tactic_evidence,
        )?;
        self.recovery
            .persist_recovery_receipt(receipt)
            .await
            .map_err(recovery_coordinator_error)
    }

    pub(super) async fn replay_recovery_receipt(
        &self,
        envelope: &CommandEnvelope,
    ) -> Result<Option<SkillRecoveryExecution>, CommandError> {
        let checkpoint = match self.recovery.load_checkpoint(&envelope.workflow_id).await {
            Ok(checkpoint) => checkpoint,
            Err(crate::RecoveryError::Store(checkpoint_store::CheckpointStoreError::NotFound(
                _,
            ))) => {
                return Ok(None);
            }
            Err(error) => return Err(recovery_coordinator_error(error)),
        };
        let in_memory = self
            .skills
            .lock()
            .await
            .session_state()
            .pending_issuance
            .clone();
        let pending = match in_memory {
            Some(pending) => Some(pending),
            None => self
                .recovery
                .load_skill_issuance(&envelope.workflow_id)
                .await
                .map_err(recovery_coordinator_error)?,
        };
        if let Some(pending) = &pending {
            let identity = issued_command_identity(envelope).map_err(skill_failure_error)?;
            if pending.command_identity.as_ref() != Some(&identity) {
                return Err(recovery_error(
                    ErrorCode::InvalidRequest,
                    "pending recovery issuance belongs to a different command identity",
                    false,
                ));
            }
        }
        for receipt in &checkpoint.recovery_receipts {
            receipt.validate().map_err(|error| {
                recovery_error(
                    ErrorCode::InvalidRequest,
                    format!("invalid durable recovery receipt: {error}"),
                    false,
                )
            })?;
        }
        let identity = recovery_identity(envelope)?;
        let exact = checkpoint
            .recovery_receipts
            .iter()
            .find(|receipt| receipt.identity == identity)
            .cloned();
        if exact.is_none()
            && checkpoint.recovery_receipts.iter().any(|receipt| {
                receipt.identity.command_id == identity.command_id && receipt.identity != identity
            })
        {
            return Err(recovery_error(
                ErrorCode::InvalidRequest,
                "durable recovery receipt belongs to a different command payload",
                false,
            ));
        }
        if exact.is_none()
            && checkpoint.recovery_receipts.iter().any(|receipt| {
                receipt.identity.session_id == envelope.session_id
                    && matches!(
                        receipt.state,
                        RecoveryReceiptState::PendingJournal | RecoveryReceiptState::Unresolved
                    )
            })
        {
            return Err(recovery_error(
                ErrorCode::InvalidRequest,
                "a recovery receipt for a different command identity must be reconciled first",
                false,
            ));
        }
        let Some(mut receipt) = exact else {
            return Ok(None);
        };
        if receipt.state == RecoveryReceiptState::Unresolved {
            return Ok(None);
        }
        if receipt.state == RecoveryReceiptState::PendingJournal {
            self.flush_recovery_receipt(envelope, &receipt).await?;
            self.recovery
                .transition_recovery_receipt(&receipt.identity, RecoveryReceiptState::Committed)
                .await
                .map_err(recovery_coordinator_error)?;
            receipt.state = RecoveryReceiptState::Committed;
        }
        self.settle_committed_receipt(&receipt).await?;
        self.recovery
            .clear_skill_issuance(&envelope.workflow_id)
            .await
            .map_err(recovery_coordinator_error)?;
        Ok(Some(execution_from_receipt(&receipt)?))
    }

    async fn flush_recovery_receipt(
        &self,
        envelope: &CommandEnvelope,
        receipt: &RecoveryReceipt,
    ) -> Result<(), CommandError> {
        let journal = self.runtime.journal.as_ref().ok_or_else(|| {
            recovery_error(
                ErrorCode::Internal,
                "command journal is not configured",
                false,
            )
        })?;
        let durable = receipt.command_outcome.journal_safe();
        let history = journal
            .history(envelope.command_id.clone())
            .await
            .map_err(journal_error)?;
        if history
            .records
            .iter()
            .any(|record| record.outcome.as_ref() == Some(&durable))
        {
            return Ok(());
        }
        self.persist_recovery_outcome(envelope, &receipt.command_outcome)
            .await
    }

    pub(super) async fn reconcile_pending_decision(
        &self,
        envelope: &CommandEnvelope,
    ) -> Result<Option<SkillRecoveryExecution>, CommandError> {
        let _stabilized =
            tokio::time::timeout(RECOVERY_FINALIZATION_BUDGET, self.stabilization_gate.lock())
                .await
                .map_err(|_| deadline_error("owned recovery is still stabilizing"))?;
        let pending = self
            .skills
            .lock()
            .await
            .session_state()
            .pending_issuance
            .clone();
        let Some(pending) = pending else {
            return Ok(None);
        };
        let identity = issued_command_identity(envelope).map_err(skill_failure_error)?;
        if pending.command_identity.as_ref() != Some(&identity) {
            return Err(recovery_error(
                ErrorCode::InvalidRequest,
                "pending recovery issuance belongs to a different command identity",
                false,
            ));
        }
        let failure = if envelope.command.class() == CommandClass::Boundary {
            SkillFailure::EffectUncertain
        } else {
            SkillFailure::DeadlineExceeded
        };
        let tactic_evidence = vec![tactic_record(
            &pending.decision,
            SkillTacticEffect::ReconciliationRequired,
        )];
        let skill_outcome = SkillOutcome::failed(failure, evidence_refs(&tactic_evidence)?)
            .map_err(skill_contract_error)?;
        let error = if failure == SkillFailure::DeadlineExceeded {
            deadline_error("reconciled a pending owned recovery before command execution")
        } else {
            recovery_error(
                ErrorCode::VerificationFailed,
                "reconciled a pending boundary recovery with an uncertain effect",
                false,
            )
        };
        let command_outcome =
            append_tactic_evidence(command_failure(envelope, error, failure), &tactic_evidence)?;
        let candidate = recovery_receipt(
            envelope,
            &pending,
            &command_outcome,
            &skill_outcome,
            &tactic_evidence,
        )?;
        let checkpoint = self
            .recovery
            .load_checkpoint(&envelope.workflow_id)
            .await
            .map_err(recovery_coordinator_error)?;
        let mut receipt = checkpoint
            .recovery_receipts
            .iter()
            .find(|receipt| receipt.identity == candidate.identity)
            .cloned()
            .unwrap_or(candidate);
        self.recovery
            .persist_recovery_receipt(receipt.clone())
            .await
            .map_err(recovery_coordinator_error)?;
        self.recovery
            .transition_recovery_receipt(&receipt.identity, RecoveryReceiptState::PendingJournal)
            .await
            .map_err(recovery_coordinator_error)?;
        self.flush_recovery_receipt(envelope, &receipt).await?;
        self.recovery
            .transition_recovery_receipt(&receipt.identity, RecoveryReceiptState::Committed)
            .await
            .map_err(recovery_coordinator_error)?;
        receipt.state = RecoveryReceiptState::Committed;
        self.settle_committed_receipt(&receipt).await?;
        Ok(Some(execution_from_receipt(&receipt)?))
    }

    pub(super) async fn persist_recovery_outcome(
        &self,
        envelope: &CommandEnvelope,
        outcome: &CommandOutcome,
    ) -> Result<(), CommandError> {
        let Some(journal) = &self.runtime.journal else {
            return Err(recovery_error(
                ErrorCode::Internal,
                "command journal is not configured",
                false,
            ));
        };
        journal
            .append(JournalRecord {
                sequence: 0,
                recorded_at: Utc::now(),
                command_id: envelope.command_id.clone(),
                phase: CommandPhase::Recovering,
                envelope: None,
                outcome: None,
                prepared_result: None,
            })
            .await
            .map_err(journal_error)?;
        let phase = if matches!(
            outcome,
            CommandOutcome::Completed { .. } | CommandOutcome::Restarted { .. }
        ) {
            CommandPhase::Completed
        } else {
            CommandPhase::Failed
        };
        journal
            .append(JournalRecord {
                sequence: 0,
                recorded_at: Utc::now(),
                command_id: envelope.command_id.clone(),
                phase,
                envelope: None,
                outcome: Some(outcome.journal_safe()),
                prepared_result: None,
            })
            .await
            .map_err(journal_error)
    }
}
