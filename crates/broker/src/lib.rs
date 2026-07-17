mod auth;
mod routes;

use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{Arc, OnceLock},
    task::{Context, Poll},
};

use axum::{extract::DefaultBodyLimit, middleware, routing::get, serve::Listener, Router};
use config::{AppConfig, InterfaceConfig};
use interface_core::{
    ArtifactContent, ArtifactReader, ArtifactReference, Authority, CapabilityHandle, EventStore,
    InterfaceResult, RuntimeInterface, SessionOwnershipRegistry,
};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use tokio::{
    io::{AsyncRead, AsyncWrite, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, RwLock, Semaphore},
};
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
    in_flight_requests: Arc<Semaphore>,
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
            in_flight_requests: Arc::new(Semaphore::new(interface.max_connections)),
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

struct ConnectionLimitedListener {
    inner: TcpListener,
    permits: Arc<Semaphore>,
}

impl ConnectionLimitedListener {
    fn new(inner: TcpListener, max_connections: usize) -> io::Result<Self> {
        if max_connections == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "max_connections must be positive",
            ));
        }
        Ok(Self {
            inner,
            permits: Arc::new(Semaphore::new(max_connections)),
        })
    }
}

struct PermittedTcpStream {
    inner: TcpStream,
    _permit: OwnedSemaphorePermit,
}

impl AsyncRead for PermittedTcpStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_read(context, buffer)
    }
}

impl AsyncWrite for PermittedTcpStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        Pin::new(&mut self.inner).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.inner).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<Result<(), io::Error>> {
        Pin::new(&mut self.inner).poll_shutdown(context)
    }
}

impl Listener for ConnectionLimitedListener {
    type Io = PermittedTcpStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        let permit = self
            .permits
            .clone()
            .acquire_owned()
            .await
            .expect("connection semaphore is never closed");
        let (inner, address) = Listener::accept(&mut self.inner).await;
        (
            PermittedTcpStream {
                inner,
                _permit: permit,
            },
            address,
        )
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.inner.local_addr()
    }
}

pub async fn serve_listener(
    listener: TcpListener,
    app: Router,
    max_connections: usize,
) -> io::Result<()> {
    axum::serve(
        ConnectionLimitedListener::new(listener, max_connections)?,
        app,
    )
    .await
}

pub async fn serve(config: AppConfig, startup: StartupCredential) -> anyhow::Result<()> {
    config.validate().map_err(anyhow::Error::msg)?;
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
            max_records: config.interface.max_event_retention,
            max_bytes: config.browser.max_artifact_bytes as u64,
        },
    )
    .map_err(anyhow::Error::new)?;
    let events = EventStore::new(config.interface.max_event_retention);
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
            config.interface.clone(),
        )
        .with_boundaries(
            events,
            ArtifactCatalog::new(artifact_reader, config.interface.max_event_retention),
        ),
    );
    let addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    serve_listener(listener, app, config.interface.max_connections).await?;
    Ok(())
}
