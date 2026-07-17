use std::path::{Path, PathBuf};

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
            .put(
                session_id,
                page_id,
                "image/png",
                "png",
                bytes,
                self.max_bytes,
            )
            .await?;
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
        if bytes.len() > self.max_bytes.min(max_bytes) {
            return Err(ArtifactError::TooLarge);
        }
        if media_type.is_empty() || media_type.len() > 255 || !valid_extension(extension) {
            return Err(ArtifactError::InvalidMetadata);
        }

        let artifact_id = Uuid::new_v4().to_string();
        let directory = self.session_dir(session_id);
        let filename = format!("{artifact_id}.{extension}");
        let sha256 = format!("{:x}", Sha256::digest(bytes));
        let manifest = ArtifactManifest {
            filename: filename.clone(),
            media_type: media_type.to_owned(),
            page_id: page_id.clone(),
            bytes: bytes.len() as u64,
            sha256: sha256.clone(),
        };
        let manifest_bytes = serde_json::to_vec(&manifest).map_err(storage_error)?;

        tokio::fs::create_dir_all(&directory)
            .await
            .map_err(storage_error)?;
        let final_path = directory.join(&filename);
        let manifest_path = directory.join(format!("{artifact_id}.json"));
        let temporary_path = directory.join(format!(".{artifact_id}.{extension}.tmp"));
        let temporary_manifest_path = directory.join(format!(".{artifact_id}.json.tmp"));

        if let Err(error) = tokio::fs::write(&temporary_path, bytes).await {
            return Err(storage_error(error));
        }
        if let Err(error) = tokio::fs::write(&temporary_manifest_path, manifest_bytes).await {
            let _ = tokio::fs::remove_file(&temporary_path).await;
            return Err(storage_error(error));
        }
        if let Err(error) = tokio::fs::rename(&temporary_path, &final_path).await {
            let _ = tokio::fs::remove_file(&temporary_path).await;
            let _ = tokio::fs::remove_file(&temporary_manifest_path).await;
            return Err(storage_error(error));
        }
        if let Err(error) = tokio::fs::rename(&temporary_manifest_path, &manifest_path).await {
            let _ = tokio::fs::remove_file(&final_path).await;
            let _ = tokio::fs::remove_file(&temporary_manifest_path).await;
            return Err(storage_error(error));
        }

        Ok(ArtifactRecord {
            artifact_id,
            page_id: page_id.clone(),
            media_type: media_type.to_owned(),
            width: 0,
            height: 0,
            bytes: bytes.len() as u64,
            sha256,
        })
    }

    pub async fn get(
        &self,
        session_id: &SessionId,
        artifact_id: &str,
    ) -> Result<Vec<u8>, ArtifactError> {
        let artifact_id = Uuid::parse_str(artifact_id).map_err(|_| ArtifactError::NotFound)?;
        let directory = self.session_dir(session_id);
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
        .join(format!("{artifact_id}.png"))
}
