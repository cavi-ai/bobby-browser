//! Live integration-test package for the automation runtime.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};
use std::{fs::OpenOptions, io::Read};

use axum::{response::Html, routing::get, Router};
use companion_protocol::{BrowserEngine, BrowserIdentity, InteractionPath};
use config::{
    AppConfig, BrowserEngineConfig, BrowserSelectionConfig, EnginePreferenceConfig,
    FirefoxCompanionConfig,
};
use firefox_companion::BidiClient;
use release_gates::{NativeBrowserOperationProof, NativeBrowserProof};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use types::{
    ClickCommand, CommandError, ErrorCode, ErrorLayer, Evidence, InspectCommand, NavigateCommand,
    PageId, SessionId, TypeTextCommand, WaitUntil,
};
use url::Url;
use worker_pool::BrowserWorker;

const PROOF_TIMEOUT: Duration = Duration::from_secs(60);
const EXTENSION_ID: &str = "firefox-companion@bobby-browser.local";
const PROOF_HTML: &str = r#"<!doctype html><title>Native Firefox Proof</title><label for="name">Name</label><input id="name"><button id="submit" onclick="const value = document.querySelector('#name').value; document.querySelector('#result').textContent = value === 'Bobby' ? 'Submitted' : 'Rejected'">Submit</button><p id="result"></p>"#;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstalledFirefoxConfig {
    pub firefox_bin: PathBuf,
    pub profile: PathBuf,
    pub companion_extension: PathBuf,
}

pub struct InstalledFirefoxRuntime {
    factory: Arc<dyn worker_pool::WorkerFactory>,
    _firefox: Child,
}

impl InstalledFirefoxRuntime {
    pub fn factory(&self) -> Arc<dyn worker_pool::WorkerFactory> {
        Arc::clone(&self.factory)
    }
}

impl InstalledFirefoxConfig {
    pub fn from_env() -> Result<Self, String> {
        fn required(name: &str) -> Result<PathBuf, String> {
            std::env::var_os(name)
                .filter(|value| !value.is_empty())
                .map(PathBuf::from)
                .ok_or_else(|| name.to_owned())
        }
        Ok(Self {
            firefox_bin: required("BOBBY_FIREFOX_BIN")?,
            profile: required("BOBBY_FIREFOX_PROFILE")?,
            companion_extension: required("BOBBY_COMPANION_EXTENSION")?,
        })
    }
}

pub async fn launch_installed_firefox_runtime(
    installed: InstalledFirefoxConfig,
    runtime_config: &AppConfig,
    startup_url: &str,
    descriptor_path: PathBuf,
) -> Result<InstalledFirefoxRuntime, CommandError> {
    validate_installed_config(&installed)?;
    let process_observations = ProcessObservationCollector::new(Vec::new());
    let enrollment = cli::start_firefox_profile_enrollment(
        cli::FirefoxProfileEnrollmentConfig {
            companion_bind: "127.0.0.1:0".parse().expect("loopback enrollment address"),
            descriptor_path: descriptor_path.clone(),
            timeout: PROOF_TIMEOUT,
            pairing_code_ttl: PROOF_TIMEOUT,
            attachment_ttl: Duration::from_secs(300),
        },
        process_observations.pairing_code_observer(),
    )
    .await?;
    let (mut firefox, bidi_url) =
        launch_firefox(&installed, startup_url, &process_observations).await?;
    let factory = async {
        let extension_session =
            BidiClient::connect_session(bidi_url.clone(), PROOF_TIMEOUT).await?;
        let (method, params) = temporary_extension_install_command(&installed.companion_extension)?;
        let installed_extension = extension_session.send(method, params).await;
        if let Err(error) = installed_extension {
            let _ = extension_session.end_session().await;
            return Err(error);
        }
        if installed_extension
            .as_ref()
            .ok()
            .and_then(|value| value["extension"].as_str())
            != Some(EXTENSION_ID)
        {
            let _ = extension_session.end_session().await;
            return Err(workflow_error(
                ErrorCode::VerificationFailed,
                "Firefox installed an unexpected companion extension",
            ));
        }
        let enrollment = enrollment.wait().await;
        let extension_session_ended = extension_session.end_session().await;
        let enrollment = enrollment?;
        extension_session_ended?;
        let profile_id = enrollment.profile_id().clone();
        cli::compose_worker_factory_with_enrolled_firefox(
            runtime_config,
            BrowserSelectionConfig {
                preference: EnginePreferenceConfig::Exact {
                    engine: BrowserEngineConfig::Firefox,
                    profile_id: Some(profile_id.0.to_string()),
                },
                firefox: vec![FirefoxCompanionConfig {
                    profile_id: profile_id.0.to_string(),
                    bidi_url: bidi_url.to_string(),
                    profile_dir: installed.profile,
                    companion_bind: "127.0.0.1:0".into(),
                    descriptor_path,
                    timeout_ms: PROOF_TIMEOUT.as_millis() as u64,
                    pairing_code_ttl_ms: PROOF_TIMEOUT.as_millis() as u64,
                    attachment_ttl_ms: 300_000,
                }],
            },
            process_observations.pairing_code_observer(),
            enrollment,
        )
        .map_err(|error| workflow_error(ErrorCode::BrowserLaunchFailed, error))
    }
    .await;
    let factory = match factory {
        Ok(factory) => factory,
        Err(error) => {
            terminate_firefox(&mut firefox).await;
            return Err(error);
        }
    };
    Ok(InstalledFirefoxRuntime {
        factory,
        _firefox: firefox,
    })
}

pub async fn run_installed_firefox_workflow(
    config: InstalledFirefoxConfig,
) -> Result<NativeBrowserProof, CommandError> {
    validate_installed_config(&config)?;
    let started = Instant::now();
    let fixture = ProofSite::spawn().await?;
    let state_dir = proof_state_dir();
    std::fs::create_dir_all(&state_dir).map_err(io_error)?;
    let descriptor_path = state_dir.join("native-host-descriptor.json");
    let process_observations = ProcessObservationCollector::new(Vec::new());
    let enrollment = cli::start_firefox_profile_enrollment(
        cli::FirefoxProfileEnrollmentConfig {
            companion_bind: "127.0.0.1:0".parse().expect("loopback enrollment address"),
            descriptor_path: descriptor_path.clone(),
            timeout: PROOF_TIMEOUT,
            pairing_code_ttl: PROOF_TIMEOUT,
            attachment_ttl: Duration::from_secs(300),
        },
        process_observations.pairing_code_observer(),
    )
    .await?;
    let (mut firefox, bidi_url) =
        launch_firefox(&config, &fixture.url, &process_observations).await?;
    let extension_session = BidiClient::connect_session(bidi_url.clone(), PROOF_TIMEOUT).await?;
    let (method, params) = temporary_extension_install_command(&config.companion_extension)?;
    let installed = extension_session.send(method, params).await;
    if let Err(error) = installed {
        let _ = extension_session.end_session().await;
        return Err(error);
    }
    if installed
        .as_ref()
        .ok()
        .and_then(|value| value["extension"].as_str())
        != Some(EXTENSION_ID)
    {
        let _ = extension_session.end_session().await;
        return Err(workflow_error(
            ErrorCode::VerificationFailed,
            "Firefox installed an unexpected companion extension",
        ));
    }
    let enrollment = enrollment.wait().await;
    let extension_session_ended = extension_session.end_session().await;
    let enrollment = enrollment?;
    extension_session_ended?;
    let profile_id = enrollment.profile_id().clone();
    let factory = cli::compose_worker_factory_with_enrolled_firefox(
        &AppConfig::default(),
        BrowserSelectionConfig {
            preference: EnginePreferenceConfig::Exact {
                engine: BrowserEngineConfig::Firefox,
                profile_id: Some(profile_id.0.to_string()),
            },
            firefox: vec![FirefoxCompanionConfig {
                profile_id: profile_id.0.to_string(),
                bidi_url: bidi_url.to_string(),
                profile_dir: config.profile.clone(),
                companion_bind: "127.0.0.1:0".into(),
                descriptor_path: descriptor_path.clone(),
                timeout_ms: PROOF_TIMEOUT.as_millis() as u64,
                pairing_code_ttl_ms: PROOF_TIMEOUT.as_millis() as u64,
                attachment_ttl_ms: 300_000,
            }],
        },
        process_observations.pairing_code_observer(),
        enrollment,
    )
    .map_err(|error| workflow_error(ErrorCode::BrowserLaunchFailed, error))?;
    let worker = factory.launch(&SessionId::new()).await?;
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await?;

    let mut operations = Vec::new();
    let mut retained = Vec::new();
    let operation_started = Instant::now();
    let navigation = worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: fixture.url.clone(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await?;
    retained.extend(navigation.clone());
    operations.push(operation_proof(
        "navigate",
        &navigation,
        operation_started,
        navigation.iter().any(|item| matches!(item, Evidence::Navigation { url, .. } if url.starts_with(&fixture.url))),
    )?);

    let operation_started = Instant::now();
    let inspection = worker
        .inspect(
            &page_id,
            &InspectCommand {
                selector: Some("#name".into()),
                include_html: true,
                ..InspectCommand::default()
            },
        )
        .await?;
    retained.extend(inspection.clone());
    operations.push(operation_proof(
        "inspect",
        &inspection,
        operation_started,
        inspection.iter().any(|item| matches!(item, Evidence::Inspection { selector: Some(selector), .. } if selector == "#name")),
    )?);

    let operation_started = Instant::now();
    let typed = worker
        .type_text(
            &page_id,
            &TypeTextCommand {
                selector: "#name".into(),
                target: None,
                value: "Bobby".into(),
                clear_first: true,
                expected_url: None,
            },
        )
        .await?;
    retained.extend(typed.clone());
    let typed_duration_ms = operation_started.elapsed().as_millis().max(1) as u64;

    let operation_started = Instant::now();
    let clicked = worker
        .click(
            &page_id,
            &ClickCommand {
                selector: "#submit".into(),
                target: None,
                boundary: true,
                expected_url: None,
            },
        )
        .await?;
    retained.extend(clicked.clone());
    let (confirmation, confirmation_evidence) =
        wait_for_confirmation(worker.as_ref(), &page_id).await?;
    retained.extend(confirmation_evidence);
    operations.push(operation_proof_with_duration(
        "typeText",
        &typed,
        typed_duration_ms,
        confirmation == "Submitted",
    )?);
    operations.push(operation_proof(
        "click",
        &clicked,
        operation_started,
        confirmation == "Submitted",
    )?);

    worker.close().await?;
    terminate_firefox(&mut firefox).await;
    let process_findings = process_observations.finish().await;
    derive_native_browser_proof(
        operations,
        confirmation,
        retained,
        process_findings,
        started.elapsed().as_millis().max(1) as u64,
        PROOF_TIMEOUT.as_millis() as u64,
    )
}

fn validate_installed_config(config: &InstalledFirefoxConfig) -> Result<(), CommandError> {
    if !config.firefox_bin.is_file() || !config.profile.is_dir() {
        return Err(workflow_error(
            ErrorCode::BrowserLaunchFailed,
            "Firefox binary and dedicated profile must exist",
        ));
    }
    if !config.companion_extension.join("manifest.json").is_file() {
        return Err(workflow_error(
            ErrorCode::BrowserLaunchFailed,
            "companion extension must be a built extension directory",
        ));
    }
    Ok(())
}

struct ProofSite {
    url: String,
    task: tokio::task::JoinHandle<()>,
}

impl ProofSite {
    async fn spawn() -> Result<Self, CommandError> {
        let app = Router::new().route("/", get(|| async { Html(PROOF_HTML) }));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .map_err(io_error)?;
        let address = listener.local_addr().map_err(io_error)?;
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self {
            url: format!("http://{address}/"),
            task,
        })
    }
}

impl Drop for ProofSite {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn temporary_extension_install_command(
    source: &Path,
) -> Result<(&'static str, serde_json::Value), CommandError> {
    let source = std::fs::canonicalize(source).map_err(io_error)?;
    let source = source.to_str().ok_or_else(|| {
        workflow_error(
            ErrorCode::BrowserLaunchFailed,
            "companion extension path must be valid UTF-8",
        )
    })?;
    Ok((
        "webExtension.install",
        serde_json::json!({
            "extensionData": {"type": "path", "path": source},
            "moz:permanent": false,
        }),
    ))
}

fn proof_state_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target/firefox-companion-proof")
}

async fn launch_firefox(
    config: &InstalledFirefoxConfig,
    startup_url: &str,
    process_observations: &ProcessObservationCollector,
) -> Result<(Child, Url), CommandError> {
    let endpoint_file = config.profile.join("WebDriverBiDiServer.json");
    match endpoint_file.symlink_metadata() {
        Ok(metadata) if metadata.file_type().is_file() => {
            std::fs::remove_file(&endpoint_file).map_err(io_error)?;
        }
        Ok(_) => {
            return Err(workflow_error(
                ErrorCode::PolicyDenied,
                "Firefox BiDi endpoint path is not a regular file",
            ));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(io_error(error)),
    }
    let mut child = Command::new(&config.firefox_bin)
        .arg("--no-remote")
        .arg("--foreground")
        .arg("--profile")
        .arg(&config.profile)
        .arg("--remote-debugging-port=0")
        .arg(startup_url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(io_error)?;
    let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
    if let Some(stdout) = child.stdout.take() {
        process_observations.spawn_reader(stdout, sender.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        process_observations.spawn_reader(stderr, sender.clone());
    }
    drop(sender);
    let url = tokio::time::timeout(PROOF_TIMEOUT, async {
        loop {
            tokio::select! {
                line_url = receiver.recv() => {
                    if let Some(url) = line_url {
                        return Ok(url);
                    }
                }
                _ = tokio::time::sleep(Duration::from_millis(50)) => {}
            }
            if let Some(url) = read_bidi_endpoint_file(&endpoint_file)? {
                return Ok(url);
            }
        }
    })
    .await
    .map_err(|_| {
        workflow_error(
            ErrorCode::BrowserLaunchFailed,
            "Firefox BiDi endpoint timed out",
        )
    })??;
    Ok((child, url))
}

fn read_bidi_endpoint_file(path: &Path) -> Result<Option<Url>, CommandError> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = match options.open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(io_error(error)),
    };
    let metadata = file.metadata().map_err(io_error)?;
    if !metadata.file_type().is_file() || metadata.len() > 4096 {
        return Err(workflow_error(
            ErrorCode::PolicyDenied,
            "Firefox BiDi endpoint file is invalid",
        ));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(4097).read_to_end(&mut bytes).map_err(io_error)?;
    if bytes.len() > 4096 {
        return Err(workflow_error(
            ErrorCode::PolicyDenied,
            "Firefox BiDi endpoint file exceeds its bound",
        ));
    }
    bidi_endpoint_file_url(&bytes)
        .map(Some)
        .map_err(|message| workflow_error(ErrorCode::BrowserLaunchFailed, message))
}

fn bidi_endpoint_file_url(bytes: &[u8]) -> Result<Url, String> {
    if bytes.len() > 4096 {
        return Err("Firefox BiDi endpoint file exceeds its bound".into());
    }
    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|_| "Firefox BiDi endpoint file is malformed".to_owned())?;
    let object = value
        .as_object()
        .ok_or_else(|| "Firefox BiDi endpoint file must be an object".to_owned())?;
    if object.len() != 2 || !object.contains_key("ws_host") || !object.contains_key("ws_port") {
        return Err("Firefox BiDi endpoint file has an unsupported schema".into());
    }
    let host = object["ws_host"]
        .as_str()
        .ok_or_else(|| "Firefox BiDi endpoint host is invalid".to_owned())?;
    let address: std::net::IpAddr = host
        .parse()
        .map_err(|_| "Firefox BiDi endpoint host is invalid".to_owned())?;
    if !address.is_loopback() {
        return Err("Firefox BiDi endpoint must be loopback".into());
    }
    let port = object["ws_port"]
        .as_u64()
        .filter(|port| *port > 0 && *port <= u16::MAX as u64)
        .ok_or_else(|| "Firefox BiDi endpoint port is invalid".to_owned())?;
    let authority = match address {
        std::net::IpAddr::V4(address) => format!("{address}:{port}"),
        std::net::IpAddr::V6(address) => format!("[{address}]:{port}"),
    };
    Url::parse(&format!("ws://{authority}/session"))
        .map_err(|_| "Firefox BiDi endpoint URL is invalid".to_owned())
}

struct ProcessObservationCollector {
    findings: Arc<std::sync::Mutex<Vec<String>>>,
    sensitive_values: Arc<std::sync::Mutex<Vec<SensitiveFingerprint>>>,
    readers: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl ProcessObservationCollector {
    fn new(sensitive_values: Vec<String>) -> Self {
        let collector = Self {
            findings: Arc::new(std::sync::Mutex::new(Vec::new())),
            sensitive_values: Arc::new(std::sync::Mutex::new(Vec::new())),
            readers: std::sync::Mutex::new(Vec::new()),
        };
        for value in sensitive_values {
            collector.observe_sensitive_value(&value);
        }
        collector
    }

    fn pairing_code_observer(&self) -> Arc<dyn Fn(&str) + Send + Sync> {
        let sensitive_values = Arc::clone(&self.sensitive_values);
        Arc::new(move |value| register_sensitive_fingerprint(&sensitive_values, value))
    }

    fn observe_sensitive_value(&self, value: &str) {
        register_sensitive_fingerprint(&self.sensitive_values, value);
    }

    fn spawn_reader(
        &self,
        stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
        sender: tokio::sync::mpsc::Sender<Url>,
    ) {
        let findings = Arc::clone(&self.findings);
        let sensitive_values = Arc::clone(&self.sensitive_values);
        let task = tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            let mut endpoint_sent = false;
            while let Ok(Some(line)) = lines.next_line().await {
                if contains_sensitive_marker(&line)
                    || contains_sensitive_fingerprint(&sensitive_values, &line)
                {
                    let mut findings = findings.lock().expect("process findings mutex poisoned");
                    push_finding(&mut findings, "firefoxProcess");
                }
                if !endpoint_sent {
                    if let Some(url) = websocket_url(&line) {
                        endpoint_sent = sender.send(url).await.is_ok();
                    }
                }
            }
        });
        self.readers
            .lock()
            .expect("process readers mutex poisoned")
            .push(task);
    }

    async fn finish(self) -> Vec<String> {
        let readers = self
            .readers
            .into_inner()
            .expect("process readers mutex poisoned");
        for reader in readers {
            let _ = reader.await;
        }
        self.findings
            .lock()
            .expect("process findings mutex poisoned")
            .clone()
    }
}

#[derive(Clone, PartialEq, Eq)]
struct SensitiveFingerprint {
    byte_len: usize,
    digest: [u8; 32],
}

fn register_sensitive_fingerprint(
    fingerprints: &std::sync::Mutex<Vec<SensitiveFingerprint>>,
    value: &str,
) {
    if value.is_empty() {
        return;
    }
    let fingerprint = SensitiveFingerprint {
        byte_len: value.len(),
        digest: Sha256::digest(value.as_bytes()).into(),
    };
    let mut fingerprints = fingerprints
        .lock()
        .expect("sensitive fingerprints mutex poisoned");
    if !fingerprints.contains(&fingerprint) {
        fingerprints.push(fingerprint);
    }
}

fn contains_sensitive_fingerprint(
    fingerprints: &std::sync::Mutex<Vec<SensitiveFingerprint>>,
    line: &str,
) -> bool {
    let fingerprints = fingerprints
        .lock()
        .expect("sensitive fingerprints mutex poisoned")
        .clone();
    fingerprints.iter().any(|fingerprint| {
        line.as_bytes()
            .windows(fingerprint.byte_len)
            .any(|candidate| <[u8; 32]>::from(Sha256::digest(candidate)) == fingerprint.digest)
    })
}

fn websocket_url(line: &str) -> Option<Url> {
    let start = line.find("ws://")?;
    let candidate = line[start..]
        .split(|character: char| character.is_whitespace() || character == '"')
        .next()?;
    let mut url = Url::parse(candidate).ok()?;
    if url.scheme() != "ws" || url.cannot_be_a_base() {
        return None;
    }
    if url.path() == "/" {
        url.set_path("/session");
    }
    Some(url)
}

fn derive_native_browser_proof(
    operations: Vec<NativeBrowserOperationProof>,
    confirmation_text: String,
    command_evidence: Vec<Evidence>,
    process_findings: Vec<String>,
    elapsed_ms: u64,
    deadline_ms: u64,
) -> Result<NativeBrowserProof, CommandError> {
    const MAX_RETAINED_RECORDS: usize = 32;
    let mut browser = None;
    let mut retained = Vec::new();
    let mut redaction_findings = process_findings;
    for evidence in command_evidence.iter().take(MAX_RETAINED_RECORDS) {
        match evidence {
            Evidence::BrowserExecution {
                engine,
                browser_version,
                profile_id,
                interaction_path,
            } => {
                if browser.is_none() {
                    let engine_identity = match engine.as_str() {
                        "firefox" => BrowserEngine::Firefox,
                        "chromium" => BrowserEngine::Chromium,
                        "webkit" => BrowserEngine::WebKit,
                        _ => {
                            return Err(workflow_error(
                                ErrorCode::VerificationFailed,
                                "browser execution engine is invalid",
                            ))
                        }
                    };
                    if browser_version.is_empty()
                        || browser_version.len() > 64
                        || uuid::Uuid::parse_str(profile_id).is_err()
                        || serde_json::from_str::<InteractionPath>(&format!(
                            "\"{interaction_path}\""
                        ))
                        .is_err()
                    {
                        return Err(workflow_error(
                            ErrorCode::VerificationFailed,
                            "browser execution identity is invalid",
                        ));
                    }
                    browser = Some(BrowserIdentity {
                        engine: engine_identity,
                        browser_name: "Firefox".into(),
                        browser_version: browser_version.clone(),
                        os: std::env::consts::OS.into(),
                        profile_label: profile_id.clone(),
                    });
                }
                if [engine, browser_version, profile_id, interaction_path]
                    .into_iter()
                    .any(|value| contains_sensitive_marker(value))
                {
                    push_finding(&mut redaction_findings, "browserExecution.identity");
                } else {
                    retained.push(format!(
                        "browserExecution:{engine}:{browser_version}:{profile_id}:{interaction_path}"
                    ));
                }
            }
            Evidence::Inspection {
                selector,
                url,
                title,
                text,
                html,
            } => {
                if selector
                    .iter()
                    .chain([url, title, text])
                    .chain(html.iter())
                    .any(|value| contains_sensitive_marker(value))
                {
                    push_finding(&mut redaction_findings, "inspection.text");
                }
                retained.push("inspection:observed".into());
            }
            Evidence::Navigation { .. } => retained.push("navigation:observed".into()),
            Evidence::Element { .. } => retained.push("element:acted".into()),
            other => {
                let encoded = serde_json::to_string(&other.journal_safe())
                    .map_err(|error| workflow_error(ErrorCode::VerificationFailed, error))?;
                if contains_sensitive_marker(&encoded) {
                    push_finding(&mut redaction_findings, "commandEvidence");
                } else {
                    retained.push("commandEvidence:observed".into());
                }
            }
        }
    }
    if command_evidence.len() > MAX_RETAINED_RECORDS {
        return Err(workflow_error(
            ErrorCode::VerificationFailed,
            "native browser evidence exceeds the bounded record count",
        ));
    }
    Ok(NativeBrowserProof {
        browser,
        operations,
        confirmation_text,
        evidence: retained,
        redaction_findings,
        elapsed_ms,
        deadline_ms,
    })
}

fn push_finding(findings: &mut Vec<String>, finding: &str) {
    const MAX_REDACTION_FINDINGS: usize = 8;
    if findings.len() < MAX_REDACTION_FINDINGS
        && !findings.iter().any(|existing| existing == finding)
    {
        findings.push(finding.into());
    }
}

fn contains_sensitive_marker(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    [
        "authorization",
        "bearer",
        "password",
        "credential",
        "api-key",
        "api_key",
        "secret",
        "token",
    ]
    .iter()
    .any(|marker| value.contains(marker))
}

fn operation_proof(
    name: &str,
    evidence: &[Evidence],
    started: Instant,
    postcondition_verified: bool,
) -> Result<NativeBrowserOperationProof, CommandError> {
    operation_proof_with_duration(
        name,
        evidence,
        started.elapsed().as_millis().max(1) as u64,
        postcondition_verified,
    )
}

fn operation_proof_with_duration(
    name: &str,
    evidence: &[Evidence],
    duration_ms: u64,
    postcondition_verified: bool,
) -> Result<NativeBrowserOperationProof, CommandError> {
    let interaction_path = evidence
        .iter()
        .find_map(|item| match item {
            Evidence::BrowserExecution {
                interaction_path, ..
            } => serde_json::from_str(&format!("\"{interaction_path}\"")).ok(),
            _ => None,
        })
        .ok_or_else(|| {
            workflow_error(
                ErrorCode::VerificationFailed,
                "browser execution identity is missing",
            )
        })?;
    if !postcondition_verified {
        return Err(workflow_error(
            ErrorCode::VerificationFailed,
            format!("{name} postcondition was not verified"),
        ));
    }
    Ok(NativeBrowserOperationProof {
        name: name.into(),
        interaction_path,
        postcondition_verified,
        duration_ms,
    })
}

async fn wait_for_confirmation(
    worker: &dyn BrowserWorker,
    page_id: &PageId,
) -> Result<(String, Vec<Evidence>), CommandError> {
    for _ in 0..50 {
        let evidence = worker
            .inspect(
                page_id,
                &InspectCommand {
                    selector: Some("#result".into()),
                    ..InspectCommand::default()
                },
            )
            .await?;
        if let Some(text) = evidence.iter().find_map(|item| match item {
            Evidence::Inspection { text, .. } if text.trim() == "Submitted" => {
                Some(text.trim().to_owned())
            }
            _ => None,
        }) {
            return Ok((text, evidence));
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(workflow_error(
        ErrorCode::VerificationFailed,
        "submission confirmation was not observed",
    ))
}

async fn terminate_firefox(child: &mut Child) {
    let _ = child.start_kill();
    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
}

fn io_error(error: std::io::Error) -> CommandError {
    workflow_error(ErrorCode::BrowserLaunchFailed, error)
}

fn workflow_error(code: ErrorCode, error: impl std::fmt::Display) -> CommandError {
    CommandError {
        code,
        message: error.to_string(),
        layer: ErrorLayer::Driver,
        retryable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn companion_install_uses_the_standard_temporary_bidi_path_command() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("extension");
        std::fs::create_dir_all(&source).unwrap();

        let (method, params) = temporary_extension_install_command(&source).unwrap();

        assert_eq!(method, "webExtension.install");
        assert_eq!(
            params,
            serde_json::json!({
                "extensionData": {
                    "type": "path",
                    "path": std::fs::canonicalize(source).unwrap().to_str().unwrap(),
                },
                "moz:permanent": false,
            })
        );
    }

    #[test]
    fn proof_site_confirmation_depends_on_the_native_typed_value() {
        assert!(PROOF_HTML.contains("value === 'Bobby' ? 'Submitted' : 'Rejected'"));
    }

    #[test]
    fn bidi_endpoint_parser_accepts_only_a_websocket_url() {
        assert_eq!(
            websocket_url("WebDriver BiDi listening on ws://127.0.0.1:9222/session")
                .unwrap()
                .as_str(),
            "ws://127.0.0.1:9222/session"
        );
        assert!(websocket_url("WebDriver BiDi listening on https://127.0.0.1").is_none());
        assert_eq!(
            websocket_url("WebDriver BiDi listening on ws://127.0.0.1:9222")
                .unwrap()
                .as_str(),
            "ws://127.0.0.1:9222/session"
        );
    }

    #[test]
    fn bidi_endpoint_file_accepts_only_a_bounded_loopback_server() {
        assert_eq!(
            bidi_endpoint_file_url(br#"{"ws_host":"127.0.0.1","ws_port":57054}"#)
                .unwrap()
                .as_str(),
            "ws://127.0.0.1:57054/session"
        );
        assert!(bidi_endpoint_file_url(br#"{"ws_host":"192.0.2.1","ws_port":57054}"#).is_err());
        assert!(bidi_endpoint_file_url(br#"{"ws_host":"127.0.0.1","ws_port":0}"#).is_err());
        assert!(bidi_endpoint_file_url(&vec![b'x'; 4097]).is_err());
    }

    #[test]
    fn proof_derivation_redacts_actual_sensitive_command_evidence() {
        let operations = [
            ("navigate", InteractionPath::EngineNative),
            ("inspect", InteractionPath::ExtensionApi),
            ("click", InteractionPath::EngineNative),
            ("typeText", InteractionPath::EngineNative),
        ]
        .into_iter()
        .map(|(name, interaction_path)| NativeBrowserOperationProof {
            name: name.into(),
            interaction_path,
            postcondition_verified: true,
            duration_ms: 1,
        })
        .collect();
        let evidence = vec![
            Evidence::BrowserExecution {
                engine: "firefox".into(),
                browser_version: "153.0b11".into(),
                profile_id: "00000000-0000-4000-8000-000000000001".into(),
                interaction_path: "engineNative".into(),
            },
            Evidence::Inspection {
                selector: Some("#result".into()),
                url: "http://127.0.0.1/".into(),
                title: "proof".into(),
                text: "Authorization: Bearer do-not-retain".into(),
                html: None,
            },
        ];
        let proof = derive_native_browser_proof(
            operations,
            "Submitted".into(),
            evidence,
            Vec::new(),
            10,
            1_000,
        )
        .unwrap();
        assert_eq!(proof.redaction_findings, vec!["inspection.text"]);
        assert!(!format!("{proof:?}").contains("do-not-retain"));
    }

    #[tokio::test]
    async fn process_redaction_collects_both_streams_after_endpoint_discovery() {
        use tokio::io::AsyncWriteExt;

        let bearer = "47c851ee-600e-4d29-8794-29a8916f962e".to_owned();
        let collector = ProcessObservationCollector::new(vec![bearer.clone()]);
        let (sender, mut receiver) = tokio::sync::mpsc::channel(2);
        let (mut stdout_writer, stdout_reader) = tokio::io::duplex(1024);
        let (mut stderr_writer, stderr_reader) = tokio::io::duplex(1024);
        collector.spawn_reader(stdout_reader, sender.clone());
        collector.spawn_reader(stderr_reader, sender.clone());
        stdout_writer
            .write_all(b"WebDriver BiDi listening on ws://127.0.0.1:9222/session\n")
            .await
            .unwrap();
        assert!(receiver.recv().await.is_some());
        stderr_writer
            .write_all(format!("unstructured output {bearer}\n").as_bytes())
            .await
            .unwrap();
        stdout_writer.shutdown().await.unwrap();
        stderr_writer.shutdown().await.unwrap();
        let findings = collector.finish().await;
        assert_eq!(findings, vec!["firefoxProcess"]);
        assert!(!format!("{findings:?}").contains(&bearer));
    }

    #[tokio::test]
    async fn production_pairing_observer_detects_an_unlabelled_raw_uuid() {
        use tokio::io::AsyncWriteExt;

        let collector = ProcessObservationCollector::new(Vec::new());
        let observer = collector.pairing_code_observer();
        let planted = uuid::Uuid::new_v4().to_string();
        observer(&planted);
        let (sender, _receiver) = tokio::sync::mpsc::channel(1);
        let (mut writer, reader) = tokio::io::duplex(1024);
        collector.spawn_reader(reader, sender);
        writer
            .write_all(format!("unstructured output {planted}\n").as_bytes())
            .await
            .unwrap();
        writer.shutdown().await.unwrap();

        let findings = collector.finish().await;
        assert_eq!(findings, vec!["firefoxProcess"]);
        assert!(!format!("{findings:?}").contains(&planted));
    }
}
