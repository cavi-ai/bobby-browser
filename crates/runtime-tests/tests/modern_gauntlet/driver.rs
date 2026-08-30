use std::fmt;
use std::path::{Path, PathBuf};

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, ServerConfig, StorageConfig};
use sdk_core::RuntimeService;
use types::{
    AccessibilitySnapshotCommand, AttemptId, CaptureScreenshotCommand, CheckpointId,
    CheckpointInvariant, ClickAndWaitForDownloadCommand, ClickAndWaitForPopupCommand, ClickCommand,
    CommandClass, CommandEnvelope, CommandId, CommandOutcome, ControlAction, ControlActionCommand,
    CreateSessionRequest, ErrorCode, Evidence, FormControlTarget, InspectCommand, IntentCommand,
    ListPagesCommand, NavigateCommand, OpenPageRequest, PageId, PrimitiveCommand, RecoveryDecision,
    RuntimeCommand, ScreenshotMode, SessionId, SolveChallengeIntent, TargetSpec, TypeTextCommand,
    UploadFilesCommand, WaitCondition, WaitForCommand, WaitUntil, WorkflowCheckpoint, WorkflowId,
};

use super::scenario::ScenarioServer;
use super::scorecard::{ModelTier, ProviderMode, Scorecard};

pub type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

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

async fn acquire_live_browser_lock() -> TestResult<std::fs::File> {
    let lock_path = repository_root().join("target/modern-gauntlet-browser.lock");
    tokio::task::spawn_blocking(move || -> TestResult<std::fs::File> {
        #[cfg(unix)]
        use std::os::unix::fs::OpenOptionsExt;

        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        options.mode(0o600).custom_flags(libc::O_CLOEXEC);
        let file = options.open(&lock_path)?;
        // Separate Cargo test binaries share the same installed browser. Hold
        // one advisory lock for the runtime lifetime so one test cannot detach
        // another test's target while the workspace suite runs concurrently.
        file.lock()?;
        Ok(file)
    })
    .await?
}

pub struct ModernRuntime {
    _browser_lock: std::fs::File,
    runtime: RuntimeService,
    session_id: SessionId,
    page_id: PageId,
    root: PathBuf,
    journal_path: PathBuf,
    downloads_dir: PathBuf,
    artifacts_dir: PathBuf,
    profile: String,
    journey: String,
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
        let browser_lock = acquire_live_browser_lock().await?;
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
            cdp: config::CdpConfig::default(),
            mcp: config::McpConfig::default(),
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
            vision: gauntlet_vision_config(),
            context: Default::default(),
            nodes: Default::default(),
        };
        let profile = format!("northstar-{journey}");
        let runtime = RuntimeService::build(&config).await?;
        let session = runtime
            .create_session(CreateSessionRequest {
                profile: profile.clone(),
                proxy: None,
                execution_policy: gauntlet_execution_policy(),
                zigzagzig: false,
            })
            .await?;
        let page = runtime
            .open_page(OpenPageRequest {
                session_id: session.id.clone(),
            })
            .await?;
        Ok(Self {
            _browser_lock: browser_lock,
            runtime,
            session_id: session.id,
            page_id: page.id,
            root,
            journal_path,
            downloads_dir,
            artifacts_dir,
            profile,
            journey: journey.to_owned(),
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
            modifiers: Vec::new(),
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
                target: None,
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
                modifiers: Vec::new(),
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
            modifiers: Vec::new(),
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
    // Only the Level 2 suite drives intents directly; the release suite
    // binary would otherwise report these as dead code.
    #[allow(dead_code)]
    pub async fn submit_intent(&self, command: IntentCommand) -> TestResult<Vec<Evidence>> {
        let debug = serde_json::to_string(&command).unwrap_or_default();
        // Vision intents need both gates: the session policy grant (set by
        // `gauntlet_execution_policy`) and the principal capability flag,
        // which plain `submit` always denies.
        let vision_capable = std::env::var("BOBBY_GAUNTLET_VISION_ENDPOINT")
            .is_ok_and(|endpoint| !endpoint.trim().is_empty());
        let outcome = self
            .runtime
            .submit_with_vision_capability(
                CommandEnvelope {
                    schema_version: CommandEnvelope::SCHEMA_VERSION,
                    command_id: CommandId::new(),
                    workflow_id: WorkflowId::new(),
                    attempt_id: AttemptId::new(),
                    session_id: self.session_id.clone(),
                    page_id: Some(self.page_id.clone()),
                    // Multi-round solves wait on a slow local model per round.
                    deadline: Utc::now() + Duration::seconds(180),
                    command: RuntimeCommand::Intent(command),
                },
                vision_capable,
            )
            .await;
        match outcome {
            CommandOutcome::Completed { evidence, .. } => Ok(evidence),
            other => {
                self.capture_failure_state(&self.page_id, &debug, &format!("{other:?}"))
                    .await;
                Err(format!("intent command {} failed: {:?}", debug, other).into())
            }
        }
    }

    #[allow(dead_code)]
    pub async fn solve_challenge(&self, purpose: &str) -> TestResult<Vec<Evidence>> {
        self.submit_intent(IntentCommand::SolveChallenge(SolveChallengeIntent {
            purpose: purpose.into(),
            hints: types::SolveChallengeHints {
                region: None,
                // One propose round against a local 27B vision model can
                // take 30s+; the default 30s budget would expire mid-grid.
                timeout_ms: 120_000,
            },
        }))
        .await
    }

    pub async fn submit(&self, command: PrimitiveCommand) -> TestResult<Vec<Evidence>> {
        self.submit_on(&self.page_id, command).await
    }

    pub async fn capture_viewport_screenshot(&self) -> TestResult<Vec<Evidence>> {
        const MAX_ATTEMPTS: usize = 2;

        for attempt in 0..MAX_ATTEMPTS {
            let command = PrimitiveCommand::CaptureScreenshot(CaptureScreenshotCommand {
                mode: ScreenshotMode::Viewport,
            });
            let outcome = self
                .runtime
                .submit(CommandEnvelope {
                    schema_version: CommandEnvelope::SCHEMA_VERSION,
                    command_id: CommandId::new(),
                    workflow_id: WorkflowId::new(),
                    attempt_id: AttemptId::new(),
                    session_id: self.session_id.clone(),
                    page_id: Some(self.page_id.clone()),
                    deadline: Utc::now() + Duration::seconds(30),
                    command: RuntimeCommand::Primitive(command),
                })
                .await;
            match outcome {
                CommandOutcome::Completed { evidence, .. } => return Ok(evidence),
                other if retryable_screenshot_failure(&other) && attempt + 1 < MAX_ATTEMPTS => {
                    tokio::task::yield_now().await;
                }
                other => {
                    self.capture_failure_state(
                        &self.page_id,
                        "CaptureScreenshot(Viewport)",
                        &format!("{other:?}"),
                    )
                    .await;
                    return Err(format!(
                        "public runtime screenshot failed after {} attempt(s): {other:?}",
                        attempt + 1
                    )
                    .into());
                }
            }
        }
        unreachable!("bounded screenshot retry loop always returns")
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
            .find_map(|item| match item.journal_safe() {
                Evidence::Inspection { url, title, .. } => Some((url, title)),
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
                    modifiers: Vec::new(),
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
            _browser_lock,
            runtime,
            root,
            journal_path,
            downloads_dir,
            artifacts_dir,
            profile,
            journey,
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
                zigzagzig: false,
            })
            .await?;
        let page = runtime
            .open_page(OpenPageRequest {
                session_id: session.id.clone(),
            })
            .await?;
        let replacement = Self {
            _browser_lock,
            runtime,
            session_id: session.id,
            page_id: page.id,
            root,
            journal_path,
            downloads_dir,
            artifacts_dir,
            profile,
            journey,
        };
        replacement.navigate(application_url).await?;
        Ok((replacement, decision))
    }

    pub fn journal_path(&self) -> &Path {
        &self.journal_path
    }

    pub fn artifacts_dir(&self) -> &Path {
        &self.artifacts_dir
    }

    pub fn scorecard(&self, passed: bool) -> TestResult<Scorecard> {
        let engine = std::env::var("BOBBY_GAUNTLET_ENGINE").unwrap_or_else(|_| "chromium".into());
        let provider_mode = ProviderMode::from_label(
            &std::env::var("BOBBY_GAUNTLET_PROVIDER_MODE").unwrap_or_default(),
        );
        let model_tier =
            ModelTier::from_label(&std::env::var("BOBBY_GAUNTLET_MODEL_TIER").unwrap_or_default());
        Ok(Scorecard::from_journal_with_environment(
            &self.journey,
            engine,
            provider_mode,
            model_tier,
            &self.journal_path,
            passed,
        )?)
    }

    pub fn emit_scorecard(&self, passed: bool) -> TestResult<Scorecard> {
        let scorecard = self.scorecard(passed)?;
        std::fs::write(
            self.root.join("scorecard.json"),
            serde_json::to_vec_pretty(&scorecard)?,
        )?;
        let directory = scorecard_directory()
            .join(&scorecard.engine)
            .join(scorecard.provider_mode.label());
        std::fs::create_dir_all(&directory)?;
        std::fs::write(
            directory.join(format!("{}.json", scorecard.station)),
            serde_json::to_vec_pretty(&scorecard)?,
        )?;
        Ok(scorecard)
    }

    pub async fn capture_diagnostics(&self, journey: &str) -> TestResult<()> {
        let mut evidence = Vec::new();
        for command in [
            PrimitiveCommand::CaptureScreenshot(CaptureScreenshotCommand {
                mode: ScreenshotMode::Viewport,
            }),
            PrimitiveCommand::AccessibilitySnapshot(AccessibilitySnapshotCommand {
                max_nodes: Some(512),
                target: None,
            }),
            PrimitiveCommand::Inspect(InspectCommand {
                selector: Some("body".into()),
                target: None,
                include_html: true,
            }),
        ] {
            evidence.extend(self.submit(command).await?);
        }
        std::fs::write(
            self.root.join("final-evidence.json"),
            serde_json::to_vec_pretty(&evidence)?,
        )?;
        self.write_run_manifest(journey, "evidence-captured", None)
    }

    pub fn mark_completed(&self, journey: &str) -> TestResult<()> {
        self.write_run_manifest(journey, "completed", None)
    }

    async fn capture_failure_state(&self, page_id: &PageId, operation: &str, outcome: &str) {
        let _ = self.emit_scorecard(false);
        let mut evidence = Vec::new();
        for command in [
            PrimitiveCommand::CaptureScreenshot(CaptureScreenshotCommand {
                mode: ScreenshotMode::Viewport,
            }),
            PrimitiveCommand::AccessibilitySnapshot(AccessibilitySnapshotCommand {
                max_nodes: Some(512),
                target: None,
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

fn retryable_screenshot_failure(outcome: &CommandOutcome) -> bool {
    matches!(
        outcome,
        CommandOutcome::RetryableFailure { error, .. }
            if error.retryable && error.code == ErrorCode::ScreenshotCaptureFailed
    )
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

/// Env-gated vision config for collection runs (mirrors the Level 2
/// `BOBBY_GAUNTLET_*` pattern). Set `BOBBY_GAUNTLET_VISION_ENDPOINT` to the
/// loopback proxy URL and `BOBBY_GAUNTLET_VISION_TOKEN_ENV` to the env var
/// holding its bearer; the harness then runs with vision assist enabled and
/// every stuck escalation flows through the configured provider.
fn gauntlet_vision_config() -> config::VisionConfig {
    match std::env::var("BOBBY_GAUNTLET_VISION_ENDPOINT") {
        Ok(endpoint) if !endpoint.trim().is_empty() => config::VisionConfig {
            endpoint_url: Some(endpoint),
            token_env: Some(
                std::env::var("BOBBY_GAUNTLET_VISION_TOKEN_ENV")
                    .unwrap_or_else(|_| "BOBBY_VISION_TOKEN".to_string()),
            ),
            // Engine-side corpus: every escalation's terminal outcome lands
            // here (verified clicks carry the resolved target index). Set
            // BOBBY_GAUNTLET_VISION_CORPUS_DIR for harvest runs.
            corpus_dir: std::env::var("BOBBY_GAUNTLET_VISION_CORPUS_DIR")
                .ok()
                .filter(|dir| !dir.trim().is_empty())
                .map(std::path::PathBuf::from),
            // Local vision models need far longer than the 15s default for a
            // full-viewport screenshot proposal.
            timeout_ms: 120_000,
            ..config::VisionConfig::default()
        },
        _ => config::VisionConfig::default(),
    }
}

fn gauntlet_execution_policy() -> types::ExecutionPolicy {
    let vision_assist = std::env::var("BOBBY_GAUNTLET_VISION_ENDPOINT")
        .is_ok_and(|endpoint| !endpoint.trim().is_empty());
    types::ExecutionPolicy {
        vision_assist,
        // The legacy `[vision]` endpoint registers as the "vision" node; the
        // session must name it or no provider resolves (deny-by-default).
        vision_node: vision_assist.then(|| "vision".to_string()),
        ..Default::default()
    }
}

fn runtime_config(
    root: &Path,
    chrome: PathBuf,
    journal_path: PathBuf,
    downloads_dir: PathBuf,
    artifacts_dir: PathBuf,
) -> AppConfig {
    AppConfig {
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
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
        context: Default::default(),
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

fn scorecard_directory() -> PathBuf {
    match std::env::var_os("BOBBY_GAUNTLET_SCORECARD_DIR").map(PathBuf::from) {
        Some(path) if path.is_absolute() => path,
        Some(path) => repository_root().join(path),
        None => repository_root().join("benchmarks/results/p0-baseline"),
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use types::{CommandError, CommandId, CommandOutcome, ErrorCode, ErrorLayer};

    use super::{retryable_screenshot_failure, HarnessError, ModernRuntime};

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

    #[test]
    fn only_retryable_screenshot_failures_are_retried() {
        let outcome = |code, retryable| CommandOutcome::RetryableFailure {
            command_id: CommandId::new(),
            error: CommandError {
                code,
                message: "oneshot canceled".into(),
                layer: ErrorLayer::Page,
                retryable,
            },
        };
        assert!(retryable_screenshot_failure(&outcome(
            ErrorCode::ScreenshotCaptureFailed,
            true
        )));
        assert!(!retryable_screenshot_failure(&outcome(
            ErrorCode::ScreenshotCaptureFailed,
            false
        )));
        assert!(!retryable_screenshot_failure(&outcome(
            ErrorCode::TargetDetached,
            true
        )));
    }
}
