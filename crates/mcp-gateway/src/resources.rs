use std::{collections::BTreeMap, fmt, sync::Arc};

use interface_core::{
    ArtifactContent, ArtifactReader, ArtifactReference, CapabilityHandle, InterfaceResult,
};
use tokio::sync::RwLock;
use types::{RequestContext, SessionId};

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

    /// Registers only an opaque reference already issued by `ArtifactReader`.
    pub async fn register_trusted(
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactCatalogFull;

impl fmt::Display for ArtifactCatalogFull {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("artifact resource catalog capacity exhausted")
    }
}

impl std::error::Error for ArtifactCatalogFull {}
