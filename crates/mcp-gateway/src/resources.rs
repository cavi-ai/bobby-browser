use std::{collections::BTreeMap, fmt, path::PathBuf, sync::Arc};

use artifact_store::{ArtifactRecord, ArtifactStore};
use interface_core::{
    ArtifactContent, ArtifactReader, ArtifactReference, CapabilityHandle, InterfaceResult,
};
use sha2::Digest as _;
use tokio::sync::RwLock;
use types::{
    CommandEnvelope, CommandOutcome, ErrorLayer, Evidence, InterfaceError, InterfaceErrorCode,
    RequestContext, SessionId,
};

#[derive(Clone)]
pub struct ArtifactResources {
    reader: Option<ArtifactReader>,
    entries: Arc<RwLock<BTreeMap<String, (SessionId, ArtifactReference)>>>,
    max_entries: usize,
    artifact_store: Option<ArtifactStore>,
    downloads_root: Option<PathBuf>,
    max_download_bytes: usize,
}

impl Default for ArtifactResources {
    fn default() -> Self {
        Self {
            reader: None,
            entries: Arc::new(RwLock::new(BTreeMap::new())),
            max_entries: 1,
            artifact_store: None,
            downloads_root: None,
            max_download_bytes: 1,
        }
    }
}

impl ArtifactResources {
    pub fn new(reader: ArtifactReader, max_entries: usize) -> Self {
        assert!(max_entries > 0, "artifact resource bound must be positive");
        Self {
            reader: Some(reader),
            entries: Arc::new(RwLock::new(BTreeMap::new())),
            max_entries,
            artifact_store: None,
            downloads_root: None,
            max_download_bytes: 1,
        }
    }

    pub fn production(
        reader: ArtifactReader,
        artifact_store: ArtifactStore,
        downloads_root: impl Into<PathBuf>,
        max_download_bytes: usize,
        max_entries: usize,
    ) -> Self {
        assert!(
            max_download_bytes > 0,
            "download import bound must be positive"
        );
        let mut resources = Self::new(reader, max_entries);
        resources.artifact_store = Some(artifact_store);
        resources.downloads_root = Some(downloads_root.into());
        resources.max_download_bytes = max_download_bytes;
        resources
    }

    async fn register_trusted(
        &self,
        session_id: SessionId,
        reference: ArtifactReference,
    ) -> Result<(), ArtifactCatalogFull> {
        let artifact_id = reference.artifact_id().to_owned();
        let mut entries = self.entries.write().await;
        if !entries.contains_key(&artifact_id) && entries.len() >= self.max_entries {
            return Err(ArtifactCatalogFull);
        }
        entries.insert(artifact_id, (session_id, reference));
        Ok(())
    }

    /// Admits runtime artifact evidence only after `ArtifactReader` independently verifies the
    /// committed artifact and issues an opaque reference. Caller-provided MCP arguments never
    /// provide a path, ownership assertion, or `ArtifactRecord`.
    pub(crate) async fn register_outcome(
        &self,
        handle: &CapabilityHandle,
        context: &RequestContext,
        envelope: &CommandEnvelope,
        outcome: &CommandOutcome,
    ) -> ArtifactAdmission {
        let mut admission = ArtifactAdmission::default();
        let evidence = match outcome {
            CommandOutcome::Completed { evidence, .. }
            | CommandOutcome::NeedsReconciliation { evidence, .. }
            | CommandOutcome::Failed { evidence, .. } => evidence,
            _ => return admission,
        };
        for item in evidence {
            let (kind, result) = match item {
                Evidence::Screenshot {
                    artifact_id,
                    media_type,
                    width,
                    height,
                    bytes,
                    sha256,
                } => {
                    admission.attempted += 1;
                    let result = envelope
                        .page_id
                        .clone()
                        .ok_or_else(|| resource_error(context, InterfaceErrorCode::InvalidRequest));
                    let result = match result {
                        Ok(page_id) => {
                            let record = ArtifactRecord {
                                artifact_id: artifact_id.clone(),
                                page_id,
                                media_type: media_type.clone(),
                                width: *width,
                                height: *height,
                                bytes: *bytes,
                                sha256: sha256.clone(),
                            };
                            self.admit_record(handle, context, envelope, &record).await
                        }
                        Err(error) => Err(error),
                    };
                    ("screenshot", result)
                }
                Evidence::Download {
                    filename,
                    path,
                    bytes,
                    sha256,
                } => {
                    admission.attempted += 1;
                    admission.download_uris.insert(sha256.clone(), None);
                    let result = self
                        .download_record(context, envelope, filename, path, *bytes, sha256)
                        .await;
                    let result = match result {
                        Ok(record) => self.admit_record(handle, context, envelope, &record).await,
                        Err(error) => Err(error),
                    };
                    ("download", result)
                }
                _ => continue,
            };
            match result {
                Ok(artifact_id) => {
                    admission.admitted += 1;
                    if let Evidence::Download { sha256, .. } = item {
                        admission
                            .download_uris
                            .insert(sha256.clone(), Some(format!("artifact://{artifact_id}")));
                    }
                }
                Err(error) => admission.failures.push(ArtifactAdmissionFailure {
                    kind,
                    code: error.code,
                }),
            }
        }
        admission
    }

    async fn admit_record(
        &self,
        handle: &CapabilityHandle,
        context: &RequestContext,
        envelope: &CommandEnvelope,
        record: &ArtifactRecord,
    ) -> InterfaceResult<String> {
        let reader = self
            .reader
            .as_ref()
            .ok_or_else(|| resource_error(context, InterfaceErrorCode::ArtifactDenied))?;
        let reference = reader
            .register(handle, context, &envelope.session_id, record)
            .await?;
        let artifact_id = reference.artifact_id().to_owned();
        self.register_trusted(envelope.session_id.clone(), reference)
            .await
            .map_err(|_| resource_error(context, InterfaceErrorCode::ResourceExhausted))?;
        Ok(artifact_id)
    }

    async fn download_record(
        &self,
        context: &RequestContext,
        envelope: &CommandEnvelope,
        filename: &str,
        path: &str,
        expected_bytes: u64,
        expected_sha256: &str,
    ) -> InterfaceResult<ArtifactRecord> {
        let page_id = envelope
            .page_id
            .clone()
            .ok_or_else(|| resource_error(context, InterfaceErrorCode::InvalidRequest))?;
        if path == expected_sha256 && valid_sha256(path) {
            return Ok(ArtifactRecord {
                artifact_id: path.to_owned(),
                page_id,
                media_type: "application/octet-stream".to_owned(),
                width: 0,
                height: 0,
                bytes: expected_bytes,
                sha256: expected_sha256.to_owned(),
            });
        }
        let store = self
            .artifact_store
            .as_ref()
            .ok_or_else(|| resource_error(context, InterfaceErrorCode::ArtifactDenied))?;
        let downloads_root = self
            .downloads_root
            .as_ref()
            .ok_or_else(|| resource_error(context, InterfaceErrorCode::ArtifactDenied))?;
        let session_root = downloads_root.join(envelope.session_id.0.to_string());
        let canonical_root = tokio::fs::canonicalize(session_root)
            .await
            .map_err(|_| resource_error(context, InterfaceErrorCode::ArtifactDenied))?;
        let canonical_path = tokio::fs::canonicalize(path)
            .await
            .map_err(|_| resource_error(context, InterfaceErrorCode::ArtifactDenied))?;
        if !canonical_path.starts_with(&canonical_root) {
            return Err(resource_error(context, InterfaceErrorCode::ArtifactDenied));
        }
        let metadata = tokio::fs::metadata(&canonical_path)
            .await
            .map_err(|_| resource_error(context, InterfaceErrorCode::ArtifactDenied))?;
        if !metadata.is_file()
            || metadata.len() != expected_bytes
            || metadata.len() > self.max_download_bytes as u64
        {
            return Err(resource_error(context, InterfaceErrorCode::ArtifactDenied));
        }
        let bytes = tokio::fs::read(&canonical_path)
            .await
            .map_err(|_| resource_error(context, InterfaceErrorCode::ArtifactDenied))?;
        if bytes.len() as u64 != expected_bytes
            || format!("{:x}", sha2::Sha256::digest(&bytes)) != expected_sha256
        {
            return Err(resource_error(context, InterfaceErrorCode::ArtifactDenied));
        }
        store
            .put(
                &envelope.session_id,
                &page_id,
                "application/octet-stream",
                safe_extension(filename),
                &bytes,
                self.max_download_bytes,
            )
            .await
            .map_err(|_| resource_error(context, InterfaceErrorCode::ArtifactDenied))
    }

    pub(crate) async fn list(&self) -> Vec<String> {
        self.entries.read().await.keys().cloned().collect()
    }

    pub(crate) async fn read(
        &self,
        handle: &CapabilityHandle,
        context: &RequestContext,
        artifact_id: &str,
    ) -> InterfaceResult<Option<ArtifactContent>> {
        let Some(reader) = &self.reader else {
            return Ok(None);
        };
        let Some((session_id, reference)) = self.entries.read().await.get(artifact_id).cloned()
        else {
            return Ok(None);
        };
        reader
            .read(handle, context, &session_id, &reference)
            .await
            .map(Some)
    }
}

#[derive(Default)]
pub(crate) struct ArtifactAdmission {
    attempted: usize,
    admitted: usize,
    download_uris: BTreeMap<String, Option<String>>,
    failures: Vec<ArtifactAdmissionFailure>,
}

struct ArtifactAdmissionFailure {
    kind: &'static str,
    code: InterfaceErrorCode,
}

impl ArtifactAdmission {
    pub(crate) fn apply_to_mcp_value(
        &self,
        value: &mut serde_json::Value,
        command_id: &types::CommandId,
    ) {
        redact_download_paths(value, &self.download_uris);
        if self.failures.is_empty() {
            return;
        }
        let status = if self.admitted == 0 {
            "failed"
        } else {
            "partial"
        };
        let failures = self
            .failures
            .iter()
            .map(|failure| serde_json::json!({"kind":failure.kind,"code":failure.code}))
            .collect::<Vec<_>>();
        if let Some(object) = value.as_object_mut() {
            object.insert(
                "artifactRegistration".to_owned(),
                serde_json::json!({
                    "status":status,
                    "commandId":command_id,
                    "attempted":self.attempted,
                    "admitted":self.admitted,
                    "failures":failures,
                    "retryable":false,
                    "reconciliationRequired":true
                }),
            );
        }
    }
}

fn redact_download_paths(
    value: &mut serde_json::Value,
    download_uris: &BTreeMap<String, Option<String>>,
) {
    match value {
        serde_json::Value::Array(values) => {
            for value in values {
                redact_download_paths(value, download_uris);
            }
        }
        serde_json::Value::Object(object) => {
            if object.get("kind") == Some(&serde_json::json!("download")) {
                let existing_uri = object
                    .get("path")
                    .and_then(serde_json::Value::as_str)
                    .filter(|path| path.starts_with("artifact://"));
                let replacement = existing_uri.unwrap_or_else(|| {
                    object
                        .get("sha256")
                        .and_then(serde_json::Value::as_str)
                        .and_then(|sha256| download_uris.get(sha256))
                        .and_then(Option::as_deref)
                        .unwrap_or("[redacted-unavailable-artifact]")
                });
                object.insert(
                    "path".to_owned(),
                    serde_json::Value::String(replacement.to_owned()),
                );
            }
            for value in object.values_mut() {
                redact_download_paths(value, download_uris);
            }
        }
        _ => {}
    }
}

pub(crate) fn redact_mcp_download_paths(value: &mut serde_json::Value) {
    redact_download_paths(value, &BTreeMap::new());
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn safe_extension(filename: &str) -> &str {
    filename
        .rsplit_once('.')
        .map(|(_, extension)| extension)
        .filter(|extension| {
            !extension.is_empty()
                && extension.len() <= 10
                && extension.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
        .unwrap_or("bin")
}

fn resource_error(context: &RequestContext, code: InterfaceErrorCode) -> InterfaceError {
    InterfaceError {
        code,
        layer: ErrorLayer::Interface,
        message: "runtime interface request failed".to_owned(),
        correlation_id: context.correlation_id.clone(),
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: false,
        required_capability: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactCatalogFull;

impl fmt::Display for ArtifactCatalogFull {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("artifact resource catalog capacity exhausted")
    }
}

impl std::error::Error for ArtifactCatalogFull {}
