use checkpoint_store::CheckpointStore;
use chrono::Utc;
use page_runtime::{evaluate_invariants, RecoveryCoordinator};
use types::{
    AttemptId, CheckpointId, CheckpointInvariant, CommandClass, Evidence, PageId, SessionId,
    WorkflowCheckpoint, WorkflowId,
};

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
