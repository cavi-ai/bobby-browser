use std::sync::Arc;

use chrono::Utc;
use tokio::io::AsyncWriteExt;
use types::{CommandId, CommandPhase};
use workflow_journal::{CommandJournal, JournalError, JournalRecord, JsonlJournal};

/// A journal line as written before `ea42d97` bumped `CommandEnvelope::SCHEMA_VERSION`
/// to 2: the command is tagged `navigate` instead of `primitive`.
fn v1_navigate_line(sequence: u64, command_id: &CommandId) -> String {
    let id = command_id.0;
    format!(
        r#"{{"sequence":{sequence},"recordedAt":"2026-07-22T16:31:49.071530Z","commandId":"{id}","phase":"accepted","envelope":{{"schemaVersion":1,"commandId":"{id}","workflowId":"d92e7a46-e0b8-4072-a0a2-bf60afec1729","attemptId":"f03ae64b-e1c0-409d-a41f-64ed1834e218","sessionId":"9a81df15-5046-4bc8-9a53-a3059d7a6126","pageId":"440af7d2-4798-4d56-9edc-cd23f66652b5","deadline":"2026-07-22T16:32:49Z","command":{{"kind":"navigate","input":{{"url":"https://example.com/","waitUntil":"interactive","timeoutMs":30000}}}}}},"outcome":null}}"#
    )
}

fn record(command_id: &CommandId, phase: CommandPhase) -> JournalRecord {
    JournalRecord {
        sequence: 0,
        recorded_at: Utc::now(),
        command_id: command_id.clone(),
        phase,
        envelope: None,
        outcome: None,
        prepared_result: None,
    }
}

#[tokio::test]
async fn reopens_committed_history_in_order() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let command_id = CommandId::new();
    let journal = JsonlJournal::open(&path).await.unwrap();
    journal
        .append(record(&command_id, CommandPhase::Accepted))
        .await
        .unwrap();
    journal
        .append(record(&command_id, CommandPhase::Prepared))
        .await
        .unwrap();
    drop(journal);

    let reopened = JsonlJournal::open(&path).await.unwrap();
    let scan = reopened.history(command_id).await.unwrap();
    assert_eq!(scan.records.len(), 2);
    assert_eq!(scan.records[0].sequence, 0);
    assert_eq!(scan.records[1].sequence, 1);
    assert!(!scan.torn_tail);
}

#[tokio::test]
async fn ignores_and_reports_a_torn_final_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let command_id = CommandId::new();
    let journal = JsonlJournal::open(&path).await.unwrap();
    journal
        .append(record(&command_id, CommandPhase::Accepted))
        .await
        .unwrap();
    drop(journal);

    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .await
        .unwrap();
    file.write_all(br#"{"sequence":1,"recordedAt":"#)
        .await
        .unwrap();
    file.flush().await.unwrap();
    drop(file);

    let reopened = JsonlJournal::open(&path).await.unwrap();
    let scan = reopened.history(command_id.clone()).await.unwrap();
    assert_eq!(scan.records.len(), 1);
    assert!(scan.torn_tail);

    reopened
        .append(record(&command_id, CommandPhase::Prepared))
        .await
        .unwrap();
    drop(reopened);
    let recovered = JsonlJournal::open(&path).await.unwrap();
    let scan = recovered.history(command_id).await.unwrap();
    assert_eq!(scan.records.len(), 2);
    assert!(!scan.torn_tail);
}

#[tokio::test]
async fn skips_records_written_under_an_older_schema_version() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let command_id = CommandId::new();
    let id = command_id.0;
    let contents = format!(
        "{}\n{}\n",
        v1_navigate_line(0, &command_id),
        format_args!(
            r#"{{"sequence":1,"recordedAt":"2026-07-22T16:31:49.077540Z","commandId":"{id}","phase":"prepared","envelope":null,"outcome":null}}"#
        )
    );
    tokio::fs::write(&path, contents).await.unwrap();

    let journal = JsonlJournal::open(&path).await.unwrap();
    let scan = journal.history(command_id.clone()).await.unwrap();
    assert_eq!(scan.incompatible_records, 1);
    assert_eq!(scan.records.len(), 1);
    assert_eq!(scan.records[0].phase, CommandPhase::Prepared);
    assert!(!scan.torn_tail);

    journal
        .append(record(&command_id, CommandPhase::Executing))
        .await
        .unwrap();
    let scan = journal.history(command_id).await.unwrap();
    assert_eq!(scan.records.len(), 2);
    assert_eq!(scan.records[1].sequence, 2);
}

#[tokio::test]
async fn rejects_a_line_it_cannot_decode_at_the_current_schema_version() {
    let dir = tempfile::tempdir().unwrap();
    let command_id = CommandId::new();
    let id = command_id.0;

    let current_version = dir.path().join("current.jsonl");
    tokio::fs::write(
        &current_version,
        format!(
            r#"{{"sequence":0,"recordedAt":"2026-07-22T16:31:49.071530Z","commandId":"{id}","phase":"accepted","envelope":{{"schemaVersion":2,"commandId":"{id}","workflowId":"d92e7a46-e0b8-4072-a0a2-bf60afec1729","attemptId":"f03ae64b-e1c0-409d-a41f-64ed1834e218","sessionId":"9a81df15-5046-4bc8-9a53-a3059d7a6126","pageId":null,"deadline":"2026-07-22T16:32:49Z","command":{{"kind":"nonsense","input":{{}}}}}},"outcome":null}}
"#
        ),
    )
    .await
    .unwrap();
    let Err(error) = JsonlJournal::open(&current_version).await else {
        panic!("a line at the current schema version must stay fatal");
    };
    assert!(matches!(error, JournalError::Corrupt { line: 1 }));

    let no_version = dir.path().join("no-version.jsonl");
    tokio::fs::write(&no_version, "{\"sequence\":0,\"phase\":\"accepted\"}\n")
        .await
        .unwrap();
    let Err(error) = JsonlJournal::open(&no_version).await else {
        panic!("a line without an envelope schema version must stay fatal");
    };
    assert!(matches!(error, JournalError::Corrupt { line: 1 }));
}

#[tokio::test]
async fn inspect_reports_torn_tail_without_truncating() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let command_id = CommandId::new();
    let journal = JsonlJournal::open(&path).await.unwrap();
    journal
        .append(record(&command_id, CommandPhase::Accepted))
        .await
        .unwrap();
    drop(journal);

    let mut file = tokio::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .await
        .unwrap();
    file.write_all(br#"{"sequence":1,"recordedAt":"#)
        .await
        .unwrap();
    file.flush().await.unwrap();
    drop(file);

    let before = tokio::fs::read(&path).await.unwrap();
    let health = JsonlJournal::inspect(&path).await.unwrap();
    assert!(health.exists);
    assert!(health.torn_tail);
    assert_eq!(health.records, 1);
    assert_eq!(health.corrupt_line, None);
    let after = tokio::fs::read(&path).await.unwrap();
    assert_eq!(before, after);
}

#[tokio::test]
async fn inspect_missing_file_is_empty_health() {
    let dir = tempfile::tempdir().unwrap();
    let health = JsonlJournal::inspect(dir.path().join("nope.jsonl"))
        .await
        .unwrap();
    assert!(!health.exists);
    assert_eq!(health.bytes, 0);
    assert!(!health.torn_tail);
}

#[tokio::test]
async fn serializes_concurrent_appends_with_unique_sequences() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let command_id = CommandId::new();
    let journal = Arc::new(JsonlJournal::open(&path).await.unwrap());

    let mut tasks = Vec::new();
    for _ in 0..32 {
        let journal = journal.clone();
        let command_id = command_id.clone();
        tasks.push(tokio::spawn(async move {
            journal
                .append(record(&command_id, CommandPhase::Accepted))
                .await
                .unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    drop(journal);

    let reopened = JsonlJournal::open(&path).await.unwrap();
    let scan = reopened.history(command_id).await.unwrap();
    let sequences: Vec<_> = scan.records.iter().map(|record| record.sequence).collect();
    assert_eq!(sequences, (0..32).collect::<Vec<_>>());
}
