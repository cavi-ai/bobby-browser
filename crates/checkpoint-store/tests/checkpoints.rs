use checkpoint_store::{CheckpointStore, CheckpointStoreError};
use chrono::Utc;
use types::{
    AttemptId, CheckpointId, CommandClass, PageId, SessionId, WorkflowCheckpoint, WorkflowId,
};

fn checkpoint(workflow_id: WorkflowId, current_url: &str) -> WorkflowCheckpoint {
    WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: CheckpointId::new(),
        workflow_id,
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: PageId::new(),
        restart_url: "https://example.test/start".into(),
        current_url: current_url.into(),
        cursor: None,
        recovery_class: CommandClass::Replayable,
        invariants: Vec::new(),
        replayable_inputs: Vec::new(),
        evidence: Vec::new(),
        recovery_history: Vec::new(),
        created_at: Utc::now(),
    }
}

#[tokio::test]
async fn saves_loads_and_atomically_replaces_a_workflow_checkpoint() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let workflow_id = WorkflowId::new();
    let first = checkpoint(workflow_id.clone(), "https://example.test/one");
    let second = checkpoint(workflow_id.clone(), "https://example.test/two");

    store.save(&first).await.unwrap();
    assert_eq!(store.load(&workflow_id).await.unwrap(), first);
    store.save(&second).await.unwrap();
    assert_eq!(store.load(&workflow_id).await.unwrap(), second);

    let entries: Vec<_> = std::fs::read_dir(root.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(entries.len(), 1, "temporary files must not survive save");
}

#[tokio::test]
async fn isolates_workflows_and_removes_idempotently() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let left = checkpoint(WorkflowId::new(), "https://example.test/left");
    let right = checkpoint(WorkflowId::new(), "https://example.test/right");

    let (left_result, right_result) = tokio::join!(store.save(&left), store.save(&right));
    left_result.unwrap();
    right_result.unwrap();
    assert_eq!(store.load(&left.workflow_id).await.unwrap(), left);
    assert_eq!(store.load(&right.workflow_id).await.unwrap(), right);

    store.remove(&left.workflow_id).await.unwrap();
    store.remove(&left.workflow_id).await.unwrap();
    assert!(matches!(
        store.load(&left.workflow_id).await,
        Err(CheckpointStoreError::NotFound(_))
    ));
}

#[tokio::test]
async fn rejects_corrupt_or_unsupported_checkpoints() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let workflow_id = WorkflowId::new();
    let path = root.path().join(format!("{}.json", workflow_id.0));
    std::fs::write(&path, b"not-json").unwrap();
    assert!(matches!(
        store.load(&workflow_id).await,
        Err(CheckpointStoreError::Serialization(_))
    ));

    let mut unsupported = checkpoint(workflow_id.clone(), "https://example.test");
    unsupported.schema_version += 1;
    std::fs::write(&path, serde_json::to_vec(&unsupported).unwrap()).unwrap();
    assert!(matches!(
        store.load(&workflow_id).await,
        Err(CheckpointStoreError::UnsupportedSchema { .. })
    ));
}
