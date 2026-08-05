mod auth;
mod authority_persist;
mod jobs;
mod mcp_http;
mod routes;

use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
    task::{Context, Poll},
};

use axum::{extract::DefaultBodyLimit, middleware, routing::get, serve::Listener, Router};
use config::{AppConfig, InterfaceConfig};
use interface_core::{
    ArtifactContent, ArtifactReader, ArtifactReference, Authority, CapabilityHandle, EventStore,
    IdempotencyStore, InterfaceResult, RuntimeInterface, SessionOwnershipRegistry,
};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use task_scheduler::JobScheduler;
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, RwLock, Semaphore},
};
use types::{CommandEnvelope, CommandOutcome, Evidence, PrincipalId, SessionId};

pub use auth::{EnrolledAuthority, StartupCredential, StartupCredentialError};
use jobs::JobSubmitOutcome;

type RuntimeBinder = dyn Fn(CapabilityHandle) -> Arc<dyn RuntimeInterface> + Send + Sync + 'static;

/// Caches one [`AuthenticatedRuntime`] per principal.
///
/// `AuthenticatedRuntime` owns an `IdempotencyStore`, so idempotency-key replay and
/// conflict detection only work while that store persists across the requests sharing a
/// key; a fresh runtime per call drops the idempotency memory.
///
/// An entry is reused only while it still matches what a fresh authentication would
/// produce: same capability set, and valid (unexpired, unrevoked) at time of use.
/// Reusing a stale handle would fail closed in `AuthorizationGuard::validate` once it
/// expires, and would intersect away newly granted capabilities in
/// `AuthorizationGuard::authorize`.
///
/// Bounded by `max_principals` in the steady state. Entries are not evicted on revoke,
/// so heavy principal-id churn grows this map without bound.
type RuntimeBindingEntry = (
    CapabilityHandle,
    Arc<AuthenticatedRuntime>,
    std::time::Instant,
);

struct RuntimeBindingCache {
    entries: Mutex<HashMap<PrincipalId, RuntimeBindingEntry>>,
    capacity: usize,
}

impl RuntimeBindingCache {
    fn new(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            capacity: capacity.max(1),
        }
    }

    /// Returns the cached runtime for `handle`'s principal, or builds and caches one via
    /// `build` if there is no entry yet, or the entry no longer reflects a live handle
    /// with the same capabilities as `handle`.
    fn bind(
        &self,
        handle: CapabilityHandle,
        build: impl FnOnce(CapabilityHandle) -> AuthenticatedRuntime,
    ) -> Arc<AuthenticatedRuntime> {
        let principal = handle.principal_id().clone();
        let mut entries =
            observability::locks::lock_recovering(&self.entries, "broker.runtime_binding_cache");
        if let Some((cached_handle, runtime, last_used)) = entries.get_mut(&principal) {
            if cached_handle.capabilities() == handle.capabilities()
                && cached_handle.is_valid_at(chrono::Utc::now())
            {
                *last_used = std::time::Instant::now();
                return runtime.clone();
            }
        }
        // Bound churn: dead entries (expired or revoked handles) are swept
        // first; a still-full map evicts the least-recently-used entry. The
        // steady-state bound is max_principals; this keeps it under churn.
        let now = std::time::Instant::now();
        entries.retain(|_, (cached_handle, _, _)| cached_handle.is_valid_at(chrono::Utc::now()));
        if entries.len() >= self.capacity {
            if let Some(oldest) = entries
                .iter()
                .min_by_key(|(_, (_, _, last_used))| *last_used)
                .map(|(principal, _)| principal.clone())
            {
                entries.remove(&oldest);
            }
        }
        let runtime = Arc::new(build(handle.clone()));
        entries.insert(principal, (handle, runtime.clone(), now));
        runtime
    }
}

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

    pub async fn admit_outcome(
        &self,
        handle: &CapabilityHandle,
        context: &types::RequestContext,
        envelope: &CommandEnvelope,
        outcome: &CommandOutcome,
    ) -> Result<(), ArtifactCatalogFull> {
        let Some(reader) = &self.reader else {
            return Ok(());
        };
        let evidence = match outcome {
            CommandOutcome::Completed { evidence, .. }
            | CommandOutcome::NeedsReconciliation { evidence, .. } => evidence,
            _ => return Ok(()),
        };
        for item in evidence {
            let record = match item {
                Evidence::Screenshot {
                    artifact_id,
                    media_type,
                    width,
                    height,
                    bytes,
                    sha256,
                } => artifact_store::ArtifactRecord {
                    artifact_id: artifact_id.clone(),
                    page_id: envelope.page_id.clone().ok_or(ArtifactCatalogFull)?,
                    media_type: media_type.clone(),
                    width: *width,
                    height: *height,
                    bytes: *bytes,
                    sha256: sha256.clone(),
                },
                _ => continue,
            };
            let reference = reader
                .register(handle, context, &envelope.session_id, &record)
                .await
                .map_err(|_| ArtifactCatalogFull)?;
            self.register_trusted(envelope.session_id.clone(), reference)
                .await?;
        }
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
    // One `Semaphore` per principal, created lazily on a principal's first request.
    // Bounded by `max_principals` in the steady state; entries are not evicted on revoke.
    principal_permits: Arc<RwLock<HashMap<PrincipalId, Arc<Semaphore>>>>,
    mcp_servers: mcp_http::McpServers,
    mcp_resources: mcp_gateway::ArtifactResources,
    scheduler: Arc<JobScheduler>,
    job_idempotency: Arc<IdempotencyStore<JobSubmitOutcome>>,
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
            principal_permits: Arc::new(RwLock::new(HashMap::new())),
            mcp_servers: mcp_http::McpServers::default(),
            mcp_resources: mcp_gateway::ArtifactResources::default(),
            scheduler: Arc::new(jobs::memory_scheduler()),
            job_idempotency: Arc::new(jobs::job_idempotency_store()),
            interface,
        }
    }

    pub fn with_scheduler(mut self, scheduler: Arc<JobScheduler>) -> Self {
        self.scheduler = scheduler;
        self
    }

    pub fn with_boundaries(mut self, events: EventStore, artifacts: ArtifactCatalog) -> Self {
        self.events = events;
        self.artifacts = artifacts;
        self
    }

    /// Gives `/v1/mcp` the same trusted artifact boundary `/v1/artifacts` uses. Without
    /// it the MCP surface falls back to `ArtifactResources::default()`, which carries no
    /// `ArtifactReader` and denies admission to every screenshot and download a tool
    /// call produces.
    pub fn with_mcp_resources(mut self, resources: mcp_gateway::ArtifactResources) -> Self {
        self.mcp_resources = resources;
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
    // `/v1/mcp` is mounted outside `protected_router()`'s strict-header middleware and
    // does its own bearer-only auth (`mcp_http::post_mcp`): standard MCP clients can only
    // send a static `Authorization` header, not the fresh
    // `x-deadline`/`x-correlation-id`/`x-interface-version` the other routes require.
    let mcp = Router::new()
        .route(
            "/v1/mcp",
            axum::routing::post(mcp_http::post_mcp).get(mcp_http::get_mcp),
        )
        .layer(DefaultBodyLimit::max(state.interface.max_request_bytes));
    Router::new()
        .route("/healthz", get(routes::healthz))
        .merge(protected)
        .merge(mcp)
        .with_state(state)
}

struct ConnectionLimitedListener {
    inner: TcpListener,
    permits: Arc<Semaphore>,
    rejection_permits: Arc<Semaphore>,
    rejection_stats: RejectionWorkerStats,
}

impl ConnectionLimitedListener {
    fn new(
        inner: TcpListener,
        max_connections: usize,
        max_rejection_workers: usize,
        rejection_stats: RejectionWorkerStats,
    ) -> io::Result<Self> {
        if max_connections == 0 || max_rejection_workers == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "connection and rejection worker limits must be positive",
            ));
        }
        Ok(Self {
            inner,
            permits: Arc::new(Semaphore::new(max_connections)),
            rejection_permits: Arc::new(Semaphore::new(max_rejection_workers)),
            rejection_stats,
        })
    }
}

#[derive(Clone, Default)]
pub struct RejectionWorkerStats {
    active: Arc<AtomicUsize>,
    peak: Arc<AtomicUsize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RejectionWorkerSnapshot {
    pub active: usize,
    pub peak: usize,
}

impl RejectionWorkerStats {
    pub fn snapshot(&self) -> RejectionWorkerSnapshot {
        RejectionWorkerSnapshot {
            active: self.active.load(Ordering::Acquire),
            peak: self.peak.load(Ordering::Acquire),
        }
    }

    pub fn active(&self) -> usize {
        self.snapshot().active
    }

    pub fn peak(&self) -> usize {
        self.snapshot().peak
    }

    fn enter(&self) -> ActiveRejectionWorker {
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak.fetch_max(active, Ordering::AcqRel);
        ActiveRejectionWorker(self.clone())
    }
}

struct ActiveRejectionWorker(RejectionWorkerStats);

impl Drop for ActiveRejectionWorker {
    fn drop(&mut self) {
        self.0.active.fetch_sub(1, Ordering::AcqRel);
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
        loop {
            let (mut inner, address) = Listener::accept(&mut self.inner).await;
            if let Ok(permit) = self.permits.clone().try_acquire_owned() {
                return (
                    PermittedTcpStream {
                        inner,
                        _permit: permit,
                    },
                    address,
                );
            }
            let Ok(rejection_permit) = self.rejection_permits.clone().try_acquire_owned() else {
                drop(inner);
                continue;
            };
            let active_rejection = self.rejection_stats.enter();
            tokio::spawn(async move {
                let _rejection_permit = rejection_permit;
                let _active_rejection = active_rejection;
                let mut request_prefix = [0_u8; 1024];
                let _ = tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    inner.read(&mut request_prefix),
                )
                .await;
                let body = serde_json::to_string(&types::InterfaceError {
                    code: types::InterfaceErrorCode::ResourceExhausted,
                    layer: types::ErrorLayer::Interface,
                    message: "connection capacity exhausted".into(),
                    correlation_id: types::CorrelationId::new(),
                    command_id: None,
                    retryable: true,
                    retry_after_ms: Some(1_000),
                    reconciliation_required: false,
                    required_capability: None,
                })
                .expect("typed overload serializes");
                let response=format!("HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\nretry-after: 1\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",body.len());
                let _ = inner.write_all(response.as_bytes()).await;
                let _ = inner.shutdown().await;
            });
        }
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
    serve_listener_with_rejection_limit(
        listener,
        app,
        max_connections,
        16,
        RejectionWorkerStats::default(),
    )
    .await
}

pub async fn serve_listener_with_rejection_limit(
    listener: TcpListener,
    app: Router,
    max_connections: usize,
    max_rejection_workers: usize,
    rejection_stats: RejectionWorkerStats,
) -> io::Result<()> {
    serve_listener_graceful(
        listener,
        app,
        max_connections,
        max_rejection_workers,
        rejection_stats,
        std::future::pending(),
        std::future::pending(),
    )
    .await
}

/// Resolves on the first SIGINT/SIGTERM; a second signal force-exits with code
/// 130. Each call registers independent handlers, so two callers (the graceful
/// shutdown trigger and the drain deadline) both resolve on the same signal.
pub async fn shutdown_signal() {
    use tokio::signal;
    #[cfg(unix)]
    let terminate = async {
        signal::unix::signal(signal::unix::SignalKind::terminate())
            .expect("install SIGTERM handler")
            .recv()
            .await;
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! {
        _ = signal::ctrl_c() => {},
        _ = terminate => {},
    }
    tracing::info!("shutdown.signal_received");
    tokio::spawn(async {
        #[cfg(unix)]
        {
            let mut term = signal::unix::signal(signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
            tokio::select! {
                _ = signal::ctrl_c() => {},
                _ = term.recv() => {},
            }
        }
        #[cfg(not(unix))]
        {
            let _ = signal::ctrl_c().await;
        }
        tracing::warn!("shutdown.forced");
        std::process::exit(130);
    });
}

/// Waits for the shutdown signal, then for `drain_timeout`, the deadline by
/// which in-flight work must finish before the server gives up draining.
async fn drain_after_signal(drain_timeout: std::time::Duration) {
    shutdown_signal().await;
    tokio::time::sleep(drain_timeout).await;
}

/// Serves `app` on `listener`, stopping acceptance when `shutdown` resolves
/// and returning when `drain_deadline` resolves even if in-flight work is
/// still outstanding.
pub async fn serve_listener_graceful<Shutdown, Drain>(
    listener: TcpListener,
    app: Router,
    max_connections: usize,
    max_rejection_workers: usize,
    rejection_stats: RejectionWorkerStats,
    shutdown: Shutdown,
    drain_deadline: Drain,
) -> io::Result<()>
where
    Shutdown: std::future::Future<Output = ()> + Send + 'static,
    Drain: std::future::Future<Output = ()> + Send + 'static,
{
    let server = axum::serve(
        ConnectionLimitedListener::new(
            listener,
            max_connections,
            max_rejection_workers,
            rejection_stats,
        )?,
        app,
    )
    .with_graceful_shutdown(shutdown);
    tokio::select! {
        result = server => result,
        _ = drain_deadline => {
            tracing::warn!("shutdown.drain_timeout_expired");
            Ok(())
        }
    }
}

struct StartupGate {
    handle: CapabilityHandle,
}

impl StartupGate {
    fn validate_at(&self, now: chrono::DateTime<chrono::Utc>) -> anyhow::Result<()> {
        if !self.handle.is_valid_at(now) {
            return Err(StartupCredentialError::Expired.into());
        }
        Ok(())
    }

    async fn bind_if_valid_at<T, Bind, BindFuture>(
        &self,
        now: chrono::DateTime<chrono::Utc>,
        bind: Bind,
    ) -> anyhow::Result<T>
    where
        Bind: FnOnce() -> BindFuture,
        BindFuture: std::future::Future<Output = anyhow::Result<T>>,
    {
        self.validate_at(now)?;
        bind().await
    }
}

async fn bootstrap_listener_with<T, Clock, Build, BuildFuture, Bind, BindFuture>(
    config: AppConfig,
    startup: StartupCredential,
    now: Clock,
    build_runtime: Build,
    bind_listener: Bind,
) -> anyhow::Result<(Router, T, Arc<JobScheduler>)>
where
    Clock: Fn() -> chrono::DateTime<chrono::Utc>,
    Build: FnOnce(AppConfig) -> BuildFuture,
    BuildFuture: std::future::Future<Output = anyhow::Result<RuntimeService>>,
    Bind: FnOnce(SocketAddr) -> BindFuture,
    BindFuture: std::future::Future<Output = anyhow::Result<T>>,
{
    config.validate().map_err(anyhow::Error::msg)?;
    let authority =
        Arc::new(EnrolledAuthority::enroll(startup, config.interface.max_principals).await?);
    // `PersistentAuthority` wraps a clone of the enrolled authority; the clone shares the
    // same `AuthorityStore` records via `Arc`, so `AppState` authenticates and issues
    // through the persisted path while `StartupGate` keeps its own handle on the
    // un-wrapped `EnrolledAuthority`.
    let persistent_authority = Arc::new(
        authority_persist::PersistentAuthority::open(
            (*authority).clone(),
            config.storage.authority_path.clone(),
        )
        .await?,
    );
    let gate = StartupGate {
        handle: authority.startup_handle(),
    };
    gate.validate_at(now())?;
    let runtime = build_runtime(config.clone()).await?;
    gate.validate_at(now())?;
    let (ownership, recorder) = SessionOwnershipRegistry::bounded(config.browser.max_active);
    let artifact_store = artifact_store::ArtifactStore::new(
        &config.browser.artifacts_dir,
        config.browser.max_artifact_bytes,
        config.browser.max_screenshot_dimension,
    );
    let artifact_reader = ArtifactReader::new(
        artifact_store.clone(),
        ownership,
        config.browser.max_artifact_bytes,
        interface_core::ArtifactOwnershipLimits {
            max_records: config.interface.max_event_retention,
            max_bytes: config.browser.max_artifact_bytes as u64,
        },
    )
    .map_err(anyhow::Error::new)?;
    let events = EventStore::new(config.interface.max_event_retention);
    let bindings = RuntimeBindingCache::new(config.interface.max_principals);
    let scheduler = Arc::new(
        jobs::journal_scheduler(&config)
            .await
            .map_err(|e| anyhow::anyhow!("scheduler bootstrap failed: {e}"))?,
    );
    let app = router(
        AppState::new(
            persistent_authority,
            move |handle| {
                bindings.bind(handle, |handle| {
                    AuthenticatedRuntime::with_session_ownership(
                        runtime.clone(),
                        handle,
                        recorder.clone(),
                    )
                }) as Arc<dyn RuntimeInterface>
            },
            config.interface.clone(),
        )
        .with_scheduler(Arc::clone(&scheduler))
        .with_boundaries(
            events,
            ArtifactCatalog::new(
                artifact_reader.clone(),
                config.interface.max_event_retention,
            ),
        )
        // The same `ArtifactReader` instance backs both surfaces, so one ownership
        // ledger accounts for artifacts however they were produced.
        .with_mcp_resources(mcp_gateway::ArtifactResources::production(
            artifact_reader,
            artifact_store,
            config.browser.downloads_dir.clone(),
            config.http.max_download_bytes,
            config.interface.max_event_retention,
        )),
    );
    let addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port).parse()?;
    let listener = gate.bind_if_valid_at(now(), || bind_listener(addr)).await?;
    Ok((app, listener, scheduler))
}

pub async fn serve(config: AppConfig, startup: StartupCredential) -> anyhow::Result<()> {
    let max_connections = config.interface.max_connections;
    let max_rejection_workers = config.interface.max_rejection_workers;
    let shutdown_timeout = std::time::Duration::from_millis(config.server.shutdown_timeout_ms);
    let (app, listener, scheduler) = bootstrap_listener_with(
        config,
        startup,
        chrono::Utc::now,
        |config| async move {
            RuntimeService::build(&config)
                .await
                .map_err(anyhow::Error::new)
        },
        |addr| async move {
            tokio::net::TcpListener::bind(addr)
                .await
                .map_err(anyhow::Error::new)
        },
    )
    .await?;
    let run_handle = {
        let scheduler = Arc::clone(&scheduler);
        tokio::spawn(async move { scheduler.run().await })
    };
    let serve_result = serve_listener_graceful(
        listener,
        app,
        max_connections,
        max_rejection_workers,
        RejectionWorkerStats::default(),
        shutdown_signal(),
        drain_after_signal(shutdown_timeout),
    )
    .await;
    jobs::shutdown_scheduler(&scheduler, run_handle, shutdown_timeout).await;
    serve_result?;
    Ok(())
}

/// Test-only helpers shared by broker integration tests. Not part of the public API.
#[doc(hidden)]
pub mod testing {
    use std::{
        cell::Cell,
        sync::{
            atomic::{AtomicU64, Ordering},
            Arc,
        },
    };

    use axum::{
        body::{to_bytes, Body},
        http::{request::Builder, StatusCode},
    };
    use chrono::{Duration, SecondsFormat, Utc};
    use config::InterfaceConfig;
    use interface_core::{Authority, RuntimeInterface, SessionOwnershipRegistry};
    use sdk_core::{AuthenticatedRuntime, RuntimeService};
    use tower::ServiceExt;
    use types::{Capability, PrincipalId, CURRENT_INTERFACE_VERSION};
    use uuid::Uuid;

    use crate::{
        authority_persist::PersistentAuthority, router, AppState, EnrolledAuthority,
        StartupCredential,
    };

    const ADMIN_BEARER: &str = "admin-bootstrap-bearer-0123456789abcdef01";

    // `testing` compiles into the production lib so integration test crates under
    // `tests/` can depend on it, which rules out `tempfile` (a dev-dependency). Each
    // call gets its own path under the OS temp dir, disambiguated by process id plus an
    // atomic counter (tests in one binary run on separate threads).
    static AUTHORITY_PATH_COUNTER: AtomicU64 = AtomicU64::new(0);

    fn unique_authority_path() -> std::path::PathBuf {
        let counter = AUTHORITY_PATH_COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!(
            "bobby-broker-test-authority-{}-{counter}.json",
            std::process::id()
        ))
    }

    // Thread-local rather than a process-wide static: each #[tokio::test] runs on its
    // own OS thread, so a shared static would let concurrent sibling tests clobber one
    // test's observation between its own bind calls.
    thread_local! {
        static LAST_BOUND_PRINCIPAL: Cell<Option<Uuid>> = const { Cell::new(None) };
    }

    /// Returns the principal UUID recorded by the most recent test runtime binding made
    /// on the calling thread, if any.
    pub fn last_bound_principal() -> Option<Uuid> {
        LAST_BOUND_PRINCIPAL.with(Cell::get)
    }

    fn record_bound_principal(principal: &PrincipalId) {
        LAST_BOUND_PRINCIPAL.with(|cell| cell.set(Some(*principal.as_uuid())));
    }

    /// Builds a router wired to an [`EnrolledAuthority`] with a fixed admin bearer that
    /// holds `authority:admin` plus the core session/page capabilities. Returns the
    /// router, the enrolled authority (for direct assertions), and the admin bearer.
    ///
    /// Delegates to [`app_with_admin_and_quota`] with the default per-principal
    /// in-flight quota.
    pub async fn app_with_admin(
        max_principals: usize,
    ) -> (axum::Router, Arc<EnrolledAuthority>, String) {
        app_with_admin_and_quota(
            max_principals,
            InterfaceConfig::default().max_in_flight_per_principal,
        )
        .await
    }

    /// Same as [`app_with_admin`], but overrides `max_in_flight_per_principal` so tests
    /// can exercise the per-principal in-flight quota under a tight bound.
    pub async fn app_with_admin_and_quota(
        max_principals: usize,
        max_in_flight_per_principal: usize,
    ) -> (axum::Router, Arc<EnrolledAuthority>, String) {
        app_with_admin_and_quota_at(
            max_principals,
            max_in_flight_per_principal,
            unique_authority_path(),
        )
        .await
    }

    /// Same as [`app_with_admin_and_quota`], but pins the on-disk authority persistence
    /// path so a test can drop the router and rebuild a fresh one over the *same* file.
    pub async fn app_with_admin_and_quota_at(
        max_principals: usize,
        max_in_flight_per_principal: usize,
        authority_path: std::path::PathBuf,
    ) -> (axum::Router, Arc<EnrolledAuthority>, String) {
        let startup = StartupCredential::new(
            ADMIN_BEARER.to_owned(),
            PrincipalId::from_uuid(Uuid::nil()),
            vec![
                Capability::AuthorityAdmin,
                Capability::SessionRead,
                Capability::SessionWrite,
                Capability::PageRead,
                Capability::PageWrite,
                Capability::BrowserMutate,
                Capability::RecoveryRead,
                Capability::RecoveryWrite,
                Capability::JobSubmit,
                Capability::JobRead,
                Capability::JobCancel,
            ],
            Utc::now() + Duration::minutes(30),
        )
        .expect("fixed admin startup credential is valid");
        let authority = Arc::new(
            EnrolledAuthority::enroll(startup, max_principals)
                .await
                .expect("admin authority enrolls"),
        );
        // Wraps a clone of `authority` the same way `bootstrap_listener_with` does: the
        // clone shares the underlying `AuthorityStore` records via `Arc`.
        let persistent_authority = Arc::new(
            PersistentAuthority::open((*authority).clone(), authority_path)
                .await
                .expect("test authority persistence path opens"),
        );
        let (_ownership, recorder) = SessionOwnershipRegistry::bounded(64);
        let runtime = RuntimeService::default();
        let interface = InterfaceConfig {
            max_principals,
            max_in_flight_per_principal,
            ..InterfaceConfig::default()
        };

        // Mirrors the production binder in `bootstrap_listener_with`: one
        // `AuthenticatedRuntime` (and thus one `IdempotencyStore`) per principal for the
        // life of this router. The observation hook fires on every bind *request*, cache
        // hit or not, so `last_bound_principal` tracks the most recent caller.
        let bindings = crate::RuntimeBindingCache::new(max_principals);
        let state = AppState::new(
            persistent_authority as Arc<dyn Authority>,
            move |handle| {
                record_bound_principal(handle.principal_id());
                bindings.bind(handle, |handle| {
                    AuthenticatedRuntime::with_session_ownership(
                        runtime.clone(),
                        handle,
                        recorder.clone(),
                    )
                }) as Arc<dyn RuntimeInterface>
            },
            interface,
        );
        let scheduler = Arc::clone(&state.scheduler);
        tokio::spawn(async move {
            let _ = scheduler.run().await;
        });
        let app = router(state);
        (app, authority, ADMIN_BEARER.to_owned())
    }

    /// Adds the standard authenticated-context headers (authorization, interface
    /// version, correlation id, deadline) shared by broker integration tests.
    pub fn context_headers(builder: Builder, bearer: &str) -> Builder {
        builder
            .header("authorization", format!("Bearer {bearer}"))
            .header("x-interface-version", CURRENT_INTERFACE_VERSION)
            .header("x-correlation-id", Uuid::new_v4().to_string())
            .header(
                "x-deadline",
                (Utc::now() + Duration::minutes(2)).to_rfc3339_opts(SecondsFormat::Millis, true),
            )
    }

    /// Issues a bearer for `principal` with `capabilities` via `POST /v1/principals`,
    /// using `admin_bearer` for authorization. Panics with context on failure.
    pub async fn issue_bearer(
        app: &axum::Router,
        admin_bearer: &str,
        principal: Uuid,
        capabilities: &[&str],
    ) -> String {
        let body = serde_json::json!({
            "principalId": principal,
            "capabilities": capabilities,
            "expiresAt": (Utc::now() + Duration::minutes(10))
                .to_rfc3339_opts(SecondsFormat::Millis, true),
        });
        let request = context_headers(axum::http::Request::post("/v1/principals"), admin_bearer)
            .header("idempotency-key", format!("issue-{principal}"))
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&body).expect("issue body serializes"),
            ))
            .expect("issue request builds");
        let response = app
            .clone()
            .oneshot(request)
            .await
            .expect("router accepts issuance request");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), 64 * 1024)
            .await
            .expect("issuance response body reads");
        assert_eq!(
            status,
            StatusCode::CREATED,
            "principal issuance failed: {}",
            String::from_utf8_lossy(&bytes)
        );
        let json: serde_json::Value =
            serde_json::from_slice(&bytes).expect("issuance response is valid JSON");
        json["bearer"]
            .as_str()
            .expect("issuance response carries a bearer")
            .to_owned()
    }
}

pub async fn serve_with_worker_factory(
    config: AppConfig,
    startup: StartupCredential,
    factory: Arc<dyn worker_pool::WorkerFactory>,
) -> anyhow::Result<()> {
    let max_connections = config.interface.max_connections;
    let max_rejection_workers = config.interface.max_rejection_workers;
    let shutdown_timeout = std::time::Duration::from_millis(config.server.shutdown_timeout_ms);
    let (app, listener, scheduler) = bootstrap_listener_with(
        config,
        startup,
        chrono::Utc::now,
        |config| async move {
            RuntimeService::build_with_worker_factory(&config, factory)
                .await
                .map_err(anyhow::Error::new)
        },
        |addr| async move {
            tokio::net::TcpListener::bind(addr)
                .await
                .map_err(anyhow::Error::new)
        },
    )
    .await?;
    let run_handle = {
        let scheduler = Arc::clone(&scheduler);
        tokio::spawn(async move { scheduler.run().await })
    };
    let serve_result = serve_listener_graceful(
        listener,
        app,
        max_connections,
        max_rejection_workers,
        RejectionWorkerStats::default(),
        shutdown_signal(),
        drain_after_signal(shutdown_timeout),
    )
    .await;
    jobs::shutdown_scheduler(&scheduler, run_handle, shutdown_timeout).await;
    serve_result?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    use chrono::{Duration, Utc};
    use types::{Capability, PrincipalId};
    use uuid::uuid;

    use super::*;

    #[tokio::test]
    async fn credential_expiring_during_runtime_construction_prevents_listener_bind() {
        let initial = Utc::now();
        let expires_at = initial + Duration::minutes(1);
        let clock = Arc::new(Mutex::new(initial));
        let construction_clock = clock.clone();
        let bind_calls = Arc::new(AtomicUsize::new(0));
        let observed_bind_calls = bind_calls.clone();
        let startup = StartupCredential::new(
            "startup-expiry-race-bearer-000001".to_owned(),
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000088")),
            vec![Capability::SessionRead],
            expires_at,
        )
        .unwrap();

        let result = bootstrap_listener_with(
            AppConfig::default(),
            startup,
            move || *clock.lock().unwrap(),
            move |_| async move {
                *construction_clock.lock().unwrap() = expires_at + Duration::seconds(1);
                Ok(RuntimeService::default())
            },
            move |_| async move {
                observed_bind_calls.fetch_add(1, Ordering::SeqCst);
                Ok(())
            },
        )
        .await;

        assert!(result.is_err());
        assert_eq!(bind_calls.load(Ordering::SeqCst), 0);
    }

    async fn issued_handle(
        store: &interface_core::AuthorityStore,
        principal: PrincipalId,
        capabilities: Vec<Capability>,
    ) -> CapabilityHandle {
        let bearer = store
            .issue(principal, capabilities, Utc::now() + Duration::minutes(10))
            .await
            .expect("token issues")
            .expose_once();
        store.verify(&bearer).await.expect("issued token verifies")
    }

    #[tokio::test]
    async fn runtime_binding_cache_reuses_one_runtime_per_principal() {
        let store = interface_core::AuthorityStore::in_memory();
        let principal = PrincipalId::from_uuid(uuid!("20000000-0000-0000-0000-000000000001"));
        let handle_one =
            issued_handle(&store, principal.clone(), vec![Capability::SessionRead]).await;
        let handle_two = issued_handle(&store, principal, vec![Capability::SessionRead]).await;

        let cache = RuntimeBindingCache::new(64);
        let build_calls = Arc::new(AtomicUsize::new(0));
        let counted_build = |calls: Arc<AtomicUsize>| {
            move |handle: CapabilityHandle| {
                calls.fetch_add(1, Ordering::SeqCst);
                AuthenticatedRuntime::new(RuntimeService::default(), handle)
            }
        };

        let runtime_one = cache.bind(handle_one, counted_build(build_calls.clone()));
        let runtime_two = cache.bind(handle_two, counted_build(build_calls.clone()));

        assert!(
            Arc::ptr_eq(&runtime_one, &runtime_two),
            "repeat calls for the same still-valid, same-capability principal must reuse \
             one runtime (and therefore one IdempotencyStore)"
        );
        assert_eq!(build_calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn runtime_binding_cache_rebuilds_when_capabilities_change() {
        let store = interface_core::AuthorityStore::in_memory();
        let principal = PrincipalId::from_uuid(uuid!("20000000-0000-0000-0000-000000000002"));
        let handle_one =
            issued_handle(&store, principal.clone(), vec![Capability::SessionRead]).await;
        // Simulates a token rotation that grants an additional capability: same
        // principal, different capability set.
        let handle_two = issued_handle(
            &store,
            principal,
            vec![Capability::SessionRead, Capability::SessionWrite],
        )
        .await;

        let cache = RuntimeBindingCache::new(64);
        let build_calls = Arc::new(AtomicUsize::new(0));
        let counted_build = |calls: Arc<AtomicUsize>| {
            move |handle: CapabilityHandle| {
                calls.fetch_add(1, Ordering::SeqCst);
                AuthenticatedRuntime::new(RuntimeService::default(), handle)
            }
        };

        cache.bind(handle_one, counted_build(build_calls.clone()));
        cache.bind(handle_two, counted_build(build_calls.clone()));

        assert_eq!(
            build_calls.load(Ordering::SeqCst),
            2,
            "a principal presenting a handle with a different capability set must rebuild \
             the cached binding rather than keep authorizing against the stale set"
        );
    }

    #[tokio::test]
    async fn runtime_binding_cache_rebuilds_after_the_cached_handle_expires() {
        let store = interface_core::AuthorityStore::in_memory();
        let principal = PrincipalId::from_uuid(uuid!("20000000-0000-0000-0000-000000000003"));
        let short_lived_bearer = store
            .issue(
                principal.clone(),
                vec![Capability::SessionRead],
                Utc::now() + Duration::milliseconds(250),
            )
            .await
            .expect("token issues")
            .expose_once();
        let expiring_handle = store
            .verify(&short_lived_bearer)
            .await
            .expect("issued token verifies before it expires");

        let cache = RuntimeBindingCache::new(64);
        let build_calls = Arc::new(AtomicUsize::new(0));
        let counted_build = |calls: Arc<AtomicUsize>| {
            move |handle: CapabilityHandle| {
                calls.fetch_add(1, Ordering::SeqCst);
                AuthenticatedRuntime::new(RuntimeService::default(), handle)
            }
        };
        cache.bind(expiring_handle, counted_build(build_calls.clone()));

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // A fresh, currently-valid handle for the same principal (as a rotated token
        // would produce) must not be blocked by the now-expired cached entry.
        let fresh_handle = issued_handle(&store, principal, vec![Capability::SessionRead]).await;
        cache.bind(fresh_handle, counted_build(build_calls.clone()));

        assert_eq!(
            build_calls.load(Ordering::SeqCst),
            2,
            "a cached binding that is no longer valid_at(now) must not be reused for a \
             fresh, currently-valid handle from the same principal"
        );
    }
}
