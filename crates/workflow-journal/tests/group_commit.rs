use types::{
    AttemptId, CommandEnvelope, CommandId, CommandPhase, PrimitiveCommand, RuntimeCommand,
    SessionId, TypeTextCommand, WorkflowId,
};
use workflow_journal::{
    CommandJournal, JournalKey, JournalOptions, JournalRecord, JournalStats, JsonlJournal,
};

fn typed_record(command_id: &CommandId, value: &str) -> JournalRecord {
    JournalRecord {
        sequence: 0,
        recorded_at: types::WireTimestamp::now(),
        command_id: command_id.clone(),
        phase: CommandPhase::Accepted,
        envelope: Some(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: command_id.clone(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: SessionId::new(),
            page_id: None,
            deadline: types::WireTimestamp::now() + jiff::SignedDuration::from_secs(30),
            command: RuntimeCommand::Primitive(PrimitiveCommand::TypeText(TypeTextCommand {
                selector: "#field".into(),
                target: None,
                value: value.into(),
                clear_first: true,
                expected_url: None,
            })),
        }),
        outcome: None,
        prepared_result: None,
    }
}

fn key() -> JournalKey {
    JournalKey::from_bytes([7; 32])
}

/// Reopens with the key and checks each command has one trusted record and the
/// sequences are exactly `0..ids.len()`.
async fn assert_contiguous(path: &std::path::Path, ids: &[CommandId]) {
    let reopened = JsonlJournal::open_sealed(path, key()).await.unwrap();
    let mut sequences = Vec::new();
    for id in ids {
        let scan = reopened.history(id.clone()).await.unwrap();
        assert_eq!(scan.incompatible_records, 0);
        assert_eq!(scan.records.len(), 1);
        sequences.push(scan.records[0].sequence);
    }
    sequences.sort_unstable();
    let expected: Vec<u64> = (0..ids.len() as u64).collect();
    assert_eq!(sequences, expected);
}

#[tokio::test]
async fn concurrent_appends_share_syncs() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let journal = JsonlJournal::open_sealed(&path, key()).await.unwrap();
    let ids: Vec<CommandId> = (0..64).map(|_| CommandId::new()).collect();
    let results = futures::future::join_all(
        ids.iter()
            .map(|id| journal.append(typed_record(id, "value"))),
    )
    .await;
    assert!(results.iter().all(Result::is_ok));
    let stats = journal.stats();
    assert_eq!(stats.appends, 64);
    assert!(stats.syncs < 64, "syncs {}", stats.syncs);
    drop(journal);
    assert_contiguous(&path, &ids).await;
}

#[tokio::test]
async fn sequential_appends_keep_one_sync_each() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let journal = JsonlJournal::open_sealed(&path, key()).await.unwrap();
    let ids: Vec<CommandId> = (0..3).map(|_| CommandId::new()).collect();
    for id in &ids {
        journal.append(typed_record(id, "value")).await.unwrap();
    }
    assert_eq!(
        journal.stats(),
        JournalStats {
            appends: 3,
            syncs: 3
        }
    );
    for id in &ids {
        let scan = journal.history(id.clone()).await.unwrap();
        assert_eq!(scan.records.len(), 1);
        assert_eq!(&scan.records[0].command_id, id);
    }
}

#[tokio::test]
async fn rejected_record_does_not_fail_its_batch() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let options = || JournalOptions {
        max_record_bytes: 4096,
        key: Some(key()),
        ..JournalOptions::default()
    };
    let journal = JsonlJournal::open_with(&path, options()).await.unwrap();
    let oversized = CommandId::new();
    let normal: Vec<CommandId> = (0..5).map(|_| CommandId::new()).collect();
    let big = "x".repeat(8192);
    let mut appends = vec![journal.append(typed_record(&normal[0], "value"))];
    appends.push(journal.append(typed_record(&normal[1], "value")));
    appends.push(journal.append(typed_record(&oversized, &big)));
    for id in &normal[2..] {
        appends.push(journal.append(typed_record(id, "value")));
    }
    let results = futures::future::join_all(appends).await;
    assert!(results[2].is_err());
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 5);
    drop(journal);

    let reopened = JsonlJournal::open_with(&path, options()).await.unwrap();
    let mut sequences = Vec::new();
    for id in &normal {
        let scan = reopened.history(id.clone()).await.unwrap();
        assert_eq!(scan.incompatible_records, 0);
        assert_eq!(scan.records.len(), 1);
        sequences.push(scan.records[0].sequence);
    }
    assert!(reopened
        .history(oversized)
        .await
        .unwrap()
        .records
        .is_empty());
    sequences.sort_unstable();
    assert_eq!(sequences, vec![0, 1, 2, 3, 4]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn multi_thread_concurrent_appends_stay_consistent() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let journal = JsonlJournal::open_sealed(&path, key()).await.unwrap();
    let ids: Vec<CommandId> = (0..200).map(|_| CommandId::new()).collect();
    let mut tasks = tokio::task::JoinSet::new();
    for id in &ids {
        let journal = journal.clone();
        let record = typed_record(id, "value");
        tasks.spawn(async move { journal.append(record).await });
    }
    while let Some(result) = tasks.join_next().await {
        result.unwrap().unwrap();
    }
    assert_eq!(journal.stats().appends, 200);
    drop(journal);
    assert_contiguous(&path, &ids).await;
}

#[tokio::test]
async fn batches_roll_between_not_within() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let journal = JsonlJournal::open_with(
        &path,
        JournalOptions {
            segment_bytes: 1,
            key: Some(key()),
            ..JournalOptions::default()
        },
    )
    .await
    .unwrap();
    let ids: Vec<CommandId> = (0..8).map(|_| CommandId::new()).collect();
    let results = futures::future::join_all(
        ids.iter()
            .map(|id| journal.append(typed_record(id, "value"))),
    )
    .await;
    assert!(results.iter().all(Result::is_ok));
    drop(journal);

    let reopened = JsonlJournal::open_with(
        &path,
        JournalOptions {
            segment_bytes: 1,
            key: Some(key()),
            ..JournalOptions::default()
        },
    )
    .await
    .unwrap();
    for id in &ids {
        let scan = reopened.history(id.clone()).await.unwrap();
        assert_eq!(scan.incompatible_records, 0);
        assert_eq!(scan.records.len(), 1);
    }
}
