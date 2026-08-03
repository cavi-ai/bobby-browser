//! Wire-level proof over the real stdio transport: spawn the `acp-gateway`
//! binary with a bootstrap credential and drive `initialize`, `session/new`,
//! and `session/prompt` the way an editor would. No browser is involved —
//! the assertions are about the ACP surface itself (framing, session
//! lifecycle, structured-prompt validation), which must hold before any
//! editor ever connects.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};

fn bootstrap_env() -> Vec<(String, String)> {
    vec![
        (
            "AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN".into(),
            "acp-wire-test-bearer-0123456789abcdef".into(),
        ),
        (
            "AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL".into(),
            uuid::Uuid::nil().to_string(),
        ),
        (
            "AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES".into(),
            "session:read,session:write,page:write,browser:mutate,intent:execute,vision:assist"
                .into(),
        ),
        (
            "AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT".into(),
            (chrono::Utc::now() + chrono::Duration::hours(1)).to_rfc3339(),
        ),
    ]
}

struct Gateway {
    child: Child,
    stdin: ChildStdin,
    stdout: Receiver<String>,
    next_id: u64,
}

impl Gateway {
    fn spawn() -> Self {
        let mut command = Command::new(env!("CARGO_BIN_EXE_acp-gateway"));
        // The binary builds its runtime from AppConfig::default(), whose
        // profile dir is CWD-relative — run it in a scratch dir so the test
        // never writes browser state into the crate.
        let scratch =
            std::env::temp_dir().join(format!("acp-gateway-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&scratch).expect("scratch dir");
        command
            .current_dir(scratch)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (key, value) in bootstrap_env() {
            command.env(key, value);
        }
        let mut child = command.spawn().expect("acp-gateway spawns");
        let stdin = child.stdin.take().expect("stdin piped");
        let child_stdout = child.stdout.take().expect("stdout piped");
        let (stdout_tx, stdout) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(child_stdout).lines() {
                let Ok(line) = line else {
                    break;
                };
                if stdout_tx.send(line).is_err() {
                    break;
                }
            }
        });
        Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
    }

    fn read_frame(&self, method: &str, deadline: std::time::Instant) -> serde_json::Value {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let line = match self.stdout.recv_timeout(remaining) {
            Ok(line) => line,
            Err(RecvTimeoutError::Timeout) => panic!("{method}: gateway did not answer in time"),
            Err(RecvTimeoutError::Disconnected) => panic!("{method}: gateway closed stdout"),
        };
        serde_json::from_str(line.trim()).expect("frame parses")
    }

    fn call(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.next_id += 1;
        let id = self.next_id;
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{}", serde_json::to_string(&frame).unwrap()).unwrap();
        self.stdin.flush().unwrap();
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        loop {
            let frame = self.read_frame(method, deadline);
            if frame.get("id").and_then(serde_json::Value::as_u64) != Some(id) {
                continue;
            }
            return frame;
        }
    }

    fn call_with_permission_then_cancel(
        &mut self,
        method: &str,
        params: serde_json::Value,
        option_id: &str,
    ) -> (
        serde_json::Value,
        Vec<serde_json::Value>,
        Vec<serde_json::Value>,
    ) {
        self.next_id += 1;
        let id = self.next_id;
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{}", serde_json::to_string(&frame).unwrap()).unwrap();
        self.stdin.flush().unwrap();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
        let mut permissions = Vec::new();
        let mut updates = Vec::new();
        loop {
            let diagnostic = format!(
                "{method} after {} permission request(s) and {} update(s)",
                permissions.len(),
                updates.len()
            );
            let frame = self.read_frame(&diagnostic, deadline);

            match frame.get("method").and_then(serde_json::Value::as_str) {
                Some("session/request_permission") => {
                    let request_id = frame.get("id").cloned().expect("permission request has id");
                    permissions.push(frame["params"].clone());
                    let response = serde_json::json!({
                        "jsonrpc": "2.0",
                        "id": request_id,
                        "result": {
                            "outcome": {
                                "outcome": "selected",
                                "optionId": option_id,
                            }
                        }
                    });
                    writeln!(self.stdin, "{}", serde_json::to_string(&response).unwrap()).unwrap();
                    let cancel = serde_json::json!({
                        "jsonrpc": "2.0",
                        "method": "session/cancel",
                        "params": {
                            "sessionId": frame["params"]["sessionId"].clone(),
                        }
                    });
                    writeln!(self.stdin, "{}", serde_json::to_string(&cancel).unwrap()).unwrap();
                    self.stdin.flush().unwrap();
                }
                Some("session/update") => updates.push(frame["params"].clone()),
                _ => {}
            }

            if frame.get("method").is_none()
                && frame.get("id").and_then(serde_json::Value::as_u64) == Some(id)
            {
                return (frame, permissions, updates);
            }
        }
    }

    fn result(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let frame = self.call(method, params);
        assert!(
            frame.get("error").is_none(),
            "{method} returned an error: {frame}"
        );
        frame
            .get("result")
            .cloned()
            .unwrap_or(serde_json::Value::Null)
    }
}

impl Drop for Gateway {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
fn initialize_session_new_and_structured_prompt_flow() {
    let mut gateway = Gateway::spawn();
    let initialized = gateway.result(
        "initialize",
        serde_json::json!({
            "protocolVersion": 1,
            "clientCapabilities": {},
        }),
    );
    assert!(
        initialized.get("agentCapabilities").is_some(),
        "initialize must advertise agent capabilities: {initialized}"
    );

    let session = gateway.result(
        "session/new",
        serde_json::json!({"cwd": "/tmp", "mcpServers": []}),
    );
    let session_id = session
        .get("sessionId")
        .and_then(serde_json::Value::as_str)
        .expect("session/new returns a sessionId")
        .to_string();

    // A freeform prompt is rejected with a JSON-RPC error, not a crash.
    let rejected = gateway.call(
        "session/prompt",
        serde_json::json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "click the submit button"}],
        }),
    );
    assert!(
        rejected.get("error").is_some(),
        "freeform text must be rejected: {rejected}"
    );

    // A structured prompt reaches the runtime. With no page navigated yet the
    // answer is a structured error, still on the wire contract.
    let no_page = gateway.call(
        "session/prompt",
        serde_json::json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": "{\"intent\":{\"kind\":\"locate\",\"input\":{\"purpose\":\"the submit button\"}}}"}],
        }),
    );
    assert!(
        no_page.get("error").is_some(),
        "a prompt without a target page must fail structured: {no_page}"
    );
}

#[test]
fn allow_once_retries_without_publishing_a_reusable_session() {
    let mut gateway = Gateway::spawn();
    gateway.result(
        "initialize",
        serde_json::json!({
            "protocolVersion": 1,
            "clientCapabilities": {},
        }),
    );
    let session = gateway.result(
        "session/new",
        serde_json::json!({"cwd": "/tmp", "mcpServers": []}),
    );
    let session_id = session["sessionId"]
        .as_str()
        .expect("session/new returns a sessionId")
        .to_owned();
    let prompt_text = serde_json::to_string(&serde_json::json!({
        "url": "data:text/html,<html><body></body></html>",
        "intent": {
            "kind": "locate",
            "input": {"purpose": "an element that does not exist"}
        }
    }))
    .unwrap();

    let (response, permissions, updates) = gateway.call_with_permission_then_cancel(
        "session/prompt",
        serde_json::json!({
            "sessionId": session_id,
            "prompt": [{"type": "text", "text": prompt_text}],
        }),
        "allow",
    );

    assert!(response.get("error").is_none(), "prompt failed: {response}");
    assert_eq!(
        response
            .pointer("/result/stopReason")
            .and_then(serde_json::Value::as_str),
        Some("cancelled"),
        "cancelling after approval must interrupt the retry: {response}"
    );
    assert_eq!(permissions.len(), 1, "expected one permission request");
    let messages: Vec<&str> = updates
        .iter()
        .filter_map(|update| {
            update
                .pointer("/update/content/text")
                .and_then(serde_json::Value::as_str)
        })
        .collect();
    assert!(
        messages
            .iter()
            .all(|message| !message.contains("rerunning in session")),
        "AllowOnce published a reusable escalated session: {messages:?}"
    );
}

#[test]
fn close_releases_capacity_and_invalidates_the_session_handle() {
    let mut gateway = Gateway::spawn();
    let initialized = gateway.result(
        "initialize",
        serde_json::json!({
            "protocolVersion": 1,
            "clientCapabilities": {},
        }),
    );
    assert!(
        initialized
            .pointer("/agentCapabilities/sessionCapabilities/close")
            .is_some(),
        "initialize must advertise session/close: {initialized}"
    );

    let mut last_session_id = String::new();
    // The production limit is eight. Successfully creating and closing more
    // than eight sessions proves each close releases runtime and ownership
    // capacity rather than only deleting the ACP map entry.
    for _ in 0..10 {
        let session = gateway.result(
            "session/new",
            serde_json::json!({"cwd": "/tmp", "mcpServers": []}),
        );
        last_session_id = session["sessionId"]
            .as_str()
            .expect("session/new returns a sessionId")
            .to_owned();
        gateway.result(
            "session/close",
            serde_json::json!({"sessionId": last_session_id.clone()}),
        );
    }

    let rejected = gateway.call(
        "session/prompt",
        serde_json::json!({
            "sessionId": last_session_id,
            "prompt": [{"type": "text", "text": "{\"intent\":{\"kind\":\"locate\",\"input\":{\"purpose\":\"anything\"}}}"}],
        }),
    );
    assert!(
        rejected.get("error").is_some(),
        "a closed session handle must be invalid: {rejected}"
    );
}
