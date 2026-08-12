use checkpoint_store::{CheckpointStore, CheckpointStoreError, LockedCheckpointSnapshot};
use chrono::Utc;
use std::sync::Arc;
use thiserror::Error;
use types::{
    AttemptId, CheckpointInvariant, CommandClass, CommandError, CommandId, Evidence,
    InspectCommand, NavigateCommand, RecoveryCommandIdentity, RecoveryDecision, RecoveryReceipt,
    RecoveryReceiptState, RecoveryRecord, RestartLineage, SessionId, SkillIssuedDecision,
    WaitUntil, WorkflowCheckpoint, WorkflowId,
};
use worker_pool::{WorkerLease, WorkerPool};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InvariantEvaluation {
    failures: Vec<String>,
}

impl InvariantEvaluation {
    pub fn is_match(&self) -> bool {
        self.failures.is_empty()
    }

    pub fn failures(&self) -> &[String] {
        &self.failures
    }
}

pub fn evaluate_invariants(
    invariants: &[CheckpointInvariant],
    evidence: &[Evidence],
) -> InvariantEvaluation {
    let mut failures = Vec::new();
    for invariant in invariants {
        let matched = match invariant {
            CheckpointInvariant::Url { value } => evidence.iter().any(|item| {
                matches!(item, Evidence::Navigation { url, .. } | Evidence::Inspection { url, .. } if url == value)
            }),
            CheckpointInvariant::Title { value } => evidence.iter().any(|item| {
                matches!(item, Evidence::Navigation { title, .. } | Evidence::Inspection { title, .. } if title == value)
            }),
            CheckpointInvariant::Text { selector, value } => evidence.iter().any(|item| match item {
                Evidence::Element { selector: actual, text } => {
                    actual == selector && text.as_deref() == Some(value.as_str())
                }
                Evidence::Inspection {
                    selector: actual,
                    text,
                    ..
                } => actual.as_deref() == Some(selector.as_str()) && text == value,
                _ => false,
            }),
        };
        if !matched {
            failures.push(match invariant {
                CheckpointInvariant::Url { value } => {
                    format!("URL invariant not observed: {value}")
                }
                CheckpointInvariant::Title { value } => {
                    format!("title invariant not observed: {value}")
                }
                CheckpointInvariant::Text { selector, value } => {
                    format!("text invariant not observed for {selector}: {value}")
                }
            });
        }
    }
    InvariantEvaluation { failures }
}

#[derive(Debug, Error)]
pub enum RecoveryError {
    #[error("checkpoint invariants failed: {0}")]
    InvariantMismatch(String),
    #[error(transparent)]
    Store(#[from] CheckpointStoreError),
    #[error("browser workers are not configured for recovery")]
    WorkersUnavailable,
    #[error("browser recovery failed: {0}")]
    Browser(String),
    #[error("checkpoint session does not match the authorized recovery session")]
    SessionMismatch,
    /// The journal has no record of this command, or the command never
    /// reached a terminal outcome (`Completed` / `NeedsReconciliation`).
    /// Distinct from `WorkersUnavailable`: this is not a configuration
    /// problem, it means the caller named work the runtime never finished —
    /// exactly the case `evidence_for_command` must fail closed on rather
    /// than return an empty (and easily mistaken for "no evidence exists")
    /// vector.
    #[error("command {0:?} has no recorded terminal outcome in the journal")]
    CommandOutcomeMissing(CommandId),
}

#[derive(Clone)]
pub struct RecoveryCoordinator {
    store: CheckpointStore,
    workers: Option<Arc<WorkerPool>>,
    operational_metrics: Option<observability::OperationalMetrics>,
}

pub struct VerifiedRecoveryCheckpoint {
    snapshot: LockedCheckpointSnapshot,
}

pub(crate) struct PreparedRecovery {
    checkpoint: WorkflowCheckpoint,
    lease: WorkerLease,
}

impl VerifiedRecoveryCheckpoint {
    pub fn checkpoint(&self) -> &WorkflowCheckpoint {
        self.snapshot.checkpoint()
    }

    pub fn digest(&self) -> &str {
        self.snapshot.digest()
    }

    pub async fn verify_unchanged(&self) -> Result<(), RecoveryError> {
        self.snapshot.verify_unchanged().await.map_err(Into::into)
    }
}

impl RecoveryCoordinator {
    pub async fn persist_skill_issuance(
        &self,
        workflow_id: &WorkflowId,
        issuance: &SkillIssuedDecision,
    ) -> Result<(), RecoveryError> {
        let identity = issuance.command_identity.as_ref().ok_or_else(|| {
            RecoveryError::InvariantMismatch("skill issuance lacks command identity".into())
        })?;
        if &identity.workflow_id != workflow_id || identity.session_id != issuance.session_id {
            return Err(RecoveryError::InvariantMismatch(
                "skill issuance authority does not match checkpoint workflow".into(),
            ));
        }
        self.store
            .save_skill_issuance(workflow_id, issuance)
            .await?;
        Ok(())
    }

    pub async fn load_skill_issuance(
        &self,
        workflow_id: &WorkflowId,
    ) -> Result<Option<SkillIssuedDecision>, RecoveryError> {
        Ok(self.store.load_skill_issuance(workflow_id).await?)
    }

    pub async fn clear_skill_issuance(
        &self,
        workflow_id: &WorkflowId,
    ) -> Result<(), RecoveryError> {
        self.store.remove_skill_issuance(workflow_id).await?;
        Ok(())
    }

    pub fn new(store: CheckpointStore) -> Self {
        Self {
            store,
            workers: None,
            operational_metrics: None,
        }
    }

    pub fn with_workers(store: CheckpointStore, workers: Arc<WorkerPool>) -> Self {
        Self {
            store,
            workers: Some(workers),
            operational_metrics: None,
        }
    }

    pub fn with_operational_metrics(mut self, metrics: observability::OperationalMetrics) -> Self {
        self.operational_metrics = Some(metrics);
        self
    }

    pub async fn save_verified(
        &self,
        mut checkpoint: WorkflowCheckpoint,
        evidence: Vec<Evidence>,
    ) -> Result<WorkflowCheckpoint, RecoveryError> {
        let evaluation = evaluate_invariants(&checkpoint.invariants, &evidence);
        if !evaluation.is_match() {
            return Err(RecoveryError::InvariantMismatch(
                evaluation.failures.join("; "),
            ));
        }
        checkpoint.evidence = evidence;
        self.store.save(&checkpoint).await?;
        Ok(checkpoint)
    }

    pub async fn load_checkpoint(
        &self,
        workflow_id: &WorkflowId,
    ) -> Result<WorkflowCheckpoint, RecoveryError> {
        self.store.load(workflow_id).await.map_err(Into::into)
    }

    pub async fn lock_verified_checkpoint(
        &self,
        workflow_id: &WorkflowId,
    ) -> Result<VerifiedRecoveryCheckpoint, RecoveryError> {
        let snapshot = self.store.lock_snapshot(workflow_id).await?;
        ensure_persisted_checkpoint_verified(snapshot.checkpoint())?;
        Ok(VerifiedRecoveryCheckpoint { snapshot })
    }

    pub async fn recover(
        &self,
        workflow_id: &WorkflowId,
    ) -> Result<RecoveryDecision, RecoveryError> {
        let mut checkpoint = self.lock_verified_checkpoint(workflow_id).await?;
        self.recover_locked(&mut checkpoint, true).await
    }

    pub async fn checkpoint_session(
        &self,
        workflow_id: &WorkflowId,
    ) -> Result<SessionId, RecoveryError> {
        Ok(self.load_checkpoint(workflow_id).await?.session_id)
    }

    /// Recoverable workflows for a session, newest first, capped at `limit`.
    ///
    /// The entry point for an agent that lost its `workflowId`.
    pub async fn workflows_for_session(
        &self,
        session_id: &SessionId,
        limit: usize,
    ) -> Result<Vec<WorkflowId>, RecoveryError> {
        Ok(self
            .store
            .list_for_session(session_id, limit)
            .await
            .map_err(|_| RecoveryError::WorkersUnavailable)?
            .into_iter()
            .map(|checkpoint| checkpoint.workflow_id)
            .collect())
    }

    pub async fn recover_for_session(
        &self,
        workflow_id: &WorkflowId,
        session_id: &SessionId,
    ) -> Result<RecoveryDecision, RecoveryError> {
        let mut checkpoint = self.lock_verified_checkpoint(workflow_id).await?;
        if &checkpoint.checkpoint().session_id != session_id {
            return Err(RecoveryError::SessionMismatch);
        }
        self.recover_locked(&mut checkpoint, true).await
    }

    pub async fn recover_after_replacement(
        &self,
        workflow_id: &WorkflowId,
    ) -> Result<RecoveryDecision, RecoveryError> {
        let mut checkpoint = self.lock_verified_checkpoint(workflow_id).await?;
        self.recover_locked(&mut checkpoint, false).await
    }

    pub async fn recover_locked(
        &self,
        verified: &mut VerifiedRecoveryCheckpoint,
        replace_session: bool,
    ) -> Result<RecoveryDecision, RecoveryError> {
        let result = async {
            verified.verify_unchanged().await?;
            self.stabilize_recovery_pool(&verified.checkpoint().session_id, replace_session)
                .await?;
            let prepared = self
                .reattach_recovery_pool(verified.checkpoint().clone())
                .await?;
            self.complete_prepared_recovery(verified, prepared).await
        }
        .await;
        if result.is_err() {
            if let Some(metrics) = &self.operational_metrics {
                metrics.record_reconciliation(observability::ReconciliationMetricOutcome::Failed);
            }
        }
        result
    }

    pub(crate) async fn stabilize_recovery_pool(
        &self,
        session_id: &SessionId,
        replace_session: bool,
    ) -> Result<(), RecoveryError> {
        let workers = self
            .workers
            .as_ref()
            .ok_or(RecoveryError::WorkersUnavailable)?;
        if replace_session {
            workers
                .invalidate_session(session_id)
                .await
                .map_err(browser_error)?;
        }
        Ok(())
    }

    pub(crate) async fn reattach_recovery_pool(
        &self,
        checkpoint: WorkflowCheckpoint,
    ) -> Result<PreparedRecovery, RecoveryError> {
        let workers = self
            .workers
            .as_ref()
            .ok_or(RecoveryError::WorkersUnavailable)?;
        let lease = workers
            .lease(checkpoint.session_id.clone())
            .await
            .map_err(browser_error)?;
        lease
            .worker()
            .open_page(checkpoint.page_id.clone())
            .await
            .map_err(browser_error)?;
        Ok(PreparedRecovery { checkpoint, lease })
    }

    pub(crate) async fn complete_prepared_recovery(
        &self,
        verified: &mut VerifiedRecoveryCheckpoint,
        prepared: PreparedRecovery,
    ) -> Result<RecoveryDecision, RecoveryError> {
        verified.verify_unchanged().await?;
        let PreparedRecovery {
            mut checkpoint,
            lease,
        } = prepared;
        let mut evidence = lease
            .worker()
            .navigate(
                &checkpoint.page_id,
                &NavigateCommand {
                    url: checkpoint.current_url.clone(),
                    wait_until: WaitUntil::Interactive,
                    timeout_ms: 30_000,
                },
            )
            .await
            .map_err(browser_error)?;
        evidence.extend(
            lease
                .worker()
                .inspect(&checkpoint.page_id, &InspectCommand::default())
                .await
                .map_err(browser_error)?,
        );
        for selector in checkpoint.invariants.iter().filter_map(|item| match item {
            CheckpointInvariant::Text { selector, .. } => Some(selector),
            _ => None,
        }) {
            evidence.extend(
                lease
                    .worker()
                    .inspect(
                        &checkpoint.page_id,
                        &InspectCommand {
                            selector: Some(selector.clone()),
                            target: None,
                            include_html: false,
                        },
                    )
                    .await
                    .map_err(browser_error)?,
            );
        }

        let evaluation = evaluate_invariants(&checkpoint.invariants, &evidence);
        let decision = if evaluation.is_match() {
            RecoveryDecision::Resumed {
                checkpoint_id: checkpoint.checkpoint_id.clone(),
                attempt_id: checkpoint.attempt_id.clone(),
                evidence,
            }
        } else if checkpoint.recovery_class == CommandClass::Boundary {
            RecoveryDecision::NeedsReconciliation {
                checkpoint_id: checkpoint.checkpoint_id.clone(),
                attempt_id: checkpoint.attempt_id.clone(),
                reason: evaluation.failures.join("; "),
                evidence,
            }
        } else {
            let reason = evaluation.failures.join("; ");
            let restart_evidence = lease
                .worker()
                .navigate(
                    &checkpoint.page_id,
                    &NavigateCommand {
                        url: checkpoint.restart_url.clone(),
                        wait_until: WaitUntil::Interactive,
                        timeout_ms: 30_000,
                    },
                )
                .await
                .map_err(browser_error)?;
            RecoveryDecision::Restarted {
                checkpoint_id: checkpoint.checkpoint_id.clone(),
                lineage: RestartLineage {
                    workflow_id: checkpoint.workflow_id.clone(),
                    abandoned_attempt_id: checkpoint.attempt_id.clone(),
                    attempt_id: AttemptId::new(),
                    reason,
                },
                evidence: restart_evidence,
            }
        };
        checkpoint.recovery_history.push(RecoveryRecord {
            recorded_at: Utc::now(),
            decision: decision.clone(),
        });
        verified.snapshot.save_if_unchanged(&checkpoint).await?;
        tracing::info!(
            outcome = match &decision {
                RecoveryDecision::Resumed { .. } => "resumed",
                RecoveryDecision::NeedsReconciliation { .. } => "needs_reconciliation",
                RecoveryDecision::Restarted { .. } => "restarted",
            },
            "checkpoint.reconciled"
        );
        if let Some(metrics) = &self.operational_metrics {
            metrics.record_reconciliation(match &decision {
                RecoveryDecision::Resumed { .. } => {
                    observability::ReconciliationMetricOutcome::Resumed
                }
                RecoveryDecision::NeedsReconciliation { .. } => {
                    observability::ReconciliationMetricOutcome::NeedsReconciliation
                }
                RecoveryDecision::Restarted { .. } => {
                    observability::ReconciliationMetricOutcome::Restarted
                }
            });
        }
        Ok(decision)
    }

    pub async fn restart_from_verified_boundary(
        &self,
        workflow_id: &WorkflowId,
    ) -> Result<RecoveryDecision, RecoveryError> {
        let mut checkpoint = self.lock_verified_checkpoint(workflow_id).await?;
        self.restart_from_locked_boundary(&mut checkpoint).await
    }

    pub async fn restart_from_locked_boundary(
        &self,
        verified: &mut VerifiedRecoveryCheckpoint,
    ) -> Result<RecoveryDecision, RecoveryError> {
        verified.verify_unchanged().await?;
        let mut checkpoint = verified.checkpoint().clone();
        let workers = self
            .workers
            .as_ref()
            .ok_or(RecoveryError::WorkersUnavailable)?;
        workers
            .invalidate_session(&checkpoint.session_id)
            .await
            .map_err(browser_error)?;
        let lease = workers
            .lease(checkpoint.session_id.clone())
            .await
            .map_err(browser_error)?;
        lease
            .worker()
            .open_page(checkpoint.page_id.clone())
            .await
            .map_err(browser_error)?;
        let evidence = lease
            .worker()
            .navigate(
                &checkpoint.page_id,
                &NavigateCommand {
                    url: checkpoint.restart_url.clone(),
                    wait_until: WaitUntil::Interactive,
                    timeout_ms: 30_000,
                },
            )
            .await
            .map_err(browser_error)?;
        let decision = RecoveryDecision::Restarted {
            checkpoint_id: checkpoint.checkpoint_id.clone(),
            lineage: RestartLineage {
                workflow_id: checkpoint.workflow_id.clone(),
                abandoned_attempt_id: checkpoint.attempt_id.clone(),
                attempt_id: AttemptId::new(),
                reason: "restarted from reviewed durable boundary".into(),
            },
            evidence,
        };
        checkpoint.recovery_history.push(RecoveryRecord {
            recorded_at: Utc::now(),
            decision: decision.clone(),
        });
        verified.snapshot.save_if_unchanged(&checkpoint).await?;

        Ok(decision)
    }

    pub async fn prepare_restart_from_locked_boundary(
        &self,
        verified: &VerifiedRecoveryCheckpoint,
    ) -> Result<RecoveryDecision, RecoveryError> {
        verified.verify_unchanged().await?;
        self.prepare_restart_pool(verified.checkpoint().clone())
            .await
    }

    pub(crate) async fn prepare_restart_pool(
        &self,
        checkpoint: WorkflowCheckpoint,
    ) -> Result<RecoveryDecision, RecoveryError> {
        self.stabilize_recovery_pool(&checkpoint.session_id, true)
            .await?;
        self.prepare_restart_after_stabilization(checkpoint).await
    }

    pub(crate) async fn prepare_restart_after_stabilization(
        &self,
        checkpoint: WorkflowCheckpoint,
    ) -> Result<RecoveryDecision, RecoveryError> {
        let prepared = self.reattach_recovery_pool(checkpoint.clone()).await?;
        drop(prepared);
        Ok(RecoveryDecision::Restarted {
            checkpoint_id: checkpoint.checkpoint_id.clone(),
            lineage: RestartLineage {
                workflow_id: checkpoint.workflow_id.clone(),
                abandoned_attempt_id: checkpoint.attempt_id.clone(),
                attempt_id: AttemptId::new(),
                reason: "restarted from reviewed durable boundary".into(),
            },
            evidence: Vec::new(),
        })
    }

    pub async fn record_locked_decision(
        &self,
        verified: &mut VerifiedRecoveryCheckpoint,
        decision: &RecoveryDecision,
    ) -> Result<(), RecoveryError> {
        let mut checkpoint = verified.checkpoint().clone();
        let decision_checkpoint_id = match decision {
            RecoveryDecision::Resumed { checkpoint_id, .. }
            | RecoveryDecision::NeedsReconciliation { checkpoint_id, .. }
            | RecoveryDecision::Restarted { checkpoint_id, .. } => checkpoint_id,
        };
        if decision_checkpoint_id != &checkpoint.checkpoint_id {
            return Err(RecoveryError::InvariantMismatch(
                "recovery decision checkpoint changed before persistence".into(),
            ));
        }
        checkpoint.recovery_history.push(RecoveryRecord {
            recorded_at: Utc::now(),
            decision: decision.clone(),
        });
        verified.snapshot.save_if_unchanged(&checkpoint).await?;
        Ok(())
    }

    pub async fn persist_recovery_receipt(
        &self,
        receipt: RecoveryReceipt,
    ) -> Result<(), RecoveryError> {
        receipt
            .validate()
            .map_err(RecoveryError::InvariantMismatch)?;
        let mut snapshot = self
            .store
            .lock_snapshot(&receipt.identity.workflow_id)
            .await?;
        let mut checkpoint = snapshot.checkpoint().clone();
        if let Some(existing) = checkpoint
            .recovery_receipts
            .iter()
            .find(|existing| existing.identity == receipt.identity)
        {
            if existing == &receipt {
                return Ok(());
            }
            return Err(RecoveryError::InvariantMismatch(
                "recovery receipt payload is immutable".into(),
            ));
        } else {
            if receipt.state != RecoveryReceiptState::Unresolved {
                return Err(RecoveryError::InvariantMismatch(
                    "recovery receipt must start unresolved".into(),
                ));
            }
            checkpoint.recovery_receipts.push(receipt);
        }
        snapshot.save_if_unchanged(&checkpoint).await?;
        Ok(())
    }

    pub async fn transition_recovery_receipt(
        &self,
        identity: &RecoveryCommandIdentity,
        state: RecoveryReceiptState,
    ) -> Result<(), RecoveryError> {
        let mut snapshot = self.store.lock_snapshot(&identity.workflow_id).await?;
        let mut checkpoint = snapshot.checkpoint().clone();
        let receipt = checkpoint
            .recovery_receipts
            .iter_mut()
            .find(|receipt| &receipt.identity == identity)
            .ok_or_else(|| {
                RecoveryError::InvariantMismatch("recovery receipt disappeared".into())
            })?;
        receipt
            .validate()
            .map_err(RecoveryError::InvariantMismatch)?;
        let allowed = matches!(
            (receipt.state, state),
            (
                RecoveryReceiptState::Unresolved,
                RecoveryReceiptState::PendingJournal
            ) | (
                RecoveryReceiptState::PendingJournal,
                RecoveryReceiptState::Committed
            )
        );
        if receipt.state == state {
            return Ok(());
        }
        if !allowed {
            return Err(RecoveryError::InvariantMismatch(
                "invalid recovery receipt state transition".into(),
            ));
        }
        receipt.state = state;
        snapshot.save_if_unchanged(&checkpoint).await?;
        Ok(())
    }
}

fn ensure_persisted_checkpoint_verified(
    checkpoint: &WorkflowCheckpoint,
) -> Result<(), RecoveryError> {
    let evaluation = evaluate_invariants(&checkpoint.invariants, &checkpoint.evidence);
    if evaluation.is_match() {
        Ok(())
    } else {
        Err(RecoveryError::InvariantMismatch(
            evaluation.failures.join("; "),
        ))
    }
}

fn browser_error(error: CommandError) -> RecoveryError {
    RecoveryError::Browser(error.message)
}
