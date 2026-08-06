use checkpoint_store::{CheckpointStore, CheckpointStoreError};
use chrono::{Duration, Utc};
use types::{
    AttemptId, CheckpointId, CommandClass, CommandId, PageId, RecoveryDecision, RecoveryRecord,
    SessionId, SkillCommandIdentity, SkillDecision, SkillFailure, SkillIssuedDecision, SkillTactic,
    WorkflowCheckpoint, WorkflowId,
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
        boundary_command_id: None,
        recovery_class: CommandClass::Replayable,
        invariants: Vec::new(),
        replayable_inputs: Vec::new(),
        evidence: Vec::new(),
        recovery_history: Vec::new(),
        recovery_receipts: Vec::new(),
        created_at: Utc::now(),
    }
}

#[tokio::test]
async fn saves_loads_and_atomically_replaces_a_workflow_checkpoint() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let workflow_id = WorkflowId::new();
    let first = checkpoint(workflow_id.clone(), "https://example.test/one");
    let mut second = checkpoint(workflow_id.clone(), "https://example.test/two");
    second.session_id = first.session_id.clone();

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
async fn established_workflow_rejects_rebinding_to_another_session() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let workflow_id = WorkflowId::new();
    let first = checkpoint(workflow_id.clone(), "https://example.test/one");
    let replacement = checkpoint(workflow_id.clone(), "https://example.test/two");
    assert_ne!(first.session_id, replacement.session_id);

    store.save(&first).await.unwrap();
    let error = store.save(&replacement).await.unwrap_err();

    assert!(matches!(error, CheckpointStoreError::IdentityChanged));
    assert_eq!(store.load(&workflow_id).await.unwrap(), first);
}

#[tokio::test]
async fn issued_skill_decision_survives_store_reopen_until_explicitly_cleared() {
    let root = tempfile::tempdir().unwrap();
    let workflow_id = WorkflowId::new();
    let session_id = SessionId::new();
    let now = Utc::now();
    let identity = SkillCommandIdentity::new(
        CommandId::new(),
        workflow_id.clone(),
        AttemptId::new(),
        session_id.clone(),
        Some(PageId::new()),
        CommandClass::Boundary,
        "a".repeat(64),
    )
    .unwrap();
    let issuance = SkillIssuedDecision::new_for_command(
        CommandId::new(),
        session_id,
        identity,
        SkillDecision::new(
            SkillTactic::ObserveAgain,
            SkillFailure::TargetDrift,
            "submitted",
            1_000,
            500,
            None,
            None,
        )
        .unwrap(),
        None,
        now,
        now + Duration::seconds(1),
    )
    .unwrap();

    CheckpointStore::open(root.path())
        .await
        .unwrap()
        .save_skill_issuance(&workflow_id, &issuance)
        .await
        .unwrap();
    let reopened = CheckpointStore::open(root.path()).await.unwrap();
    assert_eq!(
        reopened.load_skill_issuance(&workflow_id).await.unwrap(),
        Some(issuance)
    );
    reopened.remove_skill_issuance(&workflow_id).await.unwrap();
    assert_eq!(
        reopened.load_skill_issuance(&workflow_id).await.unwrap(),
        None
    );
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

#[tokio::test]
async fn loads_foundation_v1_checkpoints_without_new_recovery_fields() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let checkpoint = checkpoint(WorkflowId::new(), "https://example.test");
    let path = root
        .path()
        .join(format!("{}.json", checkpoint.workflow_id.0));
    let mut value = serde_json::to_value(&checkpoint).unwrap();
    value.as_object_mut().unwrap().remove("boundaryCommandId");
    value.as_object_mut().unwrap().remove("recoveryHistory");
    value.as_object_mut().unwrap().remove("recoveryReceipts");
    std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();

    let loaded = store.load(&checkpoint.workflow_id).await.unwrap();
    assert_eq!(loaded.boundary_command_id, None);
    assert!(loaded.recovery_history.is_empty());
    assert!(loaded.recovery_receipts.is_empty());
}

#[tokio::test]
async fn locked_snapshot_blocks_same_workflow_writes_and_detects_external_swaps() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let workflow_id = WorkflowId::new();
    let first = checkpoint(workflow_id.clone(), "https://example.test/one");
    let mut second = checkpoint(workflow_id.clone(), "https://example.test/two");
    second.session_id = first.session_id.clone();
    store.save(&first).await.unwrap();

    let locked = store.lock_snapshot(&workflow_id).await.unwrap();
    assert_eq!(locked.checkpoint(), &first);
    assert_eq!(locked.digest().len(), 64);

    let writer = tokio::spawn({
        let store = store.clone();
        let second = second.clone();
        async move { store.save(&second).await }
    });
    tokio::task::yield_now().await;
    assert!(
        !writer.is_finished(),
        "workflow writer bypassed snapshot lock"
    );

    let mut swapped = checkpoint(workflow_id.clone(), "https://example.test/external");
    swapped.session_id = first.session_id.clone();
    std::fs::write(
        root.path().join(format!("{}.json", workflow_id.0)),
        serde_json::to_vec(&swapped).unwrap(),
    )
    .unwrap();
    assert!(matches!(
        locked.verify_unchanged().await,
        Err(CheckpointStoreError::SnapshotChanged)
    ));
    drop(locked);
    writer.await.unwrap().unwrap();
    assert_eq!(store.load(&workflow_id).await.unwrap(), second);
}

#[tokio::test]
async fn authority_digest_ignores_recovery_history_but_content_version_changes() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let workflow_id = WorkflowId::new();
    let mut checkpoint = checkpoint(workflow_id.clone(), "https://example.test/one");
    store.save(&checkpoint).await.unwrap();
    let first = store.lock_snapshot(&workflow_id).await.unwrap();
    let authority_digest = first.digest().to_owned();
    let content_digest = first.content_digest().to_owned();
    drop(first);

    checkpoint.recovery_history.push(RecoveryRecord {
        recorded_at: Utc::now(),
        decision: RecoveryDecision::Resumed {
            checkpoint_id: checkpoint.checkpoint_id.clone(),
            attempt_id: checkpoint.attempt_id.clone(),
            evidence: Vec::new(),
        },
    });
    store.save(&checkpoint).await.unwrap();
    let second = store.lock_snapshot(&workflow_id).await.unwrap();

    assert_eq!(second.digest(), authority_digest);
    assert_ne!(second.content_digest(), content_digest);
}

/// An agent that lost its `workflowId` can find its workflows again.
///
/// The store is one file per workflow with no index, so before this there was
/// no way back at all: a compacted or restarted agent could not name any of
/// its own in-flight workflows, and `recovery_status`/`workflow_recover` take
/// the id as their only key.
#[tokio::test]
async fn a_session_lists_its_own_workflows_newest_first_within_the_cap() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let session = SessionId::new();

    let mut expected = Vec::new();
    for index in 0..4 {
        let mut entry = checkpoint(WorkflowId::new(), "https://example.test/step");
        entry.session_id = session.clone();
        // Deterministic ordering: no reliance on filesystem or clock ticks.
        entry.created_at = Utc::now() + Duration::seconds(index);
        store.save(&entry).await.unwrap();
        expected.push(entry.workflow_id.clone());
    }
    // Another session's workflow shares the directory and must not appear.
    let other = checkpoint(WorkflowId::new(), "https://example.test/other");
    store.save(&other).await.unwrap();

    let listed = store.list_for_session(&session, 32).await.unwrap();
    let ids: Vec<_> = listed
        .iter()
        .map(|entry| entry.workflow_id.clone())
        .collect();
    expected.reverse();
    assert_eq!(ids, expected, "newest first, and only this session's");

    let capped = store.list_for_session(&session, 2).await.unwrap();
    assert_eq!(capped.len(), 2, "the cap bounds the listing");
    assert_eq!(
        capped[0].workflow_id, expected[0],
        "the cap keeps the newest"
    );

    let none = store.list_for_session(&SessionId::new(), 32).await.unwrap();
    assert!(none.is_empty(), "an unknown session lists nothing");
}

/// One unreadable file must not hide every other recoverable workflow.
#[tokio::test]
async fn a_corrupt_entry_is_skipped_rather_than_failing_the_listing() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let session = SessionId::new();
    let mut good = checkpoint(WorkflowId::new(), "https://example.test/good");
    good.session_id = session.clone();
    store.save(&good).await.unwrap();

    std::fs::write(
        root.path().join(format!("{}.json", WorkflowId::new().0)),
        b"{ not json",
    )
    .unwrap();

    let listed = store.list_for_session(&session, 32).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].workflow_id, good.workflow_id);
}
