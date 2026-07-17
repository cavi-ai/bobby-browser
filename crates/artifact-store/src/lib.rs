use std::{
    future::Future,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use types::{PageId, SessionId};
use uuid::Uuid;

#[derive(Debug, Clone)]
pub struct ArtifactStore {
    root: PathBuf,
    max_bytes: usize,
    max_dimension: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ArtifactRecord {
    pub artifact_id: String,
    pub page_id: PageId,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u64,
    pub sha256: String,
}

#[derive(Debug)]
pub struct PendingArtifact {
    record: Option<ArtifactRecord>,
    path: Option<PathBuf>,
}

impl PendingArtifact {
    pub fn record(&self) -> &ArtifactRecord {
        self.record.as_ref().expect("pending artifact is armed")
    }

    pub fn commit(mut self) -> ArtifactRecord {
        self.path = None;
        self.record.take().expect("pending artifact is armed")
    }
}

impl Drop for PendingArtifact {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        if let Err(error) = std::fs::remove_dir_all(path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%error, "failed to clean pending artifact");
            }
        }
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ArtifactError {
    #[error("artifact was not found for this session")]
    NotFound,
    #[error("artifact exceeds the configured byte limit")]
    TooLarge,
    #[error("artifact is not a valid PNG")]
    InvalidPng,
    #[error("artifact metadata is invalid")]
    InvalidMetadata,
    #[error("artifact storage failed: {0}")]
    Storage(String),
}

impl ArtifactStore {
    pub fn new(root: impl Into<PathBuf>, max_bytes: usize, max_dimension: u32) -> Self {
        Self {
            root: root.into(),
            max_bytes,
            max_dimension,
        }
    }

    pub async fn put_png(
        &self,
        session_id: &SessionId,
        page_id: &PageId,
        bytes: &[u8],
    ) -> Result<ArtifactRecord, ArtifactError> {
        let (width, height) = png_dimensions(bytes)?;
        if width > self.max_dimension || height > self.max_dimension {
            return Err(ArtifactError::TooLarge);
        }
        let mut record = self
            .put_pending_with_before_publish(
                session_id,
                page_id,
                "image/png",
                "png",
                bytes,
                self.max_bytes,
                false,
                std::future::ready(()),
            )
            .await?
            .commit();
        record.width = width;
        record.height = height;
        Ok(record)
    }

    pub async fn put(
        &self,
        session_id: &SessionId,
        page_id: &PageId,
        media_type: &str,
        extension: &str,
        bytes: &[u8],
        max_bytes: usize,
    ) -> Result<ArtifactRecord, ArtifactError> {
        Ok(self
            .put_pending_with_before_publish(
                session_id,
                page_id,
                media_type,
                extension,
                bytes,
                max_bytes,
                true,
                std::future::ready(()),
            )
            .await?
            .commit())
    }

    pub async fn put_pending(
        &self,
        session_id: &SessionId,
        page_id: &PageId,
        media_type: &str,
        extension: &str,
        bytes: &[u8],
        max_bytes: usize,
    ) -> Result<PendingArtifact, ArtifactError> {
        self.put_pending_addressed(
            session_id, page_id, media_type, extension, bytes, max_bytes, true,
        )
        .await
    }

    async fn put_pending_addressed(
        &self,
        session_id: &SessionId,
        page_id: &PageId,
        media_type: &str,
        extension: &str,
        bytes: &[u8],
        max_bytes: usize,
        content_addressed: bool,
    ) -> Result<PendingArtifact, ArtifactError> {
        self.put_pending_with_before_publish(
            session_id,
            page_id,
            media_type,
            extension,
            bytes,
            max_bytes,
            content_addressed,
            std::future::ready(()),
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    async fn put_pending_with_before_publish<F>(
        &self,
        session_id: &SessionId,
        page_id: &PageId,
        media_type: &str,
        extension: &str,
        bytes: &[u8],
        max_bytes: usize,
        content_addressed: bool,
        before_publish: F,
    ) -> Result<PendingArtifact, ArtifactError>
    where
        F: Future<Output = ()>,
    {
        if bytes.len() > self.max_bytes.min(max_bytes) {
            return Err(ArtifactError::TooLarge);
        }
        if media_type.is_empty() || media_type.len() > 255 || !valid_extension(extension) {
            return Err(ArtifactError::InvalidMetadata);
        }

        let sha256 = format!("{:x}", Sha256::digest(bytes));
        let artifact_id = if content_addressed {
            sha256.clone()
        } else {
            Uuid::new_v4().to_string()
        };
        let session_directory = self.session_dir(session_id);
        let final_path = session_directory.join(&artifact_id);
        let filename = format!("{artifact_id}.{extension}");
        let manifest = ArtifactManifest {
            filename: filename.clone(),
            media_type: media_type.to_owned(),
            page_id: page_id.clone(),
            bytes: bytes.len() as u64,
            sha256: sha256.clone(),
        };
        let manifest_bytes = serde_json::to_vec(&manifest).map_err(storage_error)?;

        tokio::fs::create_dir_all(&session_directory)
            .await
            .map_err(storage_error)?;
        if final_path.exists() {
            let manifest_path = final_path.join(format!("{artifact_id}.json"));
            let existing = std::fs::read(manifest_path).map_err(storage_error)?;
            let existing: ArtifactManifest =
                serde_json::from_slice(&existing).map_err(storage_error)?;
            if existing.media_type != media_type
                || existing.filename != filename
                || existing.sha256 != sha256
                || existing.bytes != bytes.len() as u64
                || existing.page_id != *page_id
            {
                return Err(ArtifactError::InvalidMetadata);
            }
            return Ok(PendingArtifact {
                record: Some(ArtifactRecord {
                    artifact_id,
                    page_id: page_id.clone(),
                    media_type: media_type.to_owned(),
                    width: 0,
                    height: 0,
                    bytes: bytes.len() as u64,
                    sha256,
                }),
                path: None,
            });
        }
        let staging_path = session_directory.join(format!(".{artifact_id}.tmp"));
        std::fs::create_dir(&staging_path).map_err(storage_error)?;
        let mut staging = StagingGuard::new(staging_path);

        tokio::fs::write(staging.path().join(&filename), bytes)
            .await
            .map_err(storage_error)?;
        tokio::fs::write(
            staging.path().join(format!("{artifact_id}.json")),
            manifest_bytes,
        )
        .await
        .map_err(storage_error)?;

        before_publish.await;
        std::fs::rename(staging.path(), &final_path).map_err(storage_error)?;
        staging.disarm();

        Ok(PendingArtifact {
            record: Some(ArtifactRecord {
                artifact_id,
                page_id: page_id.clone(),
                media_type: media_type.to_owned(),
                width: 0,
                height: 0,
                bytes: bytes.len() as u64,
                sha256,
            }),
            path: Some(final_path),
        })
    }

    #[cfg(test)]
    #[allow(clippy::too_many_arguments)]
    async fn put_paused_before_publish(
        &self,
        session_id: &SessionId,
        page_id: &PageId,
        media_type: &str,
        extension: &str,
        bytes: &[u8],
        max_bytes: usize,
        staged: std::sync::Arc<tokio::sync::Notify>,
        publish: std::sync::Arc<tokio::sync::Notify>,
    ) -> Result<ArtifactRecord, ArtifactError> {
        Ok(self
            .put_pending_with_before_publish(
                session_id,
                page_id,
                media_type,
                extension,
                bytes,
                max_bytes,
                false,
                async move {
                    staged.notify_one();
                    publish.notified().await;
                },
            )
            .await?
            .commit())
    }

    pub async fn get(
        &self,
        session_id: &SessionId,
        artifact_id: &str,
    ) -> Result<Vec<u8>, ArtifactError> {
        if Uuid::parse_str(artifact_id).is_err()
            && (artifact_id.len() != 64 || !artifact_id.bytes().all(|b| b.is_ascii_hexdigit()))
        {
            return Err(ArtifactError::NotFound);
        }
        let directory = self.session_dir(session_id).join(artifact_id);
        let manifest_path = directory.join(format!("{artifact_id}.json"));
        let manifest_bytes = tokio::fs::read(manifest_path).await.map_err(read_error)?;
        let manifest: ArtifactManifest =
            serde_json::from_slice(&manifest_bytes).map_err(|_| ArtifactError::NotFound)?;
        let expected_prefix = format!("{artifact_id}.");
        let extension = manifest
            .filename
            .strip_prefix(&expected_prefix)
            .filter(|extension| valid_extension(extension))
            .ok_or(ArtifactError::NotFound)?;
        let path = directory.join(format!("{artifact_id}.{extension}"));
        tokio::fs::read(path).await.map_err(read_error)
    }

    fn session_dir(&self, session_id: &SessionId) -> PathBuf {
        self.root.join(session_id.0.to_string())
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArtifactManifest {
    filename: String,
    media_type: String,
    page_id: PageId,
    bytes: u64,
    sha256: String,
}

#[derive(Debug)]
struct StagingGuard {
    path: Option<PathBuf>,
}

impl StagingGuard {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    fn path(&self) -> &Path {
        self.path.as_deref().expect("staging guard is armed")
    }

    fn disarm(&mut self) {
        self.path = None;
    }
}

impl Drop for StagingGuard {
    fn drop(&mut self) {
        let Some(path) = self.path.take() else {
            return;
        };
        if let Err(error) = std::fs::remove_dir_all(path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%error, "failed to clean artifact staging directory");
            }
        }
    }
}

fn valid_extension(extension: &str) -> bool {
    !extension.is_empty()
        && extension.len() <= 10
        && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn read_error(error: std::io::Error) -> ArtifactError {
    match error.kind() {
        std::io::ErrorKind::NotFound => ArtifactError::NotFound,
        _ => storage_error(error),
    }
}

fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32), ArtifactError> {
    const SIGNATURE: &[u8; 8] = b"\x89PNG\r\n\x1a\n";
    if bytes.len() < 24 || &bytes[..8] != SIGNATURE || &bytes[12..16] != b"IHDR" {
        return Err(ArtifactError::InvalidPng);
    }
    let width = u32::from_be_bytes(bytes[16..20].try_into().expect("checked PNG header"));
    let height = u32::from_be_bytes(bytes[20..24].try_into().expect("checked PNG header"));
    if width == 0 || height == 0 {
        return Err(ArtifactError::InvalidPng);
    }
    Ok((width, height))
}

fn storage_error(error: impl std::fmt::Display) -> ArtifactError {
    ArtifactError::Storage(error.to_string())
}

pub fn artifact_path(root: &Path, session_id: &SessionId, artifact_id: &str) -> PathBuf {
    root.join(session_id.0.to_string())
        .join(artifact_id)
        .join(format!("{artifact_id}.png"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use tokio::sync::Notify;

    use super::*;

    #[tokio::test]
    async fn abort_before_publish_removes_staging_without_visible_artifact() {
        let root = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(root.path(), 1024, 4096);
        let session = SessionId::new();
        let session_dir = store.session_dir(&session);
        let page = PageId::new();
        let staged = Arc::new(Notify::new());
        let publish = Arc::new(Notify::new());

        let task = tokio::spawn({
            let store = store.clone();
            let session = session.clone();
            let staged = Arc::clone(&staged);
            let publish = Arc::clone(&publish);
            async move {
                store
                    .put_paused_before_publish(
                        &session,
                        &page,
                        "application/octet-stream",
                        "bin",
                        b"partial-transfer",
                        1024,
                        staged,
                        publish,
                    )
                    .await
            }
        });

        staged.notified().await;
        assert_eq!(std::fs::read_dir(&session_dir).unwrap().count(), 1);
        task.abort();
        assert!(task.await.unwrap_err().is_cancelled());
        assert!(std::fs::read_dir(&session_dir).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn publish_boundary_replaces_staging_with_one_committed_directory() {
        let root = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(root.path(), 1024, 4096);
        let session = SessionId::new();
        let session_dir = store.session_dir(&session);
        let page = PageId::new();
        let staged = Arc::new(Notify::new());
        let publish = Arc::new(Notify::new());

        let task = tokio::spawn({
            let store = store.clone();
            let session = session.clone();
            let staged = Arc::clone(&staged);
            let publish = Arc::clone(&publish);
            async move {
                store
                    .put_paused_before_publish(
                        &session,
                        &page,
                        "application/octet-stream",
                        "bin",
                        b"complete-transfer",
                        1024,
                        staged,
                        publish,
                    )
                    .await
            }
        });

        staged.notified().await;
        let staged_name = std::fs::read_dir(&session_dir)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name();
        assert!(staged_name.to_string_lossy().starts_with('.'));

        publish.notify_one();
        let record = task.await.unwrap().unwrap();
        let entries = std::fs::read_dir(&session_dir)
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].file_name(), record.artifact_id.as_str());
        assert!(entries[0].path().is_dir());
    }

    #[tokio::test]
    async fn dropping_staging_guard_removes_directory_synchronously() {
        let root = tempfile::tempdir().unwrap();
        let staging_path = root.path().join(".artifact.tmp");
        std::fs::create_dir(&staging_path).unwrap();
        std::fs::write(staging_path.join("payload.bin"), b"partial").unwrap();

        let guard = StagingGuard::new(staging_path.clone());
        drop(guard);

        assert!(!staging_path.exists());
    }
}
