use async_trait::async_trait;
use checkpoint_store::CheckpointStore;
use chrono::Utc;
use page_runtime::{evaluate_invariants, PageRuntime, RecoveryCoordinator};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use types::{
    AttemptId, CheckpointId, CheckpointInvariant, ClickCommand, CommandClass, CommandEnvelope,
    CommandError, CommandId, CommandOutcome, Evidence, InspectCommand, NavigateCommand, PageId,
    RecoveryCommandIdentity, RecoveryDecision, RecoveryReceipt, RecoveryReceiptState, SessionId,
    SkillDecision, SkillFailure, SkillOutcome, SkillTactic, TypeTextCommand, WorkerId,
    WorkflowCheckpoint, WorkflowId,
};
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};
use workflow_journal::CommandJournal;

fn checkpoint() -> WorkflowCheckpoint {
    WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: CheckpointId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: PageId::new(),
        restart_url: "https://example.test/start".into(),
        current_url: "https://example.test/step-two".into(),
        cursor: None,
        boundary_command_id: None,
        recovery_class: CommandClass::Reconciliable,
        invariants: vec![
            CheckpointInvariant::Url {
                value: "https://example.test/step-two".into(),
            },
            CheckpointInvariant::Title {
                value: "Step Two".into(),
            },
            CheckpointInvariant::Text {
                selector: "#name".into(),
                value: "Ada".into(),
            },
        ],
        replayable_inputs: vec!["Ada".into()],
        evidence: Vec::new(),
        recovery_history: Vec::new(),
        recovery_receipts: Vec::new(),
        created_at: Utc::now(),
    }
}

fn matching_evidence() -> Vec<Evidence> {
    vec![
        Evidence::Inspection {
            selector: None,
            url: "https://example.test/step-two".into(),
            title: "Step Two".into(),
            text: String::new(),
            html: None,
        },
        Evidence::Element {
            selector: "#name".into(),
            text: Some("Ada".into()),
        },
    ]
}

fn recovery_receipt(
    checkpoint: &WorkflowCheckpoint,
    state: RecoveryReceiptState,
) -> RecoveryReceipt {
    let command_id = CommandId::new();
    RecoveryReceipt::new(
        command_id.clone(),
        RecoveryCommandIdentity::new(
            command_id.clone(),
            checkpoint.workflow_id.clone(),
            checkpoint.attempt_id.clone(),
            checkpoint.session_id.clone(),
            Some(checkpoint.page_id.clone()),
            checkpoint.recovery_class,
            "a".repeat(64),
        )
        .unwrap(),
        state,
        CommandId::new(),
        SkillDecision::new(
            SkillTactic::ObserveAgain,
            SkillFailure::DeadlineExceeded,
            "observed postcondition",
            100,
            100,
            None,
            None,
        )
        .unwrap(),
        CommandOutcome::Completed {
            command_id,
            evidence: Vec::new(),
        },
        SkillOutcome::failed(SkillFailure::DeadlineExceeded, Vec::new()).unwrap(),
        Vec::new(),
        Utc::now(),
    )
    .unwrap()
}

#[test]
fn evaluates_every_checkpoint_invariant_with_actionable_failures() {
    let checkpoint = checkpoint();
    let matched = evaluate_invariants(&checkpoint.invariants, &matching_evidence());
    assert!(matched.is_match());

    let mismatched = evaluate_invariants(
        &checkpoint.invariants,
        &[Evidence::Inspection {
            selector: None,
            url: "https://example.test/wrong".into(),
            title: "Wrong".into(),
            text: String::new(),
            html: None,
        }],
    );
    assert!(!mismatched.is_match());
    assert_eq!(mismatched.failures().len(), 3);
    assert!(mismatched.failures()[0].contains("URL"));
    assert!(mismatched.failures()[1].contains("title"));
    assert!(mismatched.failures()[2].contains("#name"));
}

#[tokio::test]
async fn persists_only_a_checkpoint_proven_by_observed_evidence() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let coordinator = RecoveryCoordinator::new(store.clone());
    let mut checkpoint = checkpoint();

    let saved = coordinator
        .save_verified(checkpoint.clone(), matching_evidence())
        .await
        .unwrap();
    assert_eq!(saved.evidence, matching_evidence());
    assert_eq!(store.load(&saved.workflow_id).await.unwrap(), saved);

    checkpoint.workflow_id = WorkflowId::new();
    let error = coordinator
        .save_verified(checkpoint.clone(), Vec::new())
        .await
        .unwrap_err();
    assert!(error.to_string().contains("checkpoint invariants failed"));
    assert!(store.load(&checkpoint.workflow_id).await.is_err());
}

#[tokio::test]
async fn recovery_receipt_creation_starts_unresolved_and_rejects_payload_overwrite() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let coordinator = RecoveryCoordinator::new(store.clone());
    let checkpoint = checkpoint();
    store.save(&checkpoint).await.unwrap();

    let pending = recovery_receipt(&checkpoint, RecoveryReceiptState::PendingJournal);
    assert!(coordinator.persist_recovery_receipt(pending).await.is_err());

    let unresolved = recovery_receipt(&checkpoint, RecoveryReceiptState::Unresolved);
    coordinator
        .persist_recovery_receipt(unresolved.clone())
        .await
        .unwrap();
    coordinator
        .persist_recovery_receipt(unresolved.clone())
        .await
        .unwrap();
    let mut overwritten = unresolved;
    overwritten.command_outcome = CommandOutcome::Completed {
        command_id: overwritten.identity.command_id.clone(),
        evidence: vec![Evidence::Configuration {
            name: "changed".into(),
            value: "true".into(),
        }],
    };
    assert!(coordinator
        .persist_recovery_receipt(overwritten)
        .await
        .is_err());
}

#[tokio::test]
async fn recovery_receipt_fsm_is_forward_only_and_committed_payload_survives_reopen() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let coordinator = RecoveryCoordinator::new(store.clone());
    let checkpoint = checkpoint();
    store.save(&checkpoint).await.unwrap();
    let unresolved = recovery_receipt(&checkpoint, RecoveryReceiptState::Unresolved);
    coordinator
        .persist_recovery_receipt(unresolved.clone())
        .await
        .unwrap();

    assert!(coordinator
        .transition_recovery_receipt(&unresolved.identity, RecoveryReceiptState::Committed,)
        .await
        .is_err());
    coordinator
        .transition_recovery_receipt(&unresolved.identity, RecoveryReceiptState::PendingJournal)
        .await
        .unwrap();
    coordinator
        .transition_recovery_receipt(&unresolved.identity, RecoveryReceiptState::PendingJournal)
        .await
        .unwrap();
    assert!(coordinator
        .transition_recovery_receipt(&unresolved.identity, RecoveryReceiptState::Unresolved)
        .await
        .is_err());
    coordinator
        .transition_recovery_receipt(&unresolved.identity, RecoveryReceiptState::Committed)
        .await
        .unwrap();
    coordinator
        .transition_recovery_receipt(&unresolved.identity, RecoveryReceiptState::Committed)
        .await
        .unwrap();
    assert!(coordinator
        .transition_recovery_receipt(&unresolved.identity, RecoveryReceiptState::PendingJournal,)
        .await
        .is_err());

    let reopened_store = CheckpointStore::open(root.path()).await.unwrap();
    let reopened = RecoveryCoordinator::new(reopened_store.clone());
    let committed = reopened_store
        .load(&checkpoint.workflow_id)
        .await
        .unwrap()
        .recovery_receipts
        .into_iter()
        .find(|receipt| receipt.identity == unresolved.identity)
        .unwrap();
    assert_eq!(committed.state, RecoveryReceiptState::Committed);
    let mut changed = committed.clone();
    changed.recorded_at = Utc::now() + chrono::Duration::seconds(1);
    assert!(reopened.persist_recovery_receipt(changed).await.is_err());
    assert_eq!(
        reopened_store
            .load(&checkpoint.workflow_id)
            .await
            .unwrap()
            .recovery_receipts,
        vec![committed]
    );
}

#[tokio::test]
async fn durable_restart_rejects_a_checkpoint_without_persisted_invariant_proof() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let launches = Arc::new(AtomicUsize::new(0));
    let navigations = Arc::new(Mutex::new(Vec::new()));
    let pool = Arc::new(WorkerPool::new(
        1,
        Arc::new(RecoveryFactory {
            launches: Arc::clone(&launches),
            replacement_matches: true,
            navigations,
        }),
    ));
    let unverified = checkpoint();
    store.save(&unverified).await.unwrap();
    let coordinator = RecoveryCoordinator::with_workers(store, pool);

    let error = coordinator
        .restart_from_verified_boundary(&unverified.workflow_id)
        .await
        .unwrap_err();

    assert!(error.to_string().contains("checkpoint invariants failed"));
    assert_eq!(launches.load(Ordering::SeqCst), 0);
}

struct RecoveryFactory {
    launches: Arc<AtomicUsize>,
    replacement_matches: bool,
    navigations: Arc<Mutex<Vec<String>>>,
}

struct RecoveryWorker {
    id: WorkerId,
    profile: PathBuf,
    matches: bool,
    current_url: Mutex<String>,
    navigations: Arc<Mutex<Vec<String>>>,
}

#[async_trait]
impl WorkerFactory for RecoveryFactory {
    async fn launch(&self, session: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        let generation = self.launches.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(RecoveryWorker {
            id: WorkerId::new(),
            profile: PathBuf::from(format!("/profiles/{}", session.0)),
            matches: generation == 0 || self.replacement_matches,
            current_url: Mutex::new("about:blank".into()),
            navigations: self.navigations.clone(),
        }))
    }
}

#[async_trait]
impl BrowserWorker for RecoveryWorker {
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
        self.navigations.lock().await.push(command.url.clone());
        Ok(vec![Evidence::Navigation {
            url: command.url.clone(),
            title: if self.matches { "Step Two" } else { "Wrong" }.into(),
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
            title: if self.matches { "Step Two" } else { "Wrong" }.into(),
            text: if self.matches && command.selector.as_deref() == Some("#name") {
                "Ada"
            } else {
                "Grace"
            }
            .into(),
            html: None,
        }])
    }
    async fn click(&self, _: &PageId, _: &ClickCommand) -> Result<Vec<Evidence>, CommandError> {
        Ok(Vec::new())
    }
    async fn type_text(
        &self,
        _: &PageId,
        _: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(Vec::new())
    }
    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }
}

async fn recover(
    class: CommandClass,
    matches: bool,
) -> (RecoveryDecision, Vec<String>, WorkflowCheckpoint) {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let navigations = Arc::new(Mutex::new(Vec::new()));
    let pool = Arc::new(WorkerPool::new(
        1,
        Arc::new(RecoveryFactory {
            launches: Arc::new(AtomicUsize::new(0)),
            replacement_matches: matches,
            navigations: navigations.clone(),
        }),
    ));
    let mut checkpoint = checkpoint();
    checkpoint.recovery_class = class;
    pool.lease(checkpoint.session_id.clone()).await.unwrap();
    let coordinator = RecoveryCoordinator::with_workers(store.clone(), pool);
    coordinator
        .save_verified(checkpoint.clone(), matching_evidence())
        .await
        .unwrap();
    let decision = coordinator.recover(&checkpoint.workflow_id).await.unwrap();
    let persisted = store.load(&checkpoint.workflow_id).await.unwrap();
    assert_eq!(persisted.recovery_history.len(), 1);
    assert_eq!(persisted.recovery_history[0].decision, decision);
    let recorded = navigations.lock().await.clone();
    (decision, recorded, checkpoint)
}

#[tokio::test]
async fn recovery_resumes_reconciles_or_restarts_without_guessing() {
    let (resumed, navigations, checkpoint) = recover(CommandClass::Reconciliable, true).await;
    assert!(
        matches!(resumed, RecoveryDecision::Resumed { attempt_id, .. } if attempt_id == checkpoint.attempt_id)
    );
    assert_eq!(navigations, vec![checkpoint.current_url]);

    let (uncertain, navigations, checkpoint) = recover(CommandClass::Boundary, false).await;
    assert!(
        matches!(uncertain, RecoveryDecision::NeedsReconciliation { attempt_id, .. } if attempt_id == checkpoint.attempt_id)
    );
    assert_eq!(navigations, vec![checkpoint.current_url]);

    let (restarted, navigations, checkpoint) = recover(CommandClass::Reconciliable, false).await;
    assert!(
        matches!(restarted, RecoveryDecision::Restarted { lineage, .. }
        if lineage.abandoned_attempt_id == checkpoint.attempt_id && lineage.attempt_id != checkpoint.attempt_id)
    );
    assert_eq!(
        navigations,
        vec![checkpoint.current_url, checkpoint.restart_url]
    );
}

fn no_op_worker_pool() -> Arc<WorkerPool> {
    Arc::new(WorkerPool::new(
        1,
        Arc::new(RecoveryFactory {
            launches: Arc::new(AtomicUsize::new(0)),
            replacement_matches: true,
            navigations: Arc::new(Mutex::new(Vec::new())),
        }),
    ))
}

#[tokio::test]
async fn evidence_for_command_returns_the_terminal_outcome_evidence() {
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(
        workflow_journal::JsonlJournal::open(root.path().join("journal.jsonl"))
            .await
            .unwrap(),
    );
    let runtime = PageRuntime::new(journal.clone(), no_op_worker_pool());

    let command_id = CommandId::new();
    let evidence = vec![Evidence::Navigation {
        url: "https://example.test/".to_owned(),
        title: "fixture".to_owned(),
    }];
    journal
        .append(workflow_journal::JournalRecord {
            sequence: 0,
            recorded_at: Utc::now(),
            command_id: command_id.clone(),
            phase: types::CommandPhase::Completed,
            envelope: None,
            outcome: Some(CommandOutcome::Completed {
                command_id: command_id.clone(),
                evidence: evidence.clone(),
            }),
            prepared_result: None,
        })
        .await
        .unwrap();

    let resolved = runtime.evidence_for_command(command_id).await.unwrap();
    assert_eq!(resolved, evidence);
}

#[tokio::test]
async fn evidence_for_command_rejects_a_command_with_no_journal_record() {
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(
        workflow_journal::JsonlJournal::open(root.path().join("journal.jsonl"))
            .await
            .unwrap(),
    );
    let runtime = PageRuntime::new(journal, no_op_worker_pool());
    assert!(runtime
        .evidence_for_command(CommandId::new())
        .await
        .is_err());
}

#[tokio::test]
async fn command_session_returns_the_envelope_session_for_a_recorded_command() {
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(
        workflow_journal::JsonlJournal::open(root.path().join("journal.jsonl"))
            .await
            .unwrap(),
    );
    let runtime = PageRuntime::new(journal.clone(), no_op_worker_pool());

    let command_id = CommandId::new();
    let session_id = SessionId::new();
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: command_id.clone(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: session_id.clone(),
        page_id: None,
        deadline: Utc::now() + chrono::Duration::seconds(30),
        command: types::RuntimeCommand::Primitive(types::PrimitiveCommand::Inspect(
            InspectCommand::default(),
        )),
    };
    // Mirrors `executor.rs`'s `execute_with_vision_gate`: the `Accepted` phase
    // record carries the envelope; later phases (here, the terminal one) do not.
    journal
        .append(workflow_journal::JournalRecord {
            sequence: 0,
            recorded_at: Utc::now(),
            command_id: command_id.clone(),
            phase: types::CommandPhase::Accepted,
            envelope: Some(envelope),
            outcome: None,
            prepared_result: None,
        })
        .await
        .unwrap();
    journal
        .append(workflow_journal::JournalRecord {
            sequence: 0,
            recorded_at: Utc::now(),
            command_id: command_id.clone(),
            phase: types::CommandPhase::Completed,
            envelope: None,
            outcome: Some(CommandOutcome::Completed {
                command_id: command_id.clone(),
                evidence: Vec::new(),
            }),
            prepared_result: None,
        })
        .await
        .unwrap();

    let resolved = runtime.command_session(&command_id).await.unwrap();
    assert_eq!(resolved, session_id);
}

#[tokio::test]
async fn command_session_rejects_a_command_with_no_journal_record() {
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(
        workflow_journal::JsonlJournal::open(root.path().join("journal.jsonl"))
            .await
            .unwrap(),
    );
    let runtime = PageRuntime::new(journal, no_op_worker_pool());
    assert!(runtime.command_session(&CommandId::new()).await.is_err());
}
