use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use chrono::{Duration, Utc};
use interface_core::{canonical_sha256, IdempotencyReservation, IdempotencyStore};
use tokio::sync::Barrier;
use types::{
    AttemptId, CommandError, CommandId, CommandOutcome, CorrelationId, ErrorCode, ErrorLayer,
    IdempotencyKey, InterfaceError, InterfaceErrorCode, InterfaceOperation, PrincipalId,
};

fn principal(value: &str) -> PrincipalId {
    PrincipalId::from_uuid(uuid::Uuid::parse_str(value).unwrap())
}

fn key(value: &str) -> IdempotencyKey {
    IdempotencyKey::try_from(value).unwrap()
}

#[tokio::test]
async fn legacy_snapshot_whitespace_and_missing_newline_remain_supported() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idempotency.json");
    let bytes = b"{\n  \"schemaVersion\": 1,\n  \"entries\": []\n}";
    std::fs::write(&path, bytes).unwrap();
    let health = interface_core::inspect_idempotency_ledger(&path)
        .await
        .unwrap();
    assert_eq!(health.format, Some(1));
    assert!(health.integrity_issue.is_none());
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn v2_torn_header_remains_unreadable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idempotency.json");
    let snapshot = serde_json::json!({"schemaVersion":2,"entries":[]});
    let bytes = serde_json::to_vec(&serde_json::json!({"schemaVersion":2,"entries":[],"sha256":canonical_sha256(&snapshot).unwrap()})).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    assert_eq!(
        interface_core::inspect_idempotency_ledger(&path)
            .await
            .unwrap()
            .integrity_issue,
        Some("unreadableLedger")
    );
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 1);
}

#[tokio::test]
async fn invalid_v2_sequence_never_publishes_prefix() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idempotency.json");
    let owner = principal("10000000-0000-0000-0000-000000000001");
    let store = IdempotencyStore::open_durable(&path, |_| async { Ok(None) })
        .await
        .unwrap();
    let acquired = reserve(
        &store,
        owner.clone(),
        key("pending"),
        [1; 32],
        CorrelationId::new(),
    )
    .await
    .unwrap();
    assert!(matches!(acquired, IdempotencyReservation::Acquired(_)));
    drop(acquired);
    drop(store);
    let original = std::fs::read(&path).unwrap();
    let lines: Vec<_> = original.split_inclusive(|b| *b == b'\n').collect();
    for damage in ["sequence", "previousSha256", "sha256"] {
        let mut record: serde_json::Value = serde_json::from_slice(lines.last().unwrap()).unwrap();
        match damage {
            "sequence" => record["change"]["sequence"] = 9.into(),
            "previousSha256" => {
                record["change"]["previousSha256"] = serde_json::to_value([0_u8; 32]).unwrap()
            }
            _ => record["sha256"] = serde_json::to_value([0_u8; 32]).unwrap(),
        }
        if damage != "sha256" {
            record["sha256"] =
                serde_json::to_value(canonical_sha256(&record["change"]).unwrap()).unwrap();
        }
        let mut bytes = original[..original.len() - lines.last().unwrap().len()].to_vec();
        serde_json::to_writer(&mut bytes, &record).unwrap();
        bytes.push(b'\n');
        std::fs::write(&path, &bytes).unwrap();
        let reopened = IdempotencyStore::open_durable(&path, |_| async { Ok(None) })
            .await
            .unwrap();
        assert_eq!(reopened.integrity_issue(), Some("unreadableLedger"));
        assert!(
            reserve(
                &reopened,
                owner.clone(),
                key("fresh"),
                [2; 32],
                CorrelationId::new()
            )
            .await
            .unwrap_err()
            .reconciliation_required
        );
        drop(reopened);
        assert!(interface_core::downgrade_idempotency_ledger(&path)
            .await
            .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[tokio::test]
async fn ledger_inspection_preserves_file_and_lock_inventory() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idempotency.json");
    assert!(
        !interface_core::inspect_idempotency_ledger(&path)
            .await
            .unwrap()
            .exists
    );
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    std::fs::write(&path, b"").unwrap();
    std::fs::write(path.with_extension("lock"), b"").unwrap();
    assert_eq!(
        interface_core::inspect_idempotency_ledger(&path)
            .await
            .unwrap()
            .integrity_issue,
        Some("unreadableLedger")
    );
    assert!(std::fs::read(&path).unwrap().is_empty());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 2);
}

fn completed(command_id: CommandId) -> CommandOutcome {
    CommandOutcome::Completed {
        command_id,
        evidence: Vec::new(),
    }
}

fn command_error(retryable: bool) -> CommandError {
    CommandError {
        code: ErrorCode::Internal,
        message: "test failure".into(),
        layer: ErrorLayer::Page,
        retryable,
    }
}

fn reconciliation(command_id: CommandId) -> CommandOutcome {
    CommandOutcome::NeedsReconciliation {
        command_id,
        error: command_error(false),
        evidence: Vec::new(),
    }
}

#[tokio::test]
async fn durable_mutations_append_without_rewriting_unrelated_entries() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idempotency.json");
    let entries: Vec<_> = (0..512)
        .map(|index| {
            serde_json::json!({
                "principalId": principal(if index < 256 {
                    "10000000-0000-0000-0000-000000000001"
                } else { "10000000-0000-0000-0000-000000000002" }),
                "key": format!("old-{index}"), "operation": InterfaceOperation::SubmitCommand,
                "canonicalSha256": vec![0; 32], "expiresAt": Utc::now() + Duration::hours(1),
                "lastUsed": index + 1, "state": {"kind": "retained", "value": CommandId::new()}
            })
        })
        .collect();
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({"schemaVersion":1, "entries":entries})).unwrap(),
    )
    .unwrap();
    let store = IdempotencyStore::open_durable(&path, |value| async move {
        Ok(Some(completed(
            serde_json::from_value(value).map_err(std::io::Error::other)?,
        )))
    })
    .await
    .unwrap();
    let principal = principal("10000000-0000-0000-0000-000000000003");
    let IdempotencyReservation::Acquired(permit) =
        reserve(&store, principal, key("new"), [0; 32], CorrelationId::new())
            .await
            .unwrap()
    else {
        panic!("new key");
    };
    let reserved = std::fs::read(&path).unwrap();
    let first: serde_json::Value =
        serde_json::from_slice(reserved.split(|b| *b == b'\n').next().unwrap()).unwrap();
    assert_eq!(
        first["schemaVersion"], 2,
        "new writes use the versioned log"
    );
    store
        .finish(permit, completed(CommandId::new()), Utc::now())
        .await
        .unwrap();
    let finished = std::fs::read(&path).unwrap();
    assert!(
        finished.starts_with(&reserved),
        "finish must append, not rewrite unrelated keys"
    );
    assert!(
        finished.len() - reserved.len() < 2048,
        "one small outcome must not write the whole ledger"
    );
}

#[tokio::test]
async fn doctor_downgrade_preserves_current_keys_and_source_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idempotency.json");
    let owner = principal("10000000-0000-0000-0000-000000000001");
    let lookup = |value| async move {
        Ok(Some(completed(
            serde_json::from_value(value).map_err(std::io::Error::other)?,
        )))
    };
    let store = IdempotencyStore::open_durable(&path, lookup).await.unwrap();
    let IdempotencyReservation::Acquired(done) = reserve(
        &store,
        owner.clone(),
        key("done"),
        [1; 32],
        CorrelationId::new(),
    )
    .await
    .unwrap() else {
        panic!("acquire");
    };
    let command_id = CommandId::new();
    store
        .finish(done, completed(command_id.clone()), Utc::now())
        .await
        .unwrap();
    let IdempotencyReservation::Acquired(pending) = reserve(
        &store,
        owner.clone(),
        key("pending"),
        [2; 32],
        CorrelationId::new(),
    )
    .await
    .unwrap() else {
        panic!("acquire");
    };
    let original = std::fs::read(&path).unwrap();
    assert!(
        interface_core::downgrade_idempotency_ledger(&path)
            .await
            .is_err(),
        "running owner must prevent conversion"
    );
    assert_eq!(std::fs::read(&path).unwrap(), original);
    drop(pending);
    drop(store);
    let backup = interface_core::downgrade_idempotency_ledger(&path)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(std::fs::read(&backup).unwrap(), original);
    let legacy: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(legacy["schemaVersion"], 1);
    assert_eq!(legacy["entries"].as_array().unwrap().len(), 2);
    assert!(legacy["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["key"] == "pending"
            && e["state"]["kind"] == "unresolved"
            && e["expiresAt"].is_null()));
    assert!(
        interface_core::downgrade_idempotency_ledger(&path)
            .await
            .unwrap()
            .is_none(),
        "already legacy is idempotent"
    );
    let reopened = IdempotencyStore::open_durable(&path, lookup).await.unwrap();
    assert!(
        matches!(reserve(&reopened, owner.clone(), key("done"), [1;32], CorrelationId::new()).await.unwrap(), IdempotencyReservation::Replay(CommandOutcome::Completed {command_id:id,..}) if id == command_id)
    );
    assert!(
        reserve(
            &reopened,
            owner,
            key("pending"),
            [2; 32],
            CorrelationId::new()
        )
        .await
        .unwrap_err()
        .reconciliation_required
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&backup).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[tokio::test]
async fn log_damage_is_preserved_and_never_downgraded_or_replayed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idempotency.json");
    let owner = principal("10000000-0000-0000-0000-000000000001");
    let lookup = |value| async move {
        Ok(Some(completed(
            serde_json::from_value(value).map_err(std::io::Error::other)?,
        )))
    };
    let store = IdempotencyStore::open_durable(&path, lookup).await.unwrap();
    let IdempotencyReservation::Acquired(permit) = reserve(
        &store,
        owner.clone(),
        key("done"),
        [1; 32],
        CorrelationId::new(),
    )
    .await
    .unwrap() else {
        panic!("acquire");
    };
    store
        .finish(permit, completed(CommandId::new()), Utc::now())
        .await
        .unwrap();
    drop(store);
    let original = std::fs::read(&path).unwrap();
    let mut checksum_damage = original.clone();
    let at = checksum_damage
        .windows(4)
        .position(|part| part == b"done")
        .unwrap();
    checksum_damage[at] = b'x';
    let lines: Vec<_> = original.split_inclusive(|b| *b == b'\n').collect();
    let mut missing_checkpoint = original.clone();
    missing_checkpoint.drain(..lines[0].len());
    let mut duplicate = original.clone();
    duplicate.extend_from_slice(lines.last().unwrap());
    for bytes in [
        original[..original.len() - 2].to_vec(),
        checksum_damage,
        missing_checkpoint,
        duplicate,
    ] {
        std::fs::write(&path, &bytes).unwrap();
        let health = interface_core::inspect_idempotency_ledger(&path)
            .await
            .unwrap();
        assert!(health.integrity_issue.is_some());
        let reopened = IdempotencyStore::open_durable(&path, lookup).await.unwrap();
        assert!(
            reserve(
                &reopened,
                owner.clone(),
                key("done"),
                [1; 32],
                CorrelationId::new()
            )
            .await
            .unwrap_err()
            .reconciliation_required
        );
        drop(reopened);
        assert!(interface_core::downgrade_idempotency_ledger(&path)
            .await
            .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[tokio::test]
async fn unresolved_restoration_never_expires_or_admits_a_retry() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idempotency.json");
    let owner = principal("10000000-0000-0000-0000-000000000001");
    let original = serde_json::json!({"schemaVersion":1,"entries":[{
        "principalId":owner, "key":"lost-authority", "operation":InterfaceOperation::SubmitCommand,
        "canonicalSha256":vec![0;32], "expiresAt":Utc::now()+Duration::minutes(1),
        "lastUsed":1, "state":{"kind":"retained","value":CommandId::new()}
    }]});
    std::fs::write(&path, serde_json::to_vec(&original).unwrap()).unwrap();
    let store = IdempotencyStore::open_durable(&path, |_| async {
        Ok::<Option<CommandOutcome>, std::io::Error>(None)
    })
    .await
    .unwrap();
    let durable: serde_json::Value = serde_json::from_slice(
        std::fs::read(&path)
            .unwrap()
            .split(|b| *b == b'\n')
            .next()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(durable["entries"][0]["state"]["kind"], "unresolved");
    assert!(
        durable["entries"][0]["expiresAt"].is_null(),
        "unknown authority must be durably protected before open returns"
    );
    let error = store
        .reserve(
            owner,
            key("lost-authority"),
            InterfaceOperation::SubmitCommand,
            [0; 32],
            Utc::now() + Duration::hours(1),
            Utc::now() + Duration::hours(2),
            CorrelationId::new(),
        )
        .await
        .unwrap_err();
    assert!(
        error.reconciliation_required,
        "missing outcome authority must not age into a fresh reservation"
    );
}

#[tokio::test]
async fn ledger_inspection_and_missing_downgrade_are_read_only() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("missing.json");
    assert!(
        !interface_core::inspect_idempotency_ledger(&path)
            .await
            .unwrap()
            .exists
    );
    assert!(interface_core::downgrade_idempotency_ledger(&path)
        .await
        .unwrap()
        .is_none());
    assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
}

#[tokio::test]
async fn online_compaction_keeps_uncertain_keys_and_the_published_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idempotency.json");
    let owner = principal("10000000-0000-0000-0000-000000000001");
    let lookup = |value| async move {
        Ok(Some(completed(
            serde_json::from_value(value).map_err(std::io::Error::other)?,
        )))
    };
    let store = IdempotencyStore::open_durable(&path, lookup).await.unwrap();
    let IdempotencyReservation::Acquired(protected) = reserve(
        &store,
        owner.clone(),
        key("protected"),
        [1; 32],
        CorrelationId::new(),
    )
    .await
    .unwrap() else {
        panic!("acquire");
    };
    for _ in 0..2050 {
        let IdempotencyReservation::Acquired(permit) = reserve(
            &store,
            owner.clone(),
            key("temporary"),
            [2; 32],
            CorrelationId::new(),
        )
        .await
        .unwrap() else {
            panic!("release was not durable");
        };
        store.abandon(permit).await;
    }
    let bytes = std::fs::read(&path).unwrap();
    assert!(
        bytes.iter().filter(|b| **b == b'\n').count() < 20,
        "the online journal should compact"
    );
    drop(protected);
    let IdempotencyReservation::Acquired(permit) = reserve(
        &store,
        owner.clone(),
        key("after-compaction"),
        [3; 32],
        CorrelationId::new(),
    )
    .await
    .unwrap() else {
        panic!("acquire");
    };
    let id = CommandId::new();
    store
        .finish(permit, completed(id.clone()), Utc::now())
        .await
        .unwrap();
    drop(store);
    let reopened = IdempotencyStore::open_durable(&path, lookup).await.unwrap();
    assert!(
        reserve(
            &reopened,
            owner.clone(),
            key("protected"),
            [1; 32],
            CorrelationId::new()
        )
        .await
        .unwrap_err()
        .reconciliation_required
    );
    assert!(
        matches!(reserve(&reopened, owner, key("after-compaction"), [3;32], CorrelationId::new()).await.unwrap(), IdempotencyReservation::Replay(CommandOutcome::Completed {command_id,..}) if command_id == id)
    );
}

#[tokio::test]
async fn unreadable_durable_ledger_preserves_bytes_and_refuses_new_reservations() {
    for bytes in [
        b"not json".as_slice(),
        br#"{"schemaVersion":99,"entries":[]}"#,
    ] {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("idempotency.json");
        std::fs::write(&path, bytes).unwrap();
        for _ in 0..2 {
            let store = IdempotencyStore::open_durable(&path, |_| async {
                Ok::<Option<CommandOutcome>, std::io::Error>(None)
            })
            .await
            .unwrap();
            let error = reserve(
                &store,
                principal("10000000-0000-0000-0000-000000000001"),
                key("uncertain"),
                canonical_sha256(&"effect").unwrap(),
                CorrelationId::new(),
            )
            .await
            .expect_err("unreadable history must not admit a new effect");
            assert!(error.reconciliation_required);
            assert!(!error.retryable);
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
        }
    }
}

#[tokio::test]
async fn duplicate_or_oversized_restored_ledger_does_not_drop_safety_keys() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idempotency.json");
    let store = IdempotencyStore::open_durable(&path, |_| async {
        Ok::<Option<CommandOutcome>, std::io::Error>(None)
    })
    .await
    .unwrap();
    let permit = match reserve(
        &store,
        principal("10000000-0000-0000-0000-000000000001"),
        key("uncertain"),
        canonical_sha256(&"effect").unwrap(),
        CorrelationId::new(),
    )
    .await
    .unwrap()
    {
        IdempotencyReservation::Acquired(permit) => permit,
        _ => panic!("new ledger"),
    };
    drop(permit);
    drop(store);
    let original: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    for count in [2, 257] {
        let mut snapshot = original.clone();
        let entry = snapshot["entries"][0].clone();
        snapshot["entries"] = serde_json::Value::Array(
            (0..count)
                .map(|i| {
                    let mut item = entry.clone();
                    if count > 2 {
                        item["key"] = serde_json::json!(format!("key-{i}"));
                    }
                    item
                })
                .collect(),
        );
        let bytes = serde_json::to_vec(&snapshot).unwrap();
        std::fs::write(&path, &bytes).unwrap();
        let reopened = IdempotencyStore::open_durable(&path, |_| async {
            Ok::<Option<CommandOutcome>, std::io::Error>(None)
        })
        .await
        .unwrap();
        let error = reserve(
            &reopened,
            principal("10000000-0000-0000-0000-000000000001"),
            key("not-in-restored-prefix"),
            canonical_sha256(&"effect").unwrap(),
            CorrelationId::new(),
        )
        .await
        .expect_err("invalid restored history must not admit effects");
        assert!(error.reconciliation_required);
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
    }
}

#[tokio::test]
async fn durable_key_replays_after_reopen_without_storing_page_evidence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idempotency.json");
    let lookup = |value: serde_json::Value| async move {
        let id: CommandId = serde_json::from_value(value).map_err(std::io::Error::other)?;
        Ok(Some(completed(id)))
    };
    let principal = principal("10000000-0000-0000-0000-000000000001");
    let idempotency_key = key("durable-completed");
    let digest = canonical_sha256(&"same-command").unwrap();
    let store = IdempotencyStore::open_durable(&path, lookup).await.unwrap();
    let permit = match reserve(
        &store,
        principal.clone(),
        idempotency_key.clone(),
        digest,
        CorrelationId::new(),
    )
    .await
    .unwrap()
    {
        IdempotencyReservation::Acquired(permit) => permit,
        IdempotencyReservation::Replay(_) => panic!("new key replayed"),
    };
    let command_id = CommandId::new();
    store
        .finish(
            permit,
            CommandOutcome::Completed {
                command_id: command_id.clone(),
                evidence: vec![types::Evidence::Inspection {
                    selector: None,
                    url: "https://example.test".into(),
                    title: "Example".into(),
                    text: "page secret".into(),
                    html: None,
                }],
            },
            Utc::now(),
        )
        .await
        .unwrap();
    let bytes = std::fs::read(&path).unwrap();
    assert!(!bytes
        .windows("page secret".len())
        .any(|part| part == b"page secret"));
    drop(store);

    let reopened = IdempotencyStore::open_durable(&path, lookup).await.unwrap();
    assert!(matches!(
        reserve(&reopened, principal.clone(), idempotency_key.clone(), digest, CorrelationId::new()).await.unwrap(),
        IdempotencyReservation::Replay(CommandOutcome::Completed { command_id: id, .. }) if id == command_id
    ));
    assert!(reserve(
        &reopened,
        principal,
        idempotency_key,
        canonical_sha256(&"different").unwrap(),
        CorrelationId::new()
    )
    .await
    .is_err());
}

#[tokio::test]
async fn interrupted_durable_reservation_reopens_as_unresolved() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idempotency.json");
    let lookup = |_value| async { Ok::<Option<CommandOutcome>, std::io::Error>(None) };
    let principal = principal("10000000-0000-0000-0000-000000000001");
    let idempotency_key = key("interrupted-reservation");
    let digest = canonical_sha256(&"same-command").unwrap();
    let store = IdempotencyStore::open_durable(&path, lookup).await.unwrap();
    let permit = match reserve(
        &store,
        principal.clone(),
        idempotency_key.clone(),
        digest,
        CorrelationId::new(),
    )
    .await
    .unwrap()
    {
        IdempotencyReservation::Acquired(permit) => permit,
        IdempotencyReservation::Replay(_) => panic!("new key replayed"),
    };
    drop(permit);
    drop(store);
    let reopened = IdempotencyStore::open_durable(&path, lookup).await.unwrap();
    let error = reserve(
        &reopened,
        principal,
        idempotency_key,
        digest,
        CorrelationId::new(),
    )
    .await
    .unwrap_err();
    assert!(error.reconciliation_required);
}

/// After a restart the agent's session is new, so the same order under the
/// same key arrives with a different digest. The key's outcome is unknown,
/// so the answer must be "reconcile", never a plain conflict that reads as
/// "mint a fresh key and resubmit".
#[tokio::test]
async fn a_different_request_under_an_uncertain_key_must_reconcile_not_conflict() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("idempotency.json");
    let lookup = |_value| async { Ok::<Option<CommandOutcome>, std::io::Error>(None) };
    let principal = principal("10000000-0000-0000-0000-000000000001");
    let idempotency_key = key("order-42");
    let first_session = canonical_sha256(&"place order in session A").unwrap();
    let next_session = canonical_sha256(&"place order in session B").unwrap();
    let store = IdempotencyStore::open_durable(&path, lookup).await.unwrap();
    let IdempotencyReservation::Acquired(permit) = reserve(
        &store,
        principal.clone(),
        idempotency_key.clone(),
        first_session,
        CorrelationId::new(),
    )
    .await
    .unwrap() else {
        panic!("new key replayed");
    };
    // The process dies mid-command: the durable ledger keeps the reservation.
    std::mem::forget(permit);
    drop(store);

    let reopened = IdempotencyStore::open_durable(&path, lookup).await.unwrap();
    let error = reserve(
        &reopened,
        principal.clone(),
        idempotency_key,
        next_session,
        CorrelationId::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(error.code, InterfaceErrorCode::IdempotencyConflict);
    assert!(error.reconciliation_required, "{error:?}");

    let retained = IdempotencyStore::with_global_capacity(4, 8, Duration::minutes(5));
    let retained_key = key("order-43");
    let IdempotencyReservation::Acquired(permit) = reserve(
        &retained,
        principal.clone(),
        retained_key.clone(),
        first_session,
        CorrelationId::new(),
    )
    .await
    .unwrap() else {
        panic!("new key replayed");
    };
    retained
        .finish(permit, reconciliation(CommandId::new()), Utc::now())
        .await
        .unwrap();
    let error = reserve(
        &retained,
        principal.clone(),
        retained_key,
        next_session,
        CorrelationId::new(),
    )
    .await
    .unwrap_err();
    assert!(error.reconciliation_required, "{error:?}");

    let known_key = key("order-44");
    let IdempotencyReservation::Acquired(permit) = reserve(
        &retained,
        principal.clone(),
        known_key.clone(),
        first_session,
        CorrelationId::new(),
    )
    .await
    .unwrap() else {
        panic!("new key replayed");
    };
    retained
        .finish(permit, completed(CommandId::new()), Utc::now())
        .await
        .unwrap();
    let error = reserve(
        &retained,
        principal,
        known_key,
        next_session,
        CorrelationId::new(),
    )
    .await
    .unwrap_err();
    assert!(!error.reconciliation_required, "{error:?}");
}

#[tokio::test]
async fn durable_ledger_is_single_writer_and_ignores_uncommitted_temporary_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.json");
    let lookup = |_value| async { Ok::<Option<CommandOutcome>, std::io::Error>(None) };
    let store = IdempotencyStore::open_durable(&path, lookup).await.unwrap();
    assert!(IdempotencyStore::open_durable(&path, lookup).await.is_err());
    let principal = principal("10000000-0000-0000-0000-000000000001");
    let digest = canonical_sha256(&"same-command").unwrap();
    let permit = match reserve(
        &store,
        principal.clone(),
        key("durable-torn"),
        digest,
        CorrelationId::new(),
    )
    .await
    .unwrap()
    {
        IdempotencyReservation::Acquired(permit) => permit,
        IdempotencyReservation::Replay(_) => panic!("new key replayed"),
    };
    drop(permit);
    drop(store);
    std::fs::write(dir.path().join("ledger.abandoned.tmp"), b"{broken").unwrap();
    let reopened = IdempotencyStore::open_durable(&path, lookup).await.unwrap();
    assert!(
        reserve(
            &reopened,
            principal,
            key("durable-torn"),
            digest,
            CorrelationId::new()
        )
        .await
        .unwrap_err()
        .reconciliation_required
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
}

#[cfg(unix)]
#[tokio::test]
async fn dropped_durable_ledger_unlocks_while_a_forked_child_holds_the_descriptor() {
    use std::io::{Read, Write};
    use std::os::unix::process::CommandExt;

    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.json");
    let lookup = |_value| async { Ok::<Option<CommandOutcome>, std::io::Error>(None) };
    let store = IdempotencyStore::open_durable(&path, lookup).await.unwrap();
    let clone = store.clone();
    drop(store);
    assert!(IdempotencyStore::open_durable(&path, lookup).await.is_err());

    let (mut forked_reader, mut forked_writer) = std::io::pipe().unwrap();
    let (mut release_reader, mut release_writer) = std::io::pipe().unwrap();
    let mut command = std::process::Command::new("true");
    // SAFETY: pre_exec uses only read and write syscalls on owned pipes.
    unsafe {
        command.pre_exec(move || {
            forked_writer.write_all(&[1])?;
            release_reader.read_exact(&mut [0])?;
            Ok(())
        });
    }
    let spawner = std::thread::spawn(move || command.status());
    forked_reader.read_exact(&mut [0]).unwrap();

    drop(clone);
    let reopened = IdempotencyStore::open_durable(&path, lookup).await;

    release_writer.write_all(&[1]).unwrap();
    assert!(spawner.join().unwrap().unwrap().success());
    assert!(reopened.is_ok(), "a dropped ledger must release its lock");
}

#[tokio::test]
async fn completed_durable_key_expires_but_unresolved_key_does_not() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ledger.json");
    let lookup = |_value| async { Ok::<Option<CommandOutcome>, std::io::Error>(None) };
    let store = IdempotencyStore::open_durable(&path, lookup).await.unwrap();
    let principal = principal("10000000-0000-0000-0000-000000000001");
    let digest = canonical_sha256(&"same-command").unwrap();
    let completed_permit = match reserve(
        &store,
        principal.clone(),
        key("expired"),
        digest,
        CorrelationId::new(),
    )
    .await
    .unwrap()
    {
        IdempotencyReservation::Acquired(permit) => permit,
        IdempotencyReservation::Replay(_) => panic!("new key replayed"),
    };
    store
        .finish(
            completed_permit,
            completed(CommandId::new()),
            Utc::now() - Duration::minutes(16),
        )
        .await
        .unwrap();
    let unresolved_permit = match reserve(
        &store,
        principal.clone(),
        key("unresolved"),
        digest,
        CorrelationId::new(),
    )
    .await
    .unwrap()
    {
        IdempotencyReservation::Acquired(permit) => permit,
        IdempotencyReservation::Replay(_) => panic!("new key replayed"),
    };
    drop(unresolved_permit);
    drop(store);
    let reopened = IdempotencyStore::open_durable(&path, lookup).await.unwrap();
    assert!(matches!(
        reserve(
            &reopened,
            principal.clone(),
            key("expired"),
            digest,
            CorrelationId::new()
        )
        .await
        .unwrap(),
        IdempotencyReservation::Acquired(_)
    ));
    assert!(
        reserve(
            &reopened,
            principal,
            key("unresolved"),
            digest,
            CorrelationId::new()
        )
        .await
        .unwrap_err()
        .reconciliation_required
    );
}

async fn reserve(
    store: &IdempotencyStore,
    principal: PrincipalId,
    key: IdempotencyKey,
    digest: [u8; 32],
    correlation_id: CorrelationId,
) -> Result<IdempotencyReservation, InterfaceError> {
    let now = Utc::now();
    store
        .reserve(
            principal,
            key,
            InterfaceOperation::SubmitCommand,
            digest,
            now,
            now + Duration::seconds(5),
            correlation_id,
        )
        .await
}

struct DispatchCase {
    store: IdempotencyStore,
    principal: PrincipalId,
    key: IdempotencyKey,
    digest: [u8; 32],
    correlation_id: CorrelationId,
}

async fn counted_dispatch(
    case: DispatchCase,
    calls: Arc<AtomicUsize>,
    barrier: Arc<Barrier>,
    outcome: CommandOutcome,
) -> Result<CommandOutcome, InterfaceError> {
    barrier.wait().await;
    match reserve(
        &case.store,
        case.principal,
        case.key,
        case.digest,
        case.correlation_id,
    )
    .await?
    {
        IdempotencyReservation::Replay(outcome) => Ok(outcome),
        IdempotencyReservation::Acquired(permit) => {
            calls.fetch_add(1, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            case.store
                .finish(permit, outcome.clone(), Utc::now())
                .await?;
            Ok(outcome)
        }
    }
}

#[tokio::test]
async fn committed_success_replays_and_conflicts_preserve_request_correlation() {
    let store = IdempotencyStore::with_global_capacity(8, 16, Duration::minutes(5));
    let principal = principal("10000000-0000-0000-0000-000000000001");
    let key = key("same-request");
    let digest = canonical_sha256(&serde_json::json!({"value": 1})).unwrap();
    let command_id = CommandId::new();
    let permit = match reserve(
        &store,
        principal.clone(),
        key.clone(),
        digest,
        CorrelationId::new(),
    )
    .await
    .unwrap()
    {
        IdempotencyReservation::Acquired(permit) => permit,
        IdempotencyReservation::Replay(_) => panic!("first request must reserve"),
    };
    store
        .finish(permit, completed(command_id.clone()), Utc::now())
        .await
        .unwrap();

    assert!(matches!(
        reserve(
            &store,
            principal.clone(),
            key.clone(),
            digest,
            CorrelationId::new(),
        )
        .await
        .unwrap(),
        IdempotencyReservation::Replay(CommandOutcome::Completed { command_id: actual, .. })
            if actual == command_id
    ));

    let correlation_id = CorrelationId::new();
    let mismatch = reserve(
        &store,
        principal,
        key,
        canonical_sha256(&serde_json::json!({"value": 2})).unwrap(),
        correlation_id.clone(),
    )
    .await
    .unwrap_err();
    assert_eq!(mismatch.code, InterfaceErrorCode::IdempotencyConflict);
    assert_eq!(mismatch.correlation_id, correlation_id);
}

#[tokio::test]
async fn uncertain_outcome_is_a_tombstone_while_explicitly_retryable_outcome_releases() {
    let store = IdempotencyStore::with_global_capacity(4, 8, Duration::minutes(5));
    let principal = principal("10000000-0000-0000-0000-000000000001");
    let uncertain_key = key("uncertain");
    let uncertain_digest = canonical_sha256(&"uncertain").unwrap();
    let command_id = CommandId::new();
    let permit = match reserve(
        &store,
        principal.clone(),
        uncertain_key.clone(),
        uncertain_digest,
        CorrelationId::new(),
    )
    .await
    .unwrap()
    {
        IdempotencyReservation::Acquired(permit) => permit,
        IdempotencyReservation::Replay(_) => unreachable!(),
    };
    store
        .finish(permit, reconciliation(command_id.clone()), Utc::now())
        .await
        .unwrap();
    assert!(matches!(
        reserve(
            &store,
            principal.clone(),
            uncertain_key,
            uncertain_digest,
            CorrelationId::new(),
        )
        .await
        .unwrap(),
        IdempotencyReservation::Replay(CommandOutcome::NeedsReconciliation {
            command_id: actual,
            ..
        }) if actual == command_id
    ));

    let retryable_key = key("retryable");
    let retryable_digest = canonical_sha256(&"retryable").unwrap();
    let permit = match reserve(
        &store,
        principal.clone(),
        retryable_key.clone(),
        retryable_digest,
        CorrelationId::new(),
    )
    .await
    .unwrap()
    {
        IdempotencyReservation::Acquired(permit) => permit,
        IdempotencyReservation::Replay(_) => unreachable!(),
    };
    store
        .finish(
            permit,
            CommandOutcome::RetryableFailure {
                command_id: CommandId::new(),
                error: command_error(true),
            },
            Utc::now(),
        )
        .await
        .unwrap();
    assert!(matches!(
        reserve(
            &store,
            principal,
            retryable_key,
            retryable_digest,
            CorrelationId::new(),
        )
        .await
        .unwrap(),
        IdempotencyReservation::Acquired(_)
    ));
}

#[tokio::test]
async fn same_key_and_digest_concurrent_callers_dispatch_once() {
    let store = IdempotencyStore::with_global_capacity(4, 8, Duration::minutes(5));
    let principal = principal("10000000-0000-0000-0000-000000000001");
    let key = key("concurrent-same");
    let digest = canonical_sha256(&"same").unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));
    let command_id = CommandId::new();
    let first = tokio::spawn(counted_dispatch(
        DispatchCase {
            store: store.clone(),
            principal: principal.clone(),
            key: key.clone(),
            digest,
            correlation_id: CorrelationId::new(),
        },
        calls.clone(),
        barrier.clone(),
        completed(command_id.clone()),
    ));
    let second = tokio::spawn(counted_dispatch(
        DispatchCase {
            store,
            principal,
            key,
            digest,
            correlation_id: CorrelationId::new(),
        },
        calls.clone(),
        barrier,
        completed(command_id),
    ));
    first.await.unwrap().unwrap();
    second.await.unwrap().unwrap();
    assert_eq!(calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn different_digest_concurrent_callers_conflict_before_second_dispatch() {
    let store = IdempotencyStore::with_global_capacity(4, 8, Duration::minutes(5));
    let principal = principal("10000000-0000-0000-0000-000000000001");
    let key = key("concurrent-conflict");
    let calls = Arc::new(AtomicUsize::new(0));
    let barrier = Arc::new(Barrier::new(2));
    let first = tokio::spawn(counted_dispatch(
        DispatchCase {
            store: store.clone(),
            principal: principal.clone(),
            key: key.clone(),
            digest: canonical_sha256(&"first").unwrap(),
            correlation_id: CorrelationId::new(),
        },
        calls.clone(),
        barrier.clone(),
        completed(CommandId::new()),
    ));
    let second = tokio::spawn(counted_dispatch(
        DispatchCase {
            store,
            principal,
            key,
            digest: canonical_sha256(&"second").unwrap(),
            correlation_id: CorrelationId::new(),
        },
        calls.clone(),
        barrier,
        completed(CommandId::new()),
    ));
    let results = [first.await.unwrap(), second.await.unwrap()];
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(error) if error.code == InterfaceErrorCode::IdempotencyConflict))
            .count(),
        1
    );
}

#[tokio::test]
async fn global_bound_refuses_safety_relevant_entries_even_after_ttl() {
    let store = IdempotencyStore::with_global_capacity(2, 2, Duration::milliseconds(20));
    let first_principal = principal("10000000-0000-0000-0000-000000000001");
    let second_principal = principal("20000000-0000-0000-0000-000000000002");
    for (principal, key_value) in [(first_principal, "first"), (second_principal, "second")] {
        let digest = canonical_sha256(&key_value).unwrap();
        let permit = match reserve(
            &store,
            principal,
            key(key_value),
            digest,
            CorrelationId::new(),
        )
        .await
        .unwrap()
        {
            IdempotencyReservation::Acquired(permit) => permit,
            IdempotencyReservation::Replay(_) => unreachable!(),
        };
        store
            .finish(permit, reconciliation(CommandId::new()), Utc::now())
            .await
            .unwrap();
    }

    let correlation_id = CorrelationId::new();
    let full = reserve(
        &store,
        principal("30000000-0000-0000-0000-000000000003"),
        key("third"),
        canonical_sha256(&"third").unwrap(),
        correlation_id.clone(),
    )
    .await
    .unwrap_err();
    assert_eq!(full.code, InterfaceErrorCode::ResourceExhausted);
    assert_eq!(full.correlation_id, correlation_id);

    tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    let still_full = reserve(
        &store,
        principal("30000000-0000-0000-0000-000000000003"),
        key("third"),
        canonical_sha256(&"third").unwrap(),
        CorrelationId::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(still_full.code, InterfaceErrorCode::ResourceExhausted);
}

#[tokio::test]
async fn safety_tombstone_survives_time_advance_until_explicit_resolution() {
    let store = IdempotencyStore::with_global_capacity(1, 1, Duration::milliseconds(1));
    let principal = principal("10000000-0000-0000-0000-000000000001");
    let idempotency_key = key("durable-uncertain");
    let digest = canonical_sha256(&"durable-uncertain").unwrap();
    let command_id = CommandId::new();
    let started_at = Utc::now();
    let permit = match store
        .reserve(
            principal.clone(),
            idempotency_key.clone(),
            InterfaceOperation::SubmitCommand,
            digest,
            started_at,
            started_at + Duration::hours(2),
            CorrelationId::new(),
        )
        .await
        .unwrap()
    {
        IdempotencyReservation::Acquired(permit) => permit,
        IdempotencyReservation::Replay(_) => unreachable!(),
    };
    store
        .finish(permit, reconciliation(command_id.clone()), started_at)
        .await
        .unwrap();

    let much_later = started_at + Duration::hours(1);
    assert!(matches!(
        store
            .reserve(
                principal.clone(),
                idempotency_key.clone(),
                InterfaceOperation::SubmitCommand,
                digest,
                much_later,
                much_later + Duration::minutes(1),
                CorrelationId::new(),
            )
            .await
            .unwrap(),
        IdempotencyReservation::Replay(CommandOutcome::NeedsReconciliation {
            command_id: actual,
            ..
        }) if actual == command_id
    ));

    let dispatches = Arc::new(AtomicUsize::new(0));
    let replay = counted_dispatch(
        DispatchCase {
            store: store.clone(),
            principal: principal.clone(),
            key: idempotency_key.clone(),
            digest,
            correlation_id: CorrelationId::new(),
        },
        dispatches.clone(),
        Arc::new(Barrier::new(1)),
        completed(CommandId::new()),
    )
    .await
    .unwrap();
    assert!(matches!(
        replay,
        CommandOutcome::NeedsReconciliation {
            command_id: actual,
            ..
        } if actual == command_id
    ));
    assert_eq!(dispatches.load(Ordering::SeqCst), 0);

    let full = store
        .reserve(
            principal.clone(),
            key("blocked-by-safety"),
            InterfaceOperation::SubmitCommand,
            canonical_sha256(&"blocked-by-safety").unwrap(),
            much_later,
            much_later + Duration::minutes(1),
            CorrelationId::new(),
        )
        .await
        .unwrap_err();
    assert_eq!(full.code, InterfaceErrorCode::ResourceExhausted);

    assert!(matches!(
        store
            .reserve(
                principal,
                idempotency_key,
                InterfaceOperation::SubmitCommand,
                digest,
                much_later,
                much_later + Duration::minutes(1),
                CorrelationId::new(),
            )
            .await
            .unwrap(),
        IdempotencyReservation::Replay(CommandOutcome::NeedsReconciliation { .. })
    ));
}

#[tokio::test]
async fn per_principal_bound_refuses_safety_state_without_blocking_another_principal() {
    let store = IdempotencyStore::with_global_capacity(1, 2, Duration::minutes(5));
    let first_principal = principal("10000000-0000-0000-0000-000000000001");
    let digest = canonical_sha256(&"first").unwrap();
    let permit = match reserve(
        &store,
        first_principal.clone(),
        key("first"),
        digest,
        CorrelationId::new(),
    )
    .await
    .unwrap()
    {
        IdempotencyReservation::Acquired(permit) => permit,
        IdempotencyReservation::Replay(_) => unreachable!(),
    };
    store
        .finish(permit, reconciliation(CommandId::new()), Utc::now())
        .await
        .unwrap();

    let same_principal = reserve(
        &store,
        first_principal,
        key("second"),
        canonical_sha256(&"second").unwrap(),
        CorrelationId::new(),
    )
    .await
    .unwrap_err();
    assert_eq!(same_principal.code, InterfaceErrorCode::ResourceExhausted);
    assert!(matches!(
        reserve(
            &store,
            principal("20000000-0000-0000-0000-000000000002"),
            key("other-principal"),
            canonical_sha256(&"other").unwrap(),
            CorrelationId::new(),
        )
        .await
        .unwrap(),
        IdempotencyReservation::Acquired(_)
    ));
}

#[test]
fn retryable_failed_helper_remains_explicit_for_future_outcomes() {
    let outcome = CommandOutcome::Failed {
        command_id: CommandId::new(),
        error: command_error(true),
        evidence: vec![],
    };
    assert!(matches!(outcome, CommandOutcome::Failed { error, .. } if error.retryable));

    let restarted = CommandOutcome::Restarted {
        command_id: CommandId::new(),
        prior_attempt_id: AttemptId::new(),
        attempt_id: AttemptId::new(),
        reason: "restart".into(),
        evidence: vec![],
    };
    assert!(matches!(restarted, CommandOutcome::Restarted { .. }));
}

/// The digest is the identity of an idempotent request, so it must not depend on
/// object key order, nor on which `serde_json` map backend is linked: `BTreeMap`
/// by default (sorted), `IndexMap` under `preserve_order` (insertion order), which
/// any crate in the graph can enable workspace-wide.
#[test]
fn the_digest_ignores_object_key_order() {
    let ascending: serde_json::Value =
        serde_json::from_str(r#"{"alpha":1,"beta":2,"gamma":3}"#).expect("valid JSON");
    let descending: serde_json::Value =
        serde_json::from_str(r#"{"gamma":3,"beta":2,"alpha":1}"#).expect("valid JSON");
    let shuffled: serde_json::Value =
        serde_json::from_str(r#"{"beta":2,"gamma":3,"alpha":1}"#).expect("valid JSON");
    let digest = canonical_sha256(&ascending).expect("canonicalizes");
    assert_eq!(
        digest,
        canonical_sha256(&descending).expect("canonicalizes")
    );
    assert_eq!(digest, canonical_sha256(&shuffled).expect("canonicalizes"));
}

#[test]
fn the_digest_ignores_key_order_at_every_depth() {
    let one: serde_json::Value =
        serde_json::from_str(r#"{"outer":{"a":1,"b":{"x":1,"y":2}},"list":[{"p":1,"q":2}]}"#)
            .expect("valid JSON");
    let other: serde_json::Value =
        serde_json::from_str(r#"{"list":[{"q":2,"p":1}],"outer":{"b":{"y":2,"x":1},"a":1}}"#)
            .expect("valid JSON");
    assert_eq!(
        canonical_sha256(&one).expect("canonicalizes"),
        canonical_sha256(&other).expect("canonicalizes"),
        "nested objects, including those inside arrays, are not canonicalized"
    );
}

/// Array order carries meaning and must survive canonicalization, or two
/// different requests share a digest and one replays the other's result.
#[test]
fn the_digest_respects_array_order() {
    let forward: serde_json::Value = serde_json::from_str(r#"{"steps":[1,2,3]}"#).expect("valid");
    let reversed: serde_json::Value = serde_json::from_str(r#"{"steps":[3,2,1]}"#).expect("valid");
    assert_ne!(
        canonical_sha256(&forward).expect("canonicalizes"),
        canonical_sha256(&reversed).expect("canonicalizes"),
        "array order was normalized away, collapsing two different requests"
    );
}

/// Different values must produce different digests.
#[test]
fn the_digest_still_distinguishes_different_values() {
    let one: serde_json::Value = serde_json::from_str(r#"{"a":1}"#).expect("valid");
    let other: serde_json::Value = serde_json::from_str(r#"{"a":2}"#).expect("valid");
    assert_ne!(
        canonical_sha256(&one).expect("canonicalizes"),
        canonical_sha256(&other).expect("canonicalizes")
    );
}

#[tokio::test]
async fn a_dropped_permit_releases_its_reservation_instead_of_wedging_the_key() {
    let store = IdempotencyStore::with_global_capacity(4, 8, Duration::minutes(5));
    let principal = principal("10000000-0000-0000-0000-000000000701");
    let key = key("dropped-permit");
    let digest = canonical_sha256(&serde_json::json!({"value": 1})).unwrap();

    let reservation = reserve(
        &store,
        principal.clone(),
        key.clone(),
        digest,
        CorrelationId::new(),
    )
    .await
    .unwrap();
    let IdempotencyReservation::Acquired(permit) = reservation else {
        panic!("first reservation must be acquired");
    };
    // The request task dies here: no finish, no abandon.
    drop(permit);

    // The retry must not park on the wedged reservation: the same key
    // reserves again promptly instead of waiting for a release that never comes.
    let reservation = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        reserve(&store, principal, key, digest, CorrelationId::new()),
    )
    .await
    .expect("dropped permit must release its reservation promptly")
    .unwrap();
    assert!(
        matches!(reservation, IdempotencyReservation::Acquired(_)),
        "key stayed wedged after the permit was dropped"
    );
}
