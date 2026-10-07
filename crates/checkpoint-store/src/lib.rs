use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::{Mutex, OwnedMutexGuard};
use types::{SkillIssuedDecision, WorkflowCheckpoint, WorkflowId, MAX_RECOVERY_RECEIPTS};
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
    #[error("checkpoint workflow cannot be rebound to another session")]
    IdentityChanged,
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
    session_index: Arc<Mutex<SessionIndex>>,
}

/// Rebuildable hints only: listing always reads and validates selected files.
/// Retaining identities rather than checkpoint bodies keeps hot listings cheap
/// even for sessions exceeding the former 256-checkpoint cache limit.
#[derive(Default)]
struct SessionIndex {
    modified: Option<SystemTime>,
    sessions: HashMap<types::SessionId, Vec<CheckpointRef>>,
}

#[derive(Clone)]
struct CheckpointRef {
    workflow_id: WorkflowId,
    created_at: chrono::DateTime<chrono::Utc>,
    path: PathBuf,
}

impl SessionIndex {
    fn update(&mut self, checkpoint: &WorkflowCheckpoint, path: PathBuf) {
        let entries = self
            .sessions
            .entry(checkpoint.session_id.clone())
            .or_default();
        entries.retain(|entry| entry.workflow_id != checkpoint.workflow_id);
        entries.push(CheckpointRef {
            workflow_id: checkpoint.workflow_id.clone(),
            created_at: checkpoint.created_at,
            path,
        });
        entries.sort_by_key(|entry| std::cmp::Reverse(entry.created_at));
    }
}

impl CheckpointStore {
    pub async fn open(root: impl AsRef<Path>) -> Result<Self, CheckpointStoreError> {
        let root = root.as_ref().to_path_buf();
        tokio::fs::create_dir_all(&root).await?;
        Ok(Self {
            root: Arc::new(root),
            workflow_locks: Arc::default(),
            session_index: Arc::default(),
        })
    }

    pub async fn save(&self, checkpoint: &WorkflowCheckpoint) -> Result<(), CheckpointStoreError> {
        self.validate_schema(checkpoint)?;
        let lock = self.workflow_lock(&checkpoint.workflow_id).await;
        let _guard = lock.lock().await;
        match self.read_bytes(&checkpoint.workflow_id).await {
            Ok(bytes) => {
                let existing: WorkflowCheckpoint = serde_json::from_slice(&bytes)?;
                self.validate_schema(&existing)?;
                if existing.session_id != checkpoint.session_id {
                    return Err(CheckpointStoreError::IdentityChanged);
                }
            }
            Err(CheckpointStoreError::NotFound(_)) => {}
            Err(error) => return Err(error),
        }
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
        let before = tokio::fs::metadata(self.root.as_path()).await?.modified()?;
        let destination = self.path(&checkpoint.workflow_id);
        let temporary = self.root.join(format!(
            ".{}.{}.tmp",
            checkpoint.workflow_id.0,
            Uuid::new_v4()
        ));
        let result = async {
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            // Checkpoints carry workflow/session state; match the authority
            // store's owner-only permissions instead of the process umask.
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temporary).await?;
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
            let mut index = self.session_index.lock().await;
            if index.modified == Some(before) {
                index.update(checkpoint, self.path(&checkpoint.workflow_id));
                index.modified = Some(tokio::fs::metadata(self.root.as_path()).await?.modified()?);
            } else {
                index.modified = None;
            }
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

    /// Checkpoints belonging to `session_id`, newest first, capped at `limit`.
    ///
    /// The store is one file per workflow with no index, so an agent that lost
    /// its `workflowId` -- compacted, restarted -- had no way back to a
    /// recoverable workflow at all. This is that way back.
    ///
    /// Filtering is the caller's to finish: a checkpoint records its
    /// `session_id` but no principal, so ownership has to be enforced above
    /// this, against the session-ownership registry.
    ///
    /// Unreadable and stale-schema entries are skipped rather than failing the
    /// listing -- one corrupt file must not hide every other recoverable
    /// workflow.
    pub async fn list_for_session(
        &self,
        session_id: &types::SessionId,
        limit: usize,
    ) -> Result<Vec<WorkflowCheckpoint>, CheckpointStoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }
        let before = tokio::fs::metadata(self.root.as_path()).await?.modified()?;
        let candidates = {
            let mut index = self.session_index.lock().await;
            if index.modified != Some(before) {
                let mut rebuilt = SessionIndex::default();
                let mut entries = tokio::fs::read_dir(self.root.as_path()).await?;
                while let Some(entry) = entries.next_entry().await? {
                    let name = entry.file_name();
                    let Some(name) = name.to_str() else {
                        continue;
                    };
                    if !name.ends_with(".json") || name.ends_with(".skill-issuance.json") {
                        continue;
                    }
                    let Ok(bytes) = tokio::fs::read(entry.path()).await else {
                        continue;
                    };
                    let Ok(checkpoint) = serde_json::from_slice::<WorkflowCheckpoint>(&bytes)
                    else {
                        continue;
                    };
                    if self.validate_schema(&checkpoint).is_ok() {
                        rebuilt
                            .sessions
                            .entry(checkpoint.session_id)
                            .or_default()
                            .push(CheckpointRef {
                                workflow_id: checkpoint.workflow_id,
                                created_at: checkpoint.created_at,
                                path: entry.path(),
                            });
                    }
                }
                for entries in rebuilt.sessions.values_mut() {
                    entries.sort_by_key(|entry| std::cmp::Reverse(entry.created_at));
                }
                let after = tokio::fs::metadata(self.root.as_path()).await?.modified()?;
                rebuilt.modified = (before == after).then_some(after);
                *index = rebuilt;
            }
            index.sessions.get(session_id).cloned().unwrap_or_default()
        };
        let mut found = Vec::new();
        for candidate in candidates {
            let Ok(bytes) = tokio::fs::read(&candidate.path).await else {
                continue;
            };
            let Ok(checkpoint) = serde_json::from_slice::<WorkflowCheckpoint>(&bytes) else {
                continue;
            };
            if checkpoint.session_id == *session_id
                && checkpoint.workflow_id == candidate.workflow_id
                && self.validate_schema(&checkpoint).is_ok()
            {
                found.push(checkpoint);
                if found.len() == limit {
                    break;
                }
            }
        }
        found.sort_by_key(|entry| std::cmp::Reverse(entry.created_at));
        Ok(found)
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
        let before = tokio::fs::metadata(self.root.as_path()).await?.modified()?;
        match tokio::fs::remove_file(self.path(workflow_id)).await {
            Ok(()) => {
                File::open(self.root.as_ref()).await?.sync_all().await?;
                let mut index = self.session_index.lock().await;
                if index.modified == Some(before) {
                    for entries in index.sessions.values_mut() {
                        entries.retain(|entry| &entry.workflow_id != workflow_id);
                    }
                    index.sessions.retain(|_, entries| !entries.is_empty());
                    index.modified =
                        Some(tokio::fs::metadata(self.root.as_path()).await?.modified()?);
                } else {
                    index.modified = None;
                }
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn save_skill_issuance(
        &self,
        workflow_id: &WorkflowId,
        issuance: &SkillIssuedDecision,
    ) -> Result<(), CheckpointStoreError> {
        let lock = self.workflow_lock(workflow_id).await;
        let _guard = lock.lock().await;
        let destination = self.issuance_path(workflow_id);
        let temporary = self.root.join(format!(
            ".{}.{}.issuance.tmp",
            workflow_id.0,
            Uuid::new_v4()
        ));
        let result = async {
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&temporary)
                .await?;
            file.write_all(&serde_json::to_vec(issuance)?).await?;
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
        result
    }

    pub async fn load_skill_issuance(
        &self,
        workflow_id: &WorkflowId,
    ) -> Result<Option<SkillIssuedDecision>, CheckpointStoreError> {
        let lock = self.workflow_lock(workflow_id).await;
        let _guard = lock.lock().await;
        match tokio::fs::read(self.issuance_path(workflow_id)).await {
            Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error.into()),
        }
    }

    pub async fn remove_skill_issuance(
        &self,
        workflow_id: &WorkflowId,
    ) -> Result<(), CheckpointStoreError> {
        let lock = self.workflow_lock(workflow_id).await;
        let _guard = lock.lock().await;
        match tokio::fs::remove_file(self.issuance_path(workflow_id)).await {
            Ok(()) => File::open(self.root.as_ref())
                .await?
                .sync_all()
                .await
                .map_err(Into::into),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    fn path(&self, workflow_id: &WorkflowId) -> PathBuf {
        checkpoint_path(&self.root, workflow_id)
    }

    fn issuance_path(&self, workflow_id: &WorkflowId) -> PathBuf {
        self.root
            .join(format!("{}.skill-issuance.json", workflow_id.0))
    }

    fn validate_schema(&self, checkpoint: &WorkflowCheckpoint) -> Result<(), CheckpointStoreError> {
        if checkpoint.schema_version != WorkflowCheckpoint::SCHEMA_VERSION {
            return Err(CheckpointStoreError::UnsupportedSchema {
                actual: checkpoint.schema_version,
                expected: WorkflowCheckpoint::SCHEMA_VERSION,
            });
        }
        if checkpoint.recovery_receipts.len() > MAX_RECOVERY_RECEIPTS {
            return Err(CheckpointStoreError::Serialization(serde_json::Error::io(
                std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "checkpoint recovery receipts exceed their bound",
                ),
            )));
        }
        for receipt in &checkpoint.recovery_receipts {
            receipt.validate().map_err(|message| {
                CheckpointStoreError::Serialization(serde_json::Error::io(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    message,
                )))
            })?;
        }
        Ok(())
    }

    async fn workflow_lock(&self, workflow_id: &WorkflowId) -> Arc<Mutex<()>> {
        let mut locks = self.workflow_locks.lock().await;
        // Amortized pruning: an entry whose only reference is the map itself
        // has no operation holding or waiting on it, so it can never be
        // contended again -- without this the map gains one entry per
        // workflow id for the life of the process.
        locks.retain(|_, lock| Arc::strong_count(lock) > 1);
        locks.entry(workflow_id.clone()).or_default().clone()
    }
}

fn checkpoint_digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn checkpoint_authority_digest(
    checkpoint: &WorkflowCheckpoint,
) -> Result<String, CheckpointStoreError> {
    let mut authority = checkpoint.clone();
    authority.recovery_history.clear();
    authority.recovery_receipts.clear();
    Ok(checkpoint_digest(&serde_json::to_vec(&authority)?))
}

/// Where a store rooted at `root` keeps `workflow_id`'s checkpoint, for
/// read-only exporters that must not open the store.
pub fn checkpoint_path(root: &Path, workflow_id: &WorkflowId) -> PathBuf {
    root.join(format!("{}.json", workflow_id.0))
}
