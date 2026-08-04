use std::{fs::OpenOptions, io::Write as _, path::PathBuf};

use agent_client_protocol::{
    schema::v1::{
        AgentCapabilities, CloseSessionRequest, CloseSessionResponse, ContentBlock, ContentChunk,
        InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse,
        PermissionOption, PermissionOptionKind, PromptCapabilities, PromptRequest, PromptResponse,
        RequestPermissionRequest, SessionNotification, SessionUpdate, StopReason, TextContent,
        ToolCallUpdate, ToolCallUpdateFields,
    },
    Agent, Stdio,
};

fn record(path: &PathBuf, event: &str) {
    let mut file = OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .expect("open lifecycle log");
    writeln!(file, "{event}").expect("write lifecycle log");
}

#[tokio::main]
async fn main() -> agent_client_protocol::Result<()> {
    let log = PathBuf::from(std::env::args().nth(1).expect("lifecycle log path"));
    let mode = std::env::args().nth(2).unwrap_or_else(|| "success".into());
    let initialize_mode = mode.clone();
    let new_log = log.clone();
    let close_log = log;
    Agent
        .builder()
        .on_receive_request(
            async move |request: InitializeRequest, responder, _connection| {
                responder.respond(
                    InitializeResponse::new(request.protocol_version).agent_capabilities(
                        AgentCapabilities::new().prompt_capabilities(
                            PromptCapabilities::new().image(initialize_mode != "no-image"),
                        ),
                    ),
                )
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: NewSessionRequest, responder, _connection| {
                record(&new_log, "new");
                responder.respond(NewSessionResponse::new("vision-child"))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |request: PromptRequest, responder, connection| {
                if mode == "permission" {
                    let permission = connection.send_request(RequestPermissionRequest::new(
                        request.session_id.clone(),
                        ToolCallUpdate::new(
                            "vision-permission",
                            ToolCallUpdateFields::new().title("Allow vision access?"),
                        ),
                        vec![PermissionOption::new(
                            "allow",
                            "Allow",
                            PermissionOptionKind::AllowOnce,
                        )],
                    ));
                    tokio::spawn(async move {
                        let _ = permission.block_task().await;
                    });
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                }
                let digest = request
                    .prompt
                    .iter()
                    .find_map(|block| match block {
                        ContentBlock::Text(text) => {
                            serde_json::from_str::<serde_json::Value>(&text.text)
                                .ok()
                                .and_then(|value| {
                                    value["evidenceDigest"].as_str().map(str::to_owned)
                                })
                        }
                        _ => None,
                    })
                    .expect("packet evidence digest");
                let body = match mode.as_str() {
                    "malformed" => "not-json".to_owned(),
                    "oversized" => "x".repeat(65 * 1024),
                    "mismatched-evidence" => serde_json::json!({
                        "confidence": 0.98,
                        "action": { "kind": "click", "x": 11.0, "y": 12.0 },
                        "evidenceDigest": "b".repeat(64),
                    })
                    .to_string(),
                    _ => serde_json::json!({
                        "confidence": 0.98,
                        "action": { "kind": "click", "x": 11.0, "y": 12.0 },
                        "evidenceDigest": digest,
                    })
                    .to_string(),
                };
                connection.send_notification(SessionNotification::new(
                    request.session_id,
                    SessionUpdate::AgentMessageChunk(ContentChunk::new(ContentBlock::Text(
                        TextContent::new(body),
                    ))),
                ))?;
                responder.respond(PromptResponse::new(StopReason::EndTurn))
            },
            agent_client_protocol::on_receive_request!(),
        )
        .on_receive_request(
            async move |_request: CloseSessionRequest, responder, _connection| {
                record(&close_log, "close");
                responder.respond(CloseSessionResponse::new())
            },
            agent_client_protocol::on_receive_request!(),
        )
        .connect_to(Stdio::new())
        .await
}
