use std::path::{Path, PathBuf};

use chrono::Utc;
use types::{
    AttemptId, CommandEnvelope, CommandId, CommandPhase, PrimitiveCommand, RuntimeCommand,
    SessionId, TypeTextCommand, WorkflowId,
};
use workflow_journal::{
    CommandJournal, JournalError, JournalKey, JournalOptions, JournalRecord, JournalScan,
    JsonlJournal,
};

fn typed_record(command_id: &CommandId, value: &str) -> JournalRecord {
    JournalRecord {
        sequence: 0,
        recorded_at: Utc::now(),
        command_id: command_id.clone(),
        phase: CommandPhase::Accepted,
        envelope: Some(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: command_id.clone(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: SessionId::new(),
            page_id: None,
            deadline: Utc::now() + chrono::Duration::seconds(30),
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

fn typed_value(record: &JournalRecord) -> String {
    match &record.envelope.as_ref().unwrap().command {
        RuntimeCommand::Primitive(PrimitiveCommand::TypeText(command)) => command.value.clone(),
        other => panic!("unexpected command {other:?}"),
    }
}

fn key() -> JournalKey {
    JournalKey::from_bytes([7; 32])
}

fn options(sealed: bool) -> JournalOptions {
    JournalOptions {
        segment_bytes: 1,
        key: sealed.then(key),
        ..JournalOptions::default()
    }
}

fn segment(path: &Path, number: u32) -> PathBuf {
    PathBuf::from(format!("{}.segment-{number:08}", path.display()))
}

fn index(path: &Path, number: u32) -> PathBuf {
    PathBuf::from(format!("{}.index", segment(path, number).display()))
}

/// Appends one record per command, returning the ids in order.
async fn fill(journal: &JsonlJournal, count: usize) -> Vec<CommandId> {
    let mut ids = Vec::new();
    for position in 0..count {
        let id = CommandId::new();
        journal
            .append(typed_record(&id, &format!("value-{position}")))
            .await
            .unwrap();
        ids.push(id);
    }
    ids
}

fn assert_trusted(scan: &JournalScan, value: &str) {
    assert_eq!(scan.incompatible_records, 0);
    assert_eq!(scan.records.len(), 1);
    assert_eq!(typed_value(&scan.records[0]), value);
}

/// Archived history may still be listed, but only as diagnostic data.
fn assert_untrusted(scan: &JournalScan) {
    assert!(scan.incompatible_records > 0);
}

/// Flips one character of the named JSON string field's value.
fn flip_field(path: &Path, field: &str) {
    let mut bytes = std::fs::read(path).unwrap();
    let marker = format!("\"{field}\":\"");
    let start = bytes
        .windows(marker.len())
        .position(|window| window == marker.as_bytes())
        .unwrap()
        + marker.len()
        + 2;
    bytes[start] = if bytes[start] == b'A' { b'B' } else { b'A' };
    std::fs::write(path, bytes).unwrap();
}

#[tokio::test]
async fn segment_roll_and_reopen_keep_records_readable() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let journal = JsonlJournal::open_with(&path, options(true)).await.unwrap();
    let ids = fill(&journal, 5).await;
    assert!(segment(&path, 1).exists());
    assert!(index(&path, 1).exists());
    drop(journal);

    let reopened = JsonlJournal::open_with(&path, options(true)).await.unwrap();
    let first = reopened.history(ids[0].clone()).await.unwrap();
    assert_trusted(&first, "value-0");
    let last = reopened.history(ids[4].clone()).await.unwrap();
    assert_trusted(&last, "value-4");
    assert!(first.records[0].sequence < last.records[0].sequence);
}

#[tokio::test]
async fn segment_history_of_one_command_spans_files_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let journal = JsonlJournal::open_with(&path, options(true)).await.unwrap();
    let x = CommandId::new();
    journal.append(typed_record(&x, "first")).await.unwrap();
    fill(&journal, 3).await;
    journal.append(typed_record(&x, "second")).await.unwrap();
    drop(journal);

    let reopened = JsonlJournal::open_with(&path, options(true)).await.unwrap();
    let scan = reopened.history(x).await.unwrap();
    assert_eq!(scan.incompatible_records, 0);
    let values: Vec<String> = scan.records.iter().map(typed_value).collect();
    assert_eq!(values, ["first", "second"]);
    assert!(scan.records[0].sequence < scan.records[1].sequence);
}

#[tokio::test]
async fn segment_bodies_are_verified_only_when_read() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let journal = JsonlJournal::open_with(&path, options(true)).await.unwrap();
    let ids = fill(&journal, 5).await;
    drop(journal);
    flip_field(&segment(&path, 2), "ct");

    let reopened = JsonlJournal::open_with(&path, options(true)).await.unwrap();
    assert!(reopened.archives().await.is_empty());
    assert_trusted(&reopened.history(ids[4].clone()).await.unwrap(), "value-4");
    assert_untrusted(&reopened.history(ids[1].clone()).await.unwrap());
    assert!(!reopened.archives().await.is_empty());
}

#[tokio::test]
async fn segment_missing_index_is_rebuilt_from_the_body() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let journal = JsonlJournal::open_with(&path, options(true)).await.unwrap();
    let ids = fill(&journal, 5).await;
    drop(journal);
    std::fs::remove_file(index(&path, 2)).unwrap();

    let reopened = JsonlJournal::open_with(&path, options(true)).await.unwrap();
    assert_trusted(&reopened.history(ids[1].clone()).await.unwrap(), "value-1");
    assert!(index(&path, 2).exists());
    assert!(reopened.archives().await.is_empty());
}

#[tokio::test]
async fn segment_altered_index_is_not_trusted_and_is_rebuilt() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let journal = JsonlJournal::open_with(&path, options(true)).await.unwrap();
    let ids = fill(&journal, 5).await;
    drop(journal);
    let altered = std::fs::read(index(&path, 2)).unwrap();
    flip_field(&index(&path, 2), "ct");
    assert_ne!(std::fs::read(index(&path, 2)).unwrap(), altered);

    let reopened = JsonlJournal::open_with(&path, options(true)).await.unwrap();
    assert_trusted(&reopened.history(ids[1].clone()).await.unwrap(), "value-1");
    assert!(reopened.archives().await.is_empty());
    assert_ne!(std::fs::read(index(&path, 2)).unwrap(), altered);
}

#[tokio::test]
async fn segment_removed_in_the_middle_archives_it_and_everything_after() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let journal = JsonlJournal::open_with(&path, options(true)).await.unwrap();
    let ids = fill(&journal, 5).await;
    drop(journal);
    assert!(segment(&path, 3).exists());
    std::fs::remove_file(segment(&path, 2)).unwrap();
    std::fs::remove_file(index(&path, 2)).unwrap();

    let reopened = JsonlJournal::open_with(&path, options(true)).await.unwrap();
    assert!(!reopened.archives().await.is_empty());
    assert!(!segment(&path, 3).exists());
    assert_trusted(&reopened.history(ids[0].clone()).await.unwrap(), "value-0");
    assert_untrusted(&reopened.history(ids[2].clone()).await.unwrap());
}

#[tokio::test]
async fn segment_roll_also_applies_to_keyless_journals() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let journal = JsonlJournal::open_with(&path, options(false))
        .await
        .unwrap();
    let ids = fill(&journal, 5).await;
    assert!(segment(&path, 1).exists());
    assert!(index(&path, 1).exists());
    drop(journal);

    let reopened = JsonlJournal::open_with(&path, options(false))
        .await
        .unwrap();
    let first = reopened.history(ids[0].clone()).await.unwrap();
    assert_trusted(&first, "value-0");
    let last = reopened.history(ids[4].clone()).await.unwrap();
    assert_trusted(&last, "value-4");
    assert!(first.records[0].sequence < last.records[0].sequence);
}

#[tokio::test]
async fn segment_inspect_counts_closed_files() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let journal = JsonlJournal::open_with(&path, options(true)).await.unwrap();
    fill(&journal, 5).await;
    drop(journal);
    let before: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();

    let health = JsonlJournal::inspect_sealed(&path, &key()).await.unwrap();
    assert!(health.segments > 0);
    assert_eq!(health.incompatible_records, 0);
    assert_eq!(health.records, 5);
    assert!(!health.torn_tail);
    let after: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert_eq!(before.len(), after.len());
}

#[tokio::test]
async fn segment_archived_after_a_removed_one_still_reserves_its_commands() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let journal = JsonlJournal::open_with(&path, options(true)).await.unwrap();
    let ids = fill(&journal, 5).await;
    drop(journal);
    std::fs::remove_file(segment(&path, 2)).unwrap();
    std::fs::remove_file(index(&path, 2)).unwrap();

    let reopened = JsonlJournal::open_with(&path, options(true)).await.unwrap();
    for id in [&ids[2], &ids[4]] {
        let result = reopened.append(typed_record(id, "again")).await;
        assert!(
            matches!(result, Err(JournalError::UncertainCommand)),
            "{result:?}"
        );
    }
}

#[tokio::test]
async fn segment_archived_active_file_still_reserves_its_commands() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let journal = JsonlJournal::open_with(&path, options(true)).await.unwrap();
    let ids = fill(&journal, 2).await;
    drop(journal);
    assert!(segment(&path, 1).exists());
    let mut active = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    std::io::Write::write_all(&mut active, b"garbage without a newline").unwrap();
    drop(active);

    let reopened = JsonlJournal::open_with(&path, options(true)).await.unwrap();
    assert!(!reopened.archives().await.is_empty());
    assert_trusted(&reopened.history(ids[0].clone()).await.unwrap(), "value-0");
    let result = reopened.append(typed_record(&ids[1], "again")).await;
    assert!(
        matches!(result, Err(JournalError::UncertainCommand)),
        "{result:?}"
    );
}
