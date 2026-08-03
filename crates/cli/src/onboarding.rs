//! Agent onboarding: `bobby init --emit` client config fragments and the
//! `bobby doctor` MCP handshake check.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};

use anyhow::{anyhow, Context, Result};
use clap::ValueEnum;

/// Agent host config dialect for `bobby init --emit`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum EmitFormat {
    /// Claude Code project `.mcp.json` (`mcpServers`).
    Claude,
    /// Zed `settings.json` (`context_servers`).
    Zed,
    /// VS Code `.vscode/mcp.json` (`servers`).
    Vscode,
    /// Generic MCP JSON fragment (same shape as Claude's).
    Json,
}

const GATEWAY_COMMAND: &str = "mcp-gateway";

fn bootstrap_env_placeholders() -> serde_json::Value {
    serde_json::json!({
        "AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN": "${AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN}",
        "AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL": "${AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL}",
        "AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES": "${AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES}",
        "AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT": "${AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT}",
    })
}

/// Render the MCP client config fragment for one agent host. The fragment
/// never carries the credential itself: each host expands `${VAR}` from its
/// own environment, which is where `bootstrap.env` is expected to be sourced.
pub fn emit_mcp_config(format: EmitFormat) -> String {
    let fragment = match format {
        EmitFormat::Claude | EmitFormat::Json => serde_json::json!({
            "mcpServers": {
                "bobby-browser": {
                    "command": GATEWAY_COMMAND,
                    "env": bootstrap_env_placeholders(),
                }
            }
        }),
        EmitFormat::Vscode => serde_json::json!({
            "servers": {
                "bobby-browser": {
                    "type": "stdio",
                    "command": GATEWAY_COMMAND,
                    "env": bootstrap_env_placeholders(),
                }
            }
        }),
        EmitFormat::Zed => serde_json::json!({
            "context_servers": {
                "bobby-browser": {
                    "command": {
                        "path": GATEWAY_COMMAND,
                        "args": [],
                        "env": bootstrap_env_placeholders(),
                    }
                }
            }
        }),
    };
    serde_json::to_string_pretty(&fragment).expect("emitted config is serializable")
}

pub struct HandshakeReport {
    pub tools: usize,
    pub bytes: usize,
    pub server_version: String,
}

/// Resolve the gateway binary: a sibling of the current executable first
/// (workspace `target/` layouts keep them together), then PATH.
fn resolve_gateway() -> Result<PathBuf> {
    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            let sibling = dir.join(GATEWAY_COMMAND);
            if sibling.is_file() {
                return Ok(sibling);
            }
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(GATEWAY_COMMAND);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(anyhow!(
        "{GATEWAY_COMMAND} not found next to the bobby binary or on PATH"
    ))
}

struct JsonRpcChild {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
    next_id: u64,
}

impl JsonRpcChild {
    fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        self.next_id += 1;
        let id = self.next_id;
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{}", serde_json::to_string(&frame)?)?;
        self.stdin.flush()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            if std::time::Instant::now() > deadline {
                return Err(anyhow!("{method}: gateway did not answer within 15s"));
            }
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line)?;
            if read == 0 {
                return Err(anyhow!("{method}: gateway closed stdout"));
            }
            let frame: serde_json::Value = serde_json::from_str(line.trim())
                .with_context(|| format!("{method}: unparseable frame from gateway"))?;
            if frame.get("id").and_then(serde_json::Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = frame.get("error") {
                return Err(anyhow!("{method}: gateway returned {error}"));
            }
            return Ok(frame
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null));
        }
    }

    fn notify(&mut self, method: &str) -> Result<()> {
        let frame = serde_json::json!({"jsonrpc": "2.0", "method": method});
        writeln!(self.stdin, "{}", serde_json::to_string(&frame)?)?;
        self.stdin.flush()?;
        Ok(())
    }
}

impl Drop for JsonRpcChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn the stdio gateway with the bootstrap credential, run `initialize`
/// and `tools/list`, and report the advertised surface size. This is the
/// check `bobby doctor` runs so a host config that launches fine but answers
/// with an oversized or empty catalog is caught before the agent sees it.
pub fn mcp_handshake(bootstrap: &BTreeMap<String, String>) -> Result<HandshakeReport> {
    let gateway = resolve_gateway()?;
    let mut command = Command::new(&gateway);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (key, value) in bootstrap {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn {}", gateway.display()))?;
    let stdin = child.stdin.take().expect("stdin piped");
    let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
    let mut rpc = JsonRpcChild {
        child,
        stdin,
        stdout,
        next_id: 0,
    };
    let initialized = rpc.call(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "bobby-doctor", "version": env!("CARGO_PKG_VERSION")},
        }),
    )?;
    let server_version = initialized
        .pointer("/serverInfo/version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    rpc.notify("notifications/initialized")?;
    let listed = rpc.call("tools/list", serde_json::json!({}))?;
    let bytes = serde_json::to_string(&listed)?.len();
    let tools = listed
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    Ok(HandshakeReport {
        tools,
        bytes,
        server_version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_format_emits_valid_json_with_its_top_level_key_and_no_secret() {
        for (format, key) in [
            (EmitFormat::Claude, "mcpServers"),
            (EmitFormat::Json, "mcpServers"),
            (EmitFormat::Vscode, "servers"),
            (EmitFormat::Zed, "context_servers"),
        ] {
            let emitted = emit_mcp_config(format);
            let parsed: serde_json::Value =
                serde_json::from_str(&emitted).expect("emitted config parses");
            assert!(parsed.get(key).is_some(), "missing {key} in {emitted}");
            let server = parsed
                .pointer(&format!("/{key}/bobby-browser"))
                .expect("bobby-browser entry");
            // The fragment must name the gateway but never carry credential
            // material — only `${VAR}` placeholders.
            let text = serde_json::to_string(server).expect("serializable");
            assert!(text.contains("mcp-gateway"));
            assert!(text.contains("${AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN}"));
            assert!(!text.contains("=Bearer"));
        }
    }
}
