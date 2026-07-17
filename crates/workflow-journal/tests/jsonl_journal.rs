use std::sync::Arc;

use chrono::Utc;
use tokio::io::AsyncWriteExt;
use types::{CommandId, CommandPhase};
use workflow_journal::{CommandJournal, JournalRecord, JsonlJournal};

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
