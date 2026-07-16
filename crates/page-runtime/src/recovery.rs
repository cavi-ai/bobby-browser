use checkpoint_store::{CheckpointStore, CheckpointStoreError};
use thiserror::Error;
use types::{CheckpointInvariant, Evidence, WorkflowCheckpoint};

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
}

#[derive(Clone)]
pub struct RecoveryCoordinator {
    store: CheckpointStore,
}

impl RecoveryCoordinator {
    pub fn new(store: CheckpointStore) -> Self {
        Self { store }
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
}
