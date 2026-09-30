//! Browser engine selection and worker factory composition shared by the
//! `bobby` CLI and the stdio MCP gateway: parses the
//! `AUTOMATION_RUNTIME_BROWSER_SELECTION` value, registers the configured
//! Firefox companion profiles alongside managed Chromium, and resolves the
//! engine preference into a single [`WorkerFactory`].

use anyhow::Result;
use artifact_store::ArtifactStore;
use async_trait::async_trait;
use companion_core::{
    CompanionServer, CompanionServerConfig, CompanionServerError, CompanionServerHandle,
};
use companion_protocol::{BrowserEngine, CompanionCapabilities};
use config::{
    AppConfig, BrowserEngineConfig, BrowserSelectionConfig, EnginePreferenceConfig,
    FirefoxCompanionConfig,
};
use serde::{Deserialize, Serialize};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;
use types::{CommandError, ErrorCode, ErrorLayer, ProfileId, SessionId};
use url::Url;
use worker_pool::{
    BrowserWorker, BrowserWorkerSelector, ChromiumWorkerFactory, EnginePreference,
    FactoryRegistration, RequiredCapabilities, SelectedWorkerFactory, WorkerFactory,
};

use crate::{CompanionExtensionObserver, FirefoxCompanionFactory};

struct FirefoxRegistration {
    profile_id: ProfileId,
    factory: Arc<ConfiguredFirefoxFactory>,
}

struct ConfiguredFirefoxFactory {
    config: FirefoxRuntimeConfig,
    required: RequiredCapabilities,
    pairing_code_observer: Arc<dyn Fn(&str) + Send + Sync>,
    server: Mutex<Option<Arc<CompanionServerHandle>>>,
    artifacts: ArtifactStore,
    upload_roots: Vec<PathBuf>,
    downloads_dir: PathBuf,
    bidi: Mutex<Option<crate::BidiClient>>,
    lifecycle: Mutex<()>,
    profile_owner: std::sync::Mutex<Option<std::fs::File>>,
    closed: std::sync::atomic::AtomicBool,
}

#[derive(Clone)]
struct FirefoxRuntimeConfig {
    profile_id: ProfileId,
    bidi_url: Url,
    profile_dir: PathBuf,
    companion_bind: SocketAddr,
    descriptor_path: PathBuf,
    timeout: Duration,
    pairing_code_ttl: Duration,
    attachment_ttl: Duration,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NativeHostDescriptor {
    pub endpoint: String,
    pub pairing_code: String,
    pub ownership_id: String,
}

#[derive(Clone)]
pub struct FirefoxProfileEnrollmentConfig {
    pub companion_bind: SocketAddr,
    pub descriptor_path: PathBuf,
    pub timeout: Duration,
    pub pairing_code_ttl: Duration,
    pub attachment_ttl: Duration,
}

pub struct EnrolledFirefoxProfile {
    profile_id: ProfileId,
    server: Arc<CompanionServerHandle>,
}

impl EnrolledFirefoxProfile {
    pub fn profile_id(&self) -> &ProfileId {
        &self.profile_id
    }
}

pub struct FirefoxProfileEnrollment {
    attempt: FirefoxBootstrapAttempt,
    timeout: Duration,
}

impl FirefoxProfileEnrollment {
    pub async fn wait(self) -> Result<EnrolledFirefoxProfile, CommandError> {
        let registry = self.attempt.server().registry();
        let profile_id = tokio::time::timeout(self.timeout, async {
            loop {
                let profiles = registry.paired_profile_ids().await;
                match profiles.as_slice() {
                    [profile] => return Ok(profile.clone()),
                    [] => tokio::time::sleep(Duration::from_millis(50)).await,
                    _ => {
                        return Err(CommandError {
                            code: ErrorCode::PolicyDenied,
                            message: "Firefox enrollment observed multiple profiles".into(),
                            layer: ErrorLayer::Driver,
                            retryable: false,
                        });
                    }
                }
            }
        })
        .await
        .map_err(|_| companion_error("Firefox profile enrollment timed out"))??;
        let server = tokio::task::spawn_blocking(move || self.attempt.complete())
            .await
            .map_err(companion_error)?
            .map_err(companion_error)?;
        Ok(EnrolledFirefoxProfile { profile_id, server })
    }
}

pub async fn start_firefox_profile_enrollment(
    config: FirefoxProfileEnrollmentConfig,
    pairing_code_observer: Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<FirefoxProfileEnrollment, CommandError> {
    if !config.companion_bind.ip().is_loopback()
        || config.descriptor_path.as_os_str().is_empty()
        || config.timeout.is_zero()
        || config.pairing_code_ttl.is_zero()
        || config.attachment_ttl.is_zero()
    {
        return Err(CommandError {
            code: ErrorCode::PolicyDenied,
            message: "Firefox enrollment configuration is invalid".into(),
            layer: ErrorLayer::Driver,
            retryable: false,
        });
    }
    let attempt = start_bootstrap_attempt(
        config.companion_bind,
        config.descriptor_path,
        config.pairing_code_ttl,
        config.attachment_ttl,
        pairing_code_observer,
    )
    .await?;
    Ok(FirefoxProfileEnrollment {
        attempt,
        timeout: config.timeout,
    })
}

impl TryFrom<FirefoxCompanionConfig> for FirefoxRuntimeConfig {
    type Error = anyhow::Error;

    fn try_from(config: FirefoxCompanionConfig) -> Result<Self> {
        let profile_id = ProfileId(uuid::Uuid::parse_str(&config.profile_id)?);
        let bidi_url = Url::parse(&config.bidi_url)?;
        let bidi_loopback = match bidi_url.host() {
            Some(url::Host::Ipv4(address)) => address.is_loopback(),
            Some(url::Host::Ipv6(address)) => address.is_loopback(),
            Some(url::Host::Domain("localhost")) => true,
            _ => false,
        };
        if bidi_url.scheme() != "ws" || !bidi_loopback {
            anyhow::bail!("Firefox BiDi URL must be a loopback ws URL");
        }
        let companion_bind: SocketAddr = config.companion_bind.parse()?;
        if !companion_bind.ip().is_loopback() {
            anyhow::bail!("Firefox companion bind address must be loopback");
        }
        if config.profile_dir.as_os_str().is_empty()
            || config.descriptor_path.as_os_str().is_empty()
        {
            anyhow::bail!("Firefox profile and native-host descriptor paths must not be empty");
        }
        if config.timeout_ms == 0
            || config.pairing_code_ttl_ms == 0
            || config.attachment_ttl_ms == 0
        {
            anyhow::bail!("Firefox companion durations must be positive");
        }
        Ok(Self {
            profile_id,
            bidi_url,
            profile_dir: config.profile_dir,
            companion_bind,
            descriptor_path: config.descriptor_path,
            timeout: Duration::from_millis(config.timeout_ms),
            pairing_code_ttl: Duration::from_millis(config.pairing_code_ttl_ms),
            attachment_ttl: Duration::from_millis(config.attachment_ttl_ms),
        })
    }
}

#[async_trait]
impl WorkerFactory for ConfiguredFirefoxFactory {
    async fn launch(&self, session_id: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        let _lifecycle = self.lifecycle.lock().await;
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(companion_error("Firefox factory is shut down"));
        }
        validate_enrolled_profile(&self.config.profile_dir)?;
        let warmed = self.server.lock().await.clone();
        if let Some(server) = warmed {
            // Recycling Firefox also restarts its native relay. Wait for
            // discovery after recovery so the grant uses the new connection.
            self.ensure_bidi_slot().await?;
            server
                .wait_for_discovery(&self.config.profile_id, self.config.timeout)
                .await
                .map_err(companion_error)
                .inspect_err(|error| {
                    tracing::warn!(
                        error = %error.message,
                        "firefox companion discovery wait failed"
                    );
                })?;
            return self.launch_with_server(&server, session_id).await;
        }

        let attempt = self.start_server().await.inspect_err(|error| {
            tracing::warn!(error = %error.message, "firefox companion bootstrap failed");
        })?;
        let server = attempt.server().clone();
        self.ensure_bidi_slot().await?;
        server
            .wait_for_discovery(&self.config.profile_id, self.config.timeout)
            .await
            .map_err(companion_error)
            .inspect_err(|error| {
                tracing::warn!(
                    error = %error.message,
                    "firefox companion pairing timed out waiting for extension discovery"
                );
            })?;
        let worker = self
            .launch_with_server(&server, session_id)
            .await
            .inspect_err(|error| {
                tracing::warn!(error = %error.message, "firefox companion worker launch failed");
            })?;
        let server = tokio::task::spawn_blocking(move || attempt.complete())
            .await
            .map_err(companion_error)?
            .map_err(companion_error)?;
        let mut slot = self.server.lock().await;
        if self.closed.load(std::sync::atomic::Ordering::Acquire) {
            return Err(companion_error("Firefox factory is shut down"));
        }
        *slot = Some(server);
        Ok(worker)
    }

    async fn shutdown(&self) {
        let _lifecycle = self.lifecycle.lock().await;
        self.closed
            .store(true, std::sync::atomic::Ordering::Release);
        let client = self.bidi.lock().await.take();
        if let Some(client) = client {
            // End the WebDriver session explicitly: Firefox's RemoteAgent
            // keeps it alive past connection loss, and with its one-session
            // limit a leaked session bricks every later `session.new` until
            // the browser restarts.
            if let Err(error) = client.end_session().await {
                tracing::warn!(error = %error.message, "firefox BiDi session end on shutdown failed");
            }
        }
        self.server.lock().await.take();
        self.profile_owner
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
}

impl ConfiguredFirefoxFactory {
    async fn launch_with_server(
        &self,
        server: &Arc<CompanionServerHandle>,
        session_id: &SessionId,
    ) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        let grant = server
            .grant_discovered_targets(&self.config.profile_id)
            .await
            .map_err(companion_error)?;
        let lease = server
            .registry()
            .resolve_attachment(&grant.attachment_id)
            .await
            .map_err(|error| companion_error(error.to_string()))?;
        if !self.required.are_met_by(&lease.capabilities) {
            return Err(CommandError {
                code: ErrorCode::PolicyDenied,
                message: "paired Firefox profile lacks required runtime capabilities".into(),
                layer: ErrorLayer::Driver,
                retryable: false,
            });
        }
        let observer = Arc::new(CompanionExtensionObserver::new(
            Arc::clone(server),
            self.config.timeout,
        ));
        // Firefox's RemoteAgent accepts exactly one active WebDriver session
        // per browser instance, so every worker on this profile multiplexes
        // over a single shared BiDi connection. A dead connection (browser
        // restart, transport failure) is re-established on the next launch.
        let bidi = self.shared_bidi().await?;
        FirefoxCompanionFactory::new(
            self.config.bidi_url.clone(),
            self.config.timeout,
            self.config.profile_dir.clone(),
            lease,
            observer,
        )
        .with_artifacts(self.artifacts.clone())
        .with_upload_roots(self.upload_roots.clone())
        .with_downloads_dir(self.downloads_dir.clone())
        .with_shared_transport(bidi)
        .launch(session_id)
        .await
    }

    async fn shared_bidi(&self) -> Result<crate::BidiClient, CommandError> {
        let mut slot = self.bidi.lock().await;
        if let Some(client) = slot.as_ref() {
            if client.is_alive() {
                return Ok(client.clone());
            }
        }
        let client = self.connect_bidi().await?;
        *slot = Some(client.clone());
        Ok(client)
    }

    /// The enrolled `bidiUrl` is a snapshot of the port Firefox held when the
    /// profile was paired, but Firefox binds its BiDi port per launch: a
    /// restart on another port leaves the snapshot pointing at nothing, and
    /// every session afterwards fails to connect with the enrollment looking
    /// healthy. The profile's own `WebDriverBiDiServer.json` is the live
    /// truth, so a refused connection retries against it once. Its parser
    /// enforces the same loopback rule the enrolled URL is held to.
    async fn connect_bidi(&self) -> Result<crate::BidiClient, CommandError> {
        let configured = live_endpoint_override(&self.config.profile_dir, &self.config.bidi_url)
            .unwrap_or_else(|| self.config.bidi_url.clone());
        let error =
            match crate::BidiClient::connect_session(configured.clone(), self.config.timeout).await
            {
                Ok(client) => return Ok(client),
                Err(error) => error,
            };
        if crate::bidi::session_slot_taken(&error) {
            tracing::warn!(
                url = %configured,
                "firefox BiDi session.new still blocked; recycling enrolled profile"
            );
            recycle_enrolled_firefox(&self.config).await?;
            let endpoint =
                live_endpoint_override(&self.config.profile_dir, &configured).unwrap_or(configured);
            return crate::BidiClient::connect_session(endpoint, self.config.timeout).await;
        }
        let mut live_error = None;
        if let Some(live) = live_endpoint_override(&self.config.profile_dir, &configured) {
            tracing::warn!(
                configured = %configured,
                live = %live,
                "enrolled Firefox BiDi endpoint unreachable; retrying on the profile's live endpoint"
            );
            match crate::BidiClient::connect_session(live.clone(), self.config.timeout).await {
                Ok(client) => return Ok(client),
                Err(error) => {
                    tracing::warn!(
                        live = %live,
                        error = %error.message,
                        "live Firefox BiDi endpoint also failed"
                    );
                    live_error = Some(error);
                }
            }
        }
        if connect_failure_needs_recycle(&error, live_error.as_ref()) {
            tracing::warn!(
                url = %configured,
                error = %error.message,
                "Firefox BiDi endpoint unreachable; recycling enrolled profile"
            );
            match recycle_enrolled_firefox(&self.config).await {
                Ok(()) => {
                    let retry_url = live_endpoint_override(&self.config.profile_dir, &configured)
                        .unwrap_or(configured);
                    return crate::BidiClient::connect_session(retry_url, self.config.timeout)
                        .await;
                }
                Err(recycle_error) => {
                    return Err(CommandError {
                        code: ErrorCode::BrowserLaunchFailed,
                        message: format!(
                            "{}; Firefox recycle after unreachable BiDi also failed: {}",
                            error.message, recycle_error.message
                        ),
                        layer: ErrorLayer::Driver,
                        retryable: true,
                    });
                }
            }
        }
        // A live endpoint that answered with its own failure is the more
        // current diagnosis; otherwise the enrolled failure stands.
        Err(live_error.unwrap_or(error))
    }

    async fn ensure_bidi_slot(&self) -> Result<(), CommandError> {
        // A live shared client occupies Firefox's one WebDriver session slot
        // by design. Probing that slot as though it belonged to another
        // process would recycle our own browser during a runtime restart.
        if self
            .bidi
            .lock()
            .await
            .as_ref()
            .is_some_and(crate::BidiClient::is_alive)
        {
            return Ok(());
        }
        let endpoint = live_endpoint_override(&self.config.profile_dir, &self.config.bidi_url)
            .unwrap_or_else(|| self.config.bidi_url.clone());
        match crate::bidi::session_slot_occupied(endpoint, self.config.timeout).await {
            Ok(false) => Ok(()),
            Ok(true) => {
                tracing::warn!(
                    url = %self.config.bidi_url,
                    "firefox BiDi session slot occupied; recycling enrolled profile"
                );
                recycle_enrolled_firefox(&self.config).await
            }
            // A refused probe with no live endpoint elsewhere means Firefox's
            // BiDi listener is down: recycle once instead of failing late.
            Err(error)
                if unreachable_probe_needs_recycle(
                    &self.config.profile_dir,
                    &self.config.bidi_url,
                    &error,
                ) =>
            {
                tracing::warn!(
                    url = %self.config.bidi_url,
                    error = %error.message,
                    "firefox BiDi endpoint probe failed; recycling enrolled profile"
                );
                recycle_enrolled_firefox(&self.config).await
            }
            Err(_) => Ok(()),
        }
    }

    async fn start_server(&self) -> Result<FirefoxBootstrapAttempt, CommandError> {
        start_bootstrap_attempt(
            self.config.companion_bind,
            self.config.descriptor_path.clone(),
            self.config.pairing_code_ttl,
            self.config.attachment_ttl,
            Arc::clone(&self.pairing_code_observer),
        )
        .await
    }
}

/// Bind companion servers for every Firefox entry in a selection and publish
/// their descriptors, without waiting for extension discovery. The
/// per-session launch path binds only for a 30s window, so an already-paired
/// extension polling on its own schedule never finds the endpoint. The CLI
/// calls this at serve startup and keeps the returned handles alive for the
/// serve's lifetime; a warm handle makes first session attach skip the
/// bootstrap entirely.
pub async fn warm_companion_servers(
    selection: &BrowserSelectionConfig,
) -> Vec<Arc<CompanionServerHandle>> {
    let mut handles = Vec::new();
    for firefox in &selection.firefox {
        let Ok(config) = FirefoxRuntimeConfig::try_from(firefox.clone()) else {
            continue;
        };
        let attempt = start_bootstrap_attempt(
            config.companion_bind,
            config.descriptor_path.clone(),
            config.pairing_code_ttl,
            config.attachment_ttl,
            Arc::new(|_| {}),
        )
        .await;
        match attempt {
            Ok(attempt) => match attempt.complete_keeping_publication() {
                Ok(server) => {
                    tracing::info!(
                        bind = %config.companion_bind,
                        descriptor = %config.descriptor_path.display(),
                        "firefox companion warm: endpoint and descriptor live"
                    );
                    handles.push(server);
                }
                Err(error) => {
                    tracing::warn!(%error, "firefox companion warm completion failed")
                }
            },
            Err(error) => {
                tracing::warn!(error = %error.message, "firefox companion warm bind failed")
            }
        }
    }
    handles
}

async fn start_bootstrap_attempt(
    companion_bind: SocketAddr,
    descriptor_path: PathBuf,
    pairing_code_ttl: Duration,
    attachment_ttl: Duration,
    pairing_code_observer: Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<FirefoxBootstrapAttempt, CommandError> {
    let server = match CompanionServer::bind_loopback(CompanionServerConfig {
        bind_addr: companion_bind,
        pairing_code_ttl,
        attachment_ttl,
    })
    .await
    {
        Ok(server) => Arc::new(server),
        Err(CompanionServerError::Bind { source, .. })
            if source.kind() == std::io::ErrorKind::AddrInUse =>
        {
            tracing::warn!(
                bind = %companion_bind,
                "configured companion port is taken; selecting a dynamic loopback port"
            );
            Arc::new(
                CompanionServer::bind_loopback(CompanionServerConfig {
                    bind_addr: SocketAddr::new(companion_bind.ip(), 0),
                    pairing_code_ttl,
                    attachment_ttl,
                })
                .await
                .map_err(companion_error)?,
            )
        }
        Err(error) => return Err(companion_error(error)),
    };
    let pairing_code = server.registry().issue_pairing_code().await;
    pairing_code_observer(&pairing_code);
    let descriptor = NativeHostDescriptor {
        endpoint: format!("ws://{}/v1/companion", server.local_addr()),
        pairing_code,
        ownership_id: uuid::Uuid::new_v4().to_string(),
    };
    let publication_path = descriptor_path.clone();
    let publication = tokio::task::spawn_blocking(move || {
        let _publication_lock = DescriptorLock::claim(&publication_path)?;
        remove_stale_descriptor(&publication_path)?;
        write_descriptor(&publication_path, &descriptor)
    })
    .await
    .map_err(companion_error)?
    .map_err(companion_error)?;
    Ok(FirefoxBootstrapAttempt {
        server: Some(server),
        publication: Some(publication),
        pairing_code_ttl,
    })
}

struct FirefoxBootstrapAttempt {
    server: Option<Arc<CompanionServerHandle>>,
    publication: Option<PublishedDescriptor>,
    pairing_code_ttl: Duration,
}

impl FirefoxBootstrapAttempt {
    fn server(&self) -> &Arc<CompanionServerHandle> {
        self.server.as_ref().expect("bootstrap server must exist")
    }

    fn complete(mut self) -> std::io::Result<Arc<CompanionServerHandle>> {
        if let Some(mut publication) = self.publication.take() {
            publication.cleanup()?;
        }
        Ok(self.server.take().expect("bootstrap server must exist"))
    }

    /// Complete while keeping the descriptor published: the native host
    /// reads it whenever the extension (or a manual Pair) connects, so the
    /// warm path must not unpublish on completion. The publication lives as
    /// long as the returned server handle.
    fn complete_keeping_publication(self) -> std::io::Result<Arc<CompanionServerHandle>> {
        self.complete_warm()
    }

    /// Complete and keep the descriptor fresh: the pairing code in the
    /// descriptor has a TTL (default 5 minutes), so the warm path re-issues
    /// it and rewrites the descriptor before expiry. Without this the warm
    /// companion goes stale and every extension connection gets a 401.
    fn complete_warm(mut self) -> std::io::Result<Arc<CompanionServerHandle>> {
        let server = self.server.take().expect("bootstrap server must exist");
        let publication = self.publication.take().expect("publication must exist");
        let registry = server.registry().clone();
        let ownership_id = publication.ownership_id().to_string();
        let pairing_code_ttl = self.pairing_code_ttl;
        let server_for_refresh = Arc::downgrade(&server);
        let refresh_task = tokio::spawn(async move {
            // Hold the initial publication for the task's lifetime: dropping
            // it would remove the descriptor while the warm server is live.
            let mut publication = publication;
            loop {
                tokio::time::sleep(pairing_code_ttl / 2).await;
                let Some(server) = server_for_refresh.upgrade() else {
                    break;
                };
                let code = registry.issue_pairing_code().await;
                let descriptor = NativeHostDescriptor {
                    endpoint: format!("ws://{}/v1/companion", server.local_addr()),
                    pairing_code: code,
                    ownership_id: ownership_id.clone(),
                };
                drop(server);
                let result = tokio::task::spawn_blocking(move || {
                    let result = refresh_publication(&mut publication, &descriptor);
                    (publication, result)
                })
                .await;
                match result {
                    Ok((current, result)) => {
                        publication = current;
                        match result {
                            Ok(true) => {}
                            Ok(false) => break,
                            Err(error) => {
                                tracing::warn!(%error, "firefox companion descriptor refresh failed")
                            }
                        }
                    }
                    Err(error) => {
                        tracing::warn!(%error, "firefox companion descriptor refresh task failed");
                        break;
                    }
                }
            }
        });
        server.retain_background_task(refresh_task);
        Ok(server)
    }
}

impl Drop for FirefoxBootstrapAttempt {
    fn drop(&mut self) {
        self.publication.take();
        self.server.take();
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, PartialEq, Eq)]
struct FileIdentity {
    device: u64,
    inode: u64,
}

/// The profile's live BiDi endpoint when it disagrees with the enrolled URL.
/// `None` means there is nothing better to try: no endpoint file, an
/// unreadable or non-loopback one, or one that already matches.
fn live_endpoint_override(profile_dir: &Path, configured: &Url) -> Option<Url> {
    crate::read_bidi_url_from_profile_dir(profile_dir)
        .ok()
        .filter(|live| live != configured)
}

fn bidi_listen_port(url: &Url) -> Option<u16> {
    url.port()
}

fn enrolled_firefox_bin() -> Option<PathBuf> {
    const CANDIDATES: &[&str] = &[
        "/Applications/Firefox Developer Edition.app/Contents/MacOS/firefox",
        "/Applications/Firefox.app/Contents/MacOS/firefox",
        "/Applications/Firefox Nightly.app/Contents/MacOS/firefox",
    ];
    CANDIDATES
        .iter()
        .map(PathBuf::from)
        .find(|path| path.is_file())
        .or_else(|| {
            std::env::var_os("PATH").and_then(|paths| {
                std::env::split_paths(&paths).find_map(|dir| {
                    ["firefox", "firefox-developer-edition", "firefox-nightly"]
                        .into_iter()
                        .map(|name| dir.join(name))
                        .find(|path| path.is_file())
                })
            })
        })
}

fn tcp_listen_pids(port: u16) -> Vec<u32> {
    let output = Command::new("lsof")
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-t"])
        .output();
    let Ok(output) = output else {
        return Vec::new();
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse().ok())
        .collect()
}

fn process_command(pid: u32) -> Option<String> {
    let output = Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "command="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let command = String::from_utf8_lossy(&output.stdout).trim().to_owned();
    if command.is_empty() {
        None
    } else {
        Some(command)
    }
}

fn is_firefox_command(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    lower.contains("firefox")
}

fn terminate_pid(_pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(_pid as i32, libc::SIGTERM);
    }
}

fn kill_pid(_pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(_pid as i32, libc::SIGKILL);
    }
}

fn command_owns_profile(command: &str, profile: &Path) -> bool {
    if !is_firefox_command(command) {
        return false;
    }
    ["--profile ", "-profile ", "--profile="]
        .iter()
        .any(|flag| {
            command
                .split_once(&format!("{flag}{}", profile.display()))
                .is_some_and(|(_, tail)| tail.is_empty() || tail.trim_start().starts_with('-'))
        })
}

fn terminate_firefox_listeners(port: u16, profile: &Path) -> Result<(), CommandError> {
    for pid in tcp_listen_pids(port) {
        let Some(command) = process_command(pid) else {
            continue;
        };
        if command_owns_profile(&command, profile) {
            terminate_pid(pid);
        }
    }
    Ok(())
}

async fn wait_until_port_free(
    port: u16,
    profile: &Path,
    timeout: Duration,
) -> Result<(), CommandError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining: Vec<u32> = tcp_listen_pids(port)
            .into_iter()
            .filter(|pid| {
                process_command(*pid).is_some_and(|command| command_owns_profile(&command, profile))
            })
            .collect();
        if remaining.is_empty() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            for pid in remaining {
                kill_pid(pid);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            if tcp_listen_pids(port).into_iter().all(|pid| {
                !process_command(pid).is_some_and(|command| command_owns_profile(&command, profile))
            }) {
                return Ok(());
            }
            return Err(companion_error(
                "Firefox did not release the BiDi port after recycle",
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn validate_enrolled_profile(profile: &Path) -> Result<(), CommandError> {
    let failure = |reason: String| {
        let mut error = companion_error(format!(
            "Firefox profile {} cannot be loaded: {reason}. Check the enrolled profile path and permissions before retrying",
            profile.display()
        ));
        error.retryable = false;
        error
    };
    let metadata = std::fs::metadata(profile).map_err(|error| failure(error.to_string()))?;
    if !metadata.is_dir() {
        return Err(failure("path is not a directory".into()));
    }
    std::fs::read_dir(profile).map_err(|error| failure(error.to_string()))?;
    // Firefox needs to write its lock and profile state. Check that ability
    // before terminating an existing browser or launching a dialog-only process.
    let probe = profile.join(format!(".bobby-write-check-{}", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(&probe)
        .map_err(|error| failure(error.to_string()))?;
    drop(file);
    std::fs::remove_file(probe).map_err(|error| failure(error.to_string()))?;
    Ok(())
}

fn claim_profile_owner(profile: &Path) -> Result<Option<std::fs::File>> {
    if !profile.exists() {
        return Ok(None);
    }
    let mut options = std::fs::OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(profile.join(".bobby-runtime.lock"))?;
    if !file.metadata()?.is_file() {
        anyhow::bail!("Firefox owner lock must be a regular file");
    }
    file.try_lock().map_err(|_| anyhow::anyhow!("Firefox profile {} already has a runtime owner; use the same team/project scope or a separately enrolled profile", profile.display()))?;
    Ok(Some(file))
}

fn spawn_enrolled_firefox(bin: &Path, profile: &Path, port: u16) -> Result<(), CommandError> {
    validate_enrolled_profile(profile)?;
    let mut command = Command::new(bin);
    command
        .arg("--no-remote")
        .arg("--foreground")
        .arg("--profile")
        .arg(profile)
        .arg(format!("--remote-debugging-port={port}"))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        unsafe {
            command.pre_exec(|| {
                if libc::setsid() == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
    }
    let child = command
        .spawn()
        .map_err(|error| companion_error(format!("failed to recycle enrolled Firefox: {error}")))?;
    std::mem::forget(child);
    Ok(())
}

/// Open an installed scope's profile on an OS-assigned BiDi port, preserving
/// its existing login state. Used by the CLI's first-run pairing flow.
pub async fn start_installed_firefox(
    profile: &Path,
    timeout: Duration,
) -> Result<Url, CommandError> {
    validate_enrolled_profile(profile)?;
    if let Ok(endpoint) = crate::read_bidi_url_from_profile_dir(profile) {
        if crate::bidi::session_slot_occupied(endpoint.clone(), Duration::from_secs(2))
            .await
            .is_ok()
        {
            return Ok(endpoint);
        }
    }
    let bin = enrolled_firefox_bin().ok_or_else(|| companion_error("Firefox binary not found"))?;
    spawn_enrolled_firefox(&bin, profile, 0)?;
    tokio::time::timeout(timeout, async {
        loop {
            if let Ok(endpoint) = crate::read_bidi_url_from_profile_dir(profile) {
                if crate::bidi::session_slot_occupied(endpoint.clone(), Duration::from_secs(1))
                    .await
                    .is_ok()
                {
                    return endpoint;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| companion_error("Firefox did not publish a live BiDi endpoint"))
}

async fn wait_until_bidi_slot_free(url: &Url, timeout: Duration) -> Result<(), CommandError> {
    let deadline = tokio::time::Instant::now() + timeout;
    let probe = timeout.min(Duration::from_secs(2));
    loop {
        match crate::bidi::session_slot_occupied(url.clone(), probe).await {
            Ok(false) => return Ok(()),
            Ok(true) => {}
            Err(_) => {}
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(companion_error(
                "recycled Firefox did not accept a new BiDi session",
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

/// Firefox BiDi-only sessions are not reconnectable after the owning socket
/// dies: RemoteAgent keeps the slot, and a new `/session` socket is sessionless.
/// The enrolled Bobby profile is dedicated automation Firefox, so recycling it
/// (same profile, same remote-debugging port, no re-pair) is the recovery.
async fn recycle_enrolled_firefox(config: &FirefoxRuntimeConfig) -> Result<(), CommandError> {
    validate_enrolled_profile(&config.profile_dir)?;
    let endpoint = live_endpoint_override(&config.profile_dir, &config.bidi_url)
        .unwrap_or_else(|| config.bidi_url.clone());
    let port = bidi_listen_port(&endpoint)
        .ok_or_else(|| companion_error("Firefox BiDi URL is missing a port"))?;
    let bin = enrolled_firefox_bin().ok_or_else(|| {
        companion_error("Firefox binary not found to recycle the leaked BiDi session")
    })?;
    terminate_firefox_listeners(port, &config.profile_dir)?;
    wait_until_port_free(port, &config.profile_dir, config.timeout).await?;
    let endpoint_file = config.profile_dir.join("WebDriverBiDiServer.json");
    match std::fs::symlink_metadata(&endpoint_file) {
        Ok(metadata) if metadata.is_file() => {
            std::fs::remove_file(endpoint_file).map_err(companion_error)?
        }
        Ok(_) => {
            return Err(companion_error(
                "Firefox endpoint file must be a regular file",
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(companion_error(error)),
    }
    spawn_enrolled_firefox(&bin, &config.profile_dir, 0)?;
    tokio::time::timeout(config.timeout, async {
        loop {
            if let Ok(endpoint) = crate::read_bidi_url_from_profile_dir(&config.profile_dir) {
                if wait_until_bidi_slot_free(&endpoint, config.timeout)
                    .await
                    .is_ok()
                {
                    return;
                }
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .map_err(|_| companion_error("recycled Firefox did not publish a live BiDi endpoint"))
}

fn companion_error(error: impl std::fmt::Display) -> CommandError {
    CommandError {
        code: ErrorCode::BrowserLaunchFailed,
        message: error.to_string(),
        layer: ErrorLayer::Driver,
        retryable: true,
    }
}

/// Recycle after a failed connect only when every endpoint the profile
/// offers refused the transport. A live endpoint that answered (for example
/// with a taken session slot) is a running Firefox, and recycling the
/// enrolled port would spawn a second one on the locked profile.
fn connect_failure_needs_recycle(
    enrolled_error: &CommandError,
    live_error: Option<&CommandError>,
) -> bool {
    bidi_endpoint_unreachable(enrolled_error) && live_error.is_none_or(bidi_endpoint_unreachable)
}

fn bidi_endpoint_unreachable(error: &CommandError) -> bool {
    if error.code != ErrorCode::BrowserLaunchFailed && error.code != ErrorCode::DeadlineExceeded {
        return false;
    }
    let message = error.message.to_ascii_lowercase();
    message.contains("failed to connect")
        || message.contains("connection refused")
        || message.contains("bidi connection deadline exceeded")
        || message.contains("os error 61")
        || message.contains("os error 111")
}

/// A refused probe of the enrolled URL means a dead browser only when the
/// profile has no live endpoint elsewhere; a moved listener is left to
/// `connect_bidi`, which retries on it.
fn unreachable_probe_needs_recycle(
    profile_dir: &Path,
    configured: &Url,
    error: &CommandError,
) -> bool {
    bidi_endpoint_unreachable(error) && live_endpoint_override(profile_dir, configured).is_none()
}

struct OwnedDescriptorFile {
    path: PathBuf,
    ownership_id: String,
    #[cfg(unix)]
    identity: FileIdentity,
}

impl OwnedDescriptorFile {
    fn capture(path: PathBuf, ownership_id: &str) -> std::io::Result<Self> {
        let metadata = std::fs::metadata(&path)?;
        #[cfg(unix)]
        let identity = {
            use std::os::unix::fs::MetadataExt;
            FileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            }
        };
        #[cfg(not(unix))]
        let _ = metadata;
        Ok(Self {
            path,
            ownership_id: ownership_id.to_owned(),
            #[cfg(unix)]
            identity,
        })
    }

    fn remove_if_owned(&self) -> std::io::Result<()> {
        let metadata = match std::fs::metadata(&self.path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(error),
        };
        #[cfg(unix)]
        let current = {
            use std::os::unix::fs::MetadataExt;
            FileIdentity {
                device: metadata.dev(),
                inode: metadata.ino(),
            }
        };
        #[cfg(unix)]
        if current != self.identity {
            return Ok(());
        }
        #[cfg(not(unix))]
        let _ = metadata;
        // An unlinked file's inode may be reused by a replacement publication.
        // File identity alone cannot establish ownership after that handoff.
        let owned = std::fs::read(&self.path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<NativeHostDescriptor>(&bytes).ok())
            .is_some_and(|descriptor| descriptor.ownership_id == self.ownership_id);
        if owned {
            std::fs::remove_file(&self.path)?;
        }
        Ok(())
    }
}

struct PublishedDescriptor {
    final_file: Option<OwnedDescriptorFile>,
    pending_file: Option<OwnedDescriptorFile>,
    path: PathBuf,
    ownership_id: String,
}

impl PublishedDescriptor {
    fn ownership_id(&self) -> &str {
        &self.ownership_id
    }

    fn cleanup(&mut self) -> std::io::Result<()> {
        if self.final_file.is_none() && self.pending_file.is_none() {
            return Ok(());
        }
        let _publication_lock = DescriptorLock::claim(&self.path)?;
        self.remove_owned_files()
    }

    fn remove_owned_files(&mut self) -> std::io::Result<()> {
        if let Some(final_file) = &self.final_file {
            final_file.remove_if_owned()?;
            self.final_file = None;
        }
        if let Some(pending_file) = &self.pending_file {
            pending_file.remove_if_owned()?;
            self.pending_file = None;
        }
        Ok(())
    }
}

fn refresh_publication(
    publication: &mut PublishedDescriptor,
    descriptor: &NativeHostDescriptor,
) -> std::io::Result<bool> {
    let lock = DescriptorLock::claim(&publication.path)?;
    let still_owned = std::fs::read(&publication.path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<NativeHostDescriptor>(&bytes).ok())
        .is_some_and(|current| current.ownership_id == publication.ownership_id);
    if !still_owned {
        return Ok(false);
    }
    remove_stale_descriptor(&publication.path)?;
    let refreshed = write_descriptor(&publication.path, descriptor)?;
    // The old final path now belongs to the replacement publication. Retain
    // any old pending-file ownership until it can be cleaned up under its lock.
    publication.final_file = None;
    let previous = std::mem::replace(publication, refreshed);
    drop(lock);
    drop(previous);
    Ok(true)
}

impl Drop for PublishedDescriptor {
    fn drop(&mut self) {
        if self.final_file.is_none() && self.pending_file.is_none() {
            return;
        }
        // Destructors can run on an async worker. Never wait for a lock there.
        if let Ok(_lock) = DescriptorLock::try_claim(&self.path) {
            if self.remove_owned_files().is_ok() {
                return;
            }
        }
        let path = self.path.clone();
        let files = [self.final_file.take(), self.pending_file.take()];
        let cleanup = move || {
            let result = (|| {
                let _lock = DescriptorLock::claim(&path)?;
                for file in files.into_iter().flatten() {
                    file.remove_if_owned()?;
                }
                Ok::<_, std::io::Error>(())
            })();
            if let Err(error) = result {
                tracing::warn!(%error, "firefox companion descriptor cleanup failed");
            }
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn_blocking(cleanup);
        } else {
            cleanup();
        }
    }
}

/// Serialize publication, refresh, and cleanup across runtimes. The lock file
/// remains on disk; the OS releases the lock even after an unclean exit.
struct DescriptorLock(std::fs::File);

impl DescriptorLock {
    fn claim(descriptor: &Path) -> std::io::Result<Self> {
        let file = Self::open(descriptor)?;
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self(file)),
                Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn try_claim(descriptor: &Path) -> std::io::Result<Self> {
        let file = Self::open(descriptor)?;
        file.try_lock().map_err(std::io::Error::from)?;
        Ok(Self(file))
    }

    fn open(descriptor: &Path) -> std::io::Result<std::fs::File> {
        if let Some(parent) = descriptor.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let path = descriptor.with_extension("lock");
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(path)?;
        if !file.metadata()?.is_file() {
            return Err(std::io::Error::other(
                "descriptor lock must be a regular file",
            ));
        }
        Ok(file)
    }
}

impl Drop for DescriptorLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

fn write_descriptor(
    path: &Path,
    descriptor: &NativeHostDescriptor,
) -> std::io::Result<PublishedDescriptor> {
    write_descriptor_with_pending_remove(path, descriptor, |pending| std::fs::remove_file(pending))
}

/// Recover from a descriptor leaked by a process that died mid-publication
/// (a SIGKILL cannot run `Drop`): remove a pre-existing descriptor only when
/// it parses as our own descriptor format, never a foreign file.
fn remove_stale_descriptor(path: &Path) -> std::io::Result<()> {
    match std::fs::read(path) {
        Ok(bytes) => {
            if serde_json::from_slice::<NativeHostDescriptor>(&bytes).is_ok() {
                std::fs::remove_file(path)
            } else {
                Err(std::io::Error::new(
                    std::io::ErrorKind::AlreadyExists,
                    "native-host descriptor path holds a foreign file",
                ))
            }
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn write_descriptor_with_pending_remove(
    path: &Path,
    descriptor: &NativeHostDescriptor,
    remove_pending: impl FnOnce(&Path) -> std::io::Result<()>,
) -> std::io::Result<PublishedDescriptor> {
    use std::io::Write;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let pending = path.with_extension(format!("pending-{}", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&pending)?;
        serde_json::to_writer(&mut file, descriptor)?;
        file.flush()?;
        file.sync_all()?;
        let pending_file = OwnedDescriptorFile::capture(pending.clone(), &descriptor.ownership_id)?;
        std::fs::hard_link(&pending, path)?;
        let mut publication = PublishedDescriptor {
            final_file: Some(OwnedDescriptorFile {
                path: path.to_path_buf(),
                ownership_id: pending_file.ownership_id.clone(),
                #[cfg(unix)]
                identity: pending_file.identity,
            }),
            pending_file: Some(pending_file),
            path: path.to_path_buf(),
            ownership_id: descriptor.ownership_id.clone(),
        };
        if remove_pending(&pending).is_ok() {
            publication.pending_file = None;
        }
        Ok(publication)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(pending);
    }
    result
}

pub fn compose_worker_factory(
    config: &AppConfig,
    selection: BrowserSelectionConfig,
) -> Result<Arc<dyn WorkerFactory>> {
    compose_worker_factory_with_pairing_observer(config, selection, Arc::new(|_| {}))
}

/// Compose with companion servers warmed: each Firefox factory binds its
/// companion endpoint and publishes its descriptor at startup, so a paired
/// extension discovers the server whenever it polls — the per-session
/// bootstrap's 30s discovery window never aligns with the extension's
/// schedule. Used by `bobby serve`; tests use the cold compose.
pub fn compose_worker_factory_warm(
    config: &AppConfig,
    selection: BrowserSelectionConfig,
) -> Result<Arc<dyn WorkerFactory>> {
    compose_worker_factory_inner(config, selection, Arc::new(|_| {}), None, true)
}

pub fn compose_worker_factory_with_pairing_observer(
    config: &AppConfig,
    selection: BrowserSelectionConfig,
    pairing_code_observer: Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<Arc<dyn WorkerFactory>> {
    compose_worker_factory_with_enrollment(config, selection, pairing_code_observer, None)
}

pub fn compose_worker_factory_with_enrolled_firefox(
    config: &AppConfig,
    selection: BrowserSelectionConfig,
    pairing_code_observer: Arc<dyn Fn(&str) + Send + Sync>,
    enrollment: EnrolledFirefoxProfile,
) -> Result<Arc<dyn WorkerFactory>> {
    compose_worker_factory_with_enrollment(
        config,
        selection,
        pairing_code_observer,
        Some(enrollment),
    )
}

fn compose_worker_factory_with_enrollment(
    config: &AppConfig,
    selection: BrowserSelectionConfig,
    pairing_code_observer: Arc<dyn Fn(&str) + Send + Sync>,
    enrollment: Option<EnrolledFirefoxProfile>,
) -> Result<Arc<dyn WorkerFactory>> {
    compose_worker_factory_inner(config, selection, pairing_code_observer, enrollment, false)
}

fn compose_worker_factory_inner(
    config: &AppConfig,
    selection: BrowserSelectionConfig,
    pairing_code_observer: Arc<dyn Fn(&str) + Send + Sync>,
    enrollment: Option<EnrolledFirefoxProfile>,
    warm: bool,
) -> Result<Arc<dyn WorkerFactory>> {
    let firefox_artifacts = ArtifactStore::new(
        config.browser.artifacts_dir.clone(),
        config.browser.max_artifact_bytes,
        config.browser.max_screenshot_dimension,
    );
    let firefox_upload_roots = config.browser.upload_roots.clone();
    let firefox_downloads_dir = config.browser.downloads_dir.clone();
    let chromium_capabilities = CompanionCapabilities {
        observe: true,
        navigate: true,
        native_input: true,
        tabs: true,
        frames: true,
        native_dialogs: false,
    };
    // A Chromium exact selection with a profile id opts into the same
    // durable-profile contract Firefox's enrolled companions carry: the
    // worker factory persists its user-data-dir instead of disposing it per
    // session, and EnginePreferenceConfig::durable_profile_id (read by the
    // caller before this function's `selection` is consumed) attaches
    // context-graph promotion under the same id.
    let chromium_durable_profile_id = match &selection.preference {
        EnginePreferenceConfig::Exact {
            engine: BrowserEngineConfig::Chromium,
            profile_id: Some(profile_id),
        } => Some(profile_id.clone()),
        _ => None,
    };
    let mut chromium_factory = ChromiumWorkerFactory::new(config.browser.clone());
    if let Some(profile_id) = chromium_durable_profile_id {
        chromium_factory = chromium_factory.with_durable_profile(profile_id);
    }
    let mut registrations = vec![FactoryRegistration::new(
        BrowserEngine::Chromium,
        None,
        chromium_capabilities,
        Arc::new(chromium_factory),
    )];
    let firefox_required = crate::required_extension_capabilities();
    let mut enrolled = enrollment.map(|enrollment| (enrollment.profile_id, enrollment.server));
    let firefox = selection
        .firefox
        .into_iter()
        .map(FirefoxRuntimeConfig::try_from)
        .map(|config| {
            config
                .map(|config| {
                    let server = match enrolled.take() {
                        Some((profile_id, server)) if profile_id == config.profile_id => {
                            Some(server)
                        }
                        Some(value) => {
                            enrolled = Some(value);
                            None
                        }
                        None => None,
                    };
                    let profile_owner = claim_profile_owner(&config.profile_dir)?;
                    Ok(FirefoxRegistration {
                        profile_id: config.profile_id.clone(),
                        factory: Arc::new(ConfiguredFirefoxFactory {
                            config,
                            required: firefox_required,
                            pairing_code_observer: Arc::clone(&pairing_code_observer),
                            server: Mutex::new(server),
                            artifacts: firefox_artifacts.clone(),
                            upload_roots: firefox_upload_roots.clone(),
                            downloads_dir: firefox_downloads_dir.clone(),
                            bidi: Mutex::new(None),
                            lifecycle: Mutex::new(()),
                            profile_owner: std::sync::Mutex::new(profile_owner),
                            closed: std::sync::atomic::AtomicBool::new(false),
                        }),
                    })
                })
                .and_then(|result| result)
        })
        .collect::<Result<Vec<_>>>()?;
    if enrolled.is_some() {
        anyhow::bail!("enrolled Firefox profile is not present in selection configuration");
    }
    // Warm companion servers into each factory's live slot: the per-session
    // launch path binds only for a 30s discovery window, so an already-paired
    // extension polling on its own schedule never aligns. The warm handle
    // lives in the factory's slot, so first session attach skips bootstrap.
    if warm {
        for registration in &firefox {
            let slot_free = registration
                .factory
                .server
                .try_lock()
                .map(|guard| guard.is_none())
                .unwrap_or(false);
            if !slot_free {
                continue;
            }
            let factory = Arc::clone(&registration.factory);
            tokio::spawn(async move {
                let _lifecycle = factory.lifecycle.lock().await;
                if factory.closed.load(std::sync::atomic::Ordering::Acquire) {
                    return;
                }
                let attempt = start_bootstrap_attempt(
                    factory.config.companion_bind,
                    factory.config.descriptor_path.clone(),
                    factory.config.pairing_code_ttl,
                    factory.config.attachment_ttl,
                    Arc::clone(&factory.pairing_code_observer),
                )
                .await;
                match attempt {
                    Ok(attempt) => match attempt.complete_warm() {
                        Ok(server) => {
                            let mut slot = factory.server.lock().await;
                            if factory.closed.load(std::sync::atomic::Ordering::Acquire) {
                                return;
                            }
                            *slot = Some(server);
                            tracing::info!(
                                bind = %factory.config.companion_bind,
                                "firefox companion warm: endpoint and descriptor live"
                            );
                        }
                        Err(error) => {
                            tracing::warn!(%error, "firefox companion warm completion failed")
                        }
                    },
                    Err(error) => {
                        tracing::warn!(error = %error.message, "firefox companion warm bind failed")
                    }
                }
            });
        }
    }
    registrations.extend(firefox.into_iter().map(|registration| {
        FactoryRegistration::negotiated(
            BrowserEngine::Firefox,
            Some(registration.profile_id),
            registration.factory,
        )
    }));
    let preference = preference(selection.preference)?;
    let selector = Arc::new(BrowserWorkerSelector::new(
        registrations,
        RequiredCapabilities::default(),
    ));
    if !selector.can_select(&preference) {
        anyhow::bail!(
            "browser engine preference {preference:?} cannot be satisfied by the configured worker \
             registrations: every session_create would fail. Configure a matching profile in \
             AUTOMATION_RUNTIME_BROWSER_SELECTION (for Firefox: profileId, bidiUrl, profileDir, \
             companionBind, descriptorPath) or change the preference."
        );
    }
    Ok(Arc::new(SelectedWorkerFactory::new(selector, preference)))
}

fn preference(config: EnginePreferenceConfig) -> Result<EnginePreference> {
    Ok(match config {
        EnginePreferenceConfig::ManagedChromium => EnginePreference::ManagedChromium,
        EnginePreferenceConfig::Exact { engine, profile_id } => {
            let engine = browser_engine(engine);
            let profile_id = match engine {
                // A Chromium exact selection's profile id already picked the
                // durable-vs-disposable user-data-dir when the worker
                // factory was composed (see chromium_durable_profile_id
                // above); there is only ever one Chromium registration, so
                // routing needs no profile_id to match against, and the
                // Firefox-only ProfileId (a UUID) cannot carry an arbitrary
                // Chromium profile name anyway.
                BrowserEngine::Chromium => None,
                _ => profile_id
                    .map(|value| uuid::Uuid::parse_str(&value).map(ProfileId))
                    .transpose()?,
            };
            EnginePreference::Exact { engine, profile_id }
        }
        EnginePreferenceConfig::Prefer { engines } => EnginePreference::Prefer {
            engines: engines.into_iter().map(browser_engine).collect(),
        },
    })
}

fn browser_engine(value: BrowserEngineConfig) -> BrowserEngine {
    match value {
        BrowserEngineConfig::Firefox => BrowserEngine::Firefox,
        BrowserEngineConfig::Chromium => BrowserEngine::Chromium,
        BrowserEngineConfig::WebKit => BrowserEngine::WebKit,
    }
}

pub fn parse_selection(value: Option<&str>) -> Result<BrowserSelectionConfig> {
    value
        .map(serde_json::from_str)
        .transpose()
        .map(|selection| selection.unwrap_or_default())
        .map_err(Into::into)
}

pub const SELECTION_ENV: &str = "AUTOMATION_RUNTIME_BROWSER_SELECTION";

/// Default loopback bind address written at companion install for later
/// enrollment (CLI or native-host `enrollProfile`).
pub const DEFAULT_COMPANION_BIND: &str = "127.0.0.1:9876";

/// Install-time defaults consumed by Task 5 native-host enroll and the CLI
/// enroll command when profile paths are not passed explicitly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct FirefoxEnrollDefaults {
    pub profile_dir: PathBuf,
    pub companion_bind: SocketAddr,
    pub descriptor_path: PathBuf,
}

pub fn enroll_defaults_path(config_dir: &Path) -> PathBuf {
    config_dir.join("firefox-enroll-defaults.json")
}

pub fn write_enroll_defaults(path: &Path, defaults: &FirefoxEnrollDefaults) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| anyhow::anyhow!("failed to create {}: {error}", parent.display()))?;
    }
    let pending = path.with_extension(format!("pending-{}", uuid::Uuid::new_v4()));
    let write = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&pending)?;
        serde_json::to_writer_pretty(&mut file, defaults)?;
        use std::io::Write;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&pending, path)?;
        Ok::<_, anyhow::Error>(())
    })();
    if write.is_err() {
        let _ = std::fs::remove_file(&pending);
    }
    write
}

pub fn read_enroll_defaults(path: &Path) -> Result<FirefoxEnrollDefaults> {
    let text = std::fs::read_to_string(path).map_err(|error| {
        anyhow::anyhow!("enroll defaults {} unreadable: {error}", path.display())
    })?;
    serde_json::from_str(&text)
        .map_err(|error| anyhow::anyhow!("enroll defaults {} is invalid: {error}", path.display()))
}

/// Where a resolved browser selection came from. Reported by `bobby doctor`
/// so operators can tell env overrides apart from the persisted enrollment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionSource {
    Environment,
    Persisted(PathBuf),
    Default,
}

/// Machine-local selection written by `bobby enroll-firefox-profile`, next to
/// the bootstrap credential. Every entry point (serve, gateway, doctor)
/// resolves through the same precedence, so configuration cannot diverge
/// between the process an operator validates and the process a host launches.
pub fn default_selection_path() -> Result<PathBuf> {
    Ok(config::bobby_config_dir()
        .ok_or_else(|| anyhow::anyhow!("config directory unavailable"))?
        .join("browser-selection.json"))
}

/// Resolve the browser selection: `AUTOMATION_RUNTIME_BROWSER_SELECTION`
/// wins, then the persisted enrollment, then the built-in default. A present
/// but malformed source is always an error — never silently ignored.
pub fn resolve_browser_selection() -> Result<(BrowserSelectionConfig, SelectionSource)> {
    if std::env::var_os(SELECTION_ENV).is_some() {
        return resolve_browser_selection_with(std::env::var(SELECTION_ENV).ok().as_deref(), None);
    }
    let persisted = default_selection_path()?;
    resolve_browser_selection_with(None, Some(&persisted))
}

pub fn resolve_browser_selection_with(
    env: Option<&str>,
    persisted_path: Option<&Path>,
) -> Result<(BrowserSelectionConfig, SelectionSource)> {
    if let Some(value) = env {
        let selection = parse_selection(Some(value))
            .map_err(|error| anyhow::anyhow!("{SELECTION_ENV} is invalid: {error:#}"))?;
        return Ok((selection, SelectionSource::Environment));
    }
    if let Some(path) = persisted_path {
        match std::fs::read_to_string(path) {
            Ok(text) => {
                let selection: BrowserSelectionConfig =
                    serde_json::from_str(&text).map_err(|error| {
                        anyhow::anyhow!(
                            "persisted browser selection {} is invalid: {error}",
                            path.display()
                        )
                    })?;
                return Ok((selection, SelectionSource::Persisted(path.to_path_buf())));
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(anyhow::anyhow!(
                    "persisted browser selection {} is unreadable: {error}",
                    path.display()
                ));
            }
        }
    }
    Ok((BrowserSelectionConfig::default(), SelectionSource::Default))
}

/// Build the browser-selection document produced after a successful Firefox
/// profile enrollment. Shared by the CLI enroll command and (Task 5) the
/// native-host `enrollProfile` handler so both paths emit identical wire JSON.
pub fn build_enrolled_browser_selection(
    profile_id: &ProfileId,
    bidi_url: &str,
    profile_dir: &Path,
    companion_bind: SocketAddr,
    descriptor_path: &Path,
) -> BrowserSelectionConfig {
    let profile_id = profile_id.0.to_string();
    BrowserSelectionConfig {
        preference: EnginePreferenceConfig::Exact {
            engine: BrowserEngineConfig::Firefox,
            profile_id: Some(profile_id.clone()),
        },
        firefox: vec![FirefoxCompanionConfig {
            profile_id,
            bidi_url: bidi_url.to_owned(),
            profile_dir: profile_dir.to_path_buf(),
            companion_bind: companion_bind.to_string(),
            descriptor_path: descriptor_path.to_path_buf(),
            timeout_ms: 30_000,
            pairing_code_ttl_ms: 300_000,
            attachment_ttl_ms: 300_000,
        }],
    }
}

/// Persist a selection so subsequent serve/gateway/doctor runs resolve it
/// without any environment wiring. Written atomically with owner-only
/// permissions on Unix: the contents locate a pairing endpoint and profile.
pub fn persist_browser_selection(path: &Path, selection: &BrowserSelectionConfig) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| anyhow::anyhow!("failed to create {}: {error}", parent.display()))?;
    }
    let pending = path.with_extension(format!("pending-{}", uuid::Uuid::new_v4()));
    let write = (|| {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options.open(&pending)?;
        serde_json::to_writer_pretty(&mut file, selection)?;
        use std::io::Write;
        file.flush()?;
        file.sync_all()?;
        // Close before renaming: Windows refuses to rename an open file.
        drop(file);
        std::fs::rename(&pending, path)?;
        Ok::<_, anyhow::Error>(())
    })();
    if write.is_err() {
        let _ = std::fs::remove_file(&pending);
    }
    write
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::SessionId;

    #[test]
    fn profile_owner_lock_prevents_takeover_and_releases_on_drop() {
        let root = tempfile::tempdir().unwrap();
        let first = claim_profile_owner(root.path()).unwrap();
        assert!(claim_profile_owner(root.path()).is_err());
        drop(first);
        assert!(claim_profile_owner(root.path()).unwrap().is_some());
    }

    #[test]
    fn firefox_recycle_matches_only_the_enrolled_profile() {
        let profile = Path::new("/profiles/team alpha");
        assert!(command_owns_profile("/Applications/Firefox/firefox --profile /profiles/team alpha --remote-debugging-port=1234", profile));
        assert!(!command_owns_profile("/Applications/Firefox/firefox --profile /profiles/team beta --remote-debugging-port=1234", profile));
        assert!(!command_owns_profile("/Applications/Firefox/firefox --profile /profiles/team alpha extra --remote-debugging-port=1234", profile));
        assert!(!command_owns_profile(
            "other-server --profile /profiles/team alpha --remote-debugging-port=1234",
            profile
        ));
    }

    fn write_endpoint(profile_dir: &Path, port: u16) {
        std::fs::write(
            profile_dir.join("WebDriverBiDiServer.json"),
            format!(r#"{{"ws_host":"127.0.0.1","ws_port":{port}}}"#),
        )
        .unwrap();
    }

    /// Firefox rebinds its BiDi port every launch, so the port frozen into
    /// `browser-selection.json` at enrollment goes stale on the next restart.
    #[test]
    fn a_relaunched_profile_on_another_port_overrides_the_enrolled_url() {
        let profile = tempfile::tempdir().unwrap();
        write_endpoint(profile.path(), 9224);
        let configured = Url::parse("ws://127.0.0.1:9222/session").unwrap();
        assert_eq!(
            live_endpoint_override(profile.path(), &configured)
                .as_ref()
                .map(Url::as_str),
            Some("ws://127.0.0.1:9224/session")
        );
    }

    #[test]
    fn an_agreeing_or_absent_endpoint_file_yields_no_override() {
        let profile = tempfile::tempdir().unwrap();
        let configured = Url::parse("ws://127.0.0.1:9222/session").unwrap();
        assert!(live_endpoint_override(profile.path(), &configured).is_none());
        write_endpoint(profile.path(), 9222);
        assert!(live_endpoint_override(profile.path(), &configured).is_none());
    }

    fn launch_error(code: ErrorCode, message: &str) -> CommandError {
        CommandError {
            code,
            message: message.into(),
            layer: ErrorLayer::Driver,
            retryable: true,
        }
    }

    #[test]
    fn only_transport_refusals_count_as_an_unreachable_bidi_endpoint() {
        for message in [
            "failed to connect to ws://127.0.0.1:9222/session",
            "Connection refused (os error 61)",
            "io error: os error 111",
            "BiDi connection deadline exceeded",
        ] {
            assert!(
                bidi_endpoint_unreachable(&launch_error(ErrorCode::BrowserLaunchFailed, message)),
                "{message}"
            );
        }
        assert!(bidi_endpoint_unreachable(&launch_error(
            ErrorCode::DeadlineExceeded,
            "bidi connection deadline exceeded"
        )));
        assert!(!bidi_endpoint_unreachable(&launch_error(
            ErrorCode::PolicyDenied,
            "connection refused"
        )));
        assert!(!bidi_endpoint_unreachable(&launch_error(
            ErrorCode::BrowserLaunchFailed,
            "session not created: maximum number of active sessions"
        )));
    }

    /// Discovery already proved Firefox is running, so a refused enrolled URL
    /// with a live endpoint file on another port is a moved listener, not a
    /// dead browser: recycling would spawn a second Firefox on a locked profile.
    #[test]
    fn a_refused_probe_recycles_only_when_no_live_endpoint_exists() {
        let profile = tempfile::tempdir().unwrap();
        let configured = Url::parse("ws://127.0.0.1:9222/session").unwrap();
        let refused = launch_error(ErrorCode::BrowserLaunchFailed, "connection refused");
        assert!(unreachable_probe_needs_recycle(
            profile.path(),
            &configured,
            &refused
        ));
        write_endpoint(profile.path(), 9224);
        assert!(!unreachable_probe_needs_recycle(
            profile.path(),
            &configured,
            &refused
        ));
        let unrelated = launch_error(ErrorCode::BrowserLaunchFailed, "protocol error");
        let empty = tempfile::tempdir().unwrap();
        assert!(!unreachable_probe_needs_recycle(
            empty.path(),
            &configured,
            &unrelated
        ));
    }

    /// Regression: every configured companion port is a preference, not a
    /// startup dependency. Keep the conflicting listener alive throughout.
    #[tokio::test]
    async fn occupied_companion_port_automatically_publishes_reachable_fallback() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = held.local_addr().unwrap();
        let root = tempfile::tempdir().unwrap();
        let descriptor = root.path().join("descriptor.json");
        let attempt = start_bootstrap_attempt(
            taken,
            descriptor.clone(),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Arc::new(|_| {}),
        )
        .await
        .expect("an occupied port must not block browser startup");
        let bound = attempt.server().local_addr();
        assert_eq!(bound.ip(), taken.ip());
        assert_ne!(bound.port(), taken.port());
        assert_ne!(bound.port(), 0);
        let published: NativeHostDescriptor =
            serde_json::from_slice(&std::fs::read(&descriptor).unwrap()).unwrap();
        assert_eq!(published.endpoint, format!("ws://{bound}/v1/companion"));
        assert!(tokio::net::TcpStream::connect(bound).await.is_ok());
        assert_eq!(held.local_addr().unwrap(), taken);
        drop(attempt);
        assert!(!descriptor.exists());
    }

    #[tokio::test]
    async fn port_zero_companion_bind_still_publishes_an_ephemeral_endpoint() {
        let descriptor_dir = tempfile::tempdir().unwrap();
        let descriptor = descriptor_dir
            .path()
            .join(format!("port-zero-{}.json", uuid::Uuid::new_v4()));
        let attempt = start_bootstrap_attempt(
            "127.0.0.1:0".parse().unwrap(),
            descriptor.clone(),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Arc::new(|_| {}),
        )
        .await
        .expect("port 0 should keep ephemeral binding support");
        let bound = attempt.server().local_addr();
        assert!(bound.ip().is_loopback());
        assert_ne!(bound.port(), 0);
        let published: NativeHostDescriptor =
            serde_json::from_slice(&std::fs::read(&descriptor).unwrap()).unwrap();
        assert_eq!(published.endpoint, format!("ws://{bound}/v1/companion"));
        drop(attempt);
        assert!(!descriptor.exists());
    }

    #[tokio::test]
    async fn enrollment_also_reassigns_an_occupied_companion_port() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = held.local_addr().unwrap();
        let root = tempfile::tempdir().unwrap();
        let descriptor = root.path().join("descriptor.json");
        let enrollment = start_firefox_profile_enrollment(
            FirefoxProfileEnrollmentConfig {
                companion_bind: taken,
                descriptor_path: descriptor.clone(),
                timeout: Duration::from_secs(1),
                pairing_code_ttl: Duration::from_secs(30),
                attachment_ttl: Duration::from_secs(30),
            },
            Arc::new(|_| {}),
        )
        .await
        .expect("enrollment must not require manual port repair");
        assert_ne!(enrollment.attempt.server().local_addr(), taken);
        assert!(descriptor.exists());
        drop(enrollment);
        assert!(!descriptor.exists());
    }

    #[tokio::test]
    async fn leftover_listener_and_descriptor_do_not_block_new_runtime() {
        let root = tempfile::tempdir().unwrap();
        let descriptor = root.path().join("descriptor.json");
        let first = start_bootstrap_attempt(
            "127.0.0.1:0".parse().unwrap(),
            descriptor.clone(),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
        let taken = first.server().local_addr();
        let second = start_bootstrap_attempt(
            taken,
            descriptor.clone(),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Arc::new(|_| {}),
        )
        .await
        .expect("a leftover listener must trigger automatic reassignment");
        let bound = second.server().local_addr();
        assert_ne!(bound, taken);
        let published: NativeHostDescriptor =
            serde_json::from_slice(&std::fs::read(&descriptor).unwrap()).unwrap();
        assert_eq!(published.endpoint, format!("ws://{bound}/v1/companion"));
        assert!(tokio::net::TcpStream::connect(taken).await.is_ok());
        drop(first);
        assert!(
            descriptor.exists(),
            "old cleanup must not remove the new endpoint"
        );
        drop(second);
        assert!(!descriptor.exists());
    }

    #[tokio::test]
    async fn occupied_port_replaces_stale_dynamic_descriptor() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let taken = held.local_addr().unwrap();
        let root = tempfile::tempdir().unwrap();
        let descriptor = root.path().join("descriptor.json");
        std::fs::write(
            &descriptor,
            serde_json::to_vec(&NativeHostDescriptor {
                endpoint: "ws://127.0.0.1:1/v1/companion".into(),
                pairing_code: "stale".into(),
                ownership_id: uuid::Uuid::new_v4().to_string(),
            })
            .unwrap(),
        )
        .unwrap();
        let attempt = start_bootstrap_attempt(
            taken,
            descriptor.clone(),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Arc::new(|_| {}),
        )
        .await
        .expect("stale descriptor must not disable port fallback");
        let published: NativeHostDescriptor =
            serde_json::from_slice(&std::fs::read(&descriptor).unwrap()).unwrap();
        assert_eq!(
            published.endpoint,
            format!("ws://{}/v1/companion", attempt.server().local_addr())
        );
        drop(attempt);
        assert!(!descriptor.exists());
    }

    #[tokio::test]
    async fn concurrent_launches_reassign_ports_without_descriptor_cleanup_races() {
        let root = tempfile::tempdir().unwrap();
        let descriptor = root.path().join("descriptor.json");
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let fixed = held.local_addr().unwrap();
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let tasks = (0..2)
            .map(|_| {
                let barrier = Arc::clone(&barrier);
                let descriptor = descriptor.clone();
                tokio::spawn(async move {
                    barrier.wait().await;
                    start_bootstrap_attempt(
                        fixed,
                        descriptor,
                        Duration::from_secs(30),
                        Duration::from_secs(30),
                        Arc::new(|_| {}),
                    )
                    .await
                    .unwrap()
                })
            })
            .collect::<Vec<_>>();
        barrier.wait().await;
        let mut attempts = Vec::new();
        for task in tasks {
            attempts.push(task.await.unwrap());
        }
        assert_ne!(
            attempts[0].server().local_addr(),
            attempts[1].server().local_addr()
        );
        let published: NativeHostDescriptor =
            serde_json::from_slice(&std::fs::read(&descriptor).unwrap()).unwrap();
        let owner = attempts
            .iter()
            .position(|attempt| {
                published.endpoint == format!("ws://{}/v1/companion", attempt.server().local_addr())
            })
            .unwrap();
        let current = attempts.swap_remove(owner);
        drop(attempts);
        assert!(descriptor.exists());
        drop(current);
        assert!(!descriptor.exists());
    }

    #[tokio::test]
    async fn descriptor_lock_contention_has_a_bounded_failure() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("descriptor.json");
        let held_path = path.clone();
        let (ready, started) = std::sync::mpsc::channel();
        let holder = std::thread::spawn(move || {
            let _lock = DescriptorLock::claim(&held_path).unwrap();
            ready.send(()).unwrap();
            std::thread::sleep(Duration::from_secs(2));
        });
        started.recv().unwrap();
        let result = tokio::task::spawn_blocking(move || DescriptorLock::claim(&path))
            .await
            .unwrap();
        holder.join().unwrap();
        assert!(
            result.is_err(),
            "a held descriptor lock must time out rather than wait indefinitely"
        );
    }

    #[tokio::test]
    async fn dropping_warm_server_releases_listener_and_descriptor() {
        let root = tempfile::tempdir().unwrap();
        let descriptor = root.path().join("descriptor.json");
        let server = start_bootstrap_attempt(
            "127.0.0.1:0".parse().unwrap(),
            descriptor.clone(),
            Duration::from_millis(80),
            Duration::from_secs(30),
            Arc::new(|_| {}),
        )
        .await
        .unwrap()
        .complete_warm()
        .unwrap();
        let addr = server.local_addr();
        drop(server);
        tokio::time::timeout(Duration::from_millis(300), async {
            while descriptor.exists() || tokio::net::TcpStream::connect(addr).await.is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("warm refresh must not retain the listener or descriptor after its owner drops");
    }

    #[tokio::test]
    async fn replaced_warm_runtime_cannot_republish_its_old_endpoint() {
        let root = tempfile::tempdir().unwrap();
        let descriptor = root.path().join("descriptor.json");
        let first = start_bootstrap_attempt(
            "127.0.0.1:0".parse().unwrap(),
            descriptor.clone(),
            Duration::from_millis(80),
            Duration::from_secs(30),
            Arc::new(|_| {}),
        )
        .await
        .unwrap()
        .complete_warm()
        .unwrap();
        let second = start_bootstrap_attempt(
            first.local_addr(),
            descriptor.clone(),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
        let expected = std::fs::read(&descriptor).unwrap();
        tokio::time::sleep(Duration::from_millis(160)).await;
        assert_eq!(
            std::fs::read(&descriptor).unwrap(),
            expected,
            "old pairing-code refresh must not steal the new descriptor"
        );
        drop(first);
        drop(second);
        assert!(!descriptor.exists());
    }

    #[test]
    fn a_live_endpoint_that_answered_blocks_the_recycle() {
        let refused = launch_error(ErrorCode::BrowserLaunchFailed, "connection refused");
        let slot_taken = launch_error(
            ErrorCode::BrowserLaunchFailed,
            "session not created: maximum number of active sessions",
        );
        assert!(connect_failure_needs_recycle(&refused, None));
        assert!(connect_failure_needs_recycle(&refused, Some(&refused)));
        assert!(!connect_failure_needs_recycle(&refused, Some(&slot_taken)));
        assert!(!connect_failure_needs_recycle(&slot_taken, None));
    }

    #[test]
    fn bidi_listen_port_reads_the_enrolled_websocket_port() {
        let url = Url::parse("ws://127.0.0.1:9224/session").unwrap();
        assert_eq!(bidi_listen_port(&url), Some(9224));
    }

    #[test]
    fn firefox_command_match_does_not_treat_unrelated_listeners_as_the_profile() {
        assert!(is_firefox_command(
            "/Applications/Firefox Developer Edition.app/Contents/MacOS/firefox --profile /tmp/p"
        ));
        assert!(!is_firefox_command("chrome --remote-debugging-port=9224"));
    }

    /// A malformed or non-loopback endpoint file must not become a connection
    /// target: the original failure stands.
    #[test]
    fn an_unusable_endpoint_file_yields_no_override() {
        let profile = tempfile::tempdir().unwrap();
        let configured = Url::parse("ws://127.0.0.1:9222/session").unwrap();
        std::fs::write(profile.path().join("WebDriverBiDiServer.json"), b"{").unwrap();
        assert!(live_endpoint_override(profile.path(), &configured).is_none());
        std::fs::write(
            profile.path().join("WebDriverBiDiServer.json"),
            br#"{"ws_host":"8.8.8.8","ws_port":9224}"#,
        )
        .unwrap();
        assert!(live_endpoint_override(profile.path(), &configured).is_none());
    }

    #[test]
    fn build_enrolled_browser_selection_matches_wire_shape() {
        let profile_id = ProfileId(uuid::Uuid::nil());
        let selection = build_enrolled_browser_selection(
            &profile_id,
            "ws://127.0.0.1:9222/session",
            Path::new("/tmp/firefox-profile"),
            "127.0.0.1:9876".parse().unwrap(),
            Path::new("/tmp/descriptor.json"),
        );
        let value = serde_json::to_value(&selection).unwrap();
        assert_eq!(value["preference"]["mode"], "exact");
        assert_eq!(value["preference"]["engine"], "firefox");
        assert_eq!(
            value["preference"]["profileId"],
            "00000000-0000-0000-0000-000000000000"
        );
        assert_eq!(
            value["firefox"][0]["bidiUrl"],
            "ws://127.0.0.1:9222/session"
        );
        assert_eq!(value["firefox"][0]["profileDir"], "/tmp/firefox-profile");
        assert_eq!(value["firefox"][0]["companionBind"], "127.0.0.1:9876");
        assert_eq!(
            value["firefox"][0]["descriptorPath"],
            "/tmp/descriptor.json"
        );
        assert_eq!(value["firefox"][0]["timeoutMs"], 30_000);
        assert_eq!(value["firefox"][0]["pairingCodeTtlMs"], 300_000);
        assert_eq!(value["firefox"][0]["attachmentTtlMs"], 300_000);
    }

    #[test]
    fn absent_selection_configuration_requires_firefox_without_fallback() {
        assert_eq!(
            parse_selection(None).unwrap().preference,
            EnginePreferenceConfig::Exact {
                engine: BrowserEngineConfig::Firefox,
                profile_id: None,
            }
        );
    }

    #[test]
    fn supplied_selection_configuration_is_consumed() {
        let parsed = parse_selection(Some(
            r#"{"preference":{"mode":"prefer","engines":["firefox","chromium"]}}"#,
        ))
        .unwrap();
        assert_eq!(
            parsed.preference,
            EnginePreferenceConfig::Prefer {
                engines: vec![BrowserEngineConfig::Firefox, BrowserEngineConfig::Chromium]
            }
        );
    }

    #[tokio::test]
    async fn unsatisfiable_exact_firefox_preference_fails_at_composition() {
        let config = AppConfig::default();
        let profile_id = ProfileId::new();
        let error = match compose_worker_factory(
            &config,
            BrowserSelectionConfig {
                preference: EnginePreferenceConfig::Exact {
                    engine: BrowserEngineConfig::Firefox,
                    profile_id: Some(profile_id.0.to_string()),
                },
                firefox: Vec::new(),
            },
        ) {
            Ok(_) => panic!("unsatisfiable exact Firefox preference unexpectedly composed"),
            Err(error) => error,
        };

        assert!(error.to_string().contains("cannot be satisfied"));
    }

    #[test]
    fn production_firefox_registration_requires_vertical_slice_capabilities() {
        let required = crate::required_extension_capabilities();
        assert!(required.observe);
        assert!(required.navigate);
        assert!(!required.native_input);
        assert!(required.tabs);
        assert!(required.frames);
        assert!(!required.native_dialogs);
    }

    #[tokio::test]
    async fn configured_firefox_rejects_missing_profile_without_publishing() {
        let profile_id = ProfileId::new();
        let test_dir = tempfile::tempdir().unwrap();
        let descriptor = test_dir
            .path()
            .join(format!("firefox-companion-{}.json", uuid::Uuid::new_v4()));
        let factory = compose_worker_factory(
            &AppConfig::default(),
            BrowserSelectionConfig {
                preference: EnginePreferenceConfig::Exact {
                    engine: BrowserEngineConfig::Firefox,
                    profile_id: Some(profile_id.0.to_string()),
                },
                firefox: vec![FirefoxCompanionConfig {
                    profile_id: profile_id.0.to_string(),
                    bidi_url: "ws://127.0.0.1:9222/session".into(),
                    profile_dir: test_dir.path().join("missing-profile"),
                    companion_bind: "127.0.0.1:0".into(),
                    descriptor_path: descriptor.clone(),
                    timeout_ms: 1,
                    pairing_code_ttl_ms: 1_000,
                    attachment_ttl_ms: 1_000,
                }],
            },
        )
        .unwrap();

        let error = match factory.launch(&SessionId::new()).await {
            Err(error) => error,
            Ok(_) => panic!("unpaired Firefox unexpectedly launched"),
        };
        assert_eq!(error.code, ErrorCode::BrowserLaunchFailed);
        assert!(error.message.contains("Firefox profile"), "{error:?}");
        assert!(!descriptor.exists());
        let _ = std::fs::remove_file(descriptor);
    }

    #[tokio::test]
    async fn shutdown_releases_warm_factory_and_cancels_pending_warm_start() {
        for wait_for_publication in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let descriptor = root.path().join("descriptor.json");
            let factory = compose_worker_factory_warm(
                &AppConfig::default(),
                BrowserSelectionConfig {
                    preference: EnginePreferenceConfig::Exact {
                        engine: BrowserEngineConfig::Firefox,
                        profile_id: None,
                    },
                    firefox: vec![FirefoxCompanionConfig {
                        profile_id: ProfileId::new().0.to_string(),
                        bidi_url: "ws://127.0.0.1:9222/session".into(),
                        profile_dir: root.path().to_path_buf(),
                        companion_bind: "127.0.0.1:0".into(),
                        descriptor_path: descriptor.clone(),
                        timeout_ms: 1000,
                        pairing_code_ttl_ms: 80,
                        attachment_ttl_ms: 1000,
                    }],
                },
            )
            .unwrap();
            let address = if wait_for_publication {
                Some(
                    tokio::time::timeout(Duration::from_secs(2), async {
                        loop {
                            if let Ok(bytes) = std::fs::read(&descriptor) {
                                if let Ok(value) =
                                    serde_json::from_slice::<NativeHostDescriptor>(&bytes)
                                {
                                    break Url::parse(&value.endpoint)
                                        .unwrap()
                                        .socket_addrs(|| None)
                                        .unwrap()[0];
                                }
                            }
                            tokio::task::yield_now().await;
                        }
                    })
                    .await
                    .unwrap(),
                )
            } else {
                None
            };
            factory.shutdown().await;
            // Keep the factory alive: shutdown itself must release ownership.
            tokio::time::timeout(Duration::from_secs(2), async {
                loop {
                    let listening = match address {
                        Some(address) => tokio::net::TcpStream::connect(address).await.is_ok(),
                        None => false,
                    };
                    if !descriptor.exists() && !listening {
                        break;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("shutdown leaked the warm listener or descriptor");
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert!(
                !descriptor.exists(),
                "pending warm start republished after shutdown"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn firefox_recovery_rejects_invalid_profiles_before_spawning() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("firefox");
        let marker = root.path().join("spawned");
        std::fs::write(&bin, format!("#!/bin/sh\ntouch '{}'\n", marker.display())).unwrap();
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o700)).unwrap();
        let missing = root.path().join("missing-profile");
        let file = root.path().join("not-a-directory");
        std::fs::write(&file, b"keep").unwrap();
        for profile in [&missing, &file] {
            let error = spawn_enrolled_firefox(&bin, profile, 9224)
                .expect_err("invalid profile must not launch Firefox");
            assert!(error.message.contains("Firefox profile"), "{error:?}");
        }
        assert!(!marker.exists());
        assert!(!missing.exists());
        assert_eq!(std::fs::read(file).unwrap(), b"keep");
    }

    #[tokio::test]
    async fn bootstrap_recovers_a_descriptor_leaked_by_a_killed_process() {
        let descriptor_dir = tempfile::tempdir().unwrap();
        let descriptor = descriptor_dir
            .path()
            .join(format!("stale-descriptor-{}.json", uuid::Uuid::new_v4()));
        let attempt = start_bootstrap_attempt(
            "127.0.0.1:0".parse().unwrap(),
            descriptor.clone(),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
        assert!(descriptor.exists());
        std::mem::forget(attempt);

        let recovered = start_bootstrap_attempt(
            "127.0.0.1:0".parse().unwrap(),
            descriptor.clone(),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Arc::new(|_| {}),
        )
        .await;
        let attempt = recovered.unwrap_or_else(|error| {
            panic!("stale descriptor was not recovered: {}", error.message)
        });
        drop(attempt);
        assert!(!descriptor.exists());

        std::fs::write(&descriptor, b"not-a-descriptor").unwrap();
        let foreign = start_bootstrap_attempt(
            "127.0.0.1:0".parse().unwrap(),
            descriptor.clone(),
            Duration::from_secs(30),
            Duration::from_secs(30),
            Arc::new(|_| {}),
        )
        .await;
        assert!(foreign.is_err());
        assert_eq!(std::fs::read(&descriptor).unwrap(), b"not-a-descriptor");
        let _ = std::fs::remove_file(&descriptor);
    }

    #[tokio::test]
    async fn cancelled_firefox_bootstrap_removes_descriptor_and_listener() {
        let descriptor_dir = tempfile::tempdir().unwrap();
        let descriptor = descriptor_dir.path().join(format!(
            "cancelled-firefox-companion-{}.json",
            uuid::Uuid::new_v4()
        ));
        let task = tokio::spawn({
            let descriptor = descriptor.clone();
            async move {
                let _attempt = start_bootstrap_attempt(
                    "127.0.0.1:0".parse().unwrap(),
                    descriptor,
                    Duration::from_secs(30),
                    Duration::from_secs(30),
                    Arc::new(|_| {}),
                )
                .await
                .unwrap();
                std::future::pending::<()>().await;
            }
        });
        let descriptor_ready = tokio::time::timeout(Duration::from_secs(1), async {
            while !descriptor.exists() && !task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        descriptor_ready.expect("bootstrap did not publish its descriptor");
        assert!(!task.is_finished(), "bootstrap exited before cancellation");
        let published: NativeHostDescriptor =
            serde_json::from_slice(&std::fs::read(&descriptor).unwrap()).unwrap();
        let address: SocketAddr = Url::parse(&published.endpoint)
            .unwrap()
            .socket_addrs(|| None)
            .unwrap()[0];
        task.abort();
        let _ = task.await;
        tokio::time::timeout(Duration::from_secs(1), async {
            while descriptor.exists() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled bootstrap leaked its descriptor");
        tokio::time::timeout(Duration::from_secs(1), async {
            while tokio::net::TcpStream::connect(address).await.is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("cancelled bootstrap leaked its listener");
    }

    #[test]
    fn remote_firefox_endpoints_fail_closed_during_composition() {
        let profile_id = ProfileId::new();
        let result = compose_worker_factory(
            &AppConfig::default(),
            BrowserSelectionConfig {
                preference: EnginePreferenceConfig::ManagedChromium,
                firefox: vec![FirefoxCompanionConfig {
                    profile_id: profile_id.0.to_string(),
                    bidi_url: "ws://example.com/session".into(),
                    profile_dir: PathBuf::from("/profiles/default-release"),
                    companion_bind: "127.0.0.1:0".into(),
                    descriptor_path: PathBuf::from("target/firefox-companion.json"),
                    timeout_ms: 1,
                    pairing_code_ttl_ms: 1_000,
                    attachment_ttl_ms: 1_000,
                }],
            },
        );
        assert!(result.is_err());
    }

    #[test]
    fn descriptor_publication_never_clobbers_an_existing_destination() {
        let test_dir = tempfile::tempdir().unwrap();
        let path = test_dir.path().join(format!(
            "existing-firefox-descriptor-{}.json",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let original = b"operator-owned-state";
        std::fs::write(&path, original).unwrap();
        let result = write_descriptor(
            &path,
            &NativeHostDescriptor {
                endpoint: "ws://127.0.0.1:1234/v1/companion".into(),
                pairing_code: "must-not-replace".into(),
                ownership_id: uuid::Uuid::new_v4().to_string(),
            },
        );
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("existing descriptor was unexpectedly replaced"),
        };
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).unwrap(), original);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn concurrent_descriptor_publication_has_one_owner_and_never_clobbers() {
        let test_dir = tempfile::tempdir().unwrap();
        let path = test_dir.path().join(format!(
            "concurrent-firefox-descriptor-{}.json",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let attempts = (0..2)
            .map(|index| {
                let path = path.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let descriptor = NativeHostDescriptor {
                        endpoint: format!("ws://127.0.0.1:{}/v1/companion", 1200 + index),
                        pairing_code: format!("pairing-{index}"),
                        ownership_id: uuid::Uuid::new_v4().to_string(),
                    };
                    barrier.wait();
                    write_descriptor(&path, &descriptor)
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        let results = attempts
            .into_iter()
            .map(|attempt| attempt.join().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        assert_eq!(
            results
                .iter()
                .filter_map(|result| result.as_ref().err())
                .filter(|error| error.kind() == std::io::ErrorKind::AlreadyExists)
                .count(),
            1
        );
        drop(results);
        assert!(!path.exists());
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_publication_never_follows_an_existing_symlink() {
        let test_dir = tempfile::tempdir().unwrap();
        let root = test_dir.path().join(format!(
            "symlink-firefox-descriptor-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let destination = root.join("destination.json");
        let target = root.join("operator.json");
        std::fs::write(&target, b"operator-owned").unwrap();
        std::os::unix::fs::symlink(&target, &destination).unwrap();
        let result = write_descriptor(
            &destination,
            &NativeHostDescriptor {
                endpoint: "ws://127.0.0.1:1234/v1/companion".into(),
                pairing_code: "must-not-write".into(),
                ownership_id: uuid::Uuid::new_v4().to_string(),
            },
        );
        let error = match result {
            Err(error) => error,
            Ok(_) => panic!("symlink destination was unexpectedly replaced"),
        };
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&target).unwrap(), b"operator-owned");
        std::fs::remove_file(destination).unwrap();
        std::fs::remove_file(target).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn published_descriptor_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let test_dir = tempfile::tempdir().unwrap();
        let path = test_dir.path().join(format!(
            "private-firefox-descriptor-{}.json",
            uuid::Uuid::new_v4()
        ));
        let publication = write_descriptor(
            &path,
            &NativeHostDescriptor {
                endpoint: "ws://127.0.0.1:1234/v1/companion".into(),
                pairing_code: "private-pairing-material".into(),
                ownership_id: uuid::Uuid::new_v4().to_string(),
            },
        )
        .unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        drop(publication);
        assert!(!path.exists());
    }

    #[test]
    fn descriptor_cleanup_preserves_a_new_owner_on_the_same_inode() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("descriptor.json");
        let publication = write_descriptor(
            &path,
            &NativeHostDescriptor {
                endpoint: "ws://127.0.0.1:1234/v1/companion".into(),
                pairing_code: "original-material".into(),
                ownership_id: uuid::Uuid::new_v4().to_string(),
            },
        )
        .unwrap();
        let replacement = serde_json::to_vec(&NativeHostDescriptor {
            endpoint: "ws://127.0.0.1:5678/v1/companion".into(),
            pairing_code: "replacement-material".into(),
            ownership_id: uuid::Uuid::new_v4().to_string(),
        })
        .unwrap();
        // Overwrite in place to deterministically exercise matching file identity
        // with a different owner, as can happen after Linux reuses an inode.
        std::fs::write(&path, &replacement).unwrap();
        drop(publication);
        assert_eq!(std::fs::read(&path).unwrap(), replacement);
    }

    #[test]
    fn descriptor_cleanup_preserves_a_replacement_file() {
        let test_dir = tempfile::tempdir().unwrap();
        let path = test_dir.path().join(format!(
            "replaced-firefox-descriptor-{}.json",
            uuid::Uuid::new_v4()
        ));
        let publication = write_descriptor(
            &path,
            &NativeHostDescriptor {
                endpoint: "ws://127.0.0.1:1234/v1/companion".into(),
                pairing_code: "owned-material".into(),
                ownership_id: uuid::Uuid::new_v4().to_string(),
            },
        )
        .unwrap();
        let original_len = std::fs::metadata(&path).unwrap().len() as usize;
        let replacement_bytes = vec![b'x'; original_len];
        let replacement = path.with_extension("replacement");
        std::fs::write(&replacement, &replacement_bytes).unwrap();
        std::fs::rename(&replacement, &path).unwrap();
        drop(publication);
        assert_eq!(std::fs::read(&path).unwrap(), replacement_bytes);
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn pending_unlink_failure_keeps_every_secret_file_owned() {
        let test_dir = tempfile::tempdir().unwrap();
        let path = test_dir.path().join(format!(
            "unlink-failure-firefox-descriptor-{}.json",
            uuid::Uuid::new_v4()
        ));
        let publication = write_descriptor_with_pending_remove(
            &path,
            &NativeHostDescriptor {
                endpoint: "ws://127.0.0.1:1234/v1/companion".into(),
                pairing_code: "owned-after-unlink-failure".into(),
                ownership_id: uuid::Uuid::new_v4().to_string(),
            },
            |_| {
                Err(std::io::Error::new(
                    std::io::ErrorKind::PermissionDenied,
                    "injected",
                ))
            },
        )
        .unwrap();
        let pending = publication
            .pending_file
            .as_ref()
            .expect("failed unlink must retain pending ownership")
            .path
            .clone();
        assert!(path.exists());
        assert!(pending.exists());
        drop(publication);
        assert!(!path.exists());
        assert!(!pending.exists());
    }

    #[test]
    fn selection_resolution_prefers_env_then_persisted_then_default() {
        let root = tempfile::tempdir().unwrap();
        let persisted = root.path().join("browser-selection.json");
        std::fs::write(
            &persisted,
            r#"{"preference":{"mode":"prefer","engines":["chromium","firefox"]}}"#,
        )
        .unwrap();

        let (selection, source) = resolve_browser_selection_with(
            Some(r#"{"preference":{"mode":"managedChromium"}}"#),
            Some(&persisted),
        )
        .unwrap();
        assert_eq!(source, SelectionSource::Environment);
        assert_eq!(
            selection.preference,
            EnginePreferenceConfig::ManagedChromium
        );

        let (selection, source) = resolve_browser_selection_with(None, Some(&persisted)).unwrap();
        assert_eq!(source, SelectionSource::Persisted(persisted.clone()));
        assert_eq!(
            selection.preference,
            EnginePreferenceConfig::Prefer {
                engines: vec![BrowserEngineConfig::Chromium, BrowserEngineConfig::Firefox]
            }
        );

        let missing = root.path().join("absent.json");
        let (selection, source) = resolve_browser_selection_with(None, Some(&missing)).unwrap();
        assert_eq!(source, SelectionSource::Default);
        assert_eq!(
            selection.preference,
            EnginePreferenceConfig::Exact {
                engine: BrowserEngineConfig::Firefox,
                profile_id: None,
            }
        );
    }

    #[test]
    fn selection_resolution_fails_closed_on_malformed_sources() {
        let root = tempfile::tempdir().unwrap();
        let persisted = root.path().join("browser-selection.json");
        std::fs::write(&persisted, r#"{"preference":{"mode":"managedChromium"}}"#).unwrap();

        let error = resolve_browser_selection_with(Some("{not json"), Some(&persisted))
            .expect_err("malformed env must fail even with a valid persisted selection");
        assert!(error.to_string().contains(SELECTION_ENV));

        std::fs::write(&persisted, "{not json").unwrap();
        let error = resolve_browser_selection_with(None, Some(&persisted))
            .expect_err("malformed persisted selection must fail");
        assert!(error.to_string().contains("persisted browser selection"));
    }

    #[cfg(unix)]
    #[test]
    fn persisted_selection_roundtrips_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("nested").join("browser-selection.json");
        let selection = BrowserSelectionConfig {
            preference: EnginePreferenceConfig::ManagedChromium,
            firefox: Vec::new(),
        };

        persist_browser_selection(&path, &selection).unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let (resolved, source) = resolve_browser_selection_with(None, Some(&path)).unwrap();
        assert_eq!(source, SelectionSource::Persisted(path));
        assert_eq!(resolved, selection);
    }

    #[cfg(unix)]
    #[test]
    fn enroll_defaults_roundtrip_with_owner_only_permissions() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("firefox-enroll-defaults.json");
        let defaults = FirefoxEnrollDefaults {
            profile_dir: root.path().join("firefox-profile"),
            companion_bind: DEFAULT_COMPANION_BIND.parse().unwrap(),
            descriptor_path: root.path().join("firefox-native-host-descriptor.json"),
        };

        write_enroll_defaults(&path, &defaults).unwrap();

        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(read_enroll_defaults(&path).unwrap(), defaults);
    }
}
