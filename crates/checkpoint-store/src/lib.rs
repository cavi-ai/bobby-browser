use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, OwnedMutexGuard};
use types::{WorkflowCheckpoint, WorkflowId};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum CheckpointStoreError {
    #[error("checkpoint I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("checkpoint serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("checkpoint for workflow {0:?} was not found")]
    NotFound(WorkflowId),
    #[error("unsupported checkpoint schema {actual}; expected {expected}")]
    UnsupportedSchema { actual: u16, expected: u16 },
    #[error("checkpoint changed after the recovery snapshot was verified")]
    SnapshotChanged,
}

pub struct LockedCheckpointSnapshot {
    store: CheckpointStore,
    checkpoint: WorkflowCheckpoint,
    authority_digest: String,
    content_digest: String,
    _guard: OwnedMutexGuard<()>,
}

impl LockedCheckpointSnapshot {
    pub fn checkpoint(&self) -> &WorkflowCheckpoint {
        &self.checkpoint
    }

    pub fn digest(&self) -> &str {
        &self.authority_digest
    }

    pub fn content_digest(&self) -> &str {
        &self.content_digest
    }

    pub async fn verify_unchanged(&self) -> Result<(), CheckpointStoreError> {
        let bytes = self.store.read_bytes(&self.checkpoint.workflow_id).await?;
        if checkpoint_digest(&bytes) == self.content_digest {
            Ok(())
        } else {
            Err(CheckpointStoreError::SnapshotChanged)
        }
    }

    pub async fn save_if_unchanged(
        &mut self,
        checkpoint: &WorkflowCheckpoint,
    ) -> Result<(), CheckpointStoreError> {
        self.verify_unchanged().await?;
        if checkpoint.workflow_id != self.checkpoint.workflow_id {
            return Err(CheckpointStoreError::SnapshotChanged);
        }
        self.store.validate_schema(checkpoint)?;
        self.store.write_unlocked(checkpoint).await?;
        let bytes = serde_json::to_vec(checkpoint)?;
        self.checkpoint = checkpoint.clone();
        self.authority_digest = checkpoint_authority_digest(checkpoint)?;
        self.content_digest = checkpoint_digest(&bytes);
        Ok(())
    }
}

#[derive(Clone)]
pub struct CheckpointStore {
    root: Arc<PathBuf>,
    workflow_locks: Arc<Mutex<HashMap<WorkflowId, Arc<Mutex<()>>>>>,
}

impl CheckpointStore {
    pub async fn open(root: impl AsRef<Path>) -> Result<Self, CheckpointStoreError> {
        let root = root.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&root).await?;
        Ok(Self {
            root: Arc::new(root),
            workflow_locks: Arc::default(),
        })
    }

    pub async fn save(&self, checkpoint: &WorkflowCheckpoint) -> Result<(), CheckpointStoreError> {
        self.validate_schema(checkpoint)?;
        let lock = self.workflow_lock(&checkpoint.workflow_id).await;
        let _guard = lock.lock().await;
        self.write_unlocked(checkpoint).await
    }

    pub async fn lock_snapshot(
        &self,
        workflow_id: &WorkflowId,
    ) -> Result<LockedCheckpointSnapshot, CheckpointStoreError> {
        let lock = self.workflow_lock(workflow_id).await;
        let guard = lock.lock_owned().await;
        let bytes = self.read_bytes(workflow_id).await?;
        let checkpoint: WorkflowCheckpoint = serde_json::from_slice(&bytes)?;
        self.validate_schema(&checkpoint)?;
        Ok(LockedCheckpointSnapshot {
            store: self.clone(),
            authority_digest: checkpoint_authority_digest(&checkpoint)?,
            checkpoint,
            content_digest: checkpoint_digest(&bytes),
            _guard: guard,
        })
    }

    async fn write_unlocked(
        &self,
        checkpoint: &WorkflowCheckpoint,
    ) -> Result<(), CheckpointStoreError> {
        let destination = self.path(&checkpoint.workflow_id);
        let temporary = self.root.join(format!(
            ".{}.{}.tmp",
            checkpoint.workflow_id.0,
            Uuid::new_v4()
        ));
        let result = async {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .await?;
            file.write_all(&serde_json::to_vec(checkpoint)?).await?;
            file.sync_all().await?;
            drop(file);
            tokio::fs::rename(&temporary, destination).await?;
            File::open(self.root.as_ref()).await?.sync_all().await?;
            Ok::<_, CheckpointStoreError>(())
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temporary).await;
        }
        if result.is_ok() {
            tracing::info!(workflow_id = %checkpoint.workflow_id.0, "checkpoint.established");
        }
        result
    }

    pub async fn load(
        &self,
        workflow_id: &WorkflowId,
    ) -> Result<WorkflowCheckpoint, CheckpointStoreError> {
        let bytes = self.read_bytes(workflow_id).await?;
        let checkpoint: WorkflowCheckpoint = serde_json::from_slice(&bytes)?;
        self.validate_schema(&checkpoint)?;
        Ok(checkpoint)
    }

    async fn read_bytes(&self, workflow_id: &WorkflowId) -> Result<Vec<u8>, CheckpointStoreError> {
        tokio::fs::read(self.path(workflow_id))
            .await
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => CheckpointStoreError::NotFound(workflow_id.clone()),
                _ => error.into(),
            })
    }

    pub async fn remove(&self, workflow_id: &WorkflowId) -> Result<(), CheckpointStoreError> {
        let lock = self.workflow_lock(workflow_id).await;
        let _guard = lock.lock().await;
        match tokio::fs::remove_file(self.path(workflow_id)).await {
            Ok(()) => {
                File::open(self.root.as_ref()).await?.sync_all().await?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn path(&self, workflow_id: &WorkflowId) -> PathBuf {
        self.root.join(format!("{}.json", workflow_id.0))
    }

    fn validate_schema(&self, checkpoint: &WorkflowCheckpoint) -> Result<(), CheckpointStoreError> {
        if checkpoint.schema_version != WorkflowCheckpoint::SCHEMA_VERSION {
            return Err(CheckpointStoreError::UnsupportedSchema {
                actual: checkpoint.schema_version,
                expected: WorkflowCheckpoint::SCHEMA_VERSION,
            });
        }
        Ok(())
    }

    async fn workflow_lock(&self, workflow_id: &WorkflowId) -> Arc<Mutex<()>> {
        self.workflow_locks
            .lock()
            .await
            .entry(workflow_id.clone())
            .or_default()
            .clone()
    }
}

fn checkpoint_digest(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

fn checkpoint_authority_digest(
    checkpoint: &WorkflowCheckpoint,
) -> Result<String, CheckpointStoreError> {
    let mut authority = checkpoint.clone();
    authority.recovery_history.clear();
    authority.recovery_receipts.clear();
    Ok(checkpoint_digest(&serde_json::to_vec(&authority)?))
}
