use async_trait::async_trait;
use checkpoint_store::CheckpointStore;
use chrono::Utc;
use page_runtime::{evaluate_invariants, RecoveryCoordinator};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;
use types::{
    AttemptId, CheckpointId, CheckpointInvariant, ClickCommand, CommandClass, CommandError,
    Evidence, InspectCommand, NavigateCommand, PageId, RecoveryDecision, SessionId,
    TypeTextCommand, WorkerId, WorkflowCheckpoint, WorkflowId,
};
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};

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

struct RecoveryFactory {
    launches: AtomicUsize,
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
            launches: AtomicUsize::new(0),
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
