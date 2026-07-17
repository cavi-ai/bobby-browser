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
        if bytes.len() > self.max_bytes {
            return Err(ArtifactError::TooLarge);
        }
        let (width, height) = png_dimensions(bytes)?;
        if width > self.max_dimension || height > self.max_dimension {
            return Err(ArtifactError::TooLarge);
        }
        let artifact_id = Uuid::new_v4().to_string();
        let directory = self.session_dir(session_id);
        tokio::fs::create_dir_all(&directory)
            .await
            .map_err(storage_error)?;
        let final_path = directory.join(format!("{artifact_id}.png"));
        let temporary_path = directory.join(format!(".{artifact_id}.tmp"));
        tokio::fs::write(&temporary_path, bytes)
            .await
            .map_err(storage_error)?;
        tokio::fs::rename(&temporary_path, &final_path)
            .await
            .map_err(storage_error)?;
        Ok(ArtifactRecord {
            artifact_id,
            page_id: page_id.clone(),
            media_type: "image/png".into(),
            width,
            height,
            bytes: bytes.len() as u64,
            sha256: format!("{:x}", Sha256::digest(bytes)),
        })
    }

    pub async fn get(
        &self,
        session_id: &SessionId,
        artifact_id: &str,
    ) -> Result<Vec<u8>, ArtifactError> {
        let artifact_id = Uuid::parse_str(artifact_id).map_err(|_| ArtifactError::NotFound)?;
        let path = self
            .session_dir(session_id)
            .join(format!("{artifact_id}.png"));
        tokio::fs::read(path).await.map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ArtifactError::NotFound
            } else {
                storage_error(error)
            }
        })
    }

    fn session_dir(&self, session_id: &SessionId) -> PathBuf {
        self.root.join(session_id.0.to_string())
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
