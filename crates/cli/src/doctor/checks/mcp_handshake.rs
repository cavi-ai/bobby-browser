//! mcp handshake diagnostics and probes.
use super::DoctorContext;
use crate::doctor::{bootstrap_local, onboarding, DoctorReport, DoctorStatus, Result};

pub(super) fn mcp_handshake(context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    let config_path = context.config_path.clone();
    let bootstrap_path = context.bootstrap_path.clone();
    // MCP handshake: the stdio gateway an agent host launches must answer
    // `initialize` and `tools/list` within the advertised byte budget. A
    // missing gateway binary is a warning (it may be installed separately);
    // a gateway that starts but fails the handshake is a failure, because the
    // host will only report it as a dead server.
    let handshake_env: Option<std::collections::BTreeMap<String, String>> =
        if broker::StartupCredential::from_env().is_ok() {
            Some(std::collections::BTreeMap::new())
        } else {
            bootstrap_path
                .filter(|path| path.exists())
                .and_then(|path| bootstrap_local::load_bootstrap_env_map(&path).ok())
        };
    // Hand the same config path doctor validated into the gateway child so
    // `[mcp] startup_toolset` (and the rest of the file) apply to handshake —
    // without this, doctor always probes explore defaults while agent
    // hosts that set BOBBY_BROWSER_CONFIG see a different surface.
    let handshake_env = handshake_env.map(|mut env| {
        if config_path.exists() {
            env.insert(
                "BOBBY_BROWSER_CONFIG".into(),
                config_path.display().to_string(),
            );
        }
        env
    });
    match handshake_env {
        Some(env) => match onboarding::mcp_handshake(&env) {
            Ok(handshake) => {
                if handshake.bytes > mcp_gateway::TOOLS_LIST_BYTE_BUDGET {
                    report.fail(
                        "mcp-handshake",
                        format!(
                            "tools/list is {} bytes, over the {} byte budget",
                            handshake.bytes,
                            mcp_gateway::TOOLS_LIST_BYTE_BUDGET
                        ),
                    );
                } else {
                    report.ok(
                        "mcp-handshake",
                        format!(
                            "gateway {} answered initialize + tools/list: {} tools, {} bytes ({}% of budget)",
                            handshake.server_version,
                            handshake.tools,
                            handshake.bytes,
                            handshake.bytes * 100 / mcp_gateway::TOOLS_LIST_BYTE_BUDGET
                        ),
                    );
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                report.record(handshake_error_status(&message), "mcp-handshake", message);
            }
        },
        None => {
            report.warn(
                "mcp-handshake",
                "skipped: no bootstrap credential to launch the gateway with".to_string(),
            );
        }
    }

    Ok(())
}

pub(crate) fn handshake_error_status(message: &str) -> DoctorStatus {
    if message.contains("not found") {
        DoctorStatus::Warn
    } else {
        DoctorStatus::Fail
    }
}
