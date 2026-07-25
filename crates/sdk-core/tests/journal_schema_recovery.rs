use sdk_core::RuntimeService;

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
