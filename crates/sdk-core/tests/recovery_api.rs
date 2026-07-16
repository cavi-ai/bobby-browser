use checkpoint_store::CheckpointStore;
use chrono::Utc;
use page_runtime::RecoveryCoordinator;
use sdk_core::RuntimeService;
use types::{
    AttemptId, CheckpointId, CommandClass, Evidence, PageId, SessionId, WorkflowCheckpoint,
    WorkflowId,
};

#[tokio::test]
async fn runtime_service_exposes_durable_checkpoint_and_recovery_boundary() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let runtime = RuntimeService::with_recovery(
        Default::default(),
        Default::default(),
        RecoveryCoordinator::new(store.clone()),
    );
    let checkpoint = WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: CheckpointId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: PageId::new(),
        restart_url: "https://example.test".into(),
        current_url: "https://example.test".into(),
        cursor: None,
        boundary_command_id: None,
        recovery_class: CommandClass::Replayable,
        invariants: Vec::new(),
        replayable_inputs: Vec::new(),
        evidence: Vec::new(),
        recovery_history: Vec::new(),
        created_at: Utc::now(),
    };

    assert!(runtime
        .runtime_info()
        .await
        .capabilities
        .contains(&"checkpoint-recovery".to_string()));

    runtime
        .checkpoint(checkpoint.clone(), Vec::<Evidence>::new())
        .await
        .unwrap();
    assert_eq!(
        store.load(&checkpoint.workflow_id).await.unwrap(),
        checkpoint
    );
    let error = runtime.recover(&checkpoint.workflow_id).await.unwrap_err();
    assert!(error.to_string().contains("workers are not configured"));
}
