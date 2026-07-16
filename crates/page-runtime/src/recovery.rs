use checkpoint_store::{CheckpointStore, CheckpointStoreError};
use chrono::Utc;
use std::sync::Arc;
use thiserror::Error;
use types::{
    AttemptId, CheckpointInvariant, CommandClass, CommandError, Evidence, InspectCommand,
    NavigateCommand, RecoveryDecision, RecoveryRecord, RestartLineage, WaitUntil,
    WorkflowCheckpoint, WorkflowId,
};
use worker_pool::WorkerPool;

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
}

#[derive(Clone)]
pub struct RecoveryCoordinator {
    store: CheckpointStore,
    workers: Option<Arc<WorkerPool>>,
}

impl RecoveryCoordinator {
    pub fn new(store: CheckpointStore) -> Self {
        Self {
            store,
            workers: None,
        }
    }

    pub fn with_workers(store: CheckpointStore, workers: Arc<WorkerPool>) -> Self {
        Self {
            store,
            workers: Some(workers),
        }
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

    pub async fn recover(
        &self,
        workflow_id: &WorkflowId,
    ) -> Result<RecoveryDecision, RecoveryError> {
        let mut checkpoint = self.store.load(workflow_id).await?;
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
            lease
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
            }
        };
        checkpoint.recovery_history.push(RecoveryRecord {
            recorded_at: Utc::now(),
            decision: decision.clone(),
        });
        self.store.save(&checkpoint).await?;
        Ok(decision)
    }
}

fn browser_error(error: CommandError) -> RecoveryError {
    RecoveryError::Browser(error.message)
}
