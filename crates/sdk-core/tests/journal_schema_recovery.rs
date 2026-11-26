use sdk_core::RuntimeService;
use types::{CommandId, CommandPhase, RuntimeError, WireTimestamp};
use workflow_journal::{CommandJournal, JournalRecord, JsonlJournal};

/// A journal line as written before `ea42d97` bumped `CommandEnvelope::SCHEMA_VERSION`
/// to 2: the command is tagged `navigate` instead of `primitive`.
const V1_NAVIGATE_LINE: &str = r#"{"sequence":0,"recordedAt":"2026-07-22T16:31:49.071530Z","commandId":"3d3df64c-1ac2-4e0c-83d2-efbce4f9c87e","phase":"accepted","envelope":{"schemaVersion":1,"commandId":"3d3df64c-1ac2-4e0c-83d2-efbce4f9c87e","workflowId":"d92e7a46-e0b8-4072-a0a2-bf60afec1729","attemptId":"f03ae64b-e1c0-409d-a41f-64ed1834e218","sessionId":"9a81df15-5046-4bc8-9a53-a3059d7a6126","pageId":"440af7d2-4798-4d56-9edc-cd23f66652b5","deadline":"2026-07-22T16:32:49Z","command":{"kind":"navigate","input":{"url":"https://example.com/","waitUntil":"interactive","timeoutMs":30000}}},"outcome":null}
"#;

#[tokio::test]
async fn builds_over_a_journal_written_before_the_schema_bump() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config::AppConfig::default();
    config.storage.journal_path = root.path().join("commands.jsonl");
    config.storage.checkpoints_dir = root.path().join("checkpoints");
    config.browser.artifacts_dir = root.path().join("artifacts");
    tokio::fs::write(&config.storage.journal_path, V1_NAVIGATE_LINE)
        .await
        .unwrap();

    let runtime = RuntimeService::build(&config).await.unwrap();

    assert_eq!(runtime.runtime_info().await.active_sessions, 0);
}

#[tokio::test]
async fn building_with_a_journal_named_sessions_preserves_command_history() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config::AppConfig::default();
    config.storage.journal_path = root.path().join("sessions.json");
    config.storage.checkpoints_dir = root.path().join("checkpoints");
    let command_id = CommandId::new();
    let journal = JsonlJournal::open(&config.storage.journal_path)
        .await
        .unwrap();
    journal
        .append(JournalRecord {
            sequence: 0,
            recorded_at: WireTimestamp::now(),
            command_id: command_id.clone(),
            phase: CommandPhase::Accepted,
            envelope: None,
            outcome: None,
            prepared_result: None,
        })
        .await
        .unwrap();
    drop(journal);

    let runtime = RuntimeService::build(&config).await.unwrap();
    runtime.sessions.activate_ledger().await;

    let journal = JsonlJournal::open(&config.storage.journal_path)
        .await
        .unwrap();
    let history = journal.history(command_id).await.unwrap();
    assert_eq!(history.records.len(), 1);
    assert_eq!(history.records[0].phase, CommandPhase::Accepted);
}

#[tokio::test]
async fn a_configured_store_cannot_claim_the_session_ledger_path() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config::AppConfig::default();
    config.storage.journal_path = root.path().join("commands.jsonl");
    config.storage.scheduler_journal_path =
        root.path().join("store/../commands.jsonl.sessions.json");
    config.storage.checkpoints_dir = root.path().join("checkpoints");
    tokio::fs::create_dir(root.path().join("store"))
        .await
        .unwrap();

    let result = RuntimeService::build(&config).await;
    assert!(matches!(result, Err(RuntimeError::InvalidRequest(message))
        if message.contains("conflicts with a configured store")));
    assert!(!config.storage.journal_path.exists());
}

#[cfg(unix)]
#[tokio::test]
async fn a_dangling_journal_symlink_cannot_target_the_session_ledger() {
    let root = tempfile::tempdir().unwrap();
    let mut config = config::AppConfig::default();
    config.storage.journal_path = root.path().join("commands.jsonl");
    let target = "commands.jsonl.sessions.json";
    std::os::unix::fs::symlink(target, &config.storage.journal_path).unwrap();

    let result = RuntimeService::build(&config).await;
    assert!(matches!(result, Err(RuntimeError::InvalidRequest(message))
        if message.contains("conflicts with a configured store")));
    assert!(!root.path().join(target).exists());
}
