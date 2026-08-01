use std::collections::BTreeMap;
use std::error::Error;
use std::net::SocketAddr;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

use axum::body::Body;
use axum::extract::State;
use axum::http::{header, Request, Response, StatusCode};
use axum::routing::get;
use axum::Router;
use checkpoint_store::CheckpointStore;
use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, BrowserSelectionConfig, EnginePreferenceConfig};
use page_runtime::{
    PageRuntime, RecoveryCoordinator, SkillRecoveryCoordinator, SkillRecoveryExecution,
};
use runtime_tests::{
    launch_installed_firefox_runtime, InstalledFirefoxConfig, InstalledFirefoxRuntime,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use skill_runtime::{
    SkillCommandReceipt, SkillCommandRouter, SkillContext, SkillEngineAdapter, SkillGhost,
    SkillRegistry, SkillStateStore, SkillZigZagZigController,
};
use tokio::sync::Mutex;
use types::{
    AttemptId, CaptureScreenshotCommand, ClickAndWaitForDownloadCommand,
    ClickAndWaitForPopupCommand, ClickCommand, CommandEnvelope, CommandId, CommandOutcome,
    CompleteFormField, CompleteFormIntent, ElementState, ErrorCode, Evidence, FillIntent,
    FillValue, InspectCommand, IntentCommand, IntentHints, NavigateCommand, PageId,
    PrimitiveCommand, RuntimeCommand, ScreenshotMode, SessionId, SkillBrowserEngine,
    SkillCapability, SkillFailure, SkillOutcome, SkillProfileRequest, SkillSessionState,
    TargetSpec, UploadFilesCommand, WaitCondition, WaitForCommand, WaitUntil, WorkflowId,
};
use uuid::Uuid;
use worker_pool::{
    ChromiumSkillAdapter, FirefoxSkillAdapter, WorkerFactory, WorkerPool,
    CHROMIUM_PRODUCTION_SKILL_PROFILE_VERSION, FIREFOX_PRODUCTION_SKILL_PROFILE_VERSION,
};
use workflow_journal::JsonlJournal;

type TestResult<T> = Result<T, Box<dyn Error + Send + Sync>>;

const STATIONS: [&str; 10] = [
    "route",
    "dom-drift",
    "semantic-form",
    "validation",
    "iframe",
    "shadow-root",
    "popup",
    "file-attachment",
    "download",
    "championship",
];

#[derive(Clone)]
struct GauntletTarget {
    seed: String,
}

fn gauntlet_url(seed: &str) -> GauntletTarget {
    GauntletTarget {
        seed: seed.to_owned(),
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StationScore {
    id: String,
    version: String,
    mutation_version: String,
    passed: bool,
    evidence: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ScreenshotProof {
    station_id: String,
    artifact_id: String,
    logical_ref: String,
    media_type: String,
    width: u32,
    height: u32,
    bytes: u64,
    sha256: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppScorecardReceipt {
    manifest_digest: String,
    passed: bool,
    stations: Vec<AppStationReceipt>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppStationReceipt {
    id: String,
    version: String,
    mutation_version: String,
    passed: bool,
    evidence: Vec<AppEvidenceReceipt>,
}

#[derive(Debug, Clone, Deserialize)]
struct AppEvidenceReceipt {
    id: String,
}

fn validated_championship_stations(
    receipt: &AppScorecardReceipt,
    expected_manifest_digest: &str,
) -> TestResult<Vec<StationScore>> {
    if receipt.manifest_digest != expected_manifest_digest
        || !receipt.passed
        || receipt.stations.len() != STATIONS.len()
    {
        return Err(test_error(
            "app championship receipt is not one complete passing aggregate",
        ));
    }
    receipt
        .stations
        .iter()
        .zip(STATIONS)
        .map(|(station, expected_id)| {
            if station.id != expected_id
                || station.version != "1"
                || station.mutation_version != "1"
                || !station.passed
            {
                return Err(test_error(
                    "app championship receipt does not match the mandatory station ledger",
                ));
            }
            Ok(StationScore {
                id: station.id.clone(),
                version: station.version.clone(),
                mutation_version: station.mutation_version.clone(),
                passed: station.passed,
                evidence: station
                    .evidence
                    .iter()
                    .map(|evidence| evidence.id.clone())
                    .collect(),
            })
        })
        .collect()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SkillRouteProof {
    input: String,
    alias: String,
    skill_name: String,
    skill_version: String,
    outcome: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GhostProfileProof {
    profile_version: String,
    engine: String,
    effective_capabilities: Vec<String>,
    observable_digest: String,
    unsupported_optional: Vec<String>,
    restart_required: bool,
    worker_id: String,
    worker_profile_binding_digest: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecoveryTacticProof {
    tactic: String,
    trigger: String,
    effect: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RecoveryProof {
    station_id: String,
    command_id: String,
    terminal_failure: String,
    tactics: Vec<RecoveryTacticProof>,
    executing_records: u64,
    restarted_records: u64,
    journal_receipt_sha256: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ReleaseScorecard {
    course_version: String,
    seed: String,
    difficulty: String,
    manifest_digest: String,
    stations: Vec<StationScore>,
    screenshots: Vec<ScreenshotProof>,
    engine: String,
    active_skills: BTreeMap<String, String>,
    skill_routes: Vec<SkillRouteProof>,
    ghost_profile: GhostProfileProof,
    recovery_proofs: Vec<RecoveryProof>,
    journal_sha256: String,
    recovery_count: u64,
    strategy_changes: Vec<String>,
    duration_ms: u64,
}

impl ReleaseScorecard {
    fn verify_all_required(&self, run_dir: &Path) -> TestResult<()> {
        if self.course_version != "course-v1"
            || self.difficulty != "foundation"
            || self.manifest_digest != manifest_digest(&self.seed)
        {
            return Err(test_error(
                "scorecard is not bound to the championship manifest",
            ));
        }
        if self.stations.len() != STATIONS.len()
            || self
                .stations
                .iter()
                .zip(STATIONS)
                .any(|(station, expected)| {
                    station.id != expected
                        || station.version != "1"
                        || station.mutation_version != "1"
                        || !station.passed
                        || station.evidence.is_empty()
                })
        {
            return Err(test_error(
                "a mandatory station is missing, reordered, or failed",
            ));
        }
        verify_screenshot_proofs(run_dir, &self.screenshots)?;
        if self.active_skills.len() != 2
            || self.active_skills.get("SkillGhost").map(String::as_str) != Some("1.0.0")
            || self.active_skills.get("SkillZigZagZig").map(String::as_str) != Some("1.0.0")
            || self.strategy_changes != ["observeAgain"]
        {
            return Err(test_error("required Bobby skill evidence is missing"));
        }
        let expected_routes = [
            ("/ghost on", "/ghost", "SkillGhost"),
            ("/zigzagzig run", "/zigzagzig", "SkillZigZagZig"),
        ];
        if self.skill_routes.len() != expected_routes.len()
            || self.skill_routes.iter().zip(expected_routes).any(
                |(receipt, (input, alias, skill_name))| {
                    receipt.input != input
                        || receipt.alias != alias
                        || receipt.skill_name != skill_name
                        || receipt.skill_version != "1.0.0"
                        || receipt.outcome != "applied"
                },
            )
        {
            return Err(test_error(
                "public skill router receipts are missing or reordered",
            ));
        }
        let expected_profile_version = match self.engine.as_str() {
            "chromium" => CHROMIUM_PRODUCTION_SKILL_PROFILE_VERSION,
            "firefox" => FIREFOX_PRODUCTION_SKILL_PROFILE_VERSION,
            _ => return Err(test_error("scorecard names an unsupported browser engine")),
        };
        if self.ghost_profile.profile_version != expected_profile_version
            || self.ghost_profile.engine != self.engine
            || self.ghost_profile.effective_capabilities
                != ["engineSelection", "profilePersistence"]
            || !is_lower_sha256(&self.ghost_profile.observable_digest)
            || !self.ghost_profile.unsupported_optional.is_empty()
            || self.ghost_profile.restart_required
            || self.ghost_profile.worker_id.is_empty()
            || !is_lower_sha256(&self.ghost_profile.worker_profile_binding_digest)
        {
            return Err(test_error(
                "Ghost effective profile is not bound to the launched worker",
            ));
        }
        if self.recovery_count != 2
            || self.recovery_proofs.len() != 2
            || self.recovery_proofs[0].station_id != "dom-drift"
            || Uuid::parse_str(&self.recovery_proofs[0].command_id).is_err()
            || self.recovery_proofs[0].terminal_failure != "targetDrift"
            || self.recovery_proofs[0].tactics.len() != 1
            || self.recovery_proofs[0].tactics[0].tactic != "observeAgain"
            || self.recovery_proofs[0].tactics[0].trigger != "targetDrift"
            || self.recovery_proofs[0].tactics[0].effect != "reconciliationRequired"
            || self.recovery_proofs[0].executing_records != 1
            || self.recovery_proofs[0].restarted_records != 0
            || !is_lower_sha256(&self.recovery_proofs[0].journal_receipt_sha256)
            || self.recovery_proofs[1].station_id != "popup"
            || Uuid::parse_str(&self.recovery_proofs[1].command_id).is_err()
            || self.recovery_proofs[1].terminal_failure != "effectUncertain"
            || !self.recovery_proofs[1].tactics.is_empty()
            || self.recovery_proofs[1].executing_records != 1
            || self.recovery_proofs[1].restarted_records != 0
            || !is_lower_sha256(&self.recovery_proofs[1].journal_receipt_sha256)
            || !is_lower_sha256(&self.journal_sha256)
        {
            return Err(test_error("production skill recovery proof is incomplete"));
        }
        verify_journal_proofs(run_dir, &self.recovery_proofs, &self.journal_sha256)?;
        let serialized = self.serialized();
        if contains_secret_marker(&serialized) {
            return Err(test_error(
                "scorecard contains a secret or unrestricted host path",
            ));
        }
        Ok(())
    }

    fn serialized(&self) -> String {
        serde_json::to_string(self).expect("release scorecard serialization is infallible")
    }
}

struct StaticGauntlet {
    addr: SocketAddr,
    task: tokio::task::JoinHandle<()>,
}

struct PendingRecoveryProof {
    station_id: String,
    command_id: CommandId,
    terminal_failure: SkillFailure,
    execution: SkillRecoveryExecution,
}

impl StaticGauntlet {
    async fn start(root: PathBuf) -> TestResult<Self> {
        if !root.join("championship/index.html").is_file() {
            return Err(test_error(
                "built Bobby gauntlet is missing; run pnpm --filter @bobby-browser/gauntlet build",
            ));
        }
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let app = Router::new()
            .fallback(get(static_file))
            .with_state(Arc::new(root));
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self { addr, task })
    }

    fn station_url(&self, station: &str, seed: &str) -> String {
        format!(
            "http://{}/station/{station}/?seed={seed}&difficulty=foundation",
            self.addr
        )
    }

    fn championship_url(&self, seed: &str) -> String {
        format!(
            "http://{}/championship/?seed={seed}&difficulty=foundation",
            self.addr
        )
    }
}

impl Drop for StaticGauntlet {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn static_file(State(root): State<Arc<PathBuf>>, request: Request<Body>) -> Response<Body> {
    let relative = request.uri().path().trim_start_matches('/');
    if relative.split('/').any(|segment| segment == "..") {
        return response(StatusCode::BAD_REQUEST, "text/plain", b"bad path".to_vec());
    }
    let mut path = root.join(relative);
    if relative.is_empty() || path.is_dir() {
        path = path.join("index.html");
    }
    let canonical_root = match tokio::fs::canonicalize(root.as_ref()).await {
        Ok(path) => path,
        Err(_) => return response(StatusCode::INTERNAL_SERVER_ERROR, "text/plain", Vec::new()),
    };
    let canonical_path = match tokio::fs::canonicalize(&path).await {
        Ok(path) if path.starts_with(&canonical_root) => path,
        _ => return response(StatusCode::NOT_FOUND, "text/plain", Vec::new()),
    };
    match tokio::fs::read(&canonical_path).await {
        Ok(bytes) => response(StatusCode::OK, content_type(&canonical_path), bytes),
        Err(_) => response(StatusCode::NOT_FOUND, "text/plain", Vec::new()),
    }
}

fn response(status: StatusCode, content_type: &'static str, bytes: Vec<u8>) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(bytes))
        .expect("static response is valid")
}

fn content_type(path: &Path) -> &'static str {
    match path.extension().and_then(|extension| extension.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("txt") => "text/plain; charset=utf-8",
        _ => "application/octet-stream",
    }
}

struct ProductionBobby {
    gauntlet: StaticGauntlet,
    runtime: PageRuntime,
    workers: Arc<WorkerPool>,
    recovery: RecoveryCoordinator,
    session_id: SessionId,
    page_id: Mutex<Option<PageId>>,
    workflow_id: WorkflowId,
    attempt_id: AttemptId,
    router: SkillCommandRouter,
    context: SkillContext,
    ghost: Arc<SkillGhost>,
    state_store: Arc<SkillStateStore>,
    zigzagzig: Arc<SkillZigZagZigController>,
    coordinator: Mutex<Option<Arc<SkillRecoveryCoordinator>>>,
    skill_routes: Mutex<Vec<SkillRouteProof>>,
    recovery_proofs: Mutex<Vec<PendingRecoveryProof>>,
    engine: SkillBrowserEngine,
    seed: String,
    fixture: PathBuf,
    run_dir: PathBuf,
    _installed_firefox: Option<InstalledFirefoxRuntime>,
}

impl ProductionBobby {
    async fn launch(target: GauntletTarget) -> TestResult<Self> {
        let repository = repository_root();
        let dist = repository.join("packages/bobby-gauntlet/dist");
        let gauntlet = StaticGauntlet::start(dist).await?;
        let engine = configured_engine()?;
        let run_dir = repository
            .join("target/bobby-championship")
            .join(engine_name(engine))
            .join(&target.seed);
        if run_dir.exists() {
            std::fs::remove_dir_all(&run_dir)?;
        }
        std::fs::create_dir_all(&run_dir)?;
        let fixture = repository.join("packages/bobby-gauntlet/fixtures/approved-upload.txt");
        let config = runtime_config(&run_dir, fixture.parent().expect("fixture has a parent"))?;
        let (factory, installed_firefox) = configured_factory(
            &config,
            engine,
            &gauntlet.championship_url(&target.seed),
            repository.join("target/firefox-companion-proof/native-host-descriptor.json"),
        )
        .await?;
        let workers = Arc::new(WorkerPool::new(config.browser.max_active, factory));
        let journal = Arc::new(JsonlJournal::open(&config.storage.journal_path).await?);
        let checkpoints = CheckpointStore::open(&config.storage.checkpoints_dir).await?;
        let runtime = PageRuntime::new(journal, Arc::clone(&workers));
        let recovery = RecoveryCoordinator::with_workers(checkpoints, Arc::clone(&workers));
        let session_id = SessionId::new();
        let deadline = Utc::now() + Duration::minutes(10);
        let state_store = Arc::new(SkillStateStore::new());
        state_store.insert(SkillSessionState::new(
            session_id.clone(),
            BTreeMap::new(),
            None,
            None,
            None,
            None,
            None,
            Vec::new(),
            Vec::new(),
            deadline,
        )?)?;
        let adapter: Arc<dyn SkillEngineAdapter> = match engine {
            SkillBrowserEngine::Chromium => Arc::new(skill_result(
                ChromiumSkillAdapter::production(Arc::clone(&workers)),
            )?),
            SkillBrowserEngine::Firefox => Arc::new(skill_result(
                FirefoxSkillAdapter::production(Arc::clone(&workers)),
            )?),
            SkillBrowserEngine::WebKit => {
                return Err(test_error("WebKit is not configured for this release gate"));
            }
        };
        let ghost = Arc::new(SkillGhost::new(Arc::clone(&state_store), vec![adapter]));
        let zigzagzig = Arc::new(SkillZigZagZigController::new(
            Arc::clone(&state_store),
            5_000,
            [SkillBrowserEngine::Firefox, SkillBrowserEngine::Chromium],
        ));
        let mut registry = SkillRegistry::new();
        registry.register(ghost.clone())?;
        registry.register(zigzagzig.clone())?;
        let capabilities = [
            SkillCapability::EngineSelection,
            SkillCapability::ProfilePersistence,
        ];
        let profile = SkillProfileRequest::new(capabilities, [], [engine], BTreeMap::new())?;
        let context = SkillContext::with_granted_capabilities(capabilities, capabilities)
            .with_ghost(session_id.clone(), Some(profile));
        Ok(Self {
            gauntlet,
            runtime,
            workers,
            recovery,
            session_id,
            page_id: Mutex::new(None),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            router: SkillCommandRouter::new(registry),
            context,
            ghost,
            state_store,
            zigzagzig,
            coordinator: Mutex::new(None),
            skill_routes: Mutex::new(Vec::new()),
            recovery_proofs: Mutex::new(Vec::new()),
            engine,
            seed: target.seed,
            fixture,
            run_dir,
            _installed_firefox: installed_firefox,
        })
    }

    async fn command(&self, input: &str) -> TestResult<SkillOutcome> {
        let normalized = input.split_ascii_whitespace().collect::<Vec<_>>().join(" ");
        let receipt = self.router.execute(input, &self.context).await?;
        let outcome = receipt.outcome.clone();
        self.skill_routes
            .lock()
            .await
            .push(skill_route_proof(&normalized, &receipt));
        if normalized == "/zigzagzig run" {
            let strategy = skill_result(self.zigzagzig.strategy(&self.session_id).await)?;
            let coordinator = command_result(SkillRecoveryCoordinator::with_state_store(
                self.runtime.clone(),
                strategy,
                self.recovery.clone(),
                Arc::clone(&self.workers),
                Arc::clone(&self.state_store),
            ))?;
            *self.coordinator.lock().await = Some(Arc::new(coordinator));
        }
        Ok(outcome)
    }

    async fn complete_championship(&self) -> TestResult<ReleaseScorecard> {
        let started = Instant::now();
        let page_id = self.ensure_page().await?;
        self.navigate_championship(&page_id).await?;
        let mut screenshots = Vec::with_capacity(STATIONS.len());
        for station in STATIONS {
            self.complete_station(&page_id, station).await?;
            self.wait_for_pass(&page_id, station).await?;
            let screenshot = self.capture_screenshot(&page_id, station).await?;
            screenshots.push(screenshot);
        }
        let app_scorecard = self.final_championship_receipt(&page_id).await?;
        let stations =
            validated_championship_stations(&app_scorecard, &manifest_digest(&self.seed))?;
        let state = self.state_store.get(&self.session_id)?;
        let recovery_proofs = self.finalize_recovery_proofs().await?;
        let strategy_changes = recovery_proofs
            .iter()
            .flat_map(|proof| proof.tactics.iter().map(|tactic| tactic.tactic.clone()))
            .collect::<Vec<_>>();
        let ghost_profile = self.ghost_profile_proof().await?;
        let journal = std::fs::read(self.run_dir.join("commands.jsonl"))?;
        let journal_sha256 = format!("{:x}", Sha256::digest(&journal));
        let scorecard = ReleaseScorecard {
            course_version: "course-v1".into(),
            seed: self.seed.clone(),
            difficulty: "foundation".into(),
            manifest_digest: manifest_digest(&self.seed),
            stations,
            screenshots,
            engine: engine_name(self.engine).into(),
            active_skills: state.active_versions,
            skill_routes: self.skill_routes.lock().await.clone(),
            ghost_profile,
            recovery_count: recovery_proofs.len() as u64,
            recovery_proofs,
            journal_sha256,
            strategy_changes,
            duration_ms: started.elapsed().as_millis() as u64,
        };
        scorecard.verify_all_required(&self.run_dir)?;
        self.persist_scorecard(&scorecard).await?;
        Ok(scorecard)
    }

    async fn replay(&self) -> TestResult<ReleaseScorecard> {
        let serialized = tokio::fs::read(self.run_dir.join("scorecard.json")).await?;
        let scorecard: ReleaseScorecard = serde_json::from_slice(&serialized)?;
        scorecard.verify_all_required(&self.run_dir)?;
        Ok(scorecard)
    }

    async fn ensure_page(&self) -> TestResult<PageId> {
        let mut page = self.page_id.lock().await;
        if let Some(page_id) = page.as_ref() {
            return Ok(page_id.clone());
        }
        let opened = self.runtime.open_browser(self.session_id.clone()).await?;
        *page = Some(opened.id.clone());
        Ok(opened.id)
    }

    async fn navigate_championship(&self, page_id: &PageId) -> TestResult<()> {
        self.submit(
            page_id,
            PrimitiveCommand::Navigate(NavigateCommand {
                url: self.gauntlet.championship_url(&self.seed),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 15_000,
            }),
        )
        .await?;
        Ok(())
    }

    async fn complete_station(
        &self,
        page_id: &PageId,
        station: &str,
    ) -> TestResult<Option<String>> {
        match station {
            "route" => {
                let mut target = target_test_id("route-redirect");
                target.frame_path = vec![Box::new(target_test_id("route-challenge"))];
                self.click(page_id, target, true).await?;
            }
            "dom-drift" => {
                self.wait(
                    page_id,
                    WaitCondition::Element {
                        target: Box::new(target_test_id("replacement-target")),
                        state: ElementState::Visible,
                    },
                )
                .await?;
                let (command_id, execution) = self
                    .execute_with_recovery(
                        page_id,
                        PrimitiveCommand::Click(ClickCommand {
                            selector: String::new(),
                            target: Some(target_test_id("initial-target")),
                            boundary: false,
                            expected_url: None,
                        }),
                    )
                    .await?;
                if !matches!(
                    execution.skill_outcome,
                    SkillOutcome::Failed {
                        failure: SkillFailure::TargetDrift,
                        ..
                    }
                ) || execution
                    .tactic_evidence
                    .first()
                    .is_none_or(|proof| proof.trigger != SkillFailure::TargetDrift)
                {
                    return Err(test_error(format!(
                        "stale production command did not flow through ZigZagZig recovery: {:?} {:?}",
                        execution.skill_outcome, execution.tactic_evidence
                    )));
                }
                self.recovery_proofs
                    .lock()
                    .await
                    .push(PendingRecoveryProof {
                        station_id: "dom-drift".into(),
                        command_id,
                        terminal_failure: SkillFailure::TargetDrift,
                        execution,
                    });
                self.click(page_id, target_test_id("replacement-target"), false)
                    .await?;
            }
            "semantic-form" => {
                let field =
                    |name: &str, label: &str, role: &str, value: FillValue| CompleteFormField {
                        name: name.into(),
                        purpose: format!("fill {label}"),
                        hints: IntentHints {
                            role: Some(role.into()),
                            near_text: Some(types::TextMatch::Exact(label.into())),
                            ..Default::default()
                        },
                        value,
                    };
                let fields = vec![
                    field(
                        "name",
                        "Full name",
                        "textbox",
                        FillValue::Text {
                            text: "Ada Lovelace".into(),
                            clear_first: true,
                        },
                    ),
                    field(
                        "email",
                        "Email address",
                        "textbox",
                        FillValue::Text {
                            text: "ada@example.test".into(),
                            clear_first: true,
                        },
                    ),
                    field(
                        "plan",
                        "Plan",
                        "combobox",
                        FillValue::Select {
                            option: "pro".into(),
                        },
                    ),
                    field(
                        "terms",
                        "Accept terms",
                        "checkbox",
                        FillValue::Checked { checked: true },
                    ),
                ];
                let outcome = self
                    .runtime
                    .execute(CommandEnvelope {
                        schema_version: CommandEnvelope::SCHEMA_VERSION,
                        command_id: CommandId::new(),
                        workflow_id: self.workflow_id.clone(),
                        attempt_id: self.attempt_id.clone(),
                        session_id: self.session_id.clone(),
                        page_id: Some(page_id.clone()),
                        deadline: Utc::now() + Duration::seconds(30),
                        command: RuntimeCommand::Intent(IntentCommand::CompleteForm(
                            CompleteFormIntent {
                                purpose: "complete semantic form".into(),
                                fields,
                            },
                        )),
                    })
                    .await;
                if !matches!(outcome, CommandOutcome::Completed { .. }) {
                    return Err(test_error(format!("complete form failed: {outcome:?}")));
                }
                self.click(page_id, target_test_id("semantic-submit"), true)
                    .await?;
            }
            "validation" => {
                let rejected = self
                    .fill_intent_outcome(
                        page_id,
                        "Rejected value",
                        "textbox",
                        FillValue::Text {
                            text: "12".into(),
                            clear_first: true,
                        },
                    )
                    .await;
                if !matches!(
                    rejected,
                    CommandOutcome::Failed { ref error, ref evidence, .. }
                        if error.code == ErrorCode::VerificationFailed
                            && evidence.iter().any(|item| matches!(
                                item,
                                Evidence::Configuration { name, value }
                                    if name == "formControlValid" && value == "false"
                            ))
                ) {
                    return Err(test_error(format!(
                        "invalid form value did not fail with browser validity evidence: {rejected:?}"
                    )));
                }
                self.fill_intent(
                    page_id,
                    "Rejected value",
                    "textbox",
                    FillValue::Text {
                        text: "02139".into(),
                        clear_first: true,
                    },
                )
                .await?;
                self.click(page_id, target_test_id("validation-submit"), true)
                    .await?;
            }
            "iframe" => {
                let mut target = target_test_id("iframe-submit");
                target.frame_path = vec![Box::new(target_test_id("iframe-challenge"))];
                self.click(page_id, target, false).await?;
            }
            "shadow-root" => {
                let mut target = target_test_id("shadow-submit");
                target.shadow_path = vec![Box::new(target_test_id("shadow-host"))];
                self.click(page_id, target, false).await?;
            }
            "popup" => {
                let popup = self
                    .submit(
                        page_id,
                        PrimitiveCommand::ClickAndWaitForPopup(ClickAndWaitForPopupCommand {
                            selector: String::new(),
                            target: Some(target_test_id("popup-open")),
                            timeout_ms: 15_000,
                        }),
                    )
                    .await?
                    .into_iter()
                    .find_map(|evidence| match evidence {
                        Evidence::Popup { page_id, .. } => Some(page_id),
                        _ => None,
                    })
                    .ok_or_else(|| test_error("popup page evidence is missing"))?;
                tokio::time::sleep(StdDuration::from_millis(100)).await;
                self.click_popup_and_reconcile(&popup, page_id).await?;
            }
            "file-attachment" => {
                self.submit(
                    page_id,
                    PrimitiveCommand::UploadFiles(UploadFilesCommand {
                        selector: "[data-station-id='file-attachment'] input[type=file]".into(),
                        target: None,
                        paths: vec![self.fixture.to_string_lossy().into_owned()],
                    }),
                )
                .await?;
                tokio::time::sleep(StdDuration::from_millis(50)).await;
                self.click_raw(
                    page_id,
                    "[data-station-id='file-attachment'] button[type=submit]",
                    true,
                )
                .await?;
            }
            "download" => {
                self.click(page_id, target_test_id("download-generate"), false)
                    .await?;
                let path = self
                    .submit(
                        page_id,
                        PrimitiveCommand::ClickAndWaitForDownload(ClickAndWaitForDownloadCommand {
                            selector: "[data-station-id='download'] a[download]".into(),
                            target: None,
                            timeout_ms: 15_000,
                        }),
                    )
                    .await?
                    .into_iter()
                    .find_map(|evidence| match evidence {
                        Evidence::Download { path, .. } => Some(path),
                        _ => None,
                    })
                    .ok_or_else(|| test_error("download artifact evidence is missing"))?;
                self.submit(
                    page_id,
                    PrimitiveCommand::UploadFiles(UploadFilesCommand {
                        selector: "[data-station-id='download'] input[type=file]".into(),
                        target: None,
                        paths: vec![path],
                    }),
                )
                .await?;
                self.wait(
                    page_id,
                    WaitCondition::Element {
                        target: Box::new(target_test_id("download-confirm")),
                        state: ElementState::Enabled,
                    },
                )
                .await?;
                self.click(page_id, target_test_id("download-confirm"), true)
                    .await?;
            }
            "championship" => {
                for step in 1..=3 {
                    self.click(
                        page_id,
                        target_test_id(&format!("championship-step-{step}")),
                        step == 3,
                    )
                    .await?;
                }
            }
            _ => return Err(test_error("unknown championship station")),
        }
        Ok(None)
    }

    async fn click(&self, page_id: &PageId, target: TargetSpec, boundary: bool) -> TestResult<()> {
        self.submit(
            page_id,
            PrimitiveCommand::Click(ClickCommand {
                selector: String::new(),
                target: Some(target),
                boundary,
                expected_url: None,
            }),
        )
        .await?;
        Ok(())
    }

    async fn fill_intent(
        &self,
        page_id: &PageId,
        label: &str,
        role: &str,
        value: FillValue,
    ) -> TestResult<()> {
        let outcome = self.fill_intent_outcome(page_id, label, role, value).await;
        match outcome {
            CommandOutcome::Completed { .. } => Ok(()),
            other => Err(test_error(format!(
                "semantic fill failed for {label}: {other:?}"
            ))),
        }
    }

    async fn fill_intent_outcome(
        &self,
        page_id: &PageId,
        label: &str,
        role: &str,
        value: FillValue,
    ) -> CommandOutcome {
        self.runtime
            .execute(CommandEnvelope {
                schema_version: CommandEnvelope::SCHEMA_VERSION,
                command_id: CommandId::new(),
                workflow_id: self.workflow_id.clone(),
                attempt_id: self.attempt_id.clone(),
                session_id: self.session_id.clone(),
                page_id: Some(page_id.clone()),
                deadline: Utc::now() + Duration::seconds(30),
                command: RuntimeCommand::Intent(IntentCommand::Fill(FillIntent {
                    purpose: format!("fill {label}"),
                    hints: IntentHints {
                        role: Some(role.into()),
                        near_text: Some(types::TextMatch::Exact(label.into())),
                        ..IntentHints::default()
                    },
                    value,
                })),
            })
            .await
    }

    async fn click_popup_and_reconcile(&self, popup: &PageId, opener: &PageId) -> TestResult<()> {
        let (command_id, execution) = self
            .execute_with_recovery(
                popup,
                PrimitiveCommand::Click(ClickCommand {
                    selector: "button".into(),
                    target: None,
                    boundary: true,
                    expected_url: Some(format!(
                        "{}#committed",
                        self.gauntlet.station_url("popup", &self.seed)
                    )),
                }),
            )
            .await?;
        if !matches!(
            execution.skill_outcome,
            SkillOutcome::Failed {
                failure: SkillFailure::EffectUncertain,
                ..
            }
        ) || !execution.tactic_evidence.is_empty()
            || !matches!(
                execution.command_outcome,
                CommandOutcome::NeedsReconciliation { .. }
            )
        {
            return Err(test_error(format!(
                "popup duplicate-sensitive mutation did not fail closed through recovery: {:?} {:?} {:?}",
                execution.skill_outcome,
                execution.tactic_evidence,
                execution.command_outcome
            )));
        }
        self.wait_for_pass(opener, "popup").await?;
        self.recovery_proofs
            .lock()
            .await
            .push(PendingRecoveryProof {
                station_id: "popup".into(),
                command_id,
                terminal_failure: SkillFailure::EffectUncertain,
                execution,
            });
        Ok(())
    }

    async fn click_raw(&self, page_id: &PageId, selector: &str, boundary: bool) -> TestResult<()> {
        self.submit(
            page_id,
            PrimitiveCommand::Click(ClickCommand {
                selector: selector.into(),
                target: None,
                boundary,
                expected_url: None,
            }),
        )
        .await?;
        Ok(())
    }

    async fn wait(&self, page_id: &PageId, condition: WaitCondition) -> TestResult<()> {
        self.submit(
            page_id,
            PrimitiveCommand::WaitFor(WaitForCommand {
                condition,
                timeout_ms: 15_000,
            }),
        )
        .await?;
        Ok(())
    }

    async fn wait_for_pass(&self, page_id: &PageId, expected_station: &str) -> TestResult<()> {
        for _ in 0..80 {
            let evidence = self
                .submit(
                    page_id,
                    PrimitiveCommand::Inspect(InspectCommand {
                        selector: Some(format!(
                            "[data-station-id='{expected_station}'] [data-testid=result]"
                        )),
                        target: None,
                        include_html: false,
                    }),
                )
                .await?;
            if let Some(text) = evidence.into_iter().find_map(|item| match item {
                Evidence::Inspection { text, .. } => Some(text),
                _ => None,
            }) {
                if text.trim() == "Passed" {
                    return Ok(());
                }
                if !text.trim().is_empty() {
                    return Err(test_error(format!("station rejected Bobby: {text}")));
                }
            }
            tokio::time::sleep(StdDuration::from_millis(25)).await;
        }
        Err(test_error("station did not produce a verified outcome"))
    }

    async fn final_championship_receipt(
        &self,
        page_id: &PageId,
    ) -> TestResult<AppScorecardReceipt> {
        let receipt = self
            .submit(
                page_id,
                PrimitiveCommand::Inspect(InspectCommand {
                    selector: Some("script[data-testid=championship-scorecard]".into()),
                    target: None,
                    include_html: false,
                }),
            )
            .await?
            .into_iter()
            .find_map(|item| match item {
                Evidence::Inspection { text, .. } => Some(text),
                _ => None,
            })
            .ok_or_else(|| test_error("app championship scorecard receipt is missing"))?;
        Ok(serde_json::from_str(&receipt)?)
    }

    async fn capture_screenshot(
        &self,
        page_id: &PageId,
        station_id: &str,
    ) -> TestResult<ScreenshotProof> {
        self.submit(
            page_id,
            PrimitiveCommand::CaptureScreenshot(CaptureScreenshotCommand {
                mode: ScreenshotMode::Viewport,
            }),
        )
        .await?
        .into_iter()
        .find_map(|item| match item {
            Evidence::Screenshot {
                artifact_id,
                media_type,
                width,
                height,
                bytes,
                sha256,
            } => {
                let logical_ref = format!(
                    "artifacts/{}/{artifact_id}/{artifact_id}.png",
                    self.session_id.0
                );
                Some(ScreenshotProof {
                    station_id: station_id.into(),
                    artifact_id,
                    logical_ref,
                    media_type,
                    width,
                    height,
                    bytes,
                    sha256,
                })
            }
            _ => None,
        })
        .ok_or_else(|| test_error("runtime screenshot evidence is missing"))
    }

    async fn submit(
        &self,
        page_id: &PageId,
        command: PrimitiveCommand,
    ) -> TestResult<Vec<Evidence>> {
        let command_debug = format!("{command:?}");
        let outcome = self.execute_raw(page_id, command).await;
        match outcome {
            CommandOutcome::Completed { evidence, .. } => Ok(evidence),
            other => Err(test_error(format!(
                "production command {command_debug} failed: {other:?}"
            ))),
        }
    }

    async fn execute_raw(&self, page_id: &PageId, command: PrimitiveCommand) -> CommandOutcome {
        self.runtime
            .execute(CommandEnvelope {
                schema_version: CommandEnvelope::SCHEMA_VERSION,
                command_id: CommandId::new(),
                workflow_id: self.workflow_id.clone(),
                attempt_id: self.attempt_id.clone(),
                session_id: self.session_id.clone(),
                page_id: Some(page_id.clone()),
                deadline: Utc::now() + Duration::seconds(30),
                command: types::RuntimeCommand::Primitive(command),
            })
            .await
    }

    async fn execute_with_recovery(
        &self,
        page_id: &PageId,
        command: PrimitiveCommand,
    ) -> TestResult<(CommandId, SkillRecoveryExecution)> {
        let command_id = CommandId::new();
        let envelope = CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: command_id.clone(),
            workflow_id: self.workflow_id.clone(),
            attempt_id: self.attempt_id.clone(),
            session_id: self.session_id.clone(),
            page_id: Some(page_id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: types::RuntimeCommand::Primitive(command),
        };
        let page = self.runtime.get(page_id).await?;
        let coordinator = self
            .coordinator
            .lock()
            .await
            .clone()
            .ok_or_else(|| test_error("SkillZigZagZig coordinator is not active"))?;
        let execution = command_result(coordinator.execute_with_adaptation(&envelope, page).await)?;
        Ok((command_id, execution))
    }

    async fn ghost_profile_proof(&self) -> TestResult<GhostProfileProof> {
        let status = skill_result(self.ghost.status(&self.context).await)?;
        if !status.active {
            return Err(test_error("Ghost is not active"));
        }
        let profile = status
            .profile
            .ok_or_else(|| test_error("Ghost effective profile is missing"))?;
        let lease = command_result(self.workers.lease(self.session_id.clone()).await)?;
        let worker_id = lease.worker_id().0.to_string();
        let binding = serde_json::to_vec(&(
            engine_name(profile.engine),
            &worker_id,
            lease.profile_dir().as_os_str(),
        ))?;
        Ok(GhostProfileProof {
            profile_version: profile.version,
            engine: engine_name(profile.engine).into(),
            effective_capabilities: profile
                .effective_capabilities
                .iter()
                .map(serialized_enum_name)
                .collect::<TestResult<Vec<_>>>()?,
            observable_digest: profile.observable_digest,
            unsupported_optional: status
                .unsupported_optional
                .iter()
                .map(serialized_enum_name)
                .collect::<TestResult<Vec<_>>>()?,
            restart_required: status.restart_required,
            worker_id,
            worker_profile_binding_digest: format!("{:x}", Sha256::digest(binding)),
        })
    }

    async fn finalize_recovery_proofs(&self) -> TestResult<Vec<RecoveryProof>> {
        let journal = std::fs::read_to_string(self.run_dir.join("commands.jsonl"))?;
        if contains_secret_marker(&journal) {
            return Err(test_error(
                "retained production journal contains a secret or unrestricted host path",
            ));
        }
        let records = parse_journal_records(&journal)?;
        self.recovery_proofs
            .lock()
            .await
            .iter()
            .map(|pending| {
                let (executing_records, restarted_records, journal_receipt_sha256) =
                    journal_receipt(&records, &pending.command_id)?;
                Ok(RecoveryProof {
                    station_id: pending.station_id.clone(),
                    command_id: pending.command_id.0.to_string(),
                    terminal_failure: serialized_enum_name(&pending.terminal_failure)?,
                    tactics: pending
                        .execution
                        .tactic_evidence
                        .iter()
                        .map(|proof| {
                            Ok(RecoveryTacticProof {
                                tactic: serialized_enum_name(&proof.tactic)?,
                                trigger: serialized_enum_name(&proof.trigger)?,
                                effect: serialized_enum_name(&proof.effect)?,
                            })
                        })
                        .collect::<TestResult<Vec<_>>>()?,
                    executing_records,
                    restarted_records,
                    journal_receipt_sha256,
                })
            })
            .collect()
    }

    async fn persist_scorecard(&self, scorecard: &ReleaseScorecard) -> TestResult<()> {
        let bytes = serde_json::to_vec_pretty(scorecard)?;
        if contains_secret_marker(std::str::from_utf8(&bytes)?) {
            return Err(test_error(
                "refusing to persist a scorecard containing secrets",
            ));
        }
        tokio::fs::write(self.run_dir.join("scorecard.json"), bytes).await?;
        Ok(())
    }
}

fn skill_route_proof(input: &str, receipt: &SkillCommandReceipt) -> SkillRouteProof {
    let outcome = match &receipt.outcome {
        SkillOutcome::Applied { .. } => "applied",
        SkillOutcome::Adapted { .. } => "adapted",
        SkillOutcome::Degraded { .. } => "degraded",
        SkillOutcome::Stopped { .. } => "stopped",
        SkillOutcome::Failed { .. } => "failed",
    };
    SkillRouteProof {
        input: input.into(),
        alias: receipt.alias.into(),
        skill_name: receipt.skill_name.into(),
        skill_version: receipt.skill_version.into(),
        outcome: outcome.into(),
    }
}

fn serialized_enum_name<T: Serialize>(value: &T) -> TestResult<String> {
    serde_json::to_value(value)?
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| test_error("expected a string enum representation"))
}

fn parse_journal_records(journal: &str) -> TestResult<Vec<serde_json::Value>> {
    journal
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).map_err(Into::into))
        .collect()
}

fn journal_receipt(
    records: &[serde_json::Value],
    command_id: &CommandId,
) -> TestResult<(u64, u64, String)> {
    journal_receipt_for_id(records, &command_id.0.to_string())
}

fn journal_receipt_for_id(
    records: &[serde_json::Value],
    command_id: &str,
) -> TestResult<(u64, u64, String)> {
    let matching = records
        .iter()
        .filter(|record| record["commandId"].as_str() == Some(command_id))
        .collect::<Vec<_>>();
    let executing_records = matching
        .iter()
        .filter(|record| record["phase"] == "executing")
        .count() as u64;
    let restarted_records = matching
        .iter()
        .filter(|record| record["outcome"]["status"] == "restarted")
        .count() as u64;
    let terminal_records = matching
        .iter()
        .filter(|record| {
            matches!(record["phase"].as_str(), Some("completed" | "failed"))
                && matches!(
                    record["outcome"]["status"].as_str(),
                    Some("completed" | "failed" | "needsReconciliation")
                )
        })
        .count();
    if executing_records == 0 || terminal_records == 0 {
        return Err(test_error(
            "recovery command lacks durable executing and terminal journal records",
        ));
    }
    Ok((
        executing_records,
        restarted_records,
        format!("{:x}", Sha256::digest(serde_json::to_vec(&matching)?)),
    ))
}

fn verify_journal_proofs(
    run_dir: &Path,
    proofs: &[RecoveryProof],
    expected_sha256: &str,
) -> TestResult<()> {
    let journal = std::fs::read(run_dir.join("commands.jsonl"))?;
    if format!("{:x}", Sha256::digest(&journal)) != expected_sha256 {
        return Err(test_error(
            "retained journal digest does not match the scorecard",
        ));
    }
    let journal_text = std::str::from_utf8(&journal)?;
    if contains_secret_marker(journal_text) {
        return Err(test_error(
            "retained journal contains a secret or unrestricted host path",
        ));
    }
    let records = parse_journal_records(journal_text)?;
    for proof in proofs {
        let (executing, restarted, digest) = journal_receipt_for_id(&records, &proof.command_id)?;
        if executing != proof.executing_records
            || restarted != proof.restarted_records
            || digest != proof.journal_receipt_sha256
        {
            return Err(test_error(
                "recovery journal receipt was changed or substituted",
            ));
        }
    }
    Ok(())
}

fn target_test_id(test_id: &str) -> TargetSpec {
    TargetSpec {
        test_id: Some(test_id.into()),
        ..TargetSpec::default()
    }
}

fn configured_engine() -> TestResult<SkillBrowserEngine> {
    match std::env::var("BOBBY_CHAMPIONSHIP_ENGINE")
        .unwrap_or_else(|_| "firefox".into())
        .as_str()
    {
        "chromium" => Ok(SkillBrowserEngine::Chromium),
        "firefox" => Ok(SkillBrowserEngine::Firefox),
        value => Err(test_error(format!(
            "unsupported BOBBY_CHAMPIONSHIP_ENGINE: {value}"
        ))),
    }
}

fn runtime_config(run_dir: &Path, upload_root: &Path) -> TestResult<AppConfig> {
    let mut config = AppConfig::default();
    config.http.allow_loopback = true;
    config.browser = BrowserConfig {
        executable: chromium_executable(),
        profiles_dir: run_dir.join("profiles"),
        headless: std::env::var("BOBBY_CHAMPIONSHIP_HEADED").as_deref() != Ok("1"),
        max_active: 2,
        upload_roots: vec![upload_root.to_owned(), run_dir.join("downloads")],
        downloads_dir: run_dir.join("downloads"),
        artifacts_dir: run_dir.join("artifacts"),
        max_artifact_bytes: 16 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    };
    config.storage.journal_path = run_dir.join("commands.jsonl");
    config.storage.checkpoints_dir = run_dir.join("checkpoints");
    std::fs::create_dir_all(&config.browser.downloads_dir)?;
    std::fs::create_dir_all(&config.browser.artifacts_dir)?;
    Ok(config)
}

async fn configured_factory(
    config: &AppConfig,
    engine: SkillBrowserEngine,
    startup_url: &str,
    descriptor_path: PathBuf,
) -> TestResult<(Arc<dyn WorkerFactory>, Option<InstalledFirefoxRuntime>)> {
    if engine == SkillBrowserEngine::Firefox {
        let installed = InstalledFirefoxConfig::from_env()
            .map_err(|name| test_error(format!("{name} is required for Firefox proof")))?;
        let runtime =
            launch_installed_firefox_runtime(installed, config, startup_url, descriptor_path)
                .await
                .map_err(|error| test_error(format!("Firefox bootstrap failed: {error:?}")))?;
        return Ok((runtime.factory(), Some(runtime)));
    }
    let selection = match engine {
        SkillBrowserEngine::Chromium => BrowserSelectionConfig {
            preference: EnginePreferenceConfig::ManagedChromium,
            firefox: Vec::new(),
        },
        SkillBrowserEngine::Firefox => BrowserSelectionConfig {
            preference: EnginePreferenceConfig::default(),
            firefox: Vec::new(),
        },
        SkillBrowserEngine::WebKit => {
            return Err(test_error("WebKit is not configured for this release gate"));
        }
    };
    Ok((
        cli::compose_worker_factory_with_pairing_observer(
            config,
            selection,
            firefox_pairing_observer(|event| eprintln!("{event}")),
        )?,
        None,
    ))
}

fn firefox_pairing_observer<F>(emit: F) -> Arc<dyn Fn(&str) + Send + Sync>
where
    F: Fn(&str) + Send + Sync + 'static,
{
    Arc::new(move |_code| emit("Firefox companion pairing established (code redacted)"))
}

fn chromium_executable() -> Option<PathBuf> {
    if let Some(path) = std::env::var_os("BOBBY_CHROMIUM_EXECUTABLE") {
        return Some(PathBuf::from(path));
    }
    [
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        "/Applications/Chromium.app/Contents/MacOS/Chromium",
        "/Applications/Google Chrome for Testing.app/Contents/MacOS/Google Chrome for Testing",
    ]
    .into_iter()
    .map(PathBuf::from)
    .find(|path| path.is_file())
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("runtime-tests lives two levels below repository root")
        .to_owned()
}

fn engine_name(engine: SkillBrowserEngine) -> &'static str {
    match engine {
        SkillBrowserEngine::Chromium => "chromium",
        SkillBrowserEngine::Firefox => "firefox",
        SkillBrowserEngine::WebKit => "webkit",
    }
}

fn manifest_digest(seed: &str) -> String {
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct DigestManifest<'a> {
        course_version: &'static str,
        seed: &'a str,
        difficulty: &'static str,
        stations: Vec<DigestStation>,
    }
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct DigestStation {
        id: &'static str,
        version: &'static str,
        mutation_version: &'static str,
        capabilities: &'static [&'static str],
    }
    let capabilities: [&[&str]; 10] = [
        &["navigation"],
        &["dom-observation"],
        &["form-fill"],
        &["form-fill", "validation"],
        &["iframe", "click"],
        &["shadow-dom", "click"],
        &["popup", "click"],
        &["file-upload"],
        &["download"],
        &["form-fill", "click", "submission"],
    ];
    let stations = STATIONS
        .into_iter()
        .zip(capabilities)
        .map(|(id, capabilities)| DigestStation {
            id,
            version: "1",
            mutation_version: "1",
            capabilities,
        })
        .collect();
    let canonical = serde_json::to_vec(&DigestManifest {
        course_version: "course-v1",
        seed,
        difficulty: "foundation",
        stations,
    })
    .expect("canonical manifest serialization is infallible");
    format!("{:x}", Sha256::digest(canonical))
}

fn verify_screenshot_proofs(run_dir: &Path, proofs: &[ScreenshotProof]) -> TestResult<()> {
    if proofs.len() != STATIONS.len() {
        return Err(test_error("the screenshot proof set is not exact"));
    }
    let canonical_root = std::fs::canonicalize(run_dir)?;
    let mut artifact_ids = std::collections::BTreeSet::new();
    for (proof, station) in proofs.iter().zip(STATIONS) {
        if proof.station_id != station
            || proof.media_type != "image/png"
            || proof.width == 0
            || proof.height == 0
            || proof.bytes == 0
            || !is_lower_sha256(&proof.sha256)
            || !artifact_ids.insert(proof.artifact_id.as_str())
        {
            return Err(test_error(
                "runtime screenshot evidence is missing, reordered, duplicated, or malformed",
            ));
        }
        let logical = Path::new(&proof.logical_ref);
        let components: Vec<_> = logical.components().collect();
        if logical.is_absolute()
            || components.len() != 4
            || components
                .iter()
                .any(|component| !matches!(component, Component::Normal(_)))
            || components[0].as_os_str() != "artifacts"
            || components[2].as_os_str() != proof.artifact_id.as_str()
            || components[3].as_os_str().to_string_lossy() != format!("{}.png", proof.artifact_id)
        {
            return Err(test_error(
                "screenshot logical reference is not opaque and bound",
            ));
        }
        let path = std::fs::canonicalize(run_dir.join(logical))?;
        if !path.starts_with(&canonical_root) || !path.is_file() {
            return Err(test_error(
                "screenshot proof escaped its retained artifact root",
            ));
        }
        let bytes = std::fs::read(path)?;
        if bytes.len() as u64 != proof.bytes
            || format!("{:x}", Sha256::digest(&bytes)) != proof.sha256
        {
            return Err(test_error(
                "screenshot artifact bytes do not match the proof",
            ));
        }
    }
    Ok(())
}

fn contains_secret_marker(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "authorization",
        "cookie",
        "set-cookie",
        "bearer ",
        "password",
        "/volumes/",
        "/users/",
        "/private/",
    ]
    .iter()
    .any(|marker| lower.contains(marker))
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
}

fn test_error(message: impl Into<String>) -> Box<dyn Error + Send + Sync> {
    Box::new(std::io::Error::other(message.into()))
}

fn skill_result<T>(result: Result<T, SkillFailure>) -> TestResult<T> {
    result.map_err(|error| test_error(format!("skill failure: {error:?}")))
}

fn command_result<T>(result: Result<T, types::CommandError>) -> TestResult<T> {
    result.map_err(|error| test_error(format!("command failure: {error:?}")))
}

#[cfg(test)]
mod replay_contracts {
    use super::*;

    #[test]
    fn championship_defaults_to_firefox() {
        let saved = std::env::var_os("BOBBY_CHAMPIONSHIP_ENGINE");
        std::env::remove_var("BOBBY_CHAMPIONSHIP_ENGINE");
        let engine = configured_engine().unwrap();
        match saved {
            Some(value) => std::env::set_var("BOBBY_CHAMPIONSHIP_ENGINE", value),
            None => std::env::remove_var("BOBBY_CHAMPIONSHIP_ENGINE"),
        }
        assert_eq!(engine, SkillBrowserEngine::Firefox);
    }
    use std::sync::Mutex as StdMutex;

    fn fixture(root: &Path) -> Vec<ScreenshotProof> {
        STATIONS
            .iter()
            .enumerate()
            .map(|(index, station)| {
                let artifact_id = format!("00000000-0000-4000-8000-{index:012}");
                let logical_ref = format!(
                    "artifacts/11111111-1111-4111-8111-111111111111/{artifact_id}/{artifact_id}.png"
                );
                let bytes = format!("png-proof-{station}").into_bytes();
                let path = root.join(&logical_ref);
                std::fs::create_dir_all(path.parent().unwrap()).unwrap();
                std::fs::write(&path, &bytes).unwrap();
                ScreenshotProof {
                    station_id: (*station).into(),
                    artifact_id,
                    logical_ref,
                    media_type: "image/png".into(),
                    width: 800,
                    height: 600,
                    bytes: bytes.len() as u64,
                    sha256: format!("{:x}", Sha256::digest(bytes)),
                }
            })
            .collect()
    }

    #[test]
    fn replay_recomputes_every_png_and_rejects_reorder_missing_and_substitution() {
        let root = tempfile::tempdir().unwrap();
        let proofs = fixture(root.path());
        assert!(verify_screenshot_proofs(root.path(), &proofs).is_ok());

        let mut reordered = proofs.clone();
        reordered.swap(0, 1);
        assert!(verify_screenshot_proofs(root.path(), &reordered).is_err());

        let missing = proofs.clone();
        std::fs::remove_file(root.path().join(&missing[0].logical_ref)).unwrap();
        assert!(verify_screenshot_proofs(root.path(), &missing).is_err());

        std::fs::write(root.path().join(&missing[0].logical_ref), b"substitution").unwrap();
        assert!(verify_screenshot_proofs(root.path(), &missing).is_err());
    }

    #[test]
    fn firefox_pairing_observer_emits_only_a_fixed_redacted_event() {
        let output = Arc::new(StdMutex::new(Vec::<String>::new()));
        let captured = Arc::clone(&output);
        let observer = firefox_pairing_observer(move |event| {
            captured.lock().unwrap().push(event.to_owned());
        });
        let pairing_code = "19b89d68-7127-4d5d-9268-d675dbd599d2";

        observer(pairing_code);

        let output = output.lock().unwrap();
        assert_eq!(
            output.as_slice(),
            ["Firefox companion pairing established (code redacted)"]
        );
        assert!(!output[0].contains(pairing_code));
        assert!(!output[0].contains("19b89d68"));
    }

    #[test]
    fn release_gate_rejects_ten_independent_partial_receipts() {
        let digest = manifest_digest("aggregation-seed");
        let partials = STATIONS
            .iter()
            .map(|station| AppScorecardReceipt {
                manifest_digest: digest.clone(),
                passed: false,
                stations: vec![AppStationReceipt {
                    id: (*station).into(),
                    version: "1".into(),
                    mutation_version: "1".into(),
                    passed: true,
                    evidence: Vec::new(),
                }],
            })
            .collect::<Vec<_>>();

        for partial in &partials {
            assert!(validated_championship_stations(partial, &digest).is_err());
        }

        let combined = AppScorecardReceipt {
            manifest_digest: digest.clone(),
            passed: true,
            stations: partials
                .into_iter()
                .flat_map(|partial| partial.stations)
                .collect(),
        };
        assert_eq!(
            validated_championship_stations(&combined, &digest)
                .unwrap()
                .len(),
            STATIONS.len()
        );
    }
}

#[tokio::test]
#[ignore = "requires installed browser engines and built Bobby gauntlet"]
async fn production_bobby_passes_seeded_championship_with_replayable_evidence() {
    let seed = std::env::var("BOBBY_CHAMPIONSHIP_SEED").unwrap_or_else(|_| "release-seed-1".into());
    let run = ProductionBobby::launch(gauntlet_url(&seed)).await.unwrap();
    run.command("/ghost on").await.unwrap();
    run.command("/zigzagzig run").await.unwrap();
    let first = run.complete_championship().await.unwrap();
    assert!(first.stations.iter().all(|station| station.passed));
    assert_eq!(
        first.manifest_digest,
        run.replay().await.unwrap().manifest_digest
    );
    assert!(first.screenshots.len() >= 10);
    assert!(!first.serialized().contains("authorization"));

    let mut tampered = first.clone();
    tampered.manifest_digest = "0".repeat(64);
    assert!(tampered.verify_all_required(&run.run_dir).is_err());
    let mut tampered = first;
    tampered.screenshots[0].sha256 = "0".repeat(63);
    assert!(tampered.verify_all_required(&run.run_dir).is_err());
}
