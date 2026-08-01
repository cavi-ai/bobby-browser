use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use checkpoint_store::CheckpointStore;
use chrono::{Duration, Utc};
use companion_protocol::BrowserEngine;
use page_runtime::{RecoveryCoordinator, RecoveryPreflightObserver, SkillRecoveryCoordinator};
use skill_runtime::{SkillStateStore, SkillZigZagZig};
use tokio::sync::{Mutex, Notify};
use types::{
    AttemptId, CheckpointId, CheckpointInvariant, ClickCommand, CommandClass, CommandEnvelope,
    CommandError, CommandId, CommandOutcome, ErrorCode, ErrorLayer, Evidence, InspectCommand,
    NavigateCommand, OpenPageRequest, PageId, PrimitiveCommand, SessionId, SkillBrowserEngine,
    SkillCheckpointProof, SkillEvidenceRef, SkillOutcome, SkillProfile, SkillSessionState,
    SkillTactic, TypeTextCommand, WorkerId, WorkflowCheckpoint, WorkflowId,
};
use worker_pool::{BrowserWorker, EnginePreference, WorkerFactory, WorkerPool};
use workflow_journal::{CommandJournal, JournalError, JournalRecord, JournalScan, JsonlJournal};

struct ReplacementFactory {
    launches: Arc<AtomicUsize>,
    releases: Arc<AtomicUsize>,
    replacements: Arc<AtomicUsize>,
    replacement_active: Arc<AtomicBool>,
    recovered: Arc<AtomicBool>,
    navigations: Arc<Mutex<Vec<String>>>,
    mutation_delay_ms: u64,
    state_store: Arc<SkillStateStore>,
    finalization_failures: usize,
    journal_failures: Arc<AtomicUsize>,
    journal_failures_to_inject: usize,
    browser_events: Arc<std::sync::Mutex<Vec<(&'static str, std::time::Instant)>>>,
}

struct ReplacementWorker {
    id: WorkerId,
    profile: PathBuf,
    replacement_active: Arc<AtomicBool>,
    recovered: Arc<AtomicBool>,
    current_url: Mutex<String>,
    navigations: Arc<Mutex<Vec<String>>>,
    browser_events: Arc<std::sync::Mutex<Vec<(&'static str, std::time::Instant)>>>,
}

struct CheckpointSwapObserver {
    path: PathBuf,
    replacement: Vec<u8>,
}

struct BlockingCheckpointObserver {
    reached: Arc<Notify>,
    resume: Arc<Notify>,
}

struct FailingJournal {
    inner: JsonlJournal,
    failures: Arc<AtomicUsize>,
}

#[async_trait]
impl CommandJournal for FailingJournal {
    async fn append(&self, record: JournalRecord) -> Result<(), JournalError> {
        if self
            .failures
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                count.checked_sub(1)
            })
            .is_ok()
        {
            return Err(JournalError::Io(std::io::Error::other(
                "injected recovery journal failure",
            )));
        }
        self.inner.append(record).await
    }

    async fn history(&self, id: CommandId) -> Result<JournalScan, JournalError> {
        self.inner.history(id).await
    }
}

#[async_trait]
impl RecoveryPreflightObserver for CheckpointSwapObserver {
    async fn checkpoint_verified(&self) {
        tokio::fs::write(&self.path, &self.replacement)
            .await
            .unwrap();
    }
}

#[async_trait]
impl RecoveryPreflightObserver for BlockingCheckpointObserver {
    async fn checkpoint_verified(&self) {
        self.reached.notify_one();
        self.resume.notified().await;
    }
}

#[async_trait]
impl WorkerFactory for ReplacementFactory {
    async fn launch(&self, _: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        self.browser_events
            .lock()
            .unwrap()
            .push(("launch", std::time::Instant::now()));
        self.launches.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(ReplacementWorker {
            id: WorkerId::new(),
            profile: PathBuf::from("/profiles/runtime-recovery"),
            replacement_active: Arc::clone(&self.replacement_active),
            recovered: Arc::clone(&self.recovered),
            current_url: Mutex::new("about:blank".into()),
            navigations: Arc::clone(&self.navigations),
            browser_events: Arc::clone(&self.browser_events),
        }))
    }

    fn can_select(&self, preference: &EnginePreference) -> bool {
        matches!(
            preference,
            EnginePreference::Prefer { engines }
                if engines == &vec![BrowserEngine::Firefox]
        )
    }

    async fn release_session(&self, _: &SessionId) {
        tokio::time::sleep(std::time::Duration::from_millis(self.mutation_delay_ms)).await;
        self.releases.fetch_add(1, Ordering::SeqCst);
        if self.finalization_failures > 0 {
            self.state_store
                .inject_transition_failures(self.finalization_failures);
        }
        if self.journal_failures_to_inject > 0 {
            self.journal_failures
                .store(self.journal_failures_to_inject, Ordering::SeqCst);
        }
        self.replacement_active.store(false, Ordering::SeqCst);
    }

    async fn replace_session(
        &self,
        _: &SessionId,
        preference: &EnginePreference,
    ) -> Result<(), CommandError> {
        if !self.can_select(preference) {
            return Err(driver_error("unexpected replacement engine"));
        }
        self.replacements.fetch_add(1, Ordering::SeqCst);
        tokio::time::sleep(std::time::Duration::from_millis(self.mutation_delay_ms)).await;
        if self.finalization_failures > 0 {
            self.state_store
                .inject_transition_failures(self.finalization_failures);
        }
        if self.journal_failures_to_inject > 0 {
            self.journal_failures
                .store(self.journal_failures_to_inject, Ordering::SeqCst);
        }
        self.replacement_active.store(true, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl BrowserWorker for ReplacementWorker {
    fn worker_id(&self) -> WorkerId {
        self.id.clone()
    }

    fn profile_dir(&self) -> &Path {
        &self.profile
    }

    async fn open_page(&self, _: PageId) -> Result<(), CommandError> {
        self.browser_events
            .lock()
            .unwrap()
            .push(("open_page", std::time::Instant::now()));
        Ok(())
    }

    async fn navigate(
        &self,
        _: &PageId,
        command: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.browser_events
            .lock()
            .unwrap()
            .push(("navigate", std::time::Instant::now()));
        *self.current_url.lock().await = command.url.clone();
        self.recovered.store(true, Ordering::SeqCst);
        self.navigations.lock().await.push(command.url.clone());
        Ok(vec![Evidence::Navigation {
            url: command.url.clone(),
            title: "Recovered".into(),
        }])
    }

    async fn inspect(
        &self,
        _: &PageId,
        command: &InspectCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.browser_events
            .lock()
            .unwrap()
            .push(("inspect", std::time::Instant::now()));
        if !self.replacement_active.load(Ordering::SeqCst)
            && !self.recovered.load(Ordering::SeqCst)
            && command.selector.is_none()
        {
            return Err(driver_error("original engine inspect failed"));
        }
        Ok(vec![Evidence::Inspection {
            selector: command.selector.clone(),
            url: self.current_url.lock().await.clone(),
            title: "Recovered".into(),
            text: "ready".into(),
            html: None,
        }])
    }

    async fn click(&self, _: &PageId, _: &ClickCommand) -> Result<Vec<Evidence>, CommandError> {
        Err(driver_error("unused click"))
    }

    async fn type_text(
        &self,
        _: &PageId,
        _: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(driver_error("unused type"))
    }

    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }
}

fn driver_error(message: &str) -> CommandError {
    CommandError {
        code: ErrorCode::BrowserCommandFailed,
        message: message.into(),
        layer: ErrorLayer::Driver,
        retryable: true,
    }
}

async fn wait_past(deadline: chrono::DateTime<Utc>) {
    let remaining = deadline
        .signed_duration_since(Utc::now())
        .to_std()
        .unwrap_or_default();
    tokio::time::sleep(remaining + std::time::Duration::from_millis(100)).await;
}

fn proof(
    checkpoint_id: CheckpointId,
    session_id: SessionId,
    checkpoint_digest: &str,
    verified_at: chrono::DateTime<Utc>,
) -> SkillCheckpointProof {
    SkillCheckpointProof::new(
        checkpoint_id,
        session_id,
        verified_at,
        SkillEvidenceRef::new("runtime-checkpoint-proof", checkpoint_digest).unwrap(),
    )
    .unwrap()
}

fn strategy_state(
    session_id: SessionId,
    checkpoint_id: CheckpointId,
    attempted_tactics: Vec<SkillTactic>,
    effective_profile: Option<SkillProfile>,
    checkpoint_digest: &str,
    proof_verified_at: chrono::DateTime<Utc>,
) -> SkillSessionState {
    SkillSessionState::new(
        session_id.clone(),
        BTreeMap::from([("SkillZigZagZig".into(), "1.0.0".into())]),
        effective_profile,
        Some(checkpoint_id.clone()),
        Some(proof(
            checkpoint_id,
            session_id,
            checkpoint_digest,
            proof_verified_at,
        )),
        None,
        None,
        attempted_tactics,
        Vec::new(),
        Utc::now() + Duration::seconds(5),
    )
    .unwrap()
}

fn checkpoint(
    checkpoint_id: CheckpointId,
    workflow_id: WorkflowId,
    attempt_id: AttemptId,
    session_id: SessionId,
    page_id: PageId,
) -> WorkflowCheckpoint {
    WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id,
        workflow_id,
        attempt_id,
        session_id,
        page_id,
        restart_url: "https://example.test/restart".into(),
        current_url: "https://example.test/checkpoint".into(),
        cursor: None,
        boundary_command_id: None,
        recovery_class: CommandClass::Reconciliable,
        invariants: vec![CheckpointInvariant::Url {
            value: "https://example.test/checkpoint".into(),
        }],
        replayable_inputs: Vec::new(),
        evidence: Vec::new(),
        recovery_history: Vec::new(),
        recovery_receipts: Vec::new(),
        created_at: Utc::now(),
    }
}

#[allow(clippy::too_many_arguments)]
async fn fixture_with_proof_verified_at(
    attempted_tactics: Vec<SkillTactic>,
    effective_profile: Option<SkillProfile>,
    compatible_engines: Vec<SkillBrowserEngine>,
    checkpoint_evidence_valid: bool,
    swap_checkpoint_on_preflight: bool,
    tactic_budget_ms: u64,
    mutation_delay_ms: u64,
    finalization_failures: usize,
    journal_failures_to_inject: usize,
    proof_verified_at: chrono::DateTime<Utc>,
) -> (
    SkillRecoveryCoordinator,
    CommandEnvelope,
    types::PageState,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<String>>>,
    page_runtime::PageRuntime,
    PathBuf,
    Arc<SkillStateStore>,
    Arc<std::sync::Mutex<Vec<(&'static str, std::time::Instant)>>>,
) {
    let root = tempfile::tempdir().unwrap().keep();
    let store = CheckpointStore::open(root.join("checkpoints"))
        .await
        .unwrap();
    let journal_path = root.join("journal.jsonl");
    let journal_failures = Arc::new(AtomicUsize::new(0));
    let journal = Arc::new(FailingJournal {
        inner: JsonlJournal::open(&journal_path).await.unwrap(),
        failures: Arc::clone(&journal_failures),
    });
    let launches = Arc::new(AtomicUsize::new(0));
    let releases = Arc::new(AtomicUsize::new(0));
    let replacements = Arc::new(AtomicUsize::new(0));
    let navigations = Arc::new(Mutex::new(Vec::new()));
    let browser_events = Arc::new(std::sync::Mutex::new(Vec::new()));
    let state_store = Arc::new(SkillStateStore::new());
    let pool = Arc::new(WorkerPool::with_replacement_timeout(
        1,
        Arc::new(ReplacementFactory {
            launches: Arc::clone(&launches),
            releases: Arc::clone(&releases),
            replacements: Arc::clone(&replacements),
            replacement_active: Arc::new(AtomicBool::new(false)),
            recovered: Arc::new(AtomicBool::new(false)),
            navigations: Arc::clone(&navigations),
            mutation_delay_ms,
            state_store: Arc::clone(&state_store),
            finalization_failures,
            journal_failures,
            journal_failures_to_inject,
            browser_events: Arc::clone(&browser_events),
        }),
        std::time::Duration::from_secs(10),
    ));
    let runtime =
        page_runtime::PageRuntime::new_with_checkpoints(journal, Arc::clone(&pool), store.clone());
    let session_id = SessionId::new();
    let page = runtime
        .open(OpenPageRequest {
            session_id: session_id.clone(),
        })
        .await;
    let workflow_id = WorkflowId::new();
    let attempt_id = AttemptId::new();
    let checkpoint_id = CheckpointId::new();
    let recovery = RecoveryCoordinator::with_workers(store.clone(), Arc::clone(&pool));
    let checkpoint = checkpoint(
        checkpoint_id.clone(),
        workflow_id.clone(),
        attempt_id.clone(),
        session_id.clone(),
        page.id.clone(),
    );
    if checkpoint_evidence_valid {
        recovery
            .save_verified(
                checkpoint.clone(),
                vec![Evidence::Navigation {
                    url: "https://example.test/checkpoint".into(),
                    title: "Recovered".into(),
                }],
            )
            .await
            .unwrap();
    } else {
        store.save(&checkpoint).await.unwrap();
    }
    let snapshot = store.lock_snapshot(&workflow_id).await.unwrap();
    let checkpoint_digest = snapshot.digest().to_owned();
    drop(snapshot);
    let strategy = SkillZigZagZig::new(
        strategy_state(
            session_id.clone(),
            checkpoint_id,
            attempted_tactics,
            effective_profile,
            &checkpoint_digest,
            proof_verified_at,
        ),
        tactic_budget_ms,
        compatible_engines,
    )
    .unwrap();
    let runtime_probe = runtime.clone();
    let mut coordinator = SkillRecoveryCoordinator::with_state_store(
        runtime,
        strategy,
        recovery,
        Arc::clone(&pool),
        Arc::clone(&state_store),
    )
    .unwrap();
    if swap_checkpoint_on_preflight {
        let mut replacement = checkpoint.clone();
        replacement.current_url = "https://example.test/same-id-swap".into();
        coordinator =
            coordinator.with_recovery_preflight_observer(Arc::new(CheckpointSwapObserver {
                path: root
                    .join("checkpoints")
                    .join(format!("{}.json", workflow_id.0)),
                replacement: serde_json::to_vec(&replacement).unwrap(),
            }));
    }
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id,
        attempt_id,
        session_id,
        page_id: Some(page.id.clone()),
        deadline: Utc::now() + Duration::seconds(5),
        command: types::RuntimeCommand::Primitive(PrimitiveCommand::Inspect(
            InspectCommand::default(),
        )),
    };
    (
        coordinator,
        envelope,
        page,
        launches,
        releases,
        replacements,
        navigations,
        runtime_probe,
        journal_path,
        state_store,
        browser_events,
    )
}

#[allow(clippy::too_many_arguments)]
async fn fixture(
    attempted_tactics: Vec<SkillTactic>,
    effective_profile: Option<SkillProfile>,
    compatible_engines: Vec<SkillBrowserEngine>,
    checkpoint_evidence_valid: bool,
    swap_checkpoint_on_preflight: bool,
    tactic_budget_ms: u64,
    mutation_delay_ms: u64,
    finalization_failures: usize,
    journal_failures_to_inject: usize,
) -> (
    SkillRecoveryCoordinator,
    CommandEnvelope,
    types::PageState,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<AtomicUsize>,
    Arc<Mutex<Vec<String>>>,
    page_runtime::PageRuntime,
    PathBuf,
    Arc<SkillStateStore>,
    Arc<std::sync::Mutex<Vec<(&'static str, std::time::Instant)>>>,
) {
    fixture_with_proof_verified_at(
        attempted_tactics,
        effective_profile,
        compatible_engines,
        checkpoint_evidence_valid,
        swap_checkpoint_on_preflight,
        tactic_budget_ms,
        mutation_delay_ms,
        finalization_failures,
        journal_failures_to_inject,
        Utc::now(),
    )
    .await
}

#[tokio::test]
async fn selected_engine_replacement_is_pool_owned_and_reverifies_the_original_postcondition() {
    let profile = SkillProfile::new(
        "runtime-v1",
        SkillBrowserEngine::Chromium,
        [],
        "b".repeat(64),
    )
    .unwrap();
    let (coordinator, envelope, page, launches, _, replacements, _, runtime, _, _, _) = fixture(
        vec![
            SkillTactic::ObserveAgain,
            SkillTactic::ResolveSemanticTarget,
            SkillTactic::ChangeInteractionMethod,
            SkillTactic::ReconcileCheckpoint,
            SkillTactic::FreshGhostSession,
        ],
        Some(profile),
        vec![SkillBrowserEngine::Firefox],
        true,
        false,
        1_000,
        0,
        0,
        0,
    )
    .await;

    let execution = coordinator
        .execute_with_adaptation(&envelope, page.clone())
        .await
        .unwrap();

    assert_eq!(replacements.load(Ordering::SeqCst), 1);
    assert_eq!(launches.load(Ordering::SeqCst), 2);
    assert_eq!(
        execution
            .tactic_evidence
            .iter()
            .map(|item| item.tactic)
            .collect::<Vec<_>>(),
        vec![SkillTactic::SelectCompatibleEngine]
    );
    assert!(matches!(
        execution.command_outcome,
        CommandOutcome::Completed { .. }
    ));
    assert_eq!(
        runtime.get(&page.id).await.unwrap().url.as_deref(),
        Some("https://example.test/checkpoint")
    );
}

#[tokio::test]
async fn durable_restart_uses_restart_url_and_creates_new_attempt_lineage() {
    let (coordinator, envelope, page, launches, _, _, navigations, runtime, journal_path, _, _) =
        fixture(
            vec![
                SkillTactic::ObserveAgain,
                SkillTactic::ResolveSemanticTarget,
                SkillTactic::ChangeInteractionMethod,
                SkillTactic::ReconcileCheckpoint,
                SkillTactic::FreshGhostSession,
                SkillTactic::SelectCompatibleEngine,
            ],
            None,
            Vec::new(),
            true,
            false,
            1_000,
            0,
            0,
            0,
        )
        .await;
    let prior_attempt_id = envelope.attempt_id.clone();

    let execution = coordinator
        .execute_with_adaptation(&envelope, page.clone())
        .await
        .unwrap();

    assert_eq!(launches.load(Ordering::SeqCst), 2);
    assert_eq!(
        navigations.lock().await.as_slice(),
        ["https://example.test/restart"]
    );
    assert!(matches!(
        execution.command_outcome,
        CommandOutcome::Restarted {
            prior_attempt_id: ref prior,
            ref attempt_id,
            ref evidence,
            ..
        } if prior == &prior_attempt_id
            && attempt_id != &prior_attempt_id
            && evidence.iter().any(|item| matches!(
                item,
                Evidence::Navigation { url, .. }
                    if url == "https://example.test/restart"
            ))
            && evidence.iter().any(|item| matches!(
                item,
                Evidence::Configuration { name, .. }
                    if name == "skillRecoveryTactic"
            ))
    ));
    assert!(matches!(
        execution.skill_outcome,
        SkillOutcome::Adapted {
            tactic: SkillTactic::RestartDurableBoundary,
            ..
        }
    ));
    assert_eq!(
        runtime.get(&page.id).await.unwrap().url.as_deref(),
        Some("https://example.test/restart")
    );
    let reopened = JsonlJournal::open(journal_path).await.unwrap();
    let history = reopened.history(envelope.command_id.clone()).await.unwrap();
    assert!(history.records.iter().any(|record| matches!(
        (&record.phase, &record.outcome),
        (types::CommandPhase::Completed, Some(CommandOutcome::Restarted { evidence, .. }))
            if evidence.iter().any(|item| matches!(
                item,
                Evidence::Navigation { url, .. }
                    if url == "https://example.test/restart"
            ))
            && evidence.iter().any(|item| matches!(
                item,
                Evidence::Configuration { name, .. }
                    if name == "skillRecoveryTactic"
            ))
    )));
    assert!(!history.records.iter().any(|record| matches!(
        record.outcome,
        Some(CommandOutcome::Restarted { ref evidence, .. })
            if evidence.is_empty()
    )));
}

#[tokio::test]
async fn invalid_persisted_checkpoint_evidence_blocks_fresh_session_before_release() {
    let (coordinator, envelope, page, launches, releases, replacements, _, _, _, _, _) = fixture(
        vec![
            SkillTactic::ObserveAgain,
            SkillTactic::ResolveSemanticTarget,
            SkillTactic::ChangeInteractionMethod,
            SkillTactic::ReconcileCheckpoint,
        ],
        None,
        Vec::new(),
        false,
        false,
        1_000,
        0,
        0,
        0,
    )
    .await;

    let execution = coordinator
        .execute_with_adaptation(&envelope, page)
        .await
        .unwrap();

    assert_eq!(launches.load(Ordering::SeqCst), 1);
    assert_eq!(releases.load(Ordering::SeqCst), 0);
    assert_eq!(replacements.load(Ordering::SeqCst), 0);
    assert!(matches!(
        execution.skill_outcome,
        SkillOutcome::Failed {
            failure: types::SkillFailure::TargetDrift,
            ..
        }
    ));
    assert!(execution
        .tactic_evidence
        .iter()
        .all(|evidence| { evidence.trigger == types::SkillFailure::TargetDrift }));
}

#[tokio::test]
async fn invalid_persisted_checkpoint_evidence_blocks_engine_replacement() {
    let profile = SkillProfile::new(
        "runtime-v1",
        SkillBrowserEngine::Chromium,
        [],
        "b".repeat(64),
    )
    .unwrap();
    let (coordinator, envelope, page, launches, releases, replacements, _, _, _, _, _) = fixture(
        vec![
            SkillTactic::ObserveAgain,
            SkillTactic::ResolveSemanticTarget,
            SkillTactic::ChangeInteractionMethod,
            SkillTactic::ReconcileCheckpoint,
            SkillTactic::FreshGhostSession,
        ],
        Some(profile),
        vec![SkillBrowserEngine::Firefox],
        false,
        false,
        1_000,
        0,
        0,
        0,
    )
    .await;

    let execution = coordinator
        .execute_with_adaptation(&envelope, page)
        .await
        .unwrap();

    assert_eq!(launches.load(Ordering::SeqCst), 1);
    assert_eq!(releases.load(Ordering::SeqCst), 0);
    assert_eq!(replacements.load(Ordering::SeqCst), 0);
    assert!(matches!(
        execution.skill_outcome,
        SkillOutcome::Failed {
            failure: types::SkillFailure::TargetDrift,
            ..
        }
    ));
    assert!(execution
        .tactic_evidence
        .iter()
        .all(|evidence| { evidence.trigger == types::SkillFailure::TargetDrift }));
}

#[tokio::test]
async fn same_checkpoint_id_content_swap_blocks_pool_mutation() {
    let (coordinator, envelope, page, launches, releases, replacements, _, _, _, _, _) = fixture(
        vec![
            SkillTactic::ObserveAgain,
            SkillTactic::ResolveSemanticTarget,
            SkillTactic::ChangeInteractionMethod,
            SkillTactic::ReconcileCheckpoint,
        ],
        None,
        Vec::new(),
        true,
        true,
        1_000,
        0,
        0,
        0,
    )
    .await;

    let execution = coordinator
        .execute_with_adaptation(&envelope, page)
        .await
        .unwrap();

    assert_eq!(launches.load(Ordering::SeqCst), 1);
    assert_eq!(releases.load(Ordering::SeqCst), 0);
    assert_eq!(replacements.load(Ordering::SeqCst), 0);
    assert!(matches!(
        execution.skill_outcome,
        SkillOutcome::Failed {
            failure: types::SkillFailure::TargetDrift,
            ..
        }
    ));
    assert!(execution
        .tactic_evidence
        .iter()
        .all(|evidence| { evidence.trigger == types::SkillFailure::TargetDrift }));
}

#[tokio::test]
async fn deadline_waits_for_owned_engine_replacement_and_finalizes_once() {
    let profile = SkillProfile::new(
        "runtime-v1",
        SkillBrowserEngine::Chromium,
        [],
        "b".repeat(64),
    )
    .unwrap();
    let (coordinator, envelope, page, _, _, replacements, _, _, _, _, _) = fixture(
        vec![
            SkillTactic::ObserveAgain,
            SkillTactic::ResolveSemanticTarget,
            SkillTactic::ChangeInteractionMethod,
            SkillTactic::ReconcileCheckpoint,
            SkillTactic::FreshGhostSession,
        ],
        Some(profile),
        vec![SkillBrowserEngine::Firefox],
        true,
        false,
        20,
        80,
        0,
        0,
    )
    .await;
    let started = std::time::Instant::now();

    let execution = coordinator
        .execute_with_adaptation(&envelope, page)
        .await
        .unwrap();

    assert!(started.elapsed() >= std::time::Duration::from_millis(80));
    assert_eq!(replacements.load(Ordering::SeqCst), 1);
    assert!(matches!(
        execution.skill_outcome,
        SkillOutcome::Failed {
            failure: types::SkillFailure::DeadlineExceeded,
            ..
        }
    ));
}

#[tokio::test]
async fn deadline_waits_for_owned_session_release_before_finalization() {
    let (coordinator, envelope, page, _, releases, replacements, _, _, _, _, _) = fixture(
        vec![
            SkillTactic::ObserveAgain,
            SkillTactic::ResolveSemanticTarget,
            SkillTactic::ChangeInteractionMethod,
            SkillTactic::ReconcileCheckpoint,
        ],
        None,
        Vec::new(),
        true,
        false,
        20,
        80,
        0,
        0,
    )
    .await;
    let started = std::time::Instant::now();

    let execution = coordinator
        .execute_with_adaptation(&envelope, page)
        .await
        .unwrap();

    assert!(started.elapsed() >= std::time::Duration::from_millis(80));
    assert_eq!(releases.load(Ordering::SeqCst), 1);
    assert_eq!(replacements.load(Ordering::SeqCst), 0);
    assert!(matches!(
        execution.skill_outcome,
        SkillOutcome::Failed {
            failure: types::SkillFailure::DeadlineExceeded,
            ..
        }
    ));
}

#[tokio::test]
async fn stabilization_beyond_finalization_budget_persists_unresolved_then_reconciles() {
    let profile = SkillProfile::new(
        "runtime-v1",
        SkillBrowserEngine::Chromium,
        [],
        "b".repeat(64),
    )
    .unwrap();
    let (coordinator, envelope, page, _, _, replacements, _, _, journal_path, _, browser_events) =
        fixture(
            vec![
                SkillTactic::ObserveAgain,
                SkillTactic::ResolveSemanticTarget,
                SkillTactic::ChangeInteractionMethod,
                SkillTactic::ReconcileCheckpoint,
                SkillTactic::FreshGhostSession,
            ],
            Some(profile),
            vec![SkillBrowserEngine::Firefox],
            true,
            false,
            20,
            5_300,
            0,
            0,
        )
        .await;
    let started = std::time::Instant::now();

    let first = coordinator
        .execute_with_adaptation(&envelope, page.clone())
        .await
        .unwrap();
    assert!(started.elapsed() < std::time::Duration::from_secs(6));
    assert!(matches!(
        first.command_outcome,
        CommandOutcome::Failed { ref error, .. }
            if error.code == ErrorCode::DeadlineExceeded
    ));
    let checkpoint_store =
        CheckpointStore::open(journal_path.parent().unwrap().join("checkpoints"))
            .await
            .unwrap();
    let unresolved = checkpoint_store.load(&envelope.workflow_id).await.unwrap();
    assert!(
        unresolved.recovery_receipts.iter().any(|receipt| {
            receipt.identity.command_id == envelope.command_id
                && receipt.state == types::RecoveryReceiptState::Unresolved
        }),
        "receipts: {:?}",
        unresolved.recovery_receipts
    );
    let browser_event_count_at_unresolved = browser_events.lock().unwrap().len();
    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    let post_deadline_browser_actions = browser_events.lock().unwrap()
        [browser_event_count_at_unresolved..]
        .iter()
        .map(|(action, _)| *action)
        .collect::<Vec<_>>();
    assert!(
        post_deadline_browser_actions.is_empty(),
        "cleanup-only stabilization launched post-deadline browser work: {post_deadline_browser_actions:?}"
    );

    let reconciled = coordinator
        .execute_with_adaptation(&envelope, page)
        .await
        .unwrap();
    assert!(matches!(
        reconciled.skill_outcome,
        SkillOutcome::Failed {
            failure: types::SkillFailure::DeadlineExceeded,
            ..
        }
    ));
    assert_eq!(replacements.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn cancelled_caller_leaves_owned_replacement_to_stabilize_without_replay() {
    let profile = SkillProfile::new(
        "runtime-v1",
        SkillBrowserEngine::Chromium,
        [],
        "b".repeat(64),
    )
    .unwrap();
    let (coordinator, envelope, page, _, _, replacements, _, _, _, _, _) = fixture(
        vec![
            SkillTactic::ObserveAgain,
            SkillTactic::ResolveSemanticTarget,
            SkillTactic::ChangeInteractionMethod,
            SkillTactic::ReconcileCheckpoint,
            SkillTactic::FreshGhostSession,
        ],
        Some(profile),
        vec![SkillBrowserEngine::Firefox],
        true,
        false,
        20,
        80,
        0,
        0,
    )
    .await;
    let first = tokio::spawn({
        let coordinator = coordinator.clone();
        let envelope = envelope.clone();
        let page = page.clone();
        async move { coordinator.execute_with_adaptation(&envelope, page).await }
    });
    while replacements.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    first.abort();

    let second = coordinator
        .execute_with_adaptation(&envelope, page.clone())
        .await
        .unwrap();

    assert_eq!(replacements.load(Ordering::SeqCst), 1);
    assert!(matches!(
        second.command_outcome,
        CommandOutcome::Failed { ref error, .. }
            if error.code == ErrorCode::DeadlineExceeded
    ));

    let replayed = coordinator
        .execute_with_adaptation(&envelope, page)
        .await
        .unwrap();
    assert_eq!(
        serde_json::to_value(replayed.command_outcome).unwrap(),
        serde_json::to_value(second.command_outcome).unwrap()
    );
    assert_eq!(replacements.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_terminal_persistence_retries_on_next_call_without_replaying_replacement() {
    let profile = SkillProfile::new(
        "runtime-v1",
        SkillBrowserEngine::Chromium,
        [],
        "b".repeat(64),
    )
    .unwrap();
    let (coordinator, envelope, page, _, _, replacements, _, _, _, _, _) = fixture(
        vec![
            SkillTactic::ObserveAgain,
            SkillTactic::ResolveSemanticTarget,
            SkillTactic::ChangeInteractionMethod,
            SkillTactic::ReconcileCheckpoint,
            SkillTactic::FreshGhostSession,
        ],
        Some(profile),
        vec![SkillBrowserEngine::Firefox],
        true,
        false,
        20,
        80,
        2,
        0,
    )
    .await;

    assert!(coordinator
        .execute_with_adaptation(&envelope, page.clone())
        .await
        .is_err());
    let reconciled = coordinator
        .execute_with_adaptation(&envelope, page)
        .await
        .unwrap();

    assert_eq!(replacements.load(Ordering::SeqCst), 1);
    assert!(matches!(
        reconciled.skill_outcome,
        SkillOutcome::Failed {
            failure: types::SkillFailure::DeadlineExceeded,
            ..
        }
    ));
    assert!(!matches!(
        reconciled.command_outcome,
        CommandOutcome::Completed { .. }
    ));
}

#[tokio::test]
async fn journal_failure_retains_outbox_and_retry_flushes_exactly_once() {
    let profile = SkillProfile::new(
        "runtime-v1",
        SkillBrowserEngine::Chromium,
        [],
        "b".repeat(64),
    )
    .unwrap();
    let (coordinator, envelope, page, _, _, replacements, _, _, journal_path, _, _) = fixture(
        vec![
            SkillTactic::ObserveAgain,
            SkillTactic::ResolveSemanticTarget,
            SkillTactic::ChangeInteractionMethod,
            SkillTactic::ReconcileCheckpoint,
            SkillTactic::FreshGhostSession,
        ],
        Some(profile),
        vec![SkillBrowserEngine::Firefox],
        true,
        false,
        20,
        80,
        0,
        1,
    )
    .await;

    let first = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        coordinator.execute_with_adaptation(&envelope, page.clone()),
    )
    .await
    .expect("first recovery invocation must be bounded")
    .unwrap();
    assert!(matches!(
        first.command_outcome,
        CommandOutcome::Failed { ref error, .. }
            if error.message.starts_with("recovery outbox pending: ")
    ));
    assert_eq!(replacements.load(Ordering::SeqCst), 1);

    let checkpoint_store =
        CheckpointStore::open(journal_path.parent().unwrap().join("checkpoints"))
            .await
            .unwrap();
    let pending = checkpoint_store.load(&envelope.workflow_id).await.unwrap();
    assert!(pending.recovery_receipts.iter().any(|receipt| {
        receipt.identity.command_id == envelope.command_id
            && receipt.state == types::RecoveryReceiptState::PendingJournal
    }));

    let mut changed_command = envelope.clone();
    changed_command.command =
        types::RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
            url: "https://example.test/not-the-issued-command".into(),
            wait_until: types::WaitUntil::Interactive,
            timeout_ms: 100,
        }));
    let before_changed_retry = JsonlJournal::open(&journal_path)
        .await
        .unwrap()
        .history(envelope.command_id.clone())
        .await
        .unwrap()
        .records
        .len();
    let changed_error = coordinator
        .execute_with_adaptation(&changed_command, page.clone())
        .await
        .unwrap_err();
    assert_eq!(changed_error.code, ErrorCode::InvalidRequest);
    let before_exact_retry = JsonlJournal::open(&journal_path).await.unwrap();
    assert_eq!(
        before_exact_retry
            .history(envelope.command_id.clone())
            .await
            .unwrap()
            .records
            .len(),
        before_changed_retry
    );

    let replayed = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        coordinator.execute_with_adaptation(&envelope, page.clone()),
    )
    .await
    .expect("outbox replay must be bounded")
    .unwrap();
    let replayed_again = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        coordinator.execute_with_adaptation(&envelope, page),
    )
    .await
    .expect("committed receipt replay must be bounded")
    .unwrap();
    assert_eq!(
        serde_json::to_value(&replayed.command_outcome).unwrap(),
        serde_json::to_value(&replayed_again.command_outcome).unwrap()
    );
    assert_eq!(replacements.load(Ordering::SeqCst), 1);

    let reopened = JsonlJournal::open(journal_path).await.unwrap();
    let history = reopened.history(envelope.command_id).await.unwrap();
    let durable = replayed.command_outcome.journal_safe();
    assert_eq!(
        history
            .records
            .iter()
            .filter(|record| record.outcome.as_ref() == Some(&durable))
            .count(),
        1
    );
}

#[tokio::test]
async fn successful_owned_recovery_journal_failure_replays_without_replacement() {
    let profile = SkillProfile::new(
        "runtime-v1",
        SkillBrowserEngine::Chromium,
        [],
        "b".repeat(64),
    )
    .unwrap();
    let (coordinator, envelope, page, _, _, replacements, navigations, _, journal_path, _, _) =
        fixture(
            vec![
                SkillTactic::ObserveAgain,
                SkillTactic::ResolveSemanticTarget,
                SkillTactic::ChangeInteractionMethod,
                SkillTactic::ReconcileCheckpoint,
                SkillTactic::FreshGhostSession,
            ],
            Some(profile),
            vec![SkillBrowserEngine::Firefox],
            true,
            false,
            1_000,
            0,
            0,
            1,
        )
        .await;

    let error = coordinator
        .execute_with_adaptation(&envelope, page.clone())
        .await
        .unwrap_err();
    assert!(error.message.starts_with("recovery outbox pending: "));
    assert_eq!(replacements.load(Ordering::SeqCst), 1);

    let replayed = coordinator
        .execute_with_adaptation(&envelope, page.clone())
        .await
        .unwrap();
    let replayed_again = coordinator
        .execute_with_adaptation(&envelope, page.clone())
        .await
        .unwrap();
    assert!(matches!(
        replayed.command_outcome,
        CommandOutcome::Completed { .. }
    ));
    assert_eq!(
        serde_json::to_value(&replayed.command_outcome).unwrap(),
        serde_json::to_value(&replayed_again.command_outcome).unwrap()
    );
    assert_eq!(replacements.load(Ordering::SeqCst), 1);

    let navigation_count = navigations.lock().await.len();
    let mut changed_command = envelope.clone();
    changed_command.command =
        types::RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
            url: "https://example.test/committed-payload-swap".into(),
            wait_until: types::WaitUntil::Interactive,
            timeout_ms: 100,
        }));
    let changed_error = coordinator
        .execute_with_adaptation(&changed_command, page)
        .await
        .unwrap_err();
    assert_eq!(changed_error.code, ErrorCode::InvalidRequest);
    assert_eq!(navigations.lock().await.len(), navigation_count);
    assert_eq!(replacements.load(Ordering::SeqCst), 1);

    let reopened = JsonlJournal::open(journal_path).await.unwrap();
    let durable = replayed.command_outcome.journal_safe();
    assert_eq!(
        reopened
            .history(envelope.command_id)
            .await
            .unwrap()
            .records
            .iter()
            .filter(|record| record.outcome.as_ref() == Some(&durable))
            .count(),
        1
    );
}

#[tokio::test]
async fn committed_receipt_settles_adapted_success_after_deadline_and_proof_expiry() {
    let profile = SkillProfile::new(
        "runtime-v1",
        SkillBrowserEngine::Chromium,
        [],
        "b".repeat(64),
    )
    .unwrap();
    let (coordinator, envelope, page, _, _, replacements, _, _, _, state_store, _) =
        fixture_with_proof_verified_at(
            vec![
                SkillTactic::ObserveAgain,
                SkillTactic::ResolveSemanticTarget,
                SkillTactic::ChangeInteractionMethod,
                SkillTactic::ReconcileCheckpoint,
                SkillTactic::FreshGhostSession,
            ],
            Some(profile),
            vec![SkillBrowserEngine::Firefox],
            true,
            false,
            1_000,
            0,
            0,
            1,
            Utc::now() - Duration::minutes(15) + Duration::seconds(2),
        )
        .await;

    let error = coordinator
        .execute_with_adaptation(&envelope, page.clone())
        .await
        .unwrap_err();
    assert!(error.message.starts_with("recovery outbox pending: "));
    assert!(state_store
        .get(&envelope.session_id)
        .unwrap()
        .pending_issuance
        .is_some());
    wait_past(envelope.deadline).await;

    let settled = coordinator
        .execute_with_adaptation(&envelope, page)
        .await
        .unwrap();
    assert!(matches!(
        settled.command_outcome,
        CommandOutcome::Completed { .. }
    ));
    assert!(matches!(
        settled.skill_outcome,
        SkillOutcome::Adapted { .. }
    ));
    assert!(state_store
        .get(&envelope.session_id)
        .unwrap()
        .pending_issuance
        .is_none());
    assert_eq!(replacements.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn committed_receipt_settles_terminal_failure_after_deadline_and_proof_expiry() {
    let profile = SkillProfile::new(
        "runtime-v1",
        SkillBrowserEngine::Chromium,
        [],
        "b".repeat(64),
    )
    .unwrap();
    let (coordinator, envelope, page, _, _, replacements, _, _, _, state_store, _) =
        fixture_with_proof_verified_at(
            vec![
                SkillTactic::ObserveAgain,
                SkillTactic::ResolveSemanticTarget,
                SkillTactic::ChangeInteractionMethod,
                SkillTactic::ReconcileCheckpoint,
                SkillTactic::FreshGhostSession,
            ],
            Some(profile),
            vec![SkillBrowserEngine::Firefox],
            true,
            false,
            20,
            80,
            0,
            1,
            Utc::now() - Duration::minutes(15) + Duration::seconds(2),
        )
        .await;

    let first = coordinator
        .execute_with_adaptation(&envelope, page.clone())
        .await
        .unwrap();
    assert!(matches!(
        first.command_outcome,
        CommandOutcome::Failed { ref error, .. }
            if error.message.starts_with("recovery outbox pending: ")
    ));
    wait_past(envelope.deadline).await;

    let settled = coordinator
        .execute_with_adaptation(&envelope, page)
        .await
        .unwrap();
    assert!(matches!(
        settled.skill_outcome,
        SkillOutcome::Failed {
            failure: types::SkillFailure::DeadlineExceeded,
            ..
        }
    ));
    assert!(state_store
        .get(&envelope.session_id)
        .unwrap()
        .pending_issuance
        .is_none());
    assert_eq!(replacements.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn mismatched_committed_receipt_cannot_clear_the_issued_decision() {
    let profile = SkillProfile::new(
        "runtime-v1",
        SkillBrowserEngine::Chromium,
        [],
        "b".repeat(64),
    )
    .unwrap();
    let (coordinator, envelope, page, _, _, replacements, _, _, journal_path, state_store, _) =
        fixture(
            vec![
                SkillTactic::ObserveAgain,
                SkillTactic::ResolveSemanticTarget,
                SkillTactic::ChangeInteractionMethod,
                SkillTactic::ReconcileCheckpoint,
                SkillTactic::FreshGhostSession,
            ],
            Some(profile),
            vec![SkillBrowserEngine::Firefox],
            true,
            false,
            1_000,
            0,
            0,
            1,
        )
        .await;

    assert!(coordinator
        .execute_with_adaptation(&envelope, page.clone())
        .await
        .is_err());
    let checkpoint_store =
        CheckpointStore::open(journal_path.parent().unwrap().join("checkpoints"))
            .await
            .unwrap();
    let mut checkpoint = checkpoint_store.load(&envelope.workflow_id).await.unwrap();
    let receipt = checkpoint
        .recovery_receipts
        .iter_mut()
        .find(|receipt| receipt.identity.command_id == envelope.command_id)
        .unwrap();
    let durable = receipt.command_outcome.journal_safe();
    receipt.skill_outcome =
        SkillOutcome::failed(types::SkillFailure::StrategyExhausted, Vec::new()).unwrap();
    assert!(checkpoint_store.save(&checkpoint).await.is_err());
    drop(page);
    assert!(state_store
        .get(&envelope.session_id)
        .unwrap()
        .pending_issuance
        .is_some());
    assert_eq!(replacements.load(Ordering::SeqCst), 1);
    assert_eq!(
        JsonlJournal::open(journal_path)
            .await
            .unwrap()
            .history(envelope.command_id)
            .await
            .unwrap()
            .records
            .iter()
            .filter(|record| record.outcome.as_ref() == Some(&durable))
            .count(),
        0
    );
}

#[tokio::test]
async fn immutable_authority_digest_allows_the_recovery_ladder_to_progress() {
    let profile = SkillProfile::new(
        "runtime-v1",
        SkillBrowserEngine::Chromium,
        [],
        "b".repeat(64),
    )
    .unwrap();
    let (coordinator, mut envelope, page, _, releases, replacements, _, _, _, _, _) = fixture(
        vec![
            SkillTactic::ObserveAgain,
            SkillTactic::ResolveSemanticTarget,
            SkillTactic::ChangeInteractionMethod,
        ],
        Some(profile),
        vec![SkillBrowserEngine::Firefox],
        true,
        false,
        1_000,
        0,
        0,
        0,
    )
    .await;
    envelope.command =
        types::RuntimeCommand::Primitive(PrimitiveCommand::TypeText(TypeTextCommand {
            expected_url: None,
            selector: "#never-satisfied".into(),
            target: None,
            value: "expected-value".into(),
            clear_first: true,
        }));

    let execution = coordinator
        .execute_with_adaptation(&envelope, page)
        .await
        .unwrap();

    assert_eq!(
        execution
            .tactic_evidence
            .iter()
            .map(|evidence| evidence.tactic)
            .collect::<Vec<_>>(),
        vec![
            SkillTactic::ReconcileCheckpoint,
            SkillTactic::FreshGhostSession,
            SkillTactic::SelectCompatibleEngine,
            SkillTactic::RestartDurableBoundary,
        ]
    );
    assert!(releases.load(Ordering::SeqCst) >= 1);
    assert_eq!(replacements.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn pending_receipt_rejects_a_different_command_identity_without_wrong_journal_entry() {
    let profile = SkillProfile::new(
        "runtime-v1",
        SkillBrowserEngine::Chromium,
        [],
        "b".repeat(64),
    )
    .unwrap();
    let (coordinator, envelope, page, _, _, replacements, _, _, journal_path, _, _) = fixture(
        vec![
            SkillTactic::ObserveAgain,
            SkillTactic::ResolveSemanticTarget,
            SkillTactic::ChangeInteractionMethod,
            SkillTactic::ReconcileCheckpoint,
            SkillTactic::FreshGhostSession,
        ],
        Some(profile),
        vec![SkillBrowserEngine::Firefox],
        true,
        false,
        20,
        80,
        0,
        0,
    )
    .await;
    let first = tokio::spawn({
        let coordinator = coordinator.clone();
        let envelope = envelope.clone();
        let page = page.clone();
        async move { coordinator.execute_with_adaptation(&envelope, page).await }
    });
    while replacements.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    first.abort();
    let mut different = envelope.clone();
    different.command_id = CommandId::new();

    let error = coordinator
        .execute_with_adaptation(&different, page)
        .await
        .unwrap_err();

    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert_eq!(replacements.load(Ordering::SeqCst), 1);
    let reopened = JsonlJournal::open(journal_path).await.unwrap();
    assert!(reopened
        .history(different.command_id)
        .await
        .unwrap()
        .records
        .is_empty());
}

#[tokio::test]
async fn issued_identity_survives_receipt_write_failure_and_reconciles_only_exact_retry() {
    let profile = SkillProfile::new(
        "runtime-v1",
        SkillBrowserEngine::Chromium,
        [],
        "b".repeat(64),
    )
    .unwrap();
    let (
        coordinator,
        envelope,
        page,
        _,
        releases,
        replacements,
        _,
        _,
        journal_path,
        state_store,
        _,
    ) = fixture(
        vec![
            SkillTactic::ObserveAgain,
            SkillTactic::ResolveSemanticTarget,
            SkillTactic::ChangeInteractionMethod,
            SkillTactic::ReconcileCheckpoint,
            SkillTactic::FreshGhostSession,
        ],
        Some(profile),
        vec![SkillBrowserEngine::Firefox],
        true,
        false,
        20,
        80,
        0,
        0,
    )
    .await;
    let preflight_reached = Arc::new(Notify::new());
    let preflight_resume = Arc::new(Notify::new());
    let coordinator =
        coordinator.with_recovery_preflight_observer(Arc::new(BlockingCheckpointObserver {
            reached: Arc::clone(&preflight_reached),
            resume: Arc::clone(&preflight_resume),
        }));
    let checkpoint_root = journal_path.parent().unwrap().join("checkpoints");
    let checkpoint_store = CheckpointStore::open(&checkpoint_root).await.unwrap();
    let locked = checkpoint_store
        .lock_snapshot(&envelope.workflow_id)
        .await
        .unwrap();
    let valid_checkpoint = serde_json::to_vec(locked.checkpoint()).unwrap();
    let checkpoint_path = checkpoint_root.join(format!("{}.json", envelope.workflow_id.0));
    let first = tokio::spawn({
        let coordinator = coordinator.clone();
        let envelope = envelope.clone();
        let page = page.clone();
        async move { coordinator.execute_with_adaptation(&envelope, page).await }
    });
    tokio::time::timeout(
        std::time::Duration::from_secs(1),
        preflight_reached.notified(),
    )
    .await
    .expect("recovery must pause after checkpoint verification");
    assert!(
        state_store
            .get(&envelope.session_id)
            .unwrap()
            .pending_issuance
            .is_some(),
        "issued decision must be durably visible before receipt persistence"
    );
    assert!(
        CheckpointStore::open(&checkpoint_root)
            .await
            .unwrap()
            .load_skill_issuance(&envelope.workflow_id)
            .await
            .unwrap()
            .is_some(),
        "a reconstructed process must recover the issued decision"
    );
    tokio::fs::write(&checkpoint_path, b"{").await.unwrap();
    drop(locked);
    preflight_resume.notify_one();
    assert!(first.await.unwrap().is_err());
    tokio::fs::write(&checkpoint_path, valid_checkpoint)
        .await
        .unwrap();

    let mut different = envelope.clone();
    different.command_id = CommandId::new();
    let error = coordinator
        .execute_with_adaptation(&different, page.clone())
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert_eq!(releases.load(Ordering::SeqCst), 0);
    assert_eq!(replacements.load(Ordering::SeqCst), 0);
    let reopened = JsonlJournal::open(&journal_path).await.unwrap();
    assert!(reopened
        .history(different.command_id)
        .await
        .unwrap()
        .records
        .is_empty());

    let reconciled = coordinator
        .execute_with_adaptation(&envelope, page)
        .await
        .unwrap();
    assert!(matches!(
        reconciled.skill_outcome,
        SkillOutcome::Failed {
            failure: types::SkillFailure::DeadlineExceeded,
            ..
        }
    ));
    assert_eq!(releases.load(Ordering::SeqCst), 0);
    assert_eq!(replacements.load(Ordering::SeqCst), 0);
}
