mod auth;
mod routes;

use std::{
    collections::HashMap,
    io,
    net::SocketAddr,
    pin::Pin,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, OnceLock,
    },
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
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadBuf},
    net::{TcpListener, TcpStream},
    sync::{OwnedSemaphorePermit, RwLock, Semaphore},
};
use types::{CommandEnvelope, CommandOutcome, Evidence, SessionId};

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
    axum::serve(
        ConnectionLimitedListener::new(
            listener,
            max_connections,
            max_rejection_workers,
            rejection_stats,
        )?,
        app,
    )
    .await
}

struct StartupGate {
    authority: Arc<EnrolledAuthority>,
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
) -> anyhow::Result<(Router, T)>
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
    let gate = StartupGate {
        handle: authority.startup_handle(),
        authority: authority.clone(),
    };
    gate.validate_at(now())?;
    let runtime = build_runtime(config.clone()).await?;
    gate.validate_at(now())?;
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
            gate.authority.clone(),
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
    let listener = gate.bind_if_valid_at(now(), || bind_listener(addr)).await?;
    Ok((app, listener))
}

pub async fn serve(config: AppConfig, startup: StartupCredential) -> anyhow::Result<()> {
    let max_connections = config.interface.max_connections;
    let max_rejection_workers = config.interface.max_rejection_workers;
    let (app, listener) = bootstrap_listener_with(
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
    serve_listener_with_rejection_limit(
        listener,
        app,
        max_connections,
        max_rejection_workers,
        RejectionWorkerStats::default(),
    )
    .await?;
    Ok(())
}

/// Test-only helpers shared by broker integration tests. Not part of the public API.
#[doc(hidden)]
pub mod testing {
    use std::sync::Arc;

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

    use crate::{router, AppState, EnrolledAuthority, StartupCredential};

    const ADMIN_BEARER: &str = "admin-bootstrap-bearer-0123456789abcdef01";

    /// Builds a router wired to an [`EnrolledAuthority`] with a fixed admin bearer that
    /// holds `authority:admin` plus the core session/page capabilities. Returns the
    /// router, the enrolled authority (for direct assertions), and the admin bearer.
    pub async fn app_with_admin(
        max_principals: usize,
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
            ],
            Utc::now() + Duration::minutes(30),
        )
        .expect("fixed admin startup credential is valid");
        let authority = Arc::new(
            EnrolledAuthority::enroll(startup, max_principals)
                .await
                .expect("admin authority enrolls"),
        );
        let (_ownership, recorder) = SessionOwnershipRegistry::bounded(64);
        let runtime = RuntimeService::default();
        let mut interface = InterfaceConfig::default();
        interface.max_principals = max_principals;

        let app = router(AppState::new(
            authority.clone() as Arc<dyn Authority>,
            move |handle| {
                Arc::new(AuthenticatedRuntime::with_session_ownership(
                    runtime.clone(),
                    handle,
                    recorder.clone(),
                )) as Arc<dyn RuntimeInterface>
            },
            interface,
        ));
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
    /// using `admin_bearer` for authorization. Panics with context on failure — this is
    /// a test helper, not production code.
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
}
