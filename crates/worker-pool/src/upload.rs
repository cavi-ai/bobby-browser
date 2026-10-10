//! Shared upload admission and session-owned artifact materialization.
use artifact_store::ArtifactStore;
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    io::Write,
    path::{Path, PathBuf},
};
use types::{CommandError, ErrorCode, ErrorLayer, SessionId};

pub const MAX_UPLOAD_FILES: usize = 32;
pub const MAX_UPLOAD_BYTES: u64 = 256 * 1024 * 1024;

/// Pending artifact files are removed on refusal or cancellation.
/// Random, exclusively-created names prevent overwriting a prior upload or
/// following a planted symlink. Drop removes the task-owned materializations.
pub struct ResolvedUploads {
    pub paths: Vec<PathBuf>,
    _materialized: Vec<MaterializedUpload>,
}

struct MaterializedUpload {
    id: String,
    path: PathBuf,
    bytes: u64,
    _file: tempfile::NamedTempFile,
}

/// Browsers read selected files lazily. Keep immutable, verified copies until
/// worker shutdown, reusing identical artifacts and bounding retained storage
/// by the same file-count and byte limits as upload admission.
#[derive(Default)]
pub struct UploadCache {
    files: HashMap<String, MaterializedUpload>,
    bytes: u64,
}

impl UploadCache {
    pub fn prepare(&self, mut uploads: ResolvedUploads) -> Result<ResolvedUploads, CommandError> {
        let mut pending: HashMap<String, MaterializedUpload> = HashMap::new();
        let mut bytes = self.bytes;
        for upload in uploads._materialized.drain(..) {
            if let Some(existing) = self
                .files
                .get(&upload.id)
                .or_else(|| pending.get(&upload.id))
            {
                for path in &mut uploads.paths {
                    if *path == upload.path {
                        *path = existing.path.clone();
                    }
                }
            } else {
                bytes = bytes.checked_add(upload.bytes).ok_or_else(|| {
                    error(
                        ErrorCode::InvalidRequest,
                        "retained upload byte count overflowed",
                    )
                })?;
                if bytes > MAX_UPLOAD_BYTES || self.files.len() + pending.len() >= MAX_UPLOAD_FILES
                {
                    return Err(error(
                        ErrorCode::InvalidRequest,
                        "session artifact uploads exceed retained file or byte bounds",
                    ));
                }
                pending.insert(upload.id.clone(), upload);
            }
        }
        uploads._materialized = pending.into_values().collect();
        Ok(uploads)
    }

    /// Retain immediately before native file selection, with prepare/commit
    /// serialized by the owning worker. Cancellation or transport failure can
    /// leave the browser holding selected files even without a success reply.
    pub fn commit(&mut self, uploads: ResolvedUploads) {
        for upload in uploads._materialized {
            self.bytes += upload.bytes;
            self.files.insert(upload.id.clone(), upload);
        }
    }

    pub fn clear(&mut self) {
        self.files.clear();
        self.bytes = 0;
    }
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
            result._materialized.push(MaterializedUpload {
                id: id.to_ascii_lowercase(),
                path: path.clone(),
                bytes: bytes.len() as u64,
                _file: file,
            });
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

pub fn opaque_upload_paths(sources: &[String]) -> Vec<String> {
    sources
        .iter()
        .map(|source| types::upload_source_reference(source))
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
    async fn artifact_is_owned_verified_and_retained_until_cache_clear() {
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
        let resolved = resolve_upload_sources(
            std::slice::from_ref(&source),
            &[],
            Some(&store),
            Some(&owner),
            Some(dir.path()),
        )
        .await
        .unwrap();
        let path = resolved.paths[0].clone();
        assert_eq!(std::fs::read(&path).unwrap(), b"payload");
        assert!(!opaque_upload_paths(std::slice::from_ref(&source))[0]
            .contains(dir.path().to_str().unwrap()));
        drop(resolved);
        assert!(!path.exists());

        let mut cache = UploadCache::default();
        let mut retained_path = None;
        for _ in 0..2 {
            let resolved = resolve_upload_sources(
                std::slice::from_ref(&source),
                &[],
                Some(&store),
                Some(&owner),
                Some(dir.path()),
            )
            .await
            .unwrap();
            let fresh_path = resolved.paths[0].clone();
            let prepared = cache.prepare(resolved).unwrap();
            if let Some(path) = &retained_path {
                assert_eq!(&prepared.paths[0], path);
                assert!(
                    !fresh_path.exists(),
                    "duplicate temporary copy must be removed"
                );
            } else {
                retained_path = Some(prepared.paths[0].clone());
            }
            cache.commit(prepared);
            assert_eq!(cache.files.len(), 1);
            assert_eq!(cache.bytes, 7);
        }
        let path = retained_path.unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"payload");
        cache.clear();
        assert!(!path.exists());
        assert_eq!(cache.bytes, 0);
    }

    fn pending_file(directory: &Path, id: String, bytes: u64) -> ResolvedUploads {
        let file = tempfile::NamedTempFile::new_in(directory).unwrap();
        file.as_file().set_len(bytes).unwrap();
        let path = file.path().to_path_buf();
        ResolvedUploads {
            paths: vec![path.clone()],
            _materialized: vec![MaterializedUpload {
                id,
                path,
                bytes,
                _file: file,
            }],
        }
    }

    #[test]
    fn cache_capacity_refusals_remove_pending_files_and_preserve_selected_files() {
        let directory = tempfile::tempdir().unwrap();
        for byte_limit in [false, true] {
            let mut cache = UploadCache::default();
            let count = if byte_limit { 1 } else { MAX_UPLOAD_FILES };
            for index in 0..count {
                let pending = pending_file(
                    directory.path(),
                    index.to_string(),
                    if byte_limit { MAX_UPLOAD_BYTES } else { 1 },
                );
                let prepared = cache.prepare(pending).unwrap();
                cache.commit(prepared);
            }
            let retained: Vec<_> = cache.files.values().map(|file| file.path.clone()).collect();
            let pending = pending_file(directory.path(), "overflow".into(), 1);
            let refused_path = pending.paths[0].clone();
            assert_eq!(
                cache.prepare(pending).err().unwrap().code,
                ErrorCode::InvalidRequest
            );
            assert!(!refused_path.exists());
            assert!(retained.iter().all(|path| path.exists()));
            cache.clear();
            assert!(retained.iter().all(|path| !path.exists()));
        }
    }

    #[test]
    fn abandoned_admission_drops_pending_copy_without_retaining_it() {
        let directory = tempfile::tempdir().unwrap();
        let cache = UploadCache::default();
        let prepared = cache
            .prepare(pending_file(directory.path(), "pending".into(), 1))
            .unwrap();
        let path = prepared.paths[0].clone();
        drop(prepared);
        assert!(!path.exists());
        assert!(cache.files.is_empty());
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
