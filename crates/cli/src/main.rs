use anyhow::Result;
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
use firefox_companion::{CompanionExtensionObserver, FirefoxCompanionFactory};
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
    server: Mutex<Option<Arc<CompanionServerHandle>>>,
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
        .launch(session_id)
        .await
    }

    async fn start_server(&self) -> Result<FirefoxBootstrapAttempt, CommandError> {
        let server = Arc::new(
            CompanionServer::bind_loopback(CompanionServerConfig {
                bind_addr: self.config.companion_bind,
                pairing_code_ttl: self.config.pairing_code_ttl,
                attachment_ttl: self.config.attachment_ttl,
            })
            .await
            .map_err(|error| companion_error(error.to_string()))?,
        );
        let pairing_code = server.registry().issue_pairing_code().await;
        let descriptor = NativeHostDescriptor {
            endpoint: format!("ws://{}/v1/companion", server.local_addr()),
            pairing_code,
            ownership_id: uuid::Uuid::new_v4().to_string(),
        };
        let publication = write_descriptor(&self.config.descriptor_path, &descriptor)
            .map_err(|error| companion_error(error.to_string()))?;
        Ok(FirefoxBootstrapAttempt {
            server: Some(server),
            publication: Some(publication),
        })
    }
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

fn compose_worker_factory(
    config: &AppConfig,
    selection: BrowserSelectionConfig,
) -> Result<Arc<dyn WorkerFactory>> {
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
    let firefox_required = RequiredCapabilities {
        observe: true,
        navigate: true,
        native_input: true,
        tabs: true,
        frames: true,
        native_dialogs: false,
    };
    let firefox = selection
        .firefox
        .into_iter()
        .map(FirefoxRuntimeConfig::try_from)
        .map(|config| {
            config.map(|config| FirefoxRegistration {
                profile_id: config.profile_id.clone(),
                factory: ConfiguredFirefoxFactory {
                    config,
                    required: firefox_required,
                    server: Mutex::new(None),
                },
            })
        })
        .collect::<Result<Vec<_>>>()?;
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

fn parse_selection(value: Option<&str>) -> Result<BrowserSelectionConfig> {
    value
        .map(serde_json::from_str)
        .transpose()
        .map(|selection| selection.unwrap_or_default())
        .map_err(Into::into)
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter("info")
        .json()
        .init();

    let cmd = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "serve".to_string());

    match cmd.as_str() {
        "serve" => {
            let startup = broker::StartupCredential::from_env()?;
            let config = AppConfig::default();
            let selection_json = std::env::var("AUTOMATION_RUNTIME_BROWSER_SELECTION").ok();
            let factory =
                compose_worker_factory(&config, parse_selection(selection_json.as_deref())?)?;
            broker::serve_with_worker_factory(config, startup, factory).await?
        }
        "firefox-native-host" => run_configured_native_host().await?,
        "doctor" => println!("ok"),
        other => {
            eprintln!("unknown command: {other}");
            std::process::exit(2);
        }
    }

    Ok(())
}

async fn run_configured_native_host() -> Result<()> {
    let path = std::env::var("AUTOMATION_RUNTIME_FIREFOX_DESCRIPTOR")?;
    let descriptor: NativeHostDescriptor = serde_json::from_slice(&std::fs::read(path)?)?;
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
    fn absent_selection_configuration_keeps_managed_chromium() {
        assert_eq!(
            parse_selection(None).unwrap().preference,
            EnginePreferenceConfig::ManagedChromium
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
    async fn composed_factory_enforces_exact_firefox_without_fallback() {
        let config = AppConfig::default();
        let profile_id = ProfileId::new();
        let factory = compose_worker_factory(
            &config,
            BrowserSelectionConfig {
                preference: EnginePreferenceConfig::Exact {
                    engine: BrowserEngineConfig::Firefox,
                    profile_id: Some(profile_id.0.to_string()),
                },
                firefox: Vec::new(),
            },
        )
        .unwrap();

        let error = match factory.launch(&SessionId::new()).await {
            Err(error) => error,
            Ok(_) => panic!("exact Firefox unexpectedly launched Chromium"),
        };
        assert_eq!(error.code, types::ErrorCode::PolicyDenied);
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
