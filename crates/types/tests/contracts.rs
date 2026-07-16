use chrono::{TimeZone, Utc};
use serde_json::json;
use types::{
    AttemptId, ClickCommand, CommandClass, CommandEnvelope, CommandId, InspectCommand, PageId,
    PrimitiveCommand, SessionId, TypeTextCommand, WaitUntil, WorkflowId,
};
use uuid::Uuid;

fn uuid(value: u128) -> Uuid {
    Uuid::from_u128(value)
}

#[test]
fn command_envelope_uses_stable_camel_case_json() {
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId(uuid(1)),
        workflow_id: WorkflowId(uuid(2)),
        attempt_id: AttemptId(uuid(3)),
        session_id: SessionId(uuid(4)),
        page_id: Some(PageId(uuid(5))),
        deadline: Utc.with_ymd_and_hms(2026, 7, 16, 12, 0, 0).unwrap(),
        command: PrimitiveCommand::Navigate(types::NavigateCommand {
            url: "https://example.com".into(),
            wait_until: WaitUntil::Interactive,
            timeout_ms: 30_000,
        }),
    };

    let value = serde_json::to_value(envelope).unwrap();
    assert_eq!(value["schemaVersion"], json!(1));
    assert_eq!(value["commandId"], json!(uuid(1)));
    assert_eq!(value["sessionId"], json!(uuid(4)));
    assert_eq!(value["pageId"], json!(uuid(5)));
    assert_eq!(value["command"]["kind"], json!("navigate"));
    assert_eq!(value["command"]["input"]["waitUntil"], json!("interactive"));
}

#[test]
fn commands_expose_recovery_class() {
    assert_eq!(
        PrimitiveCommand::Inspect(InspectCommand::default()).class(),
        CommandClass::Replayable
    );
    assert_eq!(
        PrimitiveCommand::TypeText(TypeTextCommand {
            selector: "#name".into(),
            value: "Ada".into(),
            clear_first: true,
        })
        .class(),
        CommandClass::Reconciliable
    );
    assert_eq!(
        PrimitiveCommand::Click(ClickCommand {
            selector: "#submit".into(),
            boundary: true,
            expected_url: None,
        })
        .class(),
        CommandClass::Boundary
    );
}
