mod auth;
mod routes;

use std::{
    collections::HashMap,
    net::SocketAddr,
    sync::{Arc, OnceLock},
};

use axum::{extract::DefaultBodyLimit, middleware, routing::get, Router};
use config::{AppConfig, InterfaceConfig};
use interface_core::{
    ArtifactContent, ArtifactReader, ArtifactReference, Authority, CapabilityHandle, EventStore,
    InterfaceResult, RuntimeInterface, SessionOwnershipRegistry,
};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use tokio::sync::{RwLock, Semaphore};
use types::SessionId;

pub use auth::{EnrolledAuthority, StartupCredential, StartupCredentialError};

type RuntimeBinder = dyn Fn(CapabilityHandle) -> Arc<dyn RuntimeInterface> + Send + Sync + 'static;

#[derive(Clone)]
pub struct ArtifactCatalog {
    reader: Option<ArtifactReader>,
    entries: Arc<RwLock<HashMap<String, (SessionId, ArtifactReference)>>>,
    max_entries: usize,
}

impl Default for ArtifactCatalog {
    fn default() -> Self {
        Self {
            reader: None,
            entries: Arc::new(RwLock::new(HashMap::new())),
            max_entries: 1,
        }
    }
}

impl ArtifactCatalog {
    pub fn new(reader: ArtifactReader, max_entries: usize) -> Self {
        assert!(max_entries > 0, "artifact catalog bound must be positive");
        Self {
            reader: Some(reader),
            entries: Arc::new(RwLock::new(HashMap::new())),
            max_entries,
        }
    }

    /// Registers only a reference already issued by the trusted artifact boundary.
    pub async fn register_trusted(
        &self,
        session_id: SessionId,
        reference: ArtifactReference,
    ) -> Result<(), ArtifactCatalogFull> {
        let mut entries = self.entries.write().await;
        let artifact_id = reference.artifact_id().to_owned();
        if !entries.contains_key(&artifact_id) && entries.len() >= self.max_entries {
            return Err(ArtifactCatalogFull);
        }
        entries.insert(artifact_id, (session_id, reference));
        Ok(())
    }

    async fn read(
        &self,
        handle: &CapabilityHandle,
        ctx: &types::RequestContext,
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
            .read(handle, ctx, &session_id, &reference)
            .await
            .map(Some)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactCatalogFull;

impl std::fmt::Display for ArtifactCatalogFull {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("artifact catalog capacity exhausted")
    }
}

impl std::error::Error for ArtifactCatalogFull {}

#[derive(Clone)]
pub struct AppState {
    authority: Arc<dyn Authority>,
    bind_runtime: Arc<RuntimeBinder>,
    events: EventStore,
    artifacts: ArtifactCatalog,
    interface: InterfaceConfig,
    connections: Arc<Semaphore>,
}

impl AppState {
    pub fn new<F>(
        authority: Arc<dyn Authority>,
        bind_runtime: F,
        interface: InterfaceConfig,
    ) -> Self
    where
        F: Fn(CapabilityHandle) -> Arc<dyn RuntimeInterface> + Send + Sync + 'static,
    {
        interface
            .validate()
            .expect("interface configuration must be validated before router construction");
        Self {
            authority,
            bind_runtime: Arc::new(bind_runtime),
            events: EventStore::new(interface.max_event_retention),
            artifacts: ArtifactCatalog::default(),
            connections: Arc::new(Semaphore::new(interface.max_connections)),
            interface,
        }
    }

    pub fn with_boundaries(mut self, events: EventStore, artifacts: ArtifactCatalog) -> Self {
        self.events = events;
        self.artifacts = artifacts;
        self
    }
}

pub fn router(state: AppState) -> Router {
    let protected = routes::protected_router()
        .layer(DefaultBodyLimit::max(state.interface.max_request_bytes))
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            auth::authenticate,
        ));
    Router::new()
        .route("/healthz", get(routes::healthz))
        .merge(protected)
        .with_state(state)
}

pub async fn serve(config: AppConfig, startup: StartupCredential) -> anyhow::Result<()> {
    config
        .http
        .interface
        .validate()
        .map_err(anyhow::Error::msg)?;
    let runtime = RuntimeService::build(&config).await?;
    let authority = Arc::new(EnrolledAuthority::enroll(startup).await?);
    let (ownership, recorder) = SessionOwnershipRegistry::bounded(config.browser.max_active);
    let artifact_reader = ArtifactReader::new(
        artifact_store::ArtifactStore::new(
            &config.browser.artifacts_dir,
            config.browser.max_artifact_bytes,
            config.browser.max_screenshot_dimension,
        ),
        ownership,
        config.browser.max_artifact_bytes,
        interface_core::ArtifactOwnershipLimits {
            max_records: config.http.interface.max_event_retention,
            max_bytes: config.browser.max_artifact_bytes as u64,
        },
    )
    .map_err(anyhow::Error::new)?;
    let events = EventStore::new(config.http.interface.max_event_retention);
    let bound_runtime = Arc::new(OnceLock::<AuthenticatedRuntime>::new());
    let app = router(
        AppState::new(
            authority,
            move |handle| {
                Arc::new(
                    bound_runtime
                        .get_or_init(|| {
                            AuthenticatedRuntime::with_session_ownership(
                                runtime.clone(),
                                handle,
                                recorder.clone(),
                            )
                        })
                        .clone(),
                )
            },
            config.http.interface.clone(),
        )
        .with_boundaries(
            events,
            ArtifactCatalog::new(artifact_reader, config.http.interface.max_event_retention),
        ),
    );
    let addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
