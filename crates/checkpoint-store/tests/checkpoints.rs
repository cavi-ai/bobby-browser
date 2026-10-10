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
async fn mismatched_workflow_identity_is_rejected_without_hiding_other_checkpoints() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let original = checkpoint(WorkflowId::new(), "https://example.test/original");
    let mut neighbor = checkpoint(WorkflowId::new(), "https://example.test/neighbor");
    neighbor.session_id = original.session_id.clone();
    store.save(&original).await.unwrap();
    store.save(&neighbor).await.unwrap();
    assert_eq!(
        store
            .list_for_session(&original.session_id, 32)
            .await
            .unwrap()
            .len(),
        2
    );
    let path = checkpoint_store::checkpoint_path(root.path(), &original.workflow_id);
    let mut swapped = original.clone();
    swapped.workflow_id = neighbor.workflow_id.clone();
    let damaged = serde_json::to_vec(&swapped).unwrap();
    std::fs::write(&path, &damaged).unwrap();

    for store in [store, CheckpointStore::open(root.path()).await.unwrap()] {
        assert!(matches!(
            store.load(&original.workflow_id).await,
            Err(CheckpointStoreError::IdentityChanged)
        ));
        assert!(matches!(
            store.lock_snapshot(&original.workflow_id).await,
            Err(CheckpointStoreError::IdentityChanged)
        ));
        assert!(matches!(
            store.save(&original).await,
            Err(CheckpointStoreError::IdentityChanged)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), damaged);
        assert_eq!(
            store
                .list_for_session(&original.session_id, 32)
                .await
                .unwrap(),
            vec![neighbor.clone()]
        );
        assert_eq!(store.load(&neighbor.workflow_id).await.unwrap(), neighbor);
        let fresh = checkpoint(WorkflowId::new(), "https://example.test/fresh");
        store.save(&fresh).await.unwrap();
        assert_eq!(store.load(&fresh.workflow_id).await.unwrap(), fresh);
    }

    // Explicit repair restores normal reads and writes; failed operations did
    // not silently overwrite or discard the mismatched evidence.
    std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    assert_eq!(store.load(&original.workflow_id).await.unwrap(), original);
    store.save(&original).await.unwrap();
    assert_eq!(
        store
            .list_for_session(&original.session_id, 32)
            .await
            .unwrap()
            .len(),
        2
    );
}

fn skill_issuance(workflow_id: WorkflowId) -> SkillIssuedDecision {
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
    SkillIssuedDecision::new_for_command(
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
    .unwrap()
}

#[tokio::test]
async fn issued_skill_decision_survives_store_reopen_until_explicitly_cleared() {
    let root = tempfile::tempdir().unwrap();
    let workflow_id = WorkflowId::new();
    let issuance = skill_issuance(workflow_id.clone());

    CheckpointStore::open(root.path())
        .await
        .unwrap()
        .save_skill_issuance(&workflow_id, &issuance)
        .await
        .unwrap();
    #[cfg(unix)]
    let path = root
        .path()
        .join(format!("{}.skill-issuance.json", workflow_id.0));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    let reopened = CheckpointStore::open(root.path()).await.unwrap();
    assert_eq!(
        reopened.load_skill_issuance(&workflow_id).await.unwrap(),
        Some(issuance.clone())
    );
    // A replacement must also secure an existing file from older binaries.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    }
    reopened
        .save_skill_issuance(&workflow_id, &issuance)
        .await
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    assert_eq!(
        reopened.load_skill_issuance(&workflow_id).await.unwrap(),
        Some(issuance)
    );
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 1);
    reopened.remove_skill_issuance(&workflow_id).await.unwrap();
    assert_eq!(
        reopened.load_skill_issuance(&workflow_id).await.unwrap(),
        None
    );
}

#[cfg(unix)]
#[test]
fn issued_skill_decision_is_private_with_permissive_umask() {
    // Isolate the process-wide umask from the parallel test runner.
    let output = std::process::Command::new("sh")
        .args(["-c", "umask 000; exec \"$@\"", "sh"])
        .arg(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "issued_skill_decision_survives_store_reopen_until_explicitly_cleared",
            "--nocapture",
        ])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn foreign_skill_issuance_cannot_create_or_replace_a_workflows_file() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let left = WorkflowId::new();
    let right = WorkflowId::new();
    let original = skill_issuance(left.clone());
    let foreign = skill_issuance(right.clone());
    assert!(matches!(
        store.save_skill_issuance(&left, &foreign).await,
        Err(CheckpointStoreError::IdentityChanged)
    ));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
    store.save_skill_issuance(&left, &original).await.unwrap();
    let path = root.path().join(format!("{}.skill-issuance.json", left.0));
    let bytes = std::fs::read(&path).unwrap();
    assert!(matches!(
        store.save_skill_issuance(&left, &foreign).await,
        Err(CheckpointStoreError::IdentityChanged)
    ));
    assert_eq!(std::fs::read(path).unwrap(), bytes);
    store.save_skill_issuance(&right, &foreign).await.unwrap();
    assert_eq!(
        store.load_skill_issuance(&right).await.unwrap(),
        Some(foreign)
    );
}

#[tokio::test]
async fn mismatched_skill_issuance_is_preserved_without_blocking_healthy_workflows() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let original = skill_issuance(WorkflowId::new());
    let neighbor = skill_issuance(WorkflowId::new());
    let left = original
        .command_identity
        .as_ref()
        .unwrap()
        .workflow_id
        .clone();
    let right = neighbor
        .command_identity
        .as_ref()
        .unwrap()
        .workflow_id
        .clone();
    store.save_skill_issuance(&left, &original).await.unwrap();
    store.save_skill_issuance(&right, &neighbor).await.unwrap();
    let path = root.path().join(format!("{}.skill-issuance.json", left.0));
    let damaged = serde_json::to_vec(&neighbor).unwrap();
    std::fs::write(&path, &damaged).unwrap();

    for store in [store, CheckpointStore::open(root.path()).await.unwrap()] {
        assert!(matches!(
            store.load_skill_issuance(&left).await,
            Err(CheckpointStoreError::IdentityChanged)
        ));
        assert!(matches!(
            store.save_skill_issuance(&left, &original).await,
            Err(CheckpointStoreError::IdentityChanged)
        ));
        assert_eq!(std::fs::read(&path).unwrap(), damaged);
        assert_eq!(
            store.load_skill_issuance(&right).await.unwrap(),
            Some(neighbor.clone())
        );
        let fresh_id = WorkflowId::new();
        let fresh = skill_issuance(fresh_id.clone());
        store.save_skill_issuance(&fresh_id, &fresh).await.unwrap();
        assert_eq!(
            store.load_skill_issuance(&fresh_id).await.unwrap(),
            Some(fresh)
        );
    }
    // Explicit repair restores the normal lifecycle without discarding evidence.
    std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    store.save_skill_issuance(&left, &original).await.unwrap();
    assert_eq!(
        store.load_skill_issuance(&left).await.unwrap(),
        Some(original)
    );
    store.remove_skill_issuance(&left).await.unwrap();
    assert_eq!(store.load_skill_issuance(&left).await.unwrap(), None);
}

#[tokio::test]
async fn unreadable_skill_issuance_is_not_overwritten_by_an_ordinary_save() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let workflow_id = WorkflowId::new();
    let path = root
        .path()
        .join(format!("{}.skill-issuance.json", workflow_id.0));
    let damaged = b"not-json\n";
    std::fs::write(&path, damaged).unwrap();
    assert!(matches!(
        store
            .save_skill_issuance(&workflow_id, &skill_issuance(workflow_id.clone()))
            .await,
        Err(CheckpointStoreError::Serialization(_))
    ));
    assert_eq!(std::fs::read(path).unwrap(), damaged);
}

#[tokio::test]
async fn legacy_skill_issuance_without_command_identity_still_round_trips() {
    let root = tempfile::tempdir().unwrap();
    let workflow_id = WorkflowId::new();
    let mut legacy = skill_issuance(workflow_id.clone());
    legacy.command_identity = None;
    let store = CheckpointStore::open(root.path()).await.unwrap();
    store
        .save_skill_issuance(&workflow_id, &legacy)
        .await
        .unwrap();
    let reopened = CheckpointStore::open(root.path()).await.unwrap();
    assert_eq!(
        reopened.load_skill_issuance(&workflow_id).await.unwrap(),
        Some(legacy)
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

/// An agent that lost its `workflowId` can list its workflows, which
/// `recovery_status` and `workflow_recover` need as their key.
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

#[tokio::test]
async fn cached_session_listing_invalidates_after_save_remove_and_reopen() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let session = SessionId::new();
    let mut first = checkpoint(WorkflowId::new(), "https://example.test/first");
    first.session_id = session.clone();
    store.save(&first).await.unwrap();
    assert_eq!(store.list_for_session(&session, 10).await.unwrap().len(), 1);

    let mut second = checkpoint(WorkflowId::new(), "https://example.test/second");
    second.session_id = session.clone();
    store.save(&second).await.unwrap();
    assert_eq!(store.list_for_session(&session, 10).await.unwrap().len(), 2);

    store.remove(&first.workflow_id).await.unwrap();
    let listed = store.list_for_session(&session, 10).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].workflow_id, second.workflow_id);
    drop(store);

    let reopened = CheckpointStore::open(root.path()).await.unwrap();
    assert_eq!(
        reopened.list_for_session(&session, 10).await.unwrap(),
        listed
    );
}

#[tokio::test]
async fn corrupt_checkpoint_does_not_hide_recoverable_session_entries() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let entry = checkpoint(WorkflowId::new(), "https://example.test/recoverable");
    store.save(&entry).await.unwrap();
    std::fs::write(root.path().join("corrupt.json"), b"not-json").unwrap();
    assert_eq!(
        store.list_for_session(&entry.session_id, 10).await.unwrap(),
        vec![entry]
    );
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

#[tokio::test]
async fn listing_reads_current_files_even_when_directory_timestamp_is_unchanged() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let entry = checkpoint(WorkflowId::new(), "https://example.test/one");
    store.save(&entry).await.unwrap();
    assert_eq!(
        store
            .list_for_session(&entry.session_id, 10)
            .await
            .unwrap()
            .len(),
        1
    );
    tokio::fs::write(
        root.path().join(format!("{}.json", entry.workflow_id.0)),
        b"damaged",
    )
    .await
    .unwrap();
    assert!(store
        .list_for_session(&entry.session_id, 10)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn listing_refreshes_changed_files_before_applying_the_limit() {
    let root = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(root.path()).await.unwrap();
    let session = SessionId::new();
    let mut older = checkpoint(WorkflowId::new(), "https://example.test/older");
    older.session_id = session.clone();
    older.created_at = "2026-01-01T00:00:00Z".parse().unwrap();
    let mut newer = checkpoint(WorkflowId::new(), "https://example.test/newer");
    newer.session_id = session.clone();
    newer.created_at = "2026-01-02T00:00:00Z".parse().unwrap();
    store.save(&older).await.unwrap();
    store.save(&newer).await.unwrap();
    assert_eq!(
        store.list_for_session(&session, 1).await.unwrap()[0].workflow_id,
        newer.workflow_id
    );

    let directory_stamp = std::fs::metadata(root.path()).unwrap().modified().unwrap();
    let path = root.path().join(format!("{}.json", older.workflow_id.0));
    let metadata = std::fs::metadata(&path).unwrap();
    older.created_at = newer.created_at + Duration::hours(1);
    let bytes = serde_json::to_vec(&older).unwrap();
    assert_eq!(bytes.len() as u64, metadata.len());
    tokio::fs::write(&path, &bytes).await.unwrap();
    let file = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
    file.set_times(
        std::fs::FileTimes::new()
            .set_modified(metadata.modified().unwrap() + std::time::Duration::from_secs(1)),
    )
    .unwrap();
    assert_eq!(
        std::fs::metadata(root.path()).unwrap().modified().unwrap(),
        directory_stamp,
        "an in-place write does not invalidate the directory index"
    );
    let listed = store.list_for_session(&session, 1).await.unwrap();
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].workflow_id, older.workflow_id);
    assert_eq!(listed[0].current_url, older.current_url);

    // Size changes also invalidate hints on filesystems with coarse timestamps.
    let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
    older.created_at = newer.created_at - Duration::hours(1);
    older.current_url = "https://example.test/updated-checkpoint-with-a-longer-url".into();
    tokio::fs::write(&path, serde_json::to_vec(&older).unwrap())
        .await
        .unwrap();
    file.set_times(std::fs::FileTimes::new().set_modified(modified))
        .unwrap();
    assert_eq!(
        store.list_for_session(&session, 1).await.unwrap()[0].workflow_id,
        newer.workflow_id
    );
}
