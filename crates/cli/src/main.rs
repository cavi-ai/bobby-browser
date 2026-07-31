mod bootstrap_local;

use anyhow::{Context, Result};
use artifact_store::ArtifactStore;
use async_trait::async_trait;
use companion_core::{
    run_native_host, CompanionServer, CompanionServerConfig, CompanionServerHandle,
    NativeHostConfig,
};
use companion_protocol::{BrowserEngine, CompanionCapabilities};
use config::{
    AppConfig, BrowserEngineConfig, BrowserSelectionConfig, EnginePreferenceConfig,
    FirefoxCompanionConfig,
};
use firefox_companion::{
    required_extension_capabilities, CompanionExtensionObserver, FirefoxCompanionFactory,
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
struct NativeHostDescriptor {
    endpoint: String,
    pairing_code: String,
    ownership_id: String,
}

#[derive(Clone)]
pub struct NativeHostInstallConfig {
    pub wrapper_path: PathBuf,
    pub manifest_path: PathBuf,
    pub cli_path: PathBuf,
    pub descriptor_path: PathBuf,
}

#[derive(Clone)]
pub struct FirefoxProfileEnrollmentConfig {
    pub companion_bind: SocketAddr,
    pub descriptor_path: PathBuf,
    pub timeout: Duration,
    pub pairing_code_ttl: Duration,
    pub attachment_ttl: Duration,
}

pub struct FirefoxProfileEnrollment {
    attempt: FirefoxBootstrapAttempt,
    timeout: Duration,
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
                .map_err(companion_error)?;
            return self.launch_with_server(server, session_id).await;
        }

        let attempt = self.start_server().await?;
        let server = attempt.server();
        server
            .wait_for_discovery(&self.config.profile_id, self.config.timeout)
            .await
            .map_err(companion_error)?;
        let worker = self.launch_with_server(server, session_id).await?;
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
        .launch(session_id)
        .await
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
    let firefox_required = required_firefox_capabilities();
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

pub fn required_firefox_capabilities() -> RequiredCapabilities {
    required_extension_capabilities()
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

#[derive(clap::Parser)]
#[command(name = "bobby", version, about = "bobby-browser automation runtime")]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,
}

impl Cli {
    fn parse_args() -> Self {
        <Self as clap::Parser>::parse()
    }
}

#[derive(clap::Subcommand)]
enum CliCommand {
    /// Generate a loopback bootstrap credential
    Init {
        /// Overwrite an existing bootstrap file
        #[arg(long)]
        force: bool,
        /// Days until the bootstrap credential expires
        #[arg(long, default_value_t = bootstrap_local::DEFAULT_TTL_DAYS as u32)]
        ttl_days: u32,
        /// Bootstrap env file path
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Run the runtime server (default)
    Serve,
    /// Run the Firefox native-messaging host
    FirefoxNativeHost {
        /// Absolute path to the native-host descriptor JSON
        #[arg(long)]
        descriptor: PathBuf,
    },
    /// Install the Firefox native-host wrapper and manifest
    InstallFirefoxNativeHost {
        #[arg(long)]
        wrapper: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        cli: PathBuf,
        #[arg(long)]
        descriptor: PathBuf,
    },
    /// Run a one-time Firefox companion enrollment and print the discovered
    /// profile id as a ready-to-use AUTOMATION_RUNTIME_BROWSER_SELECTION value
    EnrollFirefoxProfile {
        /// Absolute path the native-host descriptor is published to
        #[arg(long)]
        descriptor: PathBuf,
        /// Loopback address the pairing server binds to
        #[arg(long, default_value = "127.0.0.1:9876")]
        bind: SocketAddr,
        /// BiDi WebSocket URL of the running Firefox (e.g. ws://127.0.0.1:9222/session)
        #[arg(long)]
        bidi_url: String,
        /// Firefox profile directory the companion extension runs in
        #[arg(long)]
        profile_dir: PathBuf,
        /// Seconds to wait for the extension to pair
        #[arg(long, default_value_t = 120)]
        timeout_secs: u64,
    },
    /// Check local setup: config, bootstrap credential, storage, browsers
    Doctor,
}

pub async fn run() -> Result<()> {
    match Cli::parse_args().command.unwrap_or(CliCommand::Serve) {
        CliCommand::Init {
            force,
            ttl_days,
            path,
        } => run_init(force, ttl_days, path)?,
        CliCommand::Serve => {
            let config_path = std::env::var("BOBBY_BROWSER_CONFIG")
                .map(PathBuf::from)
                .unwrap_or_else(|_| PathBuf::from("./config.toml"));
            let config_existed = config_path.exists();
            let config = AppConfig::load(&config_path)
                .with_context(|| format!("failed to load config from {}", config_path.display()))?;
            let bootstrap_path = std::env::var("BOBBY_BROWSER_BOOTSTRAP_ENV")
                .map(PathBuf::from)
                .unwrap_or(bootstrap_local::default_bootstrap_path()?);
            let resolved = bootstrap_local::resolve_startup_credential_with(
                &config.server.host,
                &bootstrap_path,
                broker::StartupCredential::from_env,
            )?;
            let startup = match resolved {
                bootstrap_local::ResolveOutcome::FromEnv(c)
                | bootstrap_local::ResolveOutcome::FromFile(c) => c,
                bootstrap_local::ResolveOutcome::Generated {
                    credential,
                    material,
                } => {
                    eprintln!(
                        "Generated loopback bootstrap at {}",
                        bootstrap_path.display()
                    );
                    eprintln!("Bootstrap bearer (copy now; will not be shown again):");
                    eprintln!("{}", material.bearer());
                    credential
                }
            };
            let _telemetry = observability::init(&config.observability)?;
            if config_existed {
                tracing::info!(path = %config_path.display(), "loaded config file");
            } else {
                tracing::info!(
                    path = %config_path.display(),
                    "config file not found, using built-in defaults"
                );
            }
            let selection_json = std::env::var("AUTOMATION_RUNTIME_BROWSER_SELECTION").ok();
            let factory =
                compose_worker_factory(&config, parse_selection(selection_json.as_deref())?)?;
            broker::serve_with_worker_factory(config, startup, factory).await?
        }
        CliCommand::FirefoxNativeHost { descriptor } => {
            let _telemetry = observability::init(&Default::default())?;
            run_configured_native_host(descriptor).await?
        }
        CliCommand::InstallFirefoxNativeHost {
            wrapper,
            manifest,
            cli,
            descriptor,
        } => install_configured_native_host(wrapper, manifest, cli, descriptor)?,
        CliCommand::EnrollFirefoxProfile {
            descriptor,
            bind,
            bidi_url,
            profile_dir,
            timeout_secs,
        } => {
            run_firefox_profile_enroll(descriptor, bind, bidi_url, profile_dir, timeout_secs)
                .await?
        }
        CliCommand::Doctor => run_doctor()?,
    }

    Ok(())
}

async fn run_firefox_profile_enroll(
    descriptor: PathBuf,
    bind: SocketAddr,
    bidi_url: String,
    profile_dir: PathBuf,
    timeout_secs: u64,
) -> Result<()> {
    if timeout_secs == 0 {
        anyhow::bail!("--timeout-secs must be positive");
    }
    let enrollment = start_firefox_profile_enrollment(
        FirefoxProfileEnrollmentConfig {
            companion_bind: bind,
            descriptor_path: descriptor.clone(),
            timeout: Duration::from_secs(timeout_secs),
            pairing_code_ttl: Duration::from_secs(300),
            attachment_ttl: Duration::from_secs(300),
        },
        Arc::new(|_| {}),
    )
    .await
    .map_err(|error| anyhow::anyhow!("{}", error.message))?;
    let enrolled = enrollment
        .wait()
        .await
        .map_err(|error| anyhow::anyhow!("{}", error.message))?;
    let profile_id = enrolled.profile_id().0.to_string();
    let selection = serde_json::json!({
        "preference": { "mode": "exact", "engine": "firefox", "profileId": profile_id },
        "firefox": [{
            "profileId": profile_id,
            "bidiUrl": bidi_url,
            "profileDir": profile_dir,
            "companionBind": bind.to_string(),
            "descriptorPath": descriptor,
            "timeoutMs": 30_000,
            "pairingCodeTtlMs": 300_000,
            "attachmentTtlMs": 300_000,
        }],
    });
    println!("{selection}");
    eprintln!("Enrollment paired. Export the line above as AUTOMATION_RUNTIME_BROWSER_SELECTION.");
    Ok(())
}

fn run_doctor() -> Result<()> {
    let mut failures = 0usize;
    let mut warnings = 0usize;
    let report = |status: &str, check: &str, detail: String| {
        eprintln!("[{status}] {check}: {detail}");
    };

    let config_path = std::env::var("BOBBY_BROWSER_CONFIG")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("./config.toml"));
    let config = match AppConfig::load(&config_path) {
        Ok(config) => {
            let source = if config_path.exists() {
                config_path.display().to_string()
            } else {
                "built-in defaults (no config file)".to_string()
            };
            report("ok", "config", source);
            Some(config)
        }
        Err(error) => {
            failures += 1;
            report("fail", "config", format!("{error:#}"));
            None
        }
    };

    let selection_raw = std::env::var("AUTOMATION_RUNTIME_BROWSER_SELECTION").ok();
    let selection = match parse_selection(selection_raw.as_deref()) {
        Ok(selection) => {
            report(
                "ok",
                "browser-selection",
                if selection_raw.is_some() {
                    "AUTOMATION_RUNTIME_BROWSER_SELECTION parses".to_string()
                } else {
                    "default (Firefox, exact)".to_string()
                },
            );
            Some(selection)
        }
        Err(error) => {
            failures += 1;
            report("fail", "browser-selection", format!("{error:#}"));
            None
        }
    };

    if let (Some(config), Some(selection)) = (&config, &selection) {
        match compose_worker_factory(config, selection.clone()) {
            Ok(_) => report(
                "ok",
                "engine-satisfiability",
                "engine preference can be satisfied by configured registrations".to_string(),
            ),
            Err(error) => {
                failures += 1;
                report("fail", "engine-satisfiability", format!("{error:#}"));
            }
        }
        for profile in &selection.firefox {
            match Url::parse(&profile.bidi_url) {
                Ok(url) if matches!(url.scheme(), "ws" | "wss") => {
                    let reachable = url
                        .socket_addrs(|| Some(if url.scheme() == "wss" { 443 } else { 80 }))
                        .map(|addrs| {
                            addrs.iter().any(|addr| {
                                std::net::TcpStream::connect_timeout(
                                    addr,
                                    Duration::from_millis(500),
                                )
                                .is_ok()
                            })
                        })
                        .unwrap_or(false);
                    if reachable {
                        report(
                            "ok",
                            "firefox-bidi",
                            format!("{} reachable", profile.bidi_url),
                        );
                    } else {
                        warnings += 1;
                        report(
                            "warn",
                            "firefox-bidi",
                            format!(
                                "{} not reachable (is Firefox running with --remote-debugging-port?)",
                                profile.bidi_url
                            ),
                        );
                    }
                }
                _ => {
                    failures += 1;
                    report(
                        "fail",
                        "firefox-bidi",
                        format!(
                            "profile {} has an invalid bidiUrl (expected ws:// or wss://)",
                            profile.profile_id
                        ),
                    );
                }
            }
            if profile.profile_dir.exists() {
                report(
                    "ok",
                    "firefox-profile-dir",
                    profile.profile_dir.display().to_string(),
                );
            } else {
                warnings += 1;
                report(
                    "warn",
                    "firefox-profile-dir",
                    format!("{} does not exist yet", profile.profile_dir.display()),
                );
            }
            if profile.companion_bind.parse::<SocketAddr>().is_err() {
                failures += 1;
                report(
                    "fail",
                    "firefox-companion-bind",
                    format!(
                        "profile {} has an invalid companionBind",
                        profile.profile_id
                    ),
                );
            }
        }
    }

    if broker::StartupCredential::from_env().is_ok() {
        report("ok", "bootstrap", "credential from environment".to_string());
    } else {
        match bootstrap_local::default_bootstrap_path() {
            Ok(path) if path.exists() => report(
                "ok",
                "bootstrap",
                format!("credential file at {}", path.display()),
            ),
            Ok(path) => {
                warnings += 1;
                report(
                    "warn",
                    "bootstrap",
                    format!(
                        "no credential yet; `bobby serve` will generate one at {}",
                        path.display()
                    ),
                );
            }
            Err(error) => {
                failures += 1;
                report("fail", "bootstrap", format!("{error:#}"));
            }
        }
    }

    if let Some(config) = &config {
        for (name, dir) in [
            (
                "storage-journal-dir",
                config
                    .storage
                    .journal_path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from(".")),
            ),
            (
                "storage-checkpoints-dir",
                config.storage.checkpoints_dir.clone(),
            ),
            ("artifacts-dir", config.browser.artifacts_dir.clone()),
        ] {
            match std::fs::create_dir_all(&dir) {
                Ok(()) => report("ok", name, dir.display().to_string()),
                Err(error) => {
                    failures += 1;
                    report("fail", name, format!("{}: {error}", dir.display()));
                }
            }
        }
    }

    let firefox = which_binary(&["firefox", "firefox-esr"])
        || [
            "/Applications/Firefox.app",
            "/Applications/Firefox Developer Edition.app",
            "/Applications/Firefox Nightly.app",
        ]
        .iter()
        .any(|bundle| Path::new(bundle).exists());
    if firefox {
        report("ok", "firefox", "found".to_string());
    } else {
        warnings += 1;
        report(
            "warn",
            "firefox",
            "not found on PATH or /Applications (default engine)".to_string(),
        );
    }
    let chromium = which_binary(&["google-chrome", "chromium", "chrome"])
        || Path::new("/Applications/Google Chrome.app").exists()
        || Path::new("/Applications/Chromium.app").exists();
    if chromium {
        report("ok", "chromium", "found".to_string());
    } else {
        warnings += 1;
        report(
            "warn",
            "chromium",
            "not found (required for Chromium engine selection)".to_string(),
        );
    }

    eprintln!("doctor: {failures} failure(s), {warnings} warning(s)");
    if failures > 0 {
        std::process::exit(1);
    }
    Ok(())
}

fn which_binary(names: &[&str]) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| {
        names.iter().any(|name| {
            let candidate = dir.join(name);
            candidate.is_file() && is_executable(&candidate)
        })
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

fn run_init(force: bool, ttl_days: u32, path: Option<PathBuf>) -> Result<()> {
    let path = match path {
        Some(path) => path,
        None => bootstrap_local::default_bootstrap_path()?,
    };
    let material =
        bootstrap_local::generate_bootstrap(chrono::Duration::days(i64::from(ttl_days)))?;
    bootstrap_local::write_bootstrap_env(&path, &material, force)?;
    println!("{}", material.bearer());
    eprintln!("Wrote bootstrap env to {}", path.display());
    eprintln!("Map this bearer to AUTOMATION_RUNTIME_TOKEN / Authorization bearer for the SDK.");
    eprintln!(
        "Passing --force regenerates and invalidates the previous bearer for new enrollment."
    );
    Ok(())
}

fn install_configured_native_host(
    wrapper: PathBuf,
    manifest: PathBuf,
    cli: PathBuf,
    descriptor: PathBuf,
) -> Result<()> {
    install_native_host(NativeHostInstallConfig {
        wrapper_path: wrapper,
        manifest_path: manifest,
        cli_path: cli,
        descriptor_path: descriptor,
    })?;
    Ok(())
}

pub fn install_native_host(config: NativeHostInstallConfig) -> Result<()> {
    for path in [
        &config.wrapper_path,
        &config.manifest_path,
        &config.cli_path,
        &config.descriptor_path,
    ] {
        if !path.is_absolute() {
            anyhow::bail!("native-host installation paths must be absolute");
        }
    }
    let _install_lock = NativeHostInstallLock::acquire(&config.manifest_path)?;
    let wrapper = format!(
        "#!/bin/sh\nexec {} firefox-native-host --descriptor {}\n",
        shell_quote(&config.cli_path),
        shell_quote(&config.descriptor_path),
    );
    let manifest = serde_json::to_vec_pretty(&serde_json::json!({
        "name": "com.bobby_browser.companion",
        "description": "Bobby Browser Firefox companion native host",
        "path": config.wrapper_path,
        "type": "stdio",
        "allowed_extensions": ["firefox-companion@bobby-browser.local"],
    }))?;
    preflight_exact_file(&config.wrapper_path, wrapper.as_bytes(), 0o700)?;
    preflight_exact_file(&config.manifest_path, &manifest, 0o600)?;
    let wrapper_install = install_exact_file(&config.wrapper_path, wrapper.as_bytes(), 0o700)?;
    if let Err(error) = install_exact_file(&config.manifest_path, &manifest, 0o600) {
        if let Some(created) = wrapper_install {
            created.rollback(&config.wrapper_path);
        }
        return Err(error.into());
    }
    Ok(())
}

struct NativeHostInstallLock {
    _file: std::fs::File,
}

impl NativeHostInstallLock {
    fn acquire(manifest_path: &Path) -> std::io::Result<Self> {
        let lock_path = manifest_path.with_extension("install.lock");
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Ok(metadata) = std::fs::symlink_metadata(&lock_path) {
            if !metadata.file_type().is_file() {
                return Err(unsafe_install_lock());
            }
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(&lock_path)?;
        verify_install_lock_identity(&lock_path, &file)?;
        file.lock()?;
        verify_install_lock_identity(&lock_path, &file)?;
        Ok(Self { _file: file })
    }
}

fn unsafe_install_lock() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "native-host installer lock path is unsafe",
    )
}

fn verify_install_lock_identity(path: &Path, file: &std::fs::File) -> std::io::Result<()> {
    let path_metadata = std::fs::symlink_metadata(path)?;
    let file_metadata = file.metadata()?;
    if !path_metadata.file_type().is_file() || !file_metadata.file_type().is_file() {
        return Err(unsafe_install_lock());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if path_metadata.dev() != file_metadata.dev()
            || path_metadata.ino() != file_metadata.ino()
            || file_metadata.permissions().mode() & 0o777 != 0o600
        {
            return Err(unsafe_install_lock());
        }
    }
    Ok(())
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn preflight_exact_file(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    match path.symlink_metadata() {
        Ok(_) => verify_exact_file(path, contents, mode),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn verify_exact_file(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    #[cfg(unix)]
    let mode_matches = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777 == mode
    };
    #[cfg(not(unix))]
    let mode_matches = true;
    if metadata.file_type().is_file() && std::fs::read(path)? == contents && mode_matches {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "native-host installation destination already exists",
        ))
    }
}

struct CreatedInstallFile {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl CreatedInstallFile {
    fn from_metadata(metadata: std::fs::Metadata) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        Ok(Self {})
    }

    fn rollback(self, path: &Path) {
        self.rollback_ref(path);
    }

    fn rollback_ref(&self, path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let Ok(metadata) = std::fs::symlink_metadata(path) {
                if metadata.dev() == self.device && metadata.ino() == self.inode {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        #[cfg(not(unix))]
        let _ = std::fs::remove_file(path);
    }
}

fn install_exact_file(
    path: &Path,
    contents: &[u8],
    mode: u32,
) -> std::io::Result<Option<CreatedInstallFile>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.symlink_metadata().is_ok() {
        verify_exact_file(path, contents, mode)?;
        return Ok(None);
    }
    let pending = path.with_extension(format!("pending-{}", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let result = (|| {
        use std::io::Write;
        let mut file = options.open(&pending)?;
        file.write_all(contents)?;
        file.flush()?;
        file.sync_all()?;
        let created = CreatedInstallFile::from_metadata(file.metadata()?)?;
        match std::fs::hard_link(&pending, path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                verify_exact_file(path, contents, mode)?;
                std::fs::remove_file(&pending)?;
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
        if let Err(error) = std::fs::remove_file(&pending) {
            created.rollback(path);
            return Err(error);
        }
        Ok(Some(created))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(pending);
    }
    result
}

async fn run_configured_native_host(descriptor_path: PathBuf) -> Result<()> {
    if !descriptor_path.is_absolute() {
        anyhow::bail!("firefox native-host descriptor path must be absolute");
    }
    let descriptor: NativeHostDescriptor =
        serde_json::from_slice(&std::fs::read(descriptor_path)?)?;
    run_native_host(
        tokio::io::stdin(),
        tokio::io::stdout(),
        NativeHostConfig::new(descriptor.endpoint, descriptor.pairing_code),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use types::SessionId;

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
        let required = required_firefox_capabilities();
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

    #[cfg(unix)]
    #[test]
    fn installed_native_host_wrapper_passes_descriptor_without_environment_injection() {
        let root = std::env::temp_dir().join(format!(
            "bobby-native-host-install-{}",
            uuid::Uuid::new_v4()
        ));
        let wrapper = root.join("firefox-native-host");
        let manifest = root.join("com.bobby_browser.companion.json");
        let descriptor = root.join("dynamic-descriptor.json");
        let config = NativeHostInstallConfig {
            wrapper_path: wrapper.clone(),
            manifest_path: manifest.clone(),
            cli_path: PathBuf::from("/bin/echo"),
            descriptor_path: descriptor.clone(),
        };
        install_native_host(config.clone()).unwrap();
        install_native_host(config).unwrap();
        let output = std::process::Command::new(&wrapper)
            .env_remove("AUTOMATION_RUNTIME_FIREFOX_DESCRIPTOR")
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("firefox-native-host --descriptor"));
        assert!(stdout.contains(descriptor.to_str().unwrap()));
        assert!(!stdout.contains("AUTOMATION_RUNTIME_FIREFOX_DESCRIPTOR"));
        let installed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        assert_eq!(installed["path"], wrapper.to_string_lossy().as_ref());
        assert!(!String::from_utf8_lossy(&std::fs::read(&wrapper).unwrap()).contains("pairing"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&wrapper).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&manifest).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_file(wrapper).unwrap();
        std::fs::remove_file(manifest).unwrap();
        std::fs::remove_file(root.join("com.bobby_browser.companion.install.lock")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn native_host_installation_never_clobbers_operator_owned_files() {
        let root = std::env::temp_dir().join(format!(
            "bobby-native-host-no-clobber-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let wrapper = root.join("firefox-native-host");
        let manifest = root.join("com.bobby_browser.companion.json");
        let original = b"operator-owned";
        std::fs::write(&wrapper, original).unwrap();
        let result = install_native_host(NativeHostInstallConfig {
            wrapper_path: wrapper.clone(),
            manifest_path: manifest.clone(),
            cli_path: PathBuf::from("/bin/echo"),
            descriptor_path: root.join("dynamic-descriptor.json"),
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(&wrapper).unwrap(), original);
        assert!(!manifest.exists());
        std::fs::remove_file(wrapper).unwrap();
        std::fs::remove_file(root.join("com.bobby_browser.companion.install.lock")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn native_host_manifest_conflict_rolls_back_wrapper_created_by_attempt() {
        let root = std::env::temp_dir().join(format!(
            "bobby-native-host-manifest-conflict-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let wrapper = root.join("firefox-native-host");
        let manifest = root.join("com.bobby_browser.companion.json");
        let original = b"operator-owned-manifest";
        std::fs::write(&manifest, original).unwrap();

        let result = install_native_host(NativeHostInstallConfig {
            wrapper_path: wrapper.clone(),
            manifest_path: manifest.clone(),
            cli_path: PathBuf::from("/bin/echo"),
            descriptor_path: root.join("dynamic-descriptor.json"),
        });

        assert!(result.is_err());
        assert!(!wrapper.exists());
        assert_eq!(std::fs::read(&manifest).unwrap(), original);
        std::fs::remove_file(manifest).unwrap();
        std::fs::remove_file(root.join("com.bobby_browser.companion.install.lock")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_identical_native_host_installers_are_both_successful() {
        let root = std::env::temp_dir().join(format!(
            "bobby-native-host-concurrent-{}",
            uuid::Uuid::new_v4()
        ));
        let config = NativeHostInstallConfig {
            wrapper_path: root.join("firefox-native-host"),
            manifest_path: root.join("com.bobby_browser.companion.json"),
            cli_path: PathBuf::from("/bin/echo"),
            descriptor_path: root.join("dynamic-descriptor.json"),
        };
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let attempts = (0..2)
            .map(|_| {
                let config = config.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    install_native_host(config)
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        for attempt in attempts {
            attempt.join().unwrap().unwrap();
        }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&config.wrapper_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&config.manifest_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::remove_file(config.wrapper_path).unwrap();
        std::fs::remove_file(config.manifest_path).unwrap();
        std::fs::remove_file(root.join("com.bobby_browser.companion.install.lock")).unwrap();
        std::fs::remove_dir(root).unwrap();
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
}
