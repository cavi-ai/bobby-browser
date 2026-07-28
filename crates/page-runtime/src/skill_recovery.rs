use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use skill_runtime::{SkillStateStore, SkillStateStoreError, SkillTrigger, SkillZigZagZig};
use tokio::sync::Mutex;
use types::{
    CommandClass, CommandEnvelope, CommandError, CommandId, CommandOutcome, CommandPhase,
    ErrorCode, ErrorLayer, Evidence, InspectCommand, NavigateCommand, PageState, PrimitiveCommand,
    RecoveryCommandIdentity, RecoveryDecision, RecoveryReceipt, RecoveryReceiptState,
    RuntimeCommand, SkillBrowserEngine, SkillCommandIdentity, SkillDecision, SkillEvidenceRef,
    SkillFailure, SkillIssuedDecision, SkillOutcome, SkillTactic, TargetSpec, WaitUntil,
    WorkflowCheckpoint,
};
use worker_pool::{EnginePreference, WorkerPool};
use workflow_journal::JournalRecord;

use crate::{PageRuntime, RecoveryCoordinator, VerifiedRecoveryCheckpoint};

mod checkpoint;
mod outcome;
mod tactics;

use checkpoint::*;
use outcome::*;
use tactics::*;

const RECOVERY_FINALIZATION_BUDGET: Duration = Duration::from_secs(5);
const OWNED_TERMINAL_PREFIX: &str = "owned recovery finalized: ";
const OUTBOX_PENDING_PREFIX: &str = "recovery outbox pending: ";

fn issued_command_identity(
    envelope: &CommandEnvelope,
) -> Result<SkillCommandIdentity, SkillFailure> {
    let command_sha256 = format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&envelope.command)
                .map_err(|_| SkillFailure::ConfigurationConflict)?
        )
    );
    SkillCommandIdentity::new(
        envelope.command_id.clone(),
        envelope.workflow_id.clone(),
        envelope.attempt_id.clone(),
        envelope.session_id.clone(),
        envelope.page_id.clone(),
        envelope.command.class(),
        command_sha256,
    )
    .map_err(|_| SkillFailure::ConfigurationConflict)
}

#[doc(hidden)]
#[cfg(feature = "test-support")]
#[async_trait::async_trait]
pub trait RecoveryPreflightObserver: Send + Sync {
    async fn checkpoint_verified(&self);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SkillTacticEffect {
    PostconditionConfirmed,
    Observed,
    ReResolved,
    CommandRetried,
    CheckpointResumed,
    SessionReplaced,
    EngineReplaced,
    DurableBoundaryRestarted,
    ReconciliationRequired,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SkillTacticEvidence {
    pub tactic: SkillTactic,
    pub trigger: SkillFailure,
    pub effect: SkillTacticEffect,
    pub remaining_deadline_ms: u64,
    pub tactic_budget_ms: u64,
}

#[derive(Debug)]
pub struct SkillRecoveryExecution {
    pub command_outcome: CommandOutcome,
    pub skill_outcome: SkillOutcome,
    pub tactic_evidence: Vec<SkillTacticEvidence>,
}

#[derive(Clone)]
pub struct SkillRecoveryCoordinator {
    runtime: PageRuntime,
    skills: Arc<Mutex<SkillZigZagZig>>,
    skill_state: Arc<SkillStateStore>,
    recovery: RecoveryCoordinator,
    workers: Arc<WorkerPool>,
    execution_gate: Arc<Mutex<()>>,
    stabilization_gate: Arc<Mutex<()>>,
    #[cfg(feature = "test-support")]
    preflight_observer: Option<Arc<dyn RecoveryPreflightObserver>>,
}

enum TacticProgress {
    Continue(SkillTacticEffect),
    Completed(Vec<Evidence>, SkillTacticEffect),
    Restarted(CommandOutcome, SkillTacticEffect),
    EffectUncertain(SkillTacticEffect),
    Outcome(CommandOutcome, SkillTacticEffect),
}

impl SkillRecoveryCoordinator {
    pub fn new(
        runtime: PageRuntime,
        skills: SkillZigZagZig,
        recovery: RecoveryCoordinator,
        workers: Arc<WorkerPool>,
    ) -> Result<Self, CommandError> {
        Self::with_state_store(
            runtime,
            skills,
            recovery,
            workers,
            Arc::new(SkillStateStore::new()),
        )
    }

    pub fn with_state_store(
        runtime: PageRuntime,
        skills: SkillZigZagZig,
        recovery: RecoveryCoordinator,
        workers: Arc<WorkerPool>,
        skill_state: Arc<SkillStateStore>,
    ) -> Result<Self, CommandError> {
        let initial = skills.session_state().clone();
        match skill_state.insert(initial.clone()) {
            Ok(()) => {}
            Err(SkillStateStoreError::DuplicateSession)
                if skill_state
                    .get(&initial.session_id)
                    .is_ok_and(|stored| stored == initial) => {}
            Err(error) => return Err(skill_state_error(error)),
        }
        Ok(Self {
            runtime,
            skills: Arc::new(Mutex::new(skills)),
            skill_state,
            recovery,
            workers,
            execution_gate: Arc::new(Mutex::new(())),
            stabilization_gate: Arc::new(Mutex::new(())),
            #[cfg(feature = "test-support")]
            preflight_observer: None,
        })
    }

    #[doc(hidden)]
    #[cfg(feature = "test-support")]
    pub fn with_recovery_preflight_observer(
        mut self,
        observer: Arc<dyn RecoveryPreflightObserver>,
    ) -> Self {
        self.preflight_observer = Some(observer);
        self
    }

    pub async fn execute_with_adaptation(
        &self,
        envelope: &CommandEnvelope,
        page: PageState,
    ) -> Result<SkillRecoveryExecution, CommandError> {
        let _execution = self.execution_gate.lock().await;
        if let Some(receipt) = self.replay_recovery_receipt(envelope).await? {
            return Ok(receipt);
        }
        if let Some(reconciled) = self.reconcile_pending_decision(envelope).await? {
            return Ok(reconciled);
        }
        if let Some(receipt) = self.replay_recovery_receipt(envelope).await? {
            return Ok(receipt);
        }
        self.validate_context(envelope, &page).await?;
        let mut tactic_evidence = Vec::new();

        let mut outcome = match self
            .within_total_deadline(envelope, self.runtime.execute(envelope.clone()))
            .await
        {
            Ok(outcome) => outcome,
            Err(error) if envelope.command.class() == CommandClass::Boundary => {
                CommandOutcome::NeedsReconciliation {
                    command_id: envelope.command_id.clone(),
                    error,
                    evidence: Vec::new(),
                }
            }
            Err(error) => CommandOutcome::RetryableFailure {
                command_id: envelope.command_id.clone(),
                error,
            },
        };

        if matches!(outcome, CommandOutcome::Completed { .. }) {
            return Ok(SkillRecoveryExecution {
                command_outcome: outcome,
                skill_outcome: SkillOutcome::applied(Vec::new()).map_err(skill_contract_error)?,
                tactic_evidence,
            });
        }

        loop {
            let failure = failure_from_outcome(&outcome, envelope.command.class());
            let trigger = SkillTrigger::new(failure, expected_postcondition(&envelope.command))
                .map_err(skill_failure_error)?;
            let decision = match self.issue_decision(&trigger, envelope).await {
                Ok(decision) => decision,
                Err(terminal) => {
                    let skill_outcome =
                        SkillOutcome::failed(terminal, evidence_refs(&tactic_evidence)?)
                            .map_err(skill_contract_error)?;
                    let outcome = append_tactic_evidence(outcome, &tactic_evidence)?;
                    self.persist_recovery_outcome(envelope, &outcome).await?;
                    return Ok(SkillRecoveryExecution {
                        command_outcome: outcome,
                        skill_outcome,
                        tactic_evidence,
                    });
                }
            };
            let progress = match self.execute_tactic(&decision, envelope, &page).await {
                Ok(progress) => progress,
                Err(error) => {
                    if is_owned_terminal_error(&error) {
                        if let Some(receipt) = self.replay_recovery_receipt(envelope).await? {
                            return Ok(receipt);
                        }
                    }
                    let tactic_failure = if is_owned_terminal_error(&error) {
                        if envelope.command.class() == CommandClass::Boundary {
                            SkillFailure::EffectUncertain
                        } else {
                            SkillFailure::DeadlineExceeded
                        }
                    } else if is_checkpoint_mismatch_error(&error) {
                        SkillFailure::CheckpointMismatch
                    } else if error.code == ErrorCode::DeadlineExceeded {
                        SkillFailure::DeadlineExceeded
                    } else if decision.tactic == SkillTactic::ChangeInteractionMethod
                        && envelope.command.class() == CommandClass::Boundary
                    {
                        SkillFailure::EffectUncertain
                    } else {
                        failure_from_error(&error)
                    };
                    let evidence =
                        tactic_record(&decision, SkillTacticEffect::ReconciliationRequired);
                    tactic_evidence.push(evidence);
                    let recorded = SkillOutcome::failed(
                        tactic_failure,
                        evidence_refs(tactic_evidence.last())?,
                    )
                    .map_err(skill_contract_error)?;
                    let owned_terminal =
                        is_owned_terminal_error(&error) || is_outbox_pending_error(&error);
                    if is_owned_recovery_tactic(decision.tactic) && !owned_terminal {
                        let skill_outcome =
                            SkillOutcome::failed(tactic_failure, evidence_refs(&tactic_evidence)?)
                                .map_err(skill_contract_error)?;
                        let outcome = append_tactic_evidence(
                            command_failure(envelope, error, tactic_failure),
                            &tactic_evidence,
                        )?;
                        return self
                            .commit_owned_execution(
                                &decision,
                                envelope,
                                &outcome,
                                &skill_outcome,
                                &tactic_evidence,
                            )
                            .await;
                    }
                    if !owned_terminal {
                        self.record_decision(&decision, &recorded).await?;
                    }
                    let skill_outcome =
                        SkillOutcome::failed(tactic_failure, evidence_refs(&tactic_evidence)?)
                            .map_err(skill_contract_error)?;
                    outcome = command_failure(envelope, error, tactic_failure);
                    let outcome = append_tactic_evidence(outcome, &tactic_evidence)?;
                    if !owned_terminal {
                        self.persist_recovery_outcome(envelope, &outcome).await?;
                    }
                    return Ok(SkillRecoveryExecution {
                        command_outcome: outcome,
                        skill_outcome,
                        tactic_evidence,
                    });
                }
            };

            let effect = match &progress {
                TacticProgress::Continue(effect)
                | TacticProgress::Completed(_, effect)
                | TacticProgress::Restarted(_, effect)
                | TacticProgress::EffectUncertain(effect)
                | TacticProgress::Outcome(_, effect) => *effect,
            };
            let evidence = tactic_record(&decision, effect);
            tactic_evidence.push(evidence);
            let recorded =
                SkillOutcome::adapted(decision.tactic, evidence_refs(tactic_evidence.last())?)
                    .map_err(skill_contract_error)?;
            let owned_tactic = is_owned_recovery_tactic(decision.tactic);
            if owned_tactic {
                match &progress {
                    TacticProgress::Completed(observed, _) => {
                        let command_outcome = append_tactic_evidence(
                            CommandOutcome::Completed {
                                command_id: envelope.command_id.clone(),
                                evidence: observed.clone(),
                            },
                            &tactic_evidence,
                        )?;
                        let skill_outcome = SkillOutcome::adapted(
                            decision.tactic,
                            evidence_refs(&tactic_evidence)?,
                        )
                        .map_err(skill_contract_error)?;
                        return self
                            .commit_owned_execution(
                                &decision,
                                envelope,
                                &command_outcome,
                                &skill_outcome,
                                &tactic_evidence,
                            )
                            .await;
                    }
                    TacticProgress::Restarted(restarted, _) => {
                        let command_outcome =
                            append_tactic_evidence(restarted.clone(), &tactic_evidence)?;
                        let skill_outcome = SkillOutcome::adapted(
                            decision.tactic,
                            evidence_refs(&tactic_evidence)?,
                        )
                        .map_err(skill_contract_error)?;
                        return self
                            .commit_owned_execution(
                                &decision,
                                envelope,
                                &command_outcome,
                                &skill_outcome,
                                &tactic_evidence,
                            )
                            .await;
                    }
                    TacticProgress::EffectUncertain(_) => {
                        let command_outcome = append_tactic_evidence(
                            CommandOutcome::NeedsReconciliation {
                                command_id: envelope.command_id.clone(),
                                error: recovery_error(
                                    ErrorCode::VerificationFailed,
                                    "boundary effect remains uncertain after reconciliation",
                                    false,
                                ),
                                evidence: Vec::new(),
                            },
                            &tactic_evidence,
                        )?;
                        let skill_outcome = SkillOutcome::failed(
                            SkillFailure::EffectUncertain,
                            evidence_refs(&tactic_evidence)?,
                        )
                        .map_err(skill_contract_error)?;
                        return self
                            .commit_owned_execution(
                                &decision,
                                envelope,
                                &command_outcome,
                                &skill_outcome,
                                &tactic_evidence,
                            )
                            .await;
                    }
                    _ => {}
                }
            }
            self.record_decision(&decision, &recorded).await?;
            match progress {
                TacticProgress::Continue(_) => {}
                TacticProgress::Completed(observed, _) => {
                    outcome = CommandOutcome::Completed {
                        command_id: envelope.command_id.clone(),
                        evidence: observed,
                    };
                    let command_outcome = append_tactic_evidence(outcome, &tactic_evidence)?;
                    self.persist_recovery_outcome(envelope, &command_outcome)
                        .await?;
                    let skill_outcome =
                        SkillOutcome::adapted(decision.tactic, evidence_refs(&tactic_evidence)?)
                            .map_err(skill_contract_error)?;
                    return Ok(SkillRecoveryExecution {
                        command_outcome,
                        skill_outcome,
                        tactic_evidence,
                    });
                }
                TacticProgress::Restarted(restarted, _) => {
                    let command_outcome = append_tactic_evidence(restarted, &tactic_evidence)?;
                    self.persist_recovery_outcome(envelope, &command_outcome)
                        .await?;
                    let skill_outcome =
                        SkillOutcome::adapted(decision.tactic, evidence_refs(&tactic_evidence)?)
                            .map_err(skill_contract_error)?;
                    return Ok(SkillRecoveryExecution {
                        command_outcome,
                        skill_outcome,
                        tactic_evidence,
                    });
                }
                TacticProgress::EffectUncertain(_) => {
                    let error = recovery_error(
                        ErrorCode::VerificationFailed,
                        "boundary effect remains uncertain after reconciliation",
                        false,
                    );
                    outcome = CommandOutcome::NeedsReconciliation {
                        command_id: envelope.command_id.clone(),
                        error,
                        evidence: Vec::new(),
                    };
                    let command_outcome = append_tactic_evidence(outcome, &tactic_evidence)?;
                    self.persist_recovery_outcome(envelope, &command_outcome)
                        .await?;
                    let skill_outcome = SkillOutcome::failed(
                        SkillFailure::EffectUncertain,
                        evidence_refs(&tactic_evidence)?,
                    )
                    .map_err(skill_contract_error)?;
                    return Ok(SkillRecoveryExecution {
                        command_outcome,
                        skill_outcome,
                        tactic_evidence,
                    });
                }
                TacticProgress::Outcome(next, _) => outcome = next,
            }
        }
    }

    async fn validate_context(
        &self,
        envelope: &CommandEnvelope,
        page: &PageState,
    ) -> Result<(), CommandError> {
        if envelope.page_id.as_ref() != Some(&page.id)
            || envelope.session_id != page.session_id
            || self.runtime.get(&page.id).await.is_err()
        {
            return Err(recovery_error(
                ErrorCode::InvalidRequest,
                "adaptation page does not match the command context",
                false,
            ));
        }
        let session = self.skills.lock().await.session_state().session_id.clone();
        if session != envelope.session_id {
            return Err(recovery_error(
                ErrorCode::InvalidRequest,
                "skill authority does not belong to the command session",
                false,
            ));
        }
        Ok(())
    }

    async fn issue_decision(
        &self,
        trigger: &SkillTrigger,
        envelope: &CommandEnvelope,
    ) -> Result<SkillDecision, SkillFailure> {
        let mut skills = self.skills.lock().await;
        let before = skills.clone();
        let identity = issued_command_identity(envelope)?;
        let decision = skills.next_decision_for_command(trigger, identity, Utc::now())?;
        let issued = skills.session_state().clone();
        let pending = issued
            .pending_issuance
            .as_ref()
            .ok_or(SkillFailure::ConfigurationConflict)?;
        if self
            .recovery
            .persist_skill_issuance(&envelope.workflow_id, pending)
            .await
            .is_err()
        {
            let _ = replace_skill_state(&self.skill_state, before.session_state());
            *skills = before;
            return Err(SkillFailure::ConfigurationConflict);
        }
        if let Err(error) = replace_skill_state(&self.skill_state, &issued) {
            *skills = before;
            return Err(store_failure(error));
        }
        Ok(decision)
    }

    async fn within_total_deadline<F, T>(
        &self,
        envelope: &CommandEnvelope,
        future: F,
    ) -> Result<T, CommandError>
    where
        F: std::future::Future<Output = T>,
    {
        let duration = remaining_duration(envelope.deadline)?;
        tokio::time::timeout(duration, future)
            .await
            .map_err(|_| deadline_error("command exceeded its total deadline"))
    }
}
