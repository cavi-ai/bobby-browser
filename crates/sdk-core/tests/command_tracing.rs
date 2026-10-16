use chrono::{Duration, Utc};
use observability::test_support::CaptureSink;
use sdk_core::RuntimeService;
use types::{
    AttemptId, CommandEnvelope, CommandId, NavigateCommand, PrimitiveCommand, RuntimeCommand,
    SessionId, WaitUntil, WorkflowId,
};

#[tokio::test]
async fn command_trace_correlates_outcome_without_payloads() {
    let sink = CaptureSink::install();
    let root = tempfile::tempdir().unwrap();
    let mut config = config::AppConfig::default();
    config.storage.journal_path = root.path().join("commands.jsonl");
    config.storage.checkpoints_dir = root.path().join("checkpoints");
    config.browser.artifacts_dir = root.path().join("artifacts");
    let runtime = RuntimeService::build(&config).await.unwrap();
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: SessionId::new(),
        page_id: None,
        deadline: Utc::now() + Duration::seconds(10),
        command: RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
            url: "https://example.com/SECRET_TRACE_CANARY?token=PRIVATE".into(),
            wait_until: WaitUntil::DomContentLoaded,
            timeout_ms: 1000,
        })),
    };
    let outcome = runtime.submit(envelope.clone()).await;
    let events = sink.events();
    let event = events
        .iter()
        .find(|e| e["fields"]["message"] == "command.finished")
        .expect("command completion event");
    let span = &event["span"];
    assert_eq!(span["command_id"], envelope.command_id.0.to_string());
    assert_eq!(span["workflow_id"], envelope.workflow_id.0.to_string());
    assert_eq!(span["attempt_id"], envelope.attempt_id.0.to_string());
    assert_eq!(span["session_id"], envelope.session_id.0.to_string());
    assert_eq!(span["runtime_version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        event["fields"]["outcome"],
        serde_json::to_value(outcome).unwrap()["status"]
    );
    assert!(event["fields"]["elapsed_ms"].is_u64());
    assert!(!serde_json::to_string(&events)
        .unwrap()
        .contains("SECRET_TRACE_CANARY"));
}
