use std::{fs::OpenOptions, io::Write as _, path::PathBuf};

use agent_client_protocol::{
    schema::v1::{
        AgentCapabilities, CloseSessionRequest, CloseSessionResponse, ContentBlock, ContentChunk,
        InitializeRequest, InitializeResponse, NewSessionRequest, NewSessionResponse,
        PromptCapabilities, PromptRequest, PromptResponse, SessionNotification, SessionUpdate,
        StopReason, TextContent,
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
    let new_log = log.clone();
    let close_log = log;
    Agent
        .builder()
        .on_receive_request(
            async move |request: InitializeRequest, responder, _connection| {
                responder.respond(
                    InitializeResponse::new(request.protocol_version).agent_capabilities(
                        AgentCapabilities::new()
                            .prompt_capabilities(PromptCapabilities::new().image(true)),
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
                let body = serde_json::json!({
                    "confidence": 0.98,
                    "action": { "kind": "click", "x": 11.0, "y": 12.0 },
                    "evidenceDigest": digest,
                })
                .to_string();
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
