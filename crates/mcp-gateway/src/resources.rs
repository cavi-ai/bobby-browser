use std::{collections::BTreeMap, fmt, sync::Arc};

use interface_core::{
    ArtifactContent, ArtifactReader, ArtifactReference, CapabilityHandle, InterfaceResult,
};
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
}

impl Default for ArtifactResources {
    fn default() -> Self {
        Self {
            reader: None,
            entries: Arc::new(RwLock::new(BTreeMap::new())),
            max_entries: 1,
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
        }
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

    /// Admits screenshot evidence only after `ArtifactReader` independently verifies the
    /// committed artifact and issues an opaque reference. Callers never provide a path,
    /// ownership assertion, or `ArtifactRecord`.
    pub(crate) async fn register_outcome(
        &self,
        handle: &CapabilityHandle,
        context: &RequestContext,
        envelope: &CommandEnvelope,
        outcome: &CommandOutcome,
    ) -> InterfaceResult<()> {
        let CommandOutcome::Completed { evidence, .. } = outcome else {
            return Ok(());
        };
        let Some(reader) = &self.reader else {
            return Ok(());
        };
        let Some(page_id) = envelope.page_id.clone() else {
            if evidence
                .iter()
                .any(|item| matches!(item, Evidence::Screenshot { .. }))
            {
                return Err(resource_error(context, InterfaceErrorCode::InvalidRequest));
            }
            return Ok(());
        };
        for item in evidence {
            let Evidence::Screenshot {
                artifact_id,
                media_type,
                width,
                height,
                bytes,
                sha256,
            } = item
            else {
                continue;
            };
            let record = artifact_store::ArtifactRecord {
                artifact_id: artifact_id.clone(),
                page_id: page_id.clone(),
                media_type: media_type.clone(),
                width: *width,
                height: *height,
                bytes: *bytes,
                sha256: sha256.clone(),
            };
            let reference = reader
                .register(handle, context, &envelope.session_id, &record)
                .await?;
            self.register_trusted(envelope.session_id.clone(), reference)
                .await
                .map_err(|_| resource_error(context, InterfaceErrorCode::ResourceExhausted))?;
        }
        Ok(())
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
