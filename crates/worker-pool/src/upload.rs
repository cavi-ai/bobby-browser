//! Shared upload admission and session-owned artifact materialization.
use artifact_store::ArtifactStore;
use sha2::{Digest, Sha256};
use std::{
    io::Write,
    path::{Path, PathBuf},
};
use types::{CommandError, ErrorCode, ErrorLayer, SessionId};

pub const MAX_UPLOAD_FILES: usize = 32;
pub const MAX_UPLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// Temporary artifact files stay alive until the browser has selected them.
/// Random, exclusively-created names prevent overwriting a prior upload or
/// following a planted symlink. Drop removes the task-owned materializations.
pub struct ResolvedUploads {
    pub paths: Vec<PathBuf>,
    _materialized: Vec<tempfile::NamedTempFile>,
}

fn error(code: ErrorCode, message: impl Into<String>) -> CommandError {
    CommandError {
        code,
        message: message.into(),
        layer: ErrorLayer::Driver,
        retryable: false,
    }
}

pub async fn resolve_upload_sources(
    sources: &[String],
    roots: &[PathBuf],
    artifacts: Option<&ArtifactStore>,
    session: Option<&SessionId>,
    download_dir: Option<&Path>,
) -> Result<ResolvedUploads, CommandError> {
    if sources.is_empty() || sources.len() > MAX_UPLOAD_FILES {
        return Err(error(
            ErrorCode::InvalidRequest,
            format!("upload requires 1..={MAX_UPLOAD_FILES} files"),
        ));
    }
    let mut result = ResolvedUploads {
        paths: Vec::with_capacity(sources.len()),
        _materialized: Vec::new(),
    };
    let mut total_bytes = 0_u64;
    for source in sources {
        let path = if let Some(id) = source.strip_prefix("artifact://") {
            if id.len() != 64 || !id.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(error(
                    ErrorCode::PolicyDenied,
                    "upload artifact reference is malformed",
                ));
            }
            let session = session.ok_or_else(|| {
                error(
                    ErrorCode::PolicyDenied,
                    "upload artifact has no owning runtime session",
                )
            })?;
            let store = artifacts.ok_or_else(|| {
                error(
                    ErrorCode::PolicyDenied,
                    "upload artifact store is not configured",
                )
            })?;
            let bytes = store
                .get(session, id)
                .await
                .map_err(|_| error(ErrorCode::PolicyDenied, "upload artifact is unavailable"))?;
            if hex::encode(Sha256::digest(&bytes)) != id.to_ascii_lowercase() {
                return Err(error(
                    ErrorCode::PolicyDenied,
                    "upload artifact digest verification failed",
                ));
            }
            let directory = download_dir
                .ok_or_else(|| {
                    error(
                        ErrorCode::PolicyDenied,
                        "upload artifact materialization is not configured",
                    )
                })?
                .join("upload-artifacts");
            std::fs::create_dir_all(&directory).map_err(|_| {
                error(
                    ErrorCode::PolicyDenied,
                    "upload artifact materialization failed",
                )
            })?;
            let mut file = tempfile::Builder::new()
                .prefix("upload-")
                .suffix(".bin")
                .tempfile_in(directory)
                .map_err(|_| {
                    error(
                        ErrorCode::PolicyDenied,
                        "upload artifact materialization failed",
                    )
                })?;
            file.write_all(&bytes).map_err(|_| {
                error(
                    ErrorCode::PolicyDenied,
                    "upload artifact materialization failed",
                )
            })?;
            let path = std::fs::canonicalize(file.path()).map_err(|_| {
                error(
                    ErrorCode::PolicyDenied,
                    "upload artifact materialization failed",
                )
            })?;
            result._materialized.push(file);
            path
        } else {
            crate::resolve_upload_paths(roots, &[PathBuf::from(source)])?.remove(0)
        };
        total_bytes = total_bytes
            .checked_add(
                std::fs::metadata(&path)
                    .map_err(|_| {
                        error(
                            ErrorCode::PolicyDenied,
                            "approved upload file is unavailable",
                        )
                    })?
                    .len(),
            )
            .ok_or_else(|| error(ErrorCode::InvalidRequest, "upload byte count overflowed"))?;
        if total_bytes > MAX_UPLOAD_BYTES {
            return Err(error(
                ErrorCode::InvalidRequest,
                format!("upload exceeds the {MAX_UPLOAD_BYTES} byte bound"),
            ));
        }
        result.paths.push(path);
    }
    Ok(result)
}

pub fn opaque_upload_paths(paths: &[PathBuf]) -> Vec<String> {
    paths
        .iter()
        .map(|path| {
            format!(
                "upload://sha256/{}",
                hex::encode(Sha256::digest(path.as_os_str().as_encoded_bytes()))
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn count_and_total_bytes_are_bounded_before_browser_io() {
        for sources in [vec![], vec!["missing".into(); 33]] {
            assert_eq!(
                resolve_upload_sources(&sources, &[], None, None, None)
                    .await
                    .err()
                    .unwrap()
                    .code,
                ErrorCode::InvalidRequest
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("large");
        std::fs::File::create(&path)
            .unwrap()
            .set_len(MAX_UPLOAD_BYTES + 1)
            .unwrap();
        let error = resolve_upload_sources(
            &[path.display().to_string()],
            &[dir.path().into()],
            None,
            None,
            None,
        )
        .await
        .err()
        .unwrap();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
    }
    #[tokio::test]
    async fn artifact_is_owned_verified_and_removed_after_selection() {
        let dir = tempfile::tempdir().unwrap();
        let store = ArtifactStore::new(dir.path().join("artifacts"), 1024, 1024);
        let owner = SessionId::new();
        let record = store
            .put(
                &owner,
                &types::PageId::new(),
                "application/octet-stream",
                "bin",
                b"payload",
                1024,
            )
            .await
            .unwrap();
        let source = format!("artifact://{}", record.artifact_id);
        let wrong_owner = SessionId::new();
        assert_eq!(
            resolve_upload_sources(
                std::slice::from_ref(&source),
                &[],
                Some(&store),
                Some(&wrong_owner),
                Some(dir.path())
            )
            .await
            .err()
            .unwrap()
            .code,
            ErrorCode::PolicyDenied
        );
        let resolved =
            resolve_upload_sources(&[source], &[], Some(&store), Some(&owner), Some(dir.path()))
                .await
                .unwrap();
        let path = resolved.paths[0].clone();
        assert_eq!(std::fs::read(&path).unwrap(), b"payload");
        assert!(!opaque_upload_paths(&resolved.paths)[0].contains(dir.path().to_str().unwrap()));
        drop(resolved);
        assert!(!path.exists());
    }
    #[tokio::test]
    async fn malformed_unowned_and_outside_root_sources_fail_closed() {
        for source in [
            "artifact://../secret",
            &format!("artifact://{}", "a".repeat(64)),
        ] {
            assert_eq!(
                resolve_upload_sources(&[source.into()], &[], None, None, None)
                    .await
                    .err()
                    .unwrap()
                    .code,
                ErrorCode::PolicyDenied
            );
        }
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("outside");
        std::fs::write(&file, "private").unwrap();
        assert_eq!(
            resolve_upload_sources(&[file.display().to_string()], &[], None, None, None)
                .await
                .err()
                .unwrap()
                .code,
            ErrorCode::PolicyDenied
        );
    }
}
