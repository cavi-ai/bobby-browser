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
    factory: ConfiguredFirefoxFactory,
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
        let client =
            crate::BidiClient::connect_session(self.config.bidi_url.clone(), self.config.timeout)
                .await?;
        *slot = Some(client.clone());
        Ok(client)
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
    })
}

struct FirefoxBootstrapAttempt {
    server: Option<Arc<CompanionServerHandle>>,
    publication: Option<PublishedDescriptor>,
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
}

impl PublishedDescriptor {
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
                    factory: ConfiguredFirefoxFactory {
                        config,
                        required: firefox_required,
                        pairing_code_observer: Arc::clone(&pairing_code_observer),
                        server: Mutex::new(server),
                        artifacts: firefox_artifacts.clone(),
                        upload_roots: firefox_upload_roots.clone(),
                        downloads_dir: firefox_downloads_dir.clone(),
                        bidi: Mutex::new(None),
                    },
                }
            })
        })
        .collect::<Result<Vec<_>>>()?;
    if enrolled.is_some() {
        anyhow::bail!("enrolled Firefox profile is not present in selection configuration");
    }
    registrations.extend(firefox.into_iter().map(|registration| {
        FactoryRegistration::negotiated(
            BrowserEngine::Firefox,
            Some(registration.profile_id),
            Arc::new(registration.factory),
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
        assert_eq!(value["firefox"][0]["bidiUrl"], "ws://127.0.0.1:9222/session");
        assert_eq!(value["firefox"][0]["profileDir"], "/tmp/firefox-profile");
        assert_eq!(value["firefox"][0]["companionBind"], "127.0.0.1:9876");
        assert_eq!(value["firefox"][0]["descriptorPath"], "/tmp/descriptor.json");
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
}
