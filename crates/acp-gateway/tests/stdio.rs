//! Wire-level proof over the real stdio transport: spawn the `acp-gateway`
//! binary with a bootstrap credential and drive `initialize`, `session/new`,
//! and `session/prompt` the way an editor would. No browser is involved —
//! the assertions are about the ACP surface itself (framing, session
//! lifecycle, structured-prompt validation), which must hold before any
//! editor ever connects.

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};

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
    stdout: BufReader<std::process::ChildStdout>,
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
        let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
        Self {
            child,
            stdin,
            stdout,
            next_id: 0,
        }
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
            assert!(
                std::time::Instant::now() < deadline,
                "{method}: gateway did not answer in time"
            );
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line).unwrap();
            assert!(read > 0, "{method}: gateway closed stdout");
            let frame: serde_json::Value = serde_json::from_str(line.trim()).expect("frame parses");
            if frame.get("id").and_then(serde_json::Value::as_u64) != Some(id) {
                continue;
            }
            return frame;
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
