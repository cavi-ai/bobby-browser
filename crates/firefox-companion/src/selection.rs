//! Browser engine selection and worker factory composition shared by the
//! `bobby` CLI and the stdio MCP gateway: parses the
//! `AUTOMATION_RUNTIME_BROWSER_SELECTION` value, registers the configured
//! Firefox companion profiles alongside managed Chromium, and resolves the
//! engine preference into a single [`WorkerFactory`].

use anyhow::Result;
use artifact_store::ArtifactStore;
use async_trait::async_trait;
use companion_core::{CompanionServer, CompanionServerConfig, CompanionServerHandle};
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
        let server = self
            .attempt
            .complete()
            .map_err(|error| companion_error(error.to_string()))?;
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
        self.ensure_bidi_slot().await?;
        let mut live_server = self.server.lock().await;
        if let Some(server) = live_server.as_ref() {
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
            return self.launch_with_server(server, session_id).await;
        }

        let attempt = self.start_server().await.inspect_err(|error| {
            tracing::warn!(error = %error.message, "firefox companion bootstrap failed");
        })?;
        let server = attempt.server();
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
            .launch_with_server(server, session_id)
            .await
            .inspect_err(|error| {
                tracing::warn!(error = %error.message, "firefox companion worker launch failed");
            })?;
        let server = attempt.complete().map_err(companion_error)?;
        *live_server = Some(server);
        Ok(worker)
    }

    async fn shutdown(&self) {
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
        let configured = self.config.bidi_url.clone();
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
            return crate::BidiClient::connect_session(configured, self.config.timeout).await;
        }
        let Some(live) = live_endpoint_override(&self.config.profile_dir, &configured) else {
            // No live endpoint file, or it agrees with the enrolled URL: the
            // first failure is the real one.
            return Err(error);
        };
        tracing::warn!(
            configured = %configured,
            live = %live,
            "enrolled Firefox BiDi endpoint unreachable; retrying on the profile's live endpoint"
        );
        crate::BidiClient::connect_session(live, self.config.timeout).await
    }

    async fn ensure_bidi_slot(&self) -> Result<(), CommandError> {
        match crate::bidi::session_slot_occupied(self.config.bidi_url.clone(), self.config.timeout)
            .await
        {
            Ok(false) => Ok(()),
            Ok(true) => {
                tracing::warn!(
                    url = %self.config.bidi_url,
                    "firefox BiDi session slot occupied; recycling enrolled profile"
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
    let server = Arc::new(
        CompanionServer::bind_loopback(CompanionServerConfig {
            bind_addr: companion_bind,
            pairing_code_ttl,
            attachment_ttl,
        })
        .await
        .map_err(|error| companion_error(error.to_string()))?,
    );
    let pairing_code = server.registry().issue_pairing_code().await;
    pairing_code_observer(&pairing_code);
    let descriptor = NativeHostDescriptor {
        endpoint: format!("ws://{}/v1/companion", server.local_addr()),
        pairing_code,
        ownership_id: uuid::Uuid::new_v4().to_string(),
    };
    remove_stale_descriptor(&descriptor_path)
        .map_err(|error| companion_error(error.to_string()))?;
    let publication = write_descriptor(&descriptor_path, &descriptor)
        .map_err(|error| companion_error(error.to_string()))?;
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
    fn complete_keeping_publication(mut self) -> std::io::Result<Arc<CompanionServerHandle>> {
        let server = self.server.take().expect("bootstrap server must exist");
        // Leak the publication into the server's lifetime: dropping the
        // attempt would unpublish, so forget it deliberately.
        std::mem::forget(self);
        Ok(server)
    }

    /// Complete and keep the descriptor fresh: the pairing code in the
    /// descriptor has a TTL (default 5 minutes), so the warm path re-issues
    /// it and rewrites the descriptor before expiry. Without this the warm
    /// companion goes stale and every extension connection gets a 401.
    fn complete_warm(mut self) -> std::io::Result<Arc<CompanionServerHandle>> {
        let server = self.server.take().expect("bootstrap server must exist");
        let publication = self.publication.take().expect("publication must exist");
        let registry = server.registry().clone();
        let descriptor_path = publication.path().to_path_buf();
        let ownership_id = publication.ownership_id().to_string();
        let pairing_code_ttl = self.pairing_code_ttl;
        let server_for_refresh = server.clone();
        std::mem::forget(self);
        tokio::spawn(async move {
            // Hold the initial publication for the task's lifetime: dropping
            // it would remove the descriptor while the warm server is live.
            let _initial = publication;
            loop {
                tokio::time::sleep(pairing_code_ttl / 2).await;
                let code = registry.issue_pairing_code().await;
                let descriptor = NativeHostDescriptor {
                    endpoint: format!("ws://{}/v1/companion", server_for_refresh.local_addr()),
                    pairing_code: code,
                    ownership_id: ownership_id.clone(),
                };
                // The write path uses create_new, so the prior descriptor
                // must be removed first — otherwise every refresh fails
                // and the pairing code goes stale.
                if let Err(error) = remove_stale_descriptor(&descriptor_path) {
                    tracing::warn!(%error, "firefox companion descriptor refresh: remove failed");
                    continue;
                }
                match write_descriptor(&descriptor_path, &descriptor) {
                    Ok(refreshed) => {
                        // Forget rather than drop: dropping would remove the
                        // file just written (the non-unix fallback matches on
                        // the shared ownership_id).
                        std::mem::forget(refreshed);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "firefox companion descriptor refresh failed");
                    }
                }
            }
        });
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

fn terminate_pid(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGTERM);
    }
}

fn kill_pid(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as i32, libc::SIGKILL);
    }
}

fn terminate_firefox_listeners(port: u16) -> Result<(), CommandError> {
    for pid in tcp_listen_pids(port) {
        let Some(command) = process_command(pid) else {
            continue;
        };
        if !is_firefox_command(&command) {
            return Err(companion_error(format!(
                "Firefox BiDi port {port} is held by a non-Firefox process"
            )));
        }
        terminate_pid(pid);
    }
    Ok(())
}

async fn wait_until_port_free(port: u16, timeout: Duration) -> Result<(), CommandError> {
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        let remaining: Vec<u32> = tcp_listen_pids(port)
            .into_iter()
            .filter(|pid| process_command(*pid).is_some_and(|command| is_firefox_command(&command)))
            .collect();
        if remaining.is_empty() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            for pid in remaining {
                kill_pid(pid);
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
            if tcp_listen_pids(port).is_empty() {
                return Ok(());
            }
            return Err(companion_error(
                "Firefox did not release the BiDi port after recycle",
            ));
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
}

fn spawn_enrolled_firefox(bin: &Path, profile: &Path, port: u16) -> Result<(), CommandError> {
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
    let port = bidi_listen_port(&config.bidi_url)
        .ok_or_else(|| companion_error("Firefox BiDi URL is missing a port"))?;
    let bin = enrolled_firefox_bin().ok_or_else(|| {
        companion_error("Firefox binary not found to recycle the leaked BiDi session")
    })?;
    terminate_firefox_listeners(port)?;
    wait_until_port_free(port, config.timeout).await?;
    spawn_enrolled_firefox(&bin, &config.profile_dir, port)?;
    wait_until_bidi_slot_free(&config.bidi_url, config.timeout).await
}

fn companion_error(error: impl std::fmt::Display) -> CommandError {
    CommandError {
        code: ErrorCode::BrowserLaunchFailed,
        message: error.to_string(),
        layer: ErrorLayer::Driver,
        retryable: true,
    }
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
        let owned = current == self.identity;
        #[cfg(not(unix))]
        let owned = {
            let _ = metadata;
            std::fs::read(&self.path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<NativeHostDescriptor>(&bytes).ok())
                .is_some_and(|descriptor| descriptor.ownership_id == self.ownership_id)
        };
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
    fn path(&self) -> &Path {
        &self.path
    }

    fn ownership_id(&self) -> &str {
        &self.ownership_id
    }

    fn cleanup(&mut self) -> std::io::Result<()> {
        if let Some(final_file) = self.final_file.take() {
            final_file.remove_if_owned()?;
        }
        if let Some(pending_file) = self.pending_file.take() {
            pending_file.remove_if_owned()?;
        }
        Ok(())
    }
}

impl Drop for PublishedDescriptor {
    fn drop(&mut self) {
        let _ = self.cleanup();
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
    let mut registrations = vec![FactoryRegistration::new(
        BrowserEngine::Chromium,
        None,
        chromium_capabilities,
        Arc::new(ChromiumWorkerFactory::new(config.browser.clone())),
    )];
    let firefox_required = crate::required_extension_capabilities();
    let mut enrolled = enrollment.map(|enrollment| (enrollment.profile_id, enrollment.server));
    let firefox = selection
        .firefox
        .into_iter()
        .map(FirefoxRuntimeConfig::try_from)
        .map(|config| {
            config.map(|config| {
                let server = match enrolled.take() {
                    Some((profile_id, server)) if profile_id == config.profile_id => Some(server),
                    Some(value) => {
                        enrolled = Some(value);
                        None
                    }
                    None => None,
                };
                FirefoxRegistration {
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
                    }),
                }
            })
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
                            *factory.server.lock().await = Some(server);
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
        EnginePreferenceConfig::Exact { engine, profile_id } => EnginePreference::Exact {
            engine: browser_engine(engine),
            profile_id: profile_id
                .map(|value| uuid::Uuid::parse_str(&value).map(ProfileId))
                .transpose()?,
        },
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
    Ok(dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("config directory unavailable"))?
        .join("bobby-browser")
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
    async fn configured_firefox_is_a_real_launch_candidate() {
        let profile_id = ProfileId::new();
        let descriptor = PathBuf::from("target")
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
                    profile_dir: PathBuf::from("/profiles/default-release"),
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
        assert!(!descriptor.exists());
        let _ = std::fs::remove_file(descriptor);
    }

    #[tokio::test]
    async fn bootstrap_recovers_a_descriptor_leaked_by_a_killed_process() {
        let descriptor =
            PathBuf::from("target").join(format!("stale-descriptor-{}.json", uuid::Uuid::new_v4()));
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
        let profile_id = ProfileId::new();
        let descriptor = PathBuf::from("target").join(format!(
            "cancelled-firefox-companion-{}.json",
            uuid::Uuid::new_v4()
        ));
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
                    profile_dir: PathBuf::from("/profiles/default-release"),
                    companion_bind: "127.0.0.1:0".into(),
                    descriptor_path: descriptor.clone(),
                    timeout_ms: 30_000,
                    pairing_code_ttl_ms: 30_000,
                    attachment_ttl_ms: 30_000,
                }],
            },
        )
        .unwrap();
        let task = tokio::spawn({
            let factory = factory.clone();
            async move { factory.launch(&SessionId::new()).await }
        });
        let descriptor_ready = tokio::time::timeout(Duration::from_secs(1), async {
            while !descriptor.exists() && !task.is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await;
        if descriptor_ready.is_err() || task.is_finished() {
            let _ = task.await;
            assert!(!descriptor.exists());
            return;
        }
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
        let path = PathBuf::from("target").join(format!(
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
        let path = PathBuf::from("target").join(format!(
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
        let root = PathBuf::from("target").join(format!(
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
        let path = PathBuf::from("target").join(format!(
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
    fn descriptor_cleanup_preserves_a_replacement_file() {
        let path = PathBuf::from("target").join(format!(
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
        let path = PathBuf::from("target").join(format!(
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
