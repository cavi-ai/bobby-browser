use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use checkpoint_store::CheckpointStore;
use chrono::{Duration, Utc};
use page_runtime::{RecoveryCoordinator, SkillRecoveryCoordinator, SkillTacticEffect};
use skill_runtime::{SkillTrigger, SkillZigZagZig};
use tokio::sync::Mutex;
use types::{
    AttemptId, CheckpointId, CheckpointInvariant, ClickCommand, CommandClass, CommandEnvelope,
    CommandError, CommandId, CommandOutcome, ErrorCode, ErrorLayer, Evidence, InspectCommand,
    NavigateCommand, OpenPageRequest, PageId, PrimitiveCommand, RuntimeCommand, SessionId,
    SkillCheckpointProof, SkillEvidenceRef, SkillFailure, SkillOutcome, SkillSessionState,
    SkillTactic, TypeTextCommand, WorkerId, WorkflowCheckpoint, WorkflowId,
};
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};
use workflow_journal::JsonlJournal;

struct BoundaryFactory {
    mutations: Arc<AtomicUsize>,
    inspections: Arc<AtomicUsize>,
    launches: Arc<AtomicUsize>,
    current_url: Arc<Mutex<String>>,
    inspect_delay_ms: u64,
}

struct BoundaryWorker {
    id: WorkerId,
    profile: PathBuf,
    mutations: Arc<AtomicUsize>,
    inspections: Arc<AtomicUsize>,
    current_url: Arc<Mutex<String>>,
    inspect_delay_ms: u64,
}

#[async_trait]
impl WorkerFactory for BoundaryFactory {
    async fn launch(&self, _: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(BoundaryWorker {
            id: WorkerId::new(),
            profile: PathBuf::from("/profiles/recovery-test"),
            mutations: Arc::clone(&self.mutations),
            inspections: Arc::clone(&self.inspections),
            current_url: Arc::clone(&self.current_url),
            inspect_delay_ms: self.inspect_delay_ms,
        }))
    }
}

#[async_trait]
impl BrowserWorker for BoundaryWorker {
    fn worker_id(&self) -> WorkerId {
        self.id.clone()
    }

    fn profile_dir(&self) -> &Path {
        &self.profile
    }

    async fn open_page(&self, _: PageId) -> Result<(), CommandError> {
        Ok(())
    }

    async fn navigate(
        &self,
        _: &PageId,
        command: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        *self.current_url.lock().await = command.url.clone();
        Ok(vec![Evidence::Navigation {
            url: command.url.clone(),
            title: "Boundary fixture".into(),
        }])
    }

    async fn inspect(
        &self,
        _: &PageId,
        command: &InspectCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        self.inspections.fetch_add(1, Ordering::SeqCst);
        if self.inspect_delay_ms > 0 {
            tokio::time::sleep(std::time::Duration::from_millis(self.inspect_delay_ms)).await;
        }
        Ok(vec![Evidence::Inspection {
            selector: command.selector.clone(),
            url: self.current_url.lock().await.clone(),
            title: "Boundary fixture".into(),
            text: String::new(),
            html: None,
        }])
    }

    async fn click(&self, _: &PageId, _: &ClickCommand) -> Result<Vec<Evidence>, CommandError> {
        self.mutations.fetch_add(1, Ordering::SeqCst);
        *self.current_url.lock().await = "https://example.test/done".into();
        Err(CommandError {
            code: ErrorCode::BrowserCommandFailed,
            message: "transport closed after dispatch".into(),
            layer: ErrorLayer::Driver,
            retryable: true,
        })
    }

    async fn type_text(
        &self,
        _: &PageId,
        _: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        unreachable!("the boundary fixture only executes clicks")
    }

    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }
}

fn skill_state(
    session_id: SessionId,
    checkpoint_id: CheckpointId,
    checkpoint_digest: &str,
) -> SkillSessionState {
    let now = Utc::now();
    let attestation = SkillEvidenceRef::new("checkpoint-attestation", checkpoint_digest).unwrap();
    let proof =
        SkillCheckpointProof::new(checkpoint_id.clone(), session_id.clone(), now, attestation)
            .unwrap();
    SkillSessionState::new(
        session_id,
        BTreeMap::from([("SkillZigZagZig".into(), "1.0.0".into())]),
        None,
        Some(checkpoint_id),
        Some(proof),
        None,
        None,
        Vec::new(),
        Vec::new(),
        now + Duration::seconds(5),
    )
    .unwrap()
}

fn skill_state_without_checkpoint(session_id: SessionId) -> SkillSessionState {
    let now = Utc::now();
    SkillSessionState::new(
        session_id,
        BTreeMap::from([("SkillZigZagZig".into(), "1.0.0".into())]),
        None,
        None,
        None,
        None,
        None,
        Vec::new(),
        Vec::new(),
        now + Duration::seconds(5),
    )
    .unwrap()
}

struct RetryFactory {
    mutations: Arc<AtomicUsize>,
    semantic_interactions: Arc<AtomicUsize>,
    current_url: Arc<Mutex<String>>,
}

struct RetryWorker {
    id: WorkerId,
    profile: PathBuf,
    mutations: Arc<AtomicUsize>,
    semantic_interactions: Arc<AtomicUsize>,
    current_url: Arc<Mutex<String>>,
}

#[async_trait]
impl WorkerFactory for RetryFactory {
    async fn launch(&self, _: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(Arc::new(RetryWorker {
            id: WorkerId::new(),
            profile: PathBuf::from("/profiles/retry-test"),
            mutations: Arc::clone(&self.mutations),
            semantic_interactions: Arc::clone(&self.semantic_interactions),
            current_url: Arc::clone(&self.current_url),
        }))
    }
}

#[async_trait]
impl BrowserWorker for RetryWorker {
    fn worker_id(&self) -> WorkerId {
        self.id.clone()
    }

    fn profile_dir(&self) -> &Path {
        &self.profile
    }

    async fn open_page(&self, _: PageId) -> Result<(), CommandError> {
        Ok(())
    }

    async fn navigate(
        &self,
        _: &PageId,
        command: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        *self.current_url.lock().await = command.url.clone();
        Ok(vec![Evidence::Navigation {
            url: command.url.clone(),
            title: "Retry fixture".into(),
        }])
    }

    async fn inspect(
        &self,
        _: &PageId,
        command: &InspectCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Inspection {
            selector: command.selector.clone(),
            url: self.current_url.lock().await.clone(),
            title: "Retry fixture".into(),
            text: String::new(),
            html: None,
        }])
    }

    async fn click(
        &self,
        _: &PageId,
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        if command.target.is_some() {
            self.semantic_interactions.fetch_add(1, Ordering::SeqCst);
        }
        let mutation = self.mutations.fetch_add(1, Ordering::SeqCst);
        if mutation == 1 {
            *self.current_url.lock().await = "https://example.test/done".into();
        }
        Ok(vec![Evidence::Element {
            selector: command.selector.clone(),
            text: None,
        }])
    }

    async fn type_text(
        &self,
        _: &PageId,
        _: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        unreachable!("the retry fixture only executes clicks")
    }

    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }
}

#[tokio::test]
async fn read_only_tactics_precede_interaction_retry_and_preserve_the_postcondition() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path().join("unused-checkpoints"))
        .await
        .unwrap();
    let journal = Arc::new(
        JsonlJournal::open(root.path().join("retry-journal.jsonl"))
            .await
            .unwrap(),
    );
    let mutations = Arc::new(AtomicUsize::new(0));
    let semantic_interactions = Arc::new(AtomicUsize::new(0));
    let pool = Arc::new(WorkerPool::new(
        1,
        Arc::new(RetryFactory {
            mutations: Arc::clone(&mutations),
            semantic_interactions: Arc::clone(&semantic_interactions),
            current_url: Arc::new(Mutex::new("https://example.test/start".into())),
        }),
    ));
    let runtime = page_runtime::PageRuntime::new(journal, Arc::clone(&pool));
    let session_id = SessionId::new();
    let page = runtime
        .open(OpenPageRequest {
            session_id: session_id.clone(),
        })
        .await;
    let strategy = SkillZigZagZig::new(
        skill_state_without_checkpoint(session_id.clone()),
        1_000,
        [],
    )
    .unwrap();
    let coordinator = SkillRecoveryCoordinator::new(
        runtime,
        strategy,
        RecoveryCoordinator::new(store),
        Arc::clone(&pool),
    )
    .unwrap();
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id,
        page_id: Some(page.id.clone()),
        deadline: Utc::now() + Duration::seconds(5),
        command: RuntimeCommand::Primitive(PrimitiveCommand::Click(ClickCommand {
            selector: "#credential-like-private-target".into(),
            target: None,
            boundary: false,
            expected_url: Some("https://example.test/done".into()),
        })),
    };

    let execution = coordinator
        .execute_with_adaptation(&envelope, page)
        .await
        .unwrap();

    assert_eq!(mutations.load(Ordering::SeqCst), 2);
    assert_eq!(semantic_interactions.load(Ordering::SeqCst), 1);
    assert_eq!(
        execution
            .tactic_evidence
            .iter()
            .map(|item| item.tactic)
            .collect::<Vec<_>>(),
        vec![
            SkillTactic::ObserveAgain,
            SkillTactic::ResolveSemanticTarget,
            SkillTactic::ChangeInteractionMethod,
        ]
    );
    assert!(matches!(
        execution.skill_outcome,
        SkillOutcome::Adapted {
            tactic: SkillTactic::ChangeInteractionMethod,
            ..
        }
    ));
    assert!(matches!(
        execution.command_outcome,
        CommandOutcome::Completed { ref evidence, .. }
        if evidence.iter().any(|item| matches!(item, Evidence::Inspection { url, .. } if url == "https://example.test/done"))
    ));
    let tactic_json = serde_json::to_string(&execution.tactic_evidence).unwrap();
    assert!(!tactic_json.contains("credential-like-private-target"));
    assert!(!tactic_json.contains("example.test"));
}

#[tokio::test]
async fn uncertain_boundary_reconciles_without_duplicate_click() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path().join("checkpoints"))
        .await
        .unwrap();
    let journal = Arc::new(
        JsonlJournal::open(root.path().join("journal.jsonl"))
            .await
            .unwrap(),
    );
    let mutations = Arc::new(AtomicUsize::new(0));
    let launches = Arc::new(AtomicUsize::new(0));
    let pool = Arc::new(WorkerPool::new(
        1,
        Arc::new(BoundaryFactory {
            mutations: Arc::clone(&mutations),
            inspections: Arc::new(AtomicUsize::new(0)),
            launches: Arc::clone(&launches),
            current_url: Arc::new(Mutex::new("https://example.test/start".into())),
            inspect_delay_ms: 0,
        }),
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
    let command_id = CommandId::new();
    let checkpoint_id = CheckpointId::new();
    let checkpoint = WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: checkpoint_id.clone(),
        workflow_id: workflow_id.clone(),
        attempt_id: attempt_id.clone(),
        session_id: session_id.clone(),
        page_id: page.id.clone(),
        restart_url: "https://example.test/start".into(),
        current_url: "https://example.test/start".into(),
        cursor: None,
        boundary_command_id: Some(command_id.clone()),
        recovery_class: CommandClass::Boundary,
        invariants: vec![CheckpointInvariant::Url {
            value: "https://example.test/start".into(),
        }],
        replayable_inputs: Vec::new(),
        evidence: Vec::new(),
        recovery_history: Vec::new(),
        recovery_receipts: Vec::new(),
        created_at: Utc::now(),
    };
    let recovery = RecoveryCoordinator::with_workers(store.clone(), Arc::clone(&pool));
    recovery
        .save_verified(
            checkpoint,
            vec![Evidence::Navigation {
                url: "https://example.test/start".into(),
                title: "Boundary fixture".into(),
            }],
        )
        .await
        .unwrap();
    let snapshot = store.lock_snapshot(&workflow_id).await.unwrap();
    let digest = snapshot.digest().to_owned();
    drop(snapshot);
    let strategy = SkillZigZagZig::new(
        skill_state(session_id.clone(), checkpoint_id, &digest),
        1_000,
        [],
    )
    .unwrap();
    let coordinator =
        SkillRecoveryCoordinator::new(runtime, strategy, recovery, Arc::clone(&pool)).unwrap();
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id,
        workflow_id,
        attempt_id,
        session_id,
        page_id: Some(page.id.clone()),
        deadline: Utc::now() + Duration::seconds(5),
        command: RuntimeCommand::Primitive(PrimitiveCommand::Click(ClickCommand {
            selector: "#purchase".into(),
            target: None,
            boundary: true,
            expected_url: Some("https://example.test/done".into()),
        })),
    };

    let execution = coordinator
        .execute_with_adaptation(&envelope, page)
        .await
        .unwrap();

    assert_eq!(mutations.load(Ordering::SeqCst), 1);
    assert_eq!(launches.load(Ordering::SeqCst), 1);
    assert!(matches!(
        execution.skill_outcome,
        SkillOutcome::Adapted {
            tactic: SkillTactic::ReconcileCheckpoint,
            ..
        }
    ));
    assert_eq!(execution.tactic_evidence.len(), 1);
    assert_eq!(
        execution.tactic_evidence[0].effect,
        SkillTacticEffect::PostconditionConfirmed
    );
    assert!(matches!(
        execution.command_outcome,
        CommandOutcome::Completed { ref evidence, .. }
        if evidence.iter().any(|item| matches!(item, Evidence::Configuration { name, .. } if name == "skillRecoveryTactic"))
    ));
    assert_eq!(
        SkillTrigger::new(
            SkillFailure::EffectUncertain,
            "click expected URL is observed"
        )
        .unwrap()
        .failure,
        SkillFailure::EffectUncertain
    );
}

#[tokio::test]
async fn cancelled_boundary_recovery_persists_receipt_without_duplicate_click() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path().join("deadline-checkpoints"))
        .await
        .unwrap();
    let journal = Arc::new(
        JsonlJournal::open(root.path().join("deadline-journal.jsonl"))
            .await
            .unwrap(),
    );
    let mutations = Arc::new(AtomicUsize::new(0));
    let inspections = Arc::new(AtomicUsize::new(0));
    let launches = Arc::new(AtomicUsize::new(0));
    let pool = Arc::new(WorkerPool::new(
        1,
        Arc::new(BoundaryFactory {
            mutations: Arc::clone(&mutations),
            inspections: Arc::clone(&inspections),
            launches: Arc::clone(&launches),
            current_url: Arc::new(Mutex::new("https://example.test/start".into())),
            inspect_delay_ms: 100,
        }),
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
    let command_id = CommandId::new();
    let checkpoint_id = CheckpointId::new();
    let recovery = RecoveryCoordinator::with_workers(store.clone(), Arc::clone(&pool));
    recovery
        .save_verified(
            WorkflowCheckpoint {
                schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
                checkpoint_id: checkpoint_id.clone(),
                workflow_id: workflow_id.clone(),
                attempt_id: attempt_id.clone(),
                session_id: session_id.clone(),
                page_id: page.id.clone(),
                restart_url: "https://example.test/start".into(),
                current_url: "https://example.test/start".into(),
                cursor: None,
                boundary_command_id: Some(command_id.clone()),
                recovery_class: CommandClass::Boundary,
                invariants: vec![CheckpointInvariant::Url {
                    value: "https://example.test/start".into(),
                }],
                replayable_inputs: Vec::new(),
                evidence: Vec::new(),
                recovery_history: Vec::new(),
                recovery_receipts: Vec::new(),
                created_at: Utc::now(),
            },
            vec![Evidence::Navigation {
                url: "https://example.test/start".into(),
                title: "Boundary fixture".into(),
            }],
        )
        .await
        .unwrap();
    let snapshot = store.lock_snapshot(&workflow_id).await.unwrap();
    let digest = snapshot.digest().to_owned();
    drop(snapshot);
    let strategy = SkillZigZagZig::new(
        skill_state(session_id.clone(), checkpoint_id, &digest),
        20,
        [],
    )
    .unwrap();
    let coordinator =
        SkillRecoveryCoordinator::new(runtime, strategy, recovery, Arc::clone(&pool)).unwrap();
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id,
        workflow_id,
        attempt_id,
        session_id,
        page_id: Some(page.id.clone()),
        deadline: Utc::now() + Duration::milliseconds(200),
        command: RuntimeCommand::Primitive(PrimitiveCommand::Click(ClickCommand {
            selector: "#purchase".into(),
            target: None,
            boundary: true,
            expected_url: Some("https://example.test/other".into()),
        })),
    };
    let first = tokio::spawn({
        let coordinator = coordinator.clone();
        let envelope = envelope.clone();
        let page = page.clone();
        async move { coordinator.execute_with_adaptation(&envelope, page).await }
    });
    while inspections.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    first.abort();

    let mut different = envelope.clone();
    different.command_id = CommandId::new();
    let mismatch = coordinator
        .execute_with_adaptation(&different, page.clone())
        .await
        .unwrap_err();
    assert_eq!(mismatch.code, ErrorCode::InvalidRequest);
    assert_eq!(mutations.load(Ordering::SeqCst), 1);

    let execution = coordinator
        .execute_with_adaptation(&envelope, page.clone())
        .await
        .unwrap();
    let replayed = coordinator
        .execute_with_adaptation(&envelope, page)
        .await
        .unwrap();

    assert_eq!(mutations.load(Ordering::SeqCst), 1);
    assert_eq!(launches.load(Ordering::SeqCst), 1);
    assert!(matches!(
        execution.skill_outcome,
        SkillOutcome::Failed {
            failure: SkillFailure::EffectUncertain,
            ..
        }
    ));
    assert!(matches!(
        execution.command_outcome,
        CommandOutcome::NeedsReconciliation { ref error, .. }
        if error.code == ErrorCode::VerificationFailed
    ));
    assert_eq!(
        serde_json::to_value(execution.command_outcome.journal_safe()).unwrap(),
        serde_json::to_value(replayed.command_outcome).unwrap()
    );
}

#[tokio::test]
async fn checkpoint_authority_mismatch_fails_before_pool_replacement() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path().join("checkpoints"))
        .await
        .unwrap();
    let journal = Arc::new(
        JsonlJournal::open(root.path().join("mismatch-journal.jsonl"))
            .await
            .unwrap(),
    );
    let mutations = Arc::new(AtomicUsize::new(0));
    let launches = Arc::new(AtomicUsize::new(0));
    let pool = Arc::new(WorkerPool::new(
        1,
        Arc::new(BoundaryFactory {
            mutations: Arc::clone(&mutations),
            inspections: Arc::new(AtomicUsize::new(0)),
            launches: Arc::clone(&launches),
            current_url: Arc::new(Mutex::new("https://example.test/start".into())),
            inspect_delay_ms: 0,
        }),
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
    let command_id = CommandId::new();
    let stored_checkpoint_id = CheckpointId::new();
    let checkpoint = WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: stored_checkpoint_id,
        workflow_id: workflow_id.clone(),
        attempt_id: attempt_id.clone(),
        session_id: session_id.clone(),
        page_id: page.id.clone(),
        restart_url: "https://example.test/start".into(),
        current_url: "https://example.test/start".into(),
        cursor: None,
        boundary_command_id: Some(command_id.clone()),
        recovery_class: CommandClass::Boundary,
        invariants: vec![CheckpointInvariant::Url {
            value: "https://example.test/start".into(),
        }],
        replayable_inputs: Vec::new(),
        evidence: Vec::new(),
        recovery_history: Vec::new(),
        recovery_receipts: Vec::new(),
        created_at: Utc::now(),
    };
    let recovery = RecoveryCoordinator::with_workers(store.clone(), Arc::clone(&pool));
    recovery
        .save_verified(
            checkpoint,
            vec![Evidence::Navigation {
                url: "https://example.test/start".into(),
                title: "Boundary fixture".into(),
            }],
        )
        .await
        .unwrap();
    let snapshot = store.lock_snapshot(&workflow_id).await.unwrap();
    let digest = snapshot.digest().to_owned();
    drop(snapshot);
    let reviewed_checkpoint_id = CheckpointId::new();
    let strategy = SkillZigZagZig::new(
        skill_state(session_id.clone(), reviewed_checkpoint_id, &digest),
        1_000,
        [],
    )
    .unwrap();
    let coordinator =
        SkillRecoveryCoordinator::new(runtime, strategy, recovery, Arc::clone(&pool)).unwrap();
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id,
        workflow_id,
        attempt_id,
        session_id,
        page_id: Some(page.id.clone()),
        deadline: Utc::now() + Duration::seconds(5),
        command: RuntimeCommand::Primitive(PrimitiveCommand::Click(ClickCommand {
            selector: "#purchase".into(),
            target: None,
            boundary: true,
            expected_url: Some("https://example.test/other".into()),
        })),
    };

    let execution = coordinator
        .execute_with_adaptation(&envelope, page)
        .await
        .unwrap();

    assert!(matches!(
        execution.command_outcome,
        CommandOutcome::NeedsReconciliation { ref error, .. }
        if error.code == ErrorCode::VerificationFailed
    ));
    assert!(matches!(
        execution.skill_outcome,
        SkillOutcome::Failed {
            failure: SkillFailure::EffectUncertain,
            ..
        }
    ));
    assert!(execution
        .tactic_evidence
        .iter()
        .all(|evidence| evidence.trigger == SkillFailure::EffectUncertain));
    assert_eq!(mutations.load(Ordering::SeqCst), 1);
    assert_eq!(launches.load(Ordering::SeqCst), 1);
}
