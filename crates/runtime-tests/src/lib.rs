//! Live integration-test package for the automation runtime.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::{response::Html, routing::get, Router};
use companion_protocol::{BrowserEngine, BrowserIdentity, InteractionPath};
use config::{
    AppConfig, BrowserEngineConfig, BrowserSelectionConfig, EnginePreferenceConfig,
    FirefoxCompanionConfig,
};
use release_gates::{NativeBrowserOperationProof, NativeBrowserProof};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use types::{
    ClickCommand, CommandError, ErrorCode, ErrorLayer, Evidence, InspectCommand, NavigateCommand,
    PageId, ProfileId, SessionId, TypeTextCommand, WaitUntil,
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

pub async fn run_installed_firefox_workflow(
    config: InstalledFirefoxConfig,
) -> Result<NativeBrowserProof, CommandError> {
    validate_installed_config(&config)?;
    let started = Instant::now();
    let fixture = ProofSite::spawn().await?;
    let state_dir = proof_state_dir();
    std::fs::create_dir_all(&state_dir).map_err(io_error)?;
    let descriptor_path = state_dir.join("native-host-descriptor.json");
    let _extension = ExtensionInstallation::install(&config.profile, &config.companion_extension)?;

    let (mut firefox, bidi_url, process_observations) =
        launch_firefox(&config, &descriptor_path).await?;
    let profile_id = wait_for_profile_id(&config.profile).await?;
    let factory = cli::compose_worker_factory_with_pairing_observer(
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
        Arc::new(|pairing_code| {
            println!("Firefox companion one-time pairing code: {pairing_code}");
        }),
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

struct ExtensionInstallation {
    path: PathBuf,
    owned: bool,
}

impl ExtensionInstallation {
    fn install(profile: &Path, source: &Path) -> Result<Self, CommandError> {
        let extensions = profile.join("extensions");
        std::fs::create_dir_all(&extensions).map_err(io_error)?;
        let path = extensions.join(EXTENSION_ID);
        if path.exists() || path.symlink_metadata().is_ok() {
            let current = std::fs::canonicalize(&path).map_err(io_error)?;
            let expected = std::fs::canonicalize(source).map_err(io_error)?;
            if current != expected {
                return Err(workflow_error(
                    ErrorCode::PolicyDenied,
                    "dedicated profile already contains a different companion extension",
                ));
            }
            return Ok(Self { path, owned: false });
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(source, &path).map_err(io_error)?;
        #[cfg(not(unix))]
        return Err(workflow_error(
            ErrorCode::BrowserLaunchFailed,
            "unpacked companion setup is not supported on this platform",
        ));
        Ok(Self { path, owned: true })
    }
}

impl Drop for ExtensionInstallation {
    fn drop(&mut self) {
        if self.owned {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn proof_state_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("target/firefox-companion-proof")
}

async fn launch_firefox(
    config: &InstalledFirefoxConfig,
    descriptor_path: &Path,
) -> Result<(Child, Url, ProcessObservationCollector), CommandError> {
    let mut child = Command::new(&config.firefox_bin)
        .arg("--no-remote")
        .arg("--profile")
        .arg(&config.profile)
        .arg("--remote-debugging-port=0")
        .env("AUTOMATION_RUNTIME_FIREFOX_DESCRIPTOR", descriptor_path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(io_error)?;
    let (sender, mut receiver) = tokio::sync::mpsc::channel(8);
    let process_observations = ProcessObservationCollector::new();
    if let Some(stdout) = child.stdout.take() {
        process_observations.spawn_reader(stdout, sender.clone());
    }
    if let Some(stderr) = child.stderr.take() {
        process_observations.spawn_reader(stderr, sender.clone());
    }
    drop(sender);
    let url = tokio::time::timeout(PROOF_TIMEOUT, receiver.recv())
        .await
        .map_err(|_| {
            workflow_error(
                ErrorCode::BrowserLaunchFailed,
                "Firefox BiDi endpoint timed out",
            )
        })?
        .ok_or_else(|| {
            workflow_error(
                ErrorCode::BrowserLaunchFailed,
                "Firefox exited without a BiDi endpoint",
            )
        })?;
    Ok((child, url, process_observations))
}

struct ProcessObservationCollector {
    findings: Arc<std::sync::Mutex<Vec<String>>>,
    readers: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
}

impl ProcessObservationCollector {
    fn new() -> Self {
        Self {
            findings: Arc::new(std::sync::Mutex::new(Vec::new())),
            readers: std::sync::Mutex::new(Vec::new()),
        }
    }

    fn spawn_reader(
        &self,
        stream: impl tokio::io::AsyncRead + Unpin + Send + 'static,
        sender: tokio::sync::mpsc::Sender<Url>,
    ) {
        let findings = Arc::clone(&self.findings);
        let task = tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            let mut endpoint_sent = false;
            while let Ok(Some(line)) = lines.next_line().await {
                if contains_sensitive_marker(&line) {
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

fn websocket_url(line: &str) -> Option<Url> {
    let start = line.find("ws://")?;
    let candidate = line[start..]
        .split(|character: char| character.is_whitespace() || character == '"')
        .next()?;
    Url::parse(candidate).ok()
}

async fn wait_for_profile_id(profile: &Path) -> Result<ProfileId, CommandError> {
    let storage = profile
        .join("browser-extension-data")
        .join(EXTENSION_ID)
        .join("storage.js");
    let result = tokio::time::timeout(PROOF_TIMEOUT, async {
        loop {
            if let Ok(contents) = std::fs::read(&storage) {
                if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&contents) {
                    if let Some(id) = find_string(&value, "profileId")
                        .and_then(|id| uuid::Uuid::parse_str(id).ok())
                    {
                        return ProfileId(id);
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    result.map_err(|_| {
        workflow_error(
            ErrorCode::BrowserLaunchFailed,
            "companion profile identity timed out",
        )
    })
}

fn find_string<'a>(value: &'a serde_json::Value, key: &str) -> Option<&'a str> {
    match value {
        serde_json::Value::Object(object) => object
            .get(key)
            .and_then(serde_json::Value::as_str)
            .or_else(|| object.values().find_map(|value| find_string(value, key))),
        serde_json::Value::Array(values) => values.iter().find_map(|value| find_string(value, key)),
        _ => None,
    }
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

        let collector = ProcessObservationCollector::new();
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
            .write_all(b"Authorization: Bearer never-retain-this\n")
            .await
            .unwrap();
        stdout_writer.shutdown().await.unwrap();
        stderr_writer.shutdown().await.unwrap();
        let findings = collector.finish().await;
        assert_eq!(findings, vec!["firefoxProcess"]);
        assert!(!format!("{findings:?}").contains("never-retain-this"));
    }
}
