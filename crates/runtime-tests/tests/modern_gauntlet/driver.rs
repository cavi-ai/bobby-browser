use std::fmt;
use std::path::{Path, PathBuf};

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, ServerConfig, StorageConfig};
use sdk_core::RuntimeService;
use types::{
    AccessibilitySnapshotCommand, AttemptId, CaptureScreenshotCommand, CheckpointId,
    CheckpointInvariant, ClickAndWaitForDownloadCommand, ClickAndWaitForPopupCommand, ClickCommand,
    CommandClass, CommandEnvelope, CommandId, CommandOutcome, ControlAction, ControlActionCommand,
    CreateSessionRequest, Evidence, FormControlTarget, InspectCommand, ListPagesCommand,
    NavigateCommand, OpenPageRequest, PageId, PrimitiveCommand, RecoveryDecision, RuntimeCommand,
    ScreenshotMode, SessionId, TargetSpec, TypeTextCommand, UploadFilesCommand, WaitCondition,
    WaitForCommand, WaitUntil, WorkflowCheckpoint, WorkflowId,
};

use super::scenario::ScenarioServer;

type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Clone, Copy)]
pub enum Journey {
    CustomerUpdate,
    Onboarding,
    Documents,
    Authorization,
    ReportRecovery,
}

impl Journey {
    pub fn id(self) -> &'static str {
        match self {
            Self::CustomerUpdate => "customer-update",
            Self::Onboarding => "onboarding",
            Self::Documents => "documents",
            Self::Authorization => "authorization",
            Self::ReportRecovery => "report-recovery",
        }
    }
}

#[derive(Debug)]
pub enum HarnessError {
    MissingBundle { path: PathBuf },
    MissingBrowser { path: PathBuf },
}

impl fmt::Display for HarnessError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingBundle { path } => write!(
                formatter,
                "Northstar production bundle is missing at {}",
                path.display()
            ),
            Self::MissingBrowser { path } => write!(
                formatter,
                "installed Chromium is missing at {}",
                path.display()
            ),
        }
    }
}

impl std::error::Error for HarnessError {}

pub struct ModernRuntime {
    runtime: RuntimeService,
    session_id: SessionId,
    page_id: PageId,
    root: PathBuf,
    journal_path: PathBuf,
    downloads_dir: PathBuf,
    artifacts_dir: PathBuf,
    profile: String,
}

impl fmt::Debug for ModernRuntime {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("ModernRuntime")
            .field("session_id", &self.session_id)
            .field("page_id", &self.page_id)
            .field("root", &self.root)
            .finish()
    }
}

impl ModernRuntime {
    pub async fn launch(server: &ScenarioServer, journey: Journey) -> TestResult<Self> {
        let dist = repository_root().join("packages/bobby-gauntlet/dist");
        let runtime = Self::launch_at(&dist, journey.id()).await?;
        runtime.write_run_manifest(journey.id(), "running", None)?;
        runtime
            .navigate(&server.application_url(match journey {
                Journey::CustomerUpdate => "/customers",
                Journey::Onboarding => "/onboarding",
                Journey::Documents => "/customers/cus_atlas/documents",
                Journey::Authorization => "/integrations",
                Journey::ReportRecovery => "/reports",
            }))
            .await?;
        runtime
            .submit(PrimitiveCommand::CaptureScreenshot(
                CaptureScreenshotCommand {
                    mode: ScreenshotMode::Viewport,
                },
            ))
            .await?;
        Ok(runtime)
    }

    pub async fn launch_at(dist: &Path, journey: &str) -> TestResult<Self> {
        if !dist.join("index.html").is_file()
            || !dist.join("app.js").is_file()
            || !dist.join("app.css").is_file()
        {
            return Err(Box::new(HarnessError::MissingBundle {
                path: dist.to_path_buf(),
            }));
        }
        let chrome = chrome_executable();
        if !chrome.is_file() {
            return Err(Box::new(HarnessError::MissingBrowser { path: chrome }));
        }
        let root = repository_root()
            .join("target/modern-gauntlet-artifacts/runtime")
            .join(format!(
                "{journey}-{}-{}",
                std::process::id(),
                Utc::now().timestamp_nanos_opt().unwrap_or_default()
            ));
        std::fs::create_dir_all(&root)?;
        let journal_path = root.join("commands.jsonl");
        let downloads_dir = root.join("downloads");
        let artifacts_dir = root.join("artifacts");
        let uploads_dir = root.join("uploads");
        std::fs::create_dir_all(&uploads_dir)?;
        let config = AppConfig {
            http: config::HttpConfig {
                allow_loopback: true,
                ..config::HttpConfig::default()
            },
            server: ServerConfig {
                host: "127.0.0.1".into(),
                port: 0,
                shutdown_timeout_ms: 10_000,
            },
            browser: BrowserConfig {
                executable: Some(chrome),
                profiles_dir: root.join("profiles"),
                headless: true,
                max_active: 1,
                upload_roots: vec![
                    repository_root().join("crates/runtime-tests/tests/fixtures"),
                    uploads_dir,
                ],
                downloads_dir: downloads_dir.clone(),
                artifacts_dir: artifacts_dir.clone(),
                max_artifact_bytes: 8 * 1024 * 1024,
                max_screenshot_dimension: 16_384,
                max_js_result_bytes: 64 * 1024,
                max_js_timeout_ms: 30_000,
            },
            storage: StorageConfig {
                journal_path: journal_path.clone(),
                checkpoints_dir: root.join("checkpoints"),
                authority_path: root.join("authority.json"),
                scheduler_journal_path: root.join("scheduler-jobs.jsonl"),
            },
            interface: config::InterfaceConfig::default(),
            observability: config::ObservabilityConfig::default(),
            vision: config::VisionConfig::default(),
            nodes: Default::default(),
        };
        let profile = format!("northstar-{journey}");
        let runtime = RuntimeService::build(&config).await?;
        let session = runtime
            .create_session(CreateSessionRequest {
                profile: profile.clone(),
                proxy: None,
                execution_policy: Default::default(),
            })
            .await?;
        let page = runtime
            .open_page(OpenPageRequest {
                session_id: session.id.clone(),
            })
            .await?;
        Ok(Self {
            runtime,
            session_id: session.id,
            page_id: page.id,
            root,
            journal_path,
            downloads_dir,
            artifacts_dir,
            profile,
        })
    }

    pub async fn navigate(&self, url: &str) -> TestResult<Vec<Evidence>> {
        self.submit(PrimitiveCommand::Navigate(NavigateCommand {
            url: url.into(),
            wait_until: WaitUntil::Interactive,
            timeout_ms: 15_000,
        }))
        .await
    }

    pub async fn click(&self, selector: &str, boundary: bool) -> TestResult<Vec<Evidence>> {
        let command = PrimitiveCommand::Click(ClickCommand {
            selector: selector.into(),
            target: None,
            boundary,
            expected_url: None,
        });
        if boundary {
            self.submit_boundary(command).await
        } else {
            self.submit(command).await
        }
    }

    pub async fn type_text(&self, selector: &str, value: &str) -> TestResult<Vec<Evidence>> {
        self.submit(PrimitiveCommand::TypeText(TypeTextCommand {
            selector: selector.into(),
            target: None,
            value: value.into(),
            clear_first: true,
            expected_url: None,
        }))
        .await
    }

    pub async fn wait_visible(&self, selector: &str) -> TestResult<Vec<Evidence>> {
        self.submit(PrimitiveCommand::WaitFor(WaitForCommand {
            condition: WaitCondition::Element {
                target: Box::new(css_target(selector)),
                state: types::ElementState::Visible,
            },
            timeout_ms: 10_000,
        }))
        .await
    }

    pub async fn inspect(&self, selector: Option<&str>) -> TestResult<Vec<Evidence>> {
        self.submit(PrimitiveCommand::Inspect(InspectCommand {
            selector: selector.map(Into::into),
            target: None,
            include_html: true,
        }))
        .await
    }

    pub async fn accessibility_snapshot(&self) -> TestResult<Vec<Evidence>> {
        self.submit(PrimitiveCommand::AccessibilitySnapshot(
            AccessibilitySnapshotCommand {
                max_nodes: Some(256),
            },
        ))
        .await
    }

    pub async fn page_count(&self) -> TestResult<usize> {
        self.submit(PrimitiveCommand::ListPages(ListPagesCommand))
            .await?
            .into_iter()
            .find_map(|item| match item {
                Evidence::Pages { pages } => Some(pages.len()),
                _ => None,
            })
            .ok_or_else(|| "list-pages command completed without page evidence".into())
    }

    pub async fn select_one(
        &self,
        accessible_name: &str,
        value: &str,
    ) -> TestResult<Vec<Evidence>> {
        self.submit(PrimitiveCommand::ControlAction(ControlActionCommand {
            target: FormControlTarget {
                role: "combobox".into(),
                accessible_name: accessible_name.into(),
                ordinal: None,
                frame_path: Vec::new(),
                shadow_path: Vec::new(),
            },
            action: ControlAction::SelectOne {
                value: value.into(),
            },
        }))
        .await
    }

    pub async fn upload(&self, selector: &str, path: &Path) -> TestResult<Vec<Evidence>> {
        self.submit(PrimitiveCommand::UploadFiles(UploadFilesCommand {
            selector: selector.into(),
            target: None,
            paths: vec![path.to_string_lossy().into_owned()],
        }))
        .await
    }

    pub async fn click_popup(&self, selector: &str) -> TestResult<PageId> {
        let evidence = self
            .submit_boundary(PrimitiveCommand::ClickAndWaitForPopup(
                ClickAndWaitForPopupCommand {
                    selector: selector.into(),
                    target: None,
                    timeout_ms: 10_000,
                },
            ))
            .await?;
        evidence
            .into_iter()
            .find_map(|item| match item {
                Evidence::Popup { page_id, .. } => Some(page_id),
                _ => None,
            })
            .ok_or_else(|| "popup command completed without popup evidence".into())
    }

    pub async fn click_on(&self, page_id: &PageId, selector: &str) -> TestResult<Vec<Evidence>> {
        self.submit_boundary_on(
            page_id,
            PrimitiveCommand::Click(ClickCommand {
                selector: selector.into(),
                target: None,
                boundary: true,
                expected_url: None,
            }),
        )
        .await
    }

    pub async fn click_in_frame(
        &self,
        frame_selector: &str,
        button_selector: &str,
    ) -> TestResult<Vec<Evidence>> {
        let mut target = TargetSpec {
            css: Some(button_selector.into()),
            ..TargetSpec::default()
        };
        target.frame_path = vec![Box::new(TargetSpec {
            css: Some(frame_selector.into()),
            ..TargetSpec::default()
        })];
        self.submit_boundary(PrimitiveCommand::Click(ClickCommand {
            selector: String::new(),
            target: Some(target),
            boundary: true,
            expected_url: None,
        }))
        .await
    }

    pub async fn wait_in_frame_button(
        &self,
        frame_selector: &str,
        button_selector: &str,
    ) -> TestResult<Vec<Evidence>> {
        let mut target = TargetSpec {
            css: Some(button_selector.into()),
            ..TargetSpec::default()
        };
        target.frame_path = vec![Box::new(TargetSpec {
            css: Some(frame_selector.into()),
            ..TargetSpec::default()
        })];
        self.submit(PrimitiveCommand::WaitFor(WaitForCommand {
            condition: WaitCondition::Element {
                target: Box::new(target),
                state: types::ElementState::Visible,
            },
            timeout_ms: 10_000,
        }))
        .await
    }

    pub async fn click_download(&self, selector: &str) -> TestResult<Vec<Evidence>> {
        self.submit_boundary(PrimitiveCommand::ClickAndWaitForDownload(
            ClickAndWaitForDownloadCommand {
                selector: selector.into(),
                target: None,
                timeout_ms: 15_000,
            },
        ))
        .await
    }

    pub async fn submit(&self, command: PrimitiveCommand) -> TestResult<Vec<Evidence>> {
        self.submit_on(&self.page_id, command).await
    }

    async fn submit_on(
        &self,
        page_id: &PageId,
        command: PrimitiveCommand,
    ) -> TestResult<Vec<Evidence>> {
        let debug = format!("{command:?}");
        let outcome = self
            .runtime
            .submit(CommandEnvelope {
                schema_version: CommandEnvelope::SCHEMA_VERSION,
                command_id: CommandId::new(),
                workflow_id: WorkflowId::new(),
                attempt_id: AttemptId::new(),
                session_id: self.session_id.clone(),
                page_id: Some(page_id.clone()),
                deadline: Utc::now() + Duration::seconds(30),
                command: RuntimeCommand::Primitive(command),
            })
            .await;
        match outcome {
            CommandOutcome::Completed { evidence, .. } => Ok(evidence),
            other => {
                self.capture_failure_state(page_id, &debug, &format!("{other:?}"))
                    .await;
                Err(format!("public runtime command {debug} failed: {other:?}").into())
            }
        }
    }

    async fn submit_boundary(&self, command: PrimitiveCommand) -> TestResult<Vec<Evidence>> {
        Ok(self
            .submit_boundary_on_with_workflow(&self.page_id, command)
            .await?
            .0)
    }

    async fn submit_boundary_on(
        &self,
        page_id: &PageId,
        command: PrimitiveCommand,
    ) -> TestResult<Vec<Evidence>> {
        Ok(self
            .submit_boundary_on_with_workflow(page_id, command)
            .await?
            .0)
    }

    async fn submit_boundary_on_with_workflow(
        &self,
        page_id: &PageId,
        command: PrimitiveCommand,
    ) -> TestResult<(Vec<Evidence>, WorkflowId)> {
        let workflow_id = WorkflowId::new();
        let attempt_id = AttemptId::new();
        let inspect_id = CommandId::new();
        let preflight = self
            .runtime
            .submit(CommandEnvelope {
                schema_version: CommandEnvelope::SCHEMA_VERSION,
                command_id: inspect_id.clone(),
                workflow_id: workflow_id.clone(),
                attempt_id: attempt_id.clone(),
                session_id: self.session_id.clone(),
                page_id: Some(page_id.clone()),
                deadline: Utc::now() + Duration::seconds(30),
                command: RuntimeCommand::Primitive(PrimitiveCommand::Inspect(
                    InspectCommand::default(),
                )),
            })
            .await;
        let observed = match preflight {
            CommandOutcome::Completed { evidence, .. } => evidence,
            other => {
                self.capture_failure_state(page_id, "boundary preflight", &format!("{other:?}"))
                    .await;
                return Err(format!("boundary preflight failed: {other:?}").into());
            }
        };
        let (url, title) = observed
            .iter()
            .find_map(|item| match item {
                Evidence::Inspection { url, title, .. } => Some((url.clone(), title.clone())),
                _ => None,
            })
            .ok_or("boundary preflight completed without inspection evidence")?;
        let command_id = CommandId::new();
        self.runtime
            .checkpoint(
                WorkflowCheckpoint {
                    schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
                    checkpoint_id: CheckpointId::new(),
                    workflow_id: workflow_id.clone(),
                    attempt_id: attempt_id.clone(),
                    session_id: self.session_id.clone(),
                    page_id: page_id.clone(),
                    restart_url: url.clone(),
                    current_url: url.clone(),
                    cursor: Some(inspect_id.clone()),
                    boundary_command_id: Some(command_id.clone()),
                    recovery_class: CommandClass::Boundary,
                    invariants: vec![
                        CheckpointInvariant::Url { value: url },
                        CheckpointInvariant::Title { value: title },
                    ],
                    replayable_inputs: Vec::new(),
                    evidence: Vec::new(),
                    recovery_history: Vec::new(),
                    recovery_receipts: Vec::new(),
                    created_at: Utc::now(),
                },
                vec![inspect_id],
            )
            .await?;
        let debug = format!("{command:?}");
        match self
            .runtime
            .submit(CommandEnvelope {
                schema_version: CommandEnvelope::SCHEMA_VERSION,
                command_id,
                workflow_id: workflow_id.clone(),
                attempt_id,
                session_id: self.session_id.clone(),
                page_id: Some(page_id.clone()),
                deadline: Utc::now() + Duration::seconds(30),
                command: RuntimeCommand::Primitive(command),
            })
            .await
        {
            CommandOutcome::Completed { evidence, .. } => Ok((evidence, workflow_id)),
            other => {
                self.capture_failure_state(page_id, &debug, &format!("{other:?}"))
                    .await;
                Err(format!("public boundary command {debug} failed: {other:?}").into())
            }
        }
    }

    pub async fn click_boundary_with_workflow(&self, selector: &str) -> TestResult<WorkflowId> {
        let (_, workflow_id) = self
            .submit_boundary_on_with_workflow(
                &self.page_id,
                PrimitiveCommand::Click(ClickCommand {
                    selector: selector.into(),
                    target: None,
                    boundary: true,
                    expected_url: None,
                }),
            )
            .await?;
        Ok(workflow_id)
    }

    pub async fn restart_and_recover(
        self,
        workflow_id: &WorkflowId,
        application_url: &str,
    ) -> TestResult<(Self, RecoveryDecision)> {
        let Self {
            runtime,
            root,
            journal_path,
            downloads_dir,
            artifacts_dir,
            profile,
            ..
        } = self;
        drop(runtime);
        let config = runtime_config(
            &root,
            chrome_executable(),
            journal_path.clone(),
            downloads_dir.clone(),
            artifacts_dir.clone(),
        );
        let runtime = RuntimeService::build(&config).await?;
        let decision = runtime.recover(workflow_id).await?;
        let _checkpoint = runtime.recovery_status(workflow_id).await?.checkpoint;
        let session = runtime
            .create_session(CreateSessionRequest {
                profile: format!("{profile}-replacement"),
                proxy: None,
                execution_policy: Default::default(),
            })
            .await?;
        let page = runtime
            .open_page(OpenPageRequest {
                session_id: session.id.clone(),
            })
            .await?;
        let replacement = Self {
            runtime,
            session_id: session.id,
            page_id: page.id,
            root,
            journal_path,
            downloads_dir,
            artifacts_dir,
            profile,
        };
        replacement.navigate(application_url).await?;
        Ok((replacement, decision))
    }

    pub fn journal_path(&self) -> &Path {
        &self.journal_path
    }

    pub async fn capture_diagnostics(&self, journey: &str) -> TestResult<()> {
        let evidence = self
            .submit(PrimitiveCommand::CaptureScreenshot(
                CaptureScreenshotCommand {
                    mode: ScreenshotMode::Viewport,
                },
            ))
            .await?;
        std::fs::write(
            self.root.join("final-evidence.json"),
            serde_json::to_vec_pretty(&evidence)?,
        )?;
        self.write_run_manifest(journey, "completed", None)
    }

    async fn capture_failure_state(&self, page_id: &PageId, operation: &str, outcome: &str) {
        let mut evidence = Vec::new();
        for command in [
            PrimitiveCommand::CaptureScreenshot(CaptureScreenshotCommand {
                mode: ScreenshotMode::Viewport,
            }),
            PrimitiveCommand::AccessibilitySnapshot(AccessibilitySnapshotCommand {
                max_nodes: Some(512),
            }),
            PrimitiveCommand::Inspect(InspectCommand {
                selector: Some("body".into()),
                target: None,
                include_html: true,
            }),
        ] {
            if let CommandOutcome::Completed {
                evidence: captured, ..
            } = self
                .runtime
                .submit(CommandEnvelope {
                    schema_version: CommandEnvelope::SCHEMA_VERSION,
                    command_id: CommandId::new(),
                    workflow_id: WorkflowId::new(),
                    attempt_id: AttemptId::new(),
                    session_id: self.session_id.clone(),
                    page_id: Some(page_id.clone()),
                    deadline: Utc::now() + Duration::seconds(10),
                    command: RuntimeCommand::Primitive(command),
                })
                .await
            {
                evidence.extend(captured);
            }
        }
        let _ = std::fs::write(
            self.root.join("failure-evidence.json"),
            serde_json::to_vec_pretty(&evidence).unwrap_or_default(),
        );
        let _ = self.write_run_manifest(operation, "failed", Some(outcome));
    }

    fn write_run_manifest(
        &self,
        journey: &str,
        status: &str,
        error: Option<&str>,
    ) -> TestResult<()> {
        std::fs::write(
            self.root.join("run-manifest.json"),
            serde_json::to_vec_pretty(&serde_json::json!({
                "journey": journey,
                "status": status,
                "error": error,
                "browser": chrome_executable(),
                "journal": self.journal_path,
                "artifacts": self.artifacts_dir,
                "capturedAt": Utc::now(),
            }))?,
        )?;
        Ok(())
    }
    pub fn fixture_path(&self, name: &str) -> PathBuf {
        repository_root()
            .join("crates/runtime-tests/tests/fixtures")
            .join(name)
    }
}

pub fn css_target(selector: &str) -> TargetSpec {
    TargetSpec {
        css: Some(selector.into()),
        ..TargetSpec::default()
    }
}

fn chrome_executable() -> PathBuf {
    std::env::var_os("BOBBY_CHROME_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

fn runtime_config(
    root: &Path,
    chrome: PathBuf,
    journal_path: PathBuf,
    downloads_dir: PathBuf,
    artifacts_dir: PathBuf,
) -> AppConfig {
    AppConfig {
        http: config::HttpConfig {
            allow_loopback: true,
            ..config::HttpConfig::default()
        },
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            shutdown_timeout_ms: 10_000,
        },
        browser: BrowserConfig {
            executable: Some(chrome),
            profiles_dir: root.join("profiles"),
            headless: true,
            max_active: 1,
            upload_roots: vec![
                repository_root().join("crates/runtime-tests/tests/fixtures"),
                root.join("uploads"),
            ],
            downloads_dir,
            artifacts_dir,
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
            max_js_result_bytes: 64 * 1024,
            max_js_timeout_ms: 30_000,
        },
        storage: StorageConfig {
            journal_path,
            checkpoints_dir: root.join("checkpoints"),
            authority_path: root.join("authority.json"),
            scheduler_journal_path: root.join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        nodes: Default::default(),
    }
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("runtime-tests is nested beneath repository root")
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::{HarnessError, ModernRuntime};

    #[tokio::test]
    async fn missing_bundle_is_a_typed_startup_failure() {
        let error = ModernRuntime::launch_at(Path::new("missing-northstar-dist"), "missing-bundle")
            .await
            .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<HarnessError>(),
            Some(HarnessError::MissingBundle { .. })
        ));
    }
}
