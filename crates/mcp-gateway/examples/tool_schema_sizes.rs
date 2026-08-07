//! Print per-tool `tools/list` entry sizes and phase frames vs `TOOLS_LIST_BYTE_BUDGET`.
//!
//!   cargo run -p mcp-gateway --example tool_schema_sizes

use std::sync::Arc;

use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use mcp_gateway::{InProcessJobPort, Server, Toolset, TOOLS_LIST_BYTE_BUDGET};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::json;
use types::{Capability, PrincipalId};
use uuid::uuid;

async fn list_for(toolset: Toolset) -> (usize, Vec<(String, usize)>) {
    let authority = AuthorityStore::with_capacity(8);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000099")),
            Capability::ALL.to_vec(),
            Utc::now() + Duration::hours(1),
        )
        .await
        .expect("issue");
    let handle = authority.verify(&token.expose_once()).await.expect("verify");
    let (jobs, _sched) = InProcessJobPort::memory();
    let server = Server::new(Arc::new(AuthenticatedRuntime::new(
        RuntimeService::default(),
        handle,
    )))
    .with_startup_toolset(toolset)
    .with_jobs(Arc::new(jobs));
    server
        .handle_message(json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-11-25","capabilities":{},
                      "clientInfo":{"name":"sizes","version":"1"}}
        }))
        .await
        .expect("initialize");
    server
        .handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;
    let response = server
        .handle_message(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .await
        .expect("tools/list");
    let tools = response["result"]["tools"].as_array().expect("tools");
    let mut rows: Vec<(String, usize)> = tools
        .iter()
        .map(|t| {
            (
                t["name"].as_str().unwrap_or("").to_owned(),
                serde_json::to_vec(t).expect("ser").len(),
            )
        })
        .collect();
    rows.sort_by(|a, b| b.1.cmp(&a.1));
    let tools_bytes = serde_json::to_vec(tools).expect("ser").len();
    (tools_bytes, rows)
}

#[tokio::main]
async fn main() {
    for phase in [
        Toolset::Full,
        Toolset::Verify,
        Toolset::Explore,
        Toolset::Act,
        Toolset::Intent,
    ] {
        let (frame, rows) = list_for(phase).await;
        println!(
            "{}\t{}\t{}\theadroom={}",
            phase.as_str(),
            frame,
            rows.len(),
            TOOLS_LIST_BYTE_BUDGET.saturating_sub(frame)
        );
        if phase == Toolset::Full {
            for (n, b) in rows.iter().take(12) {
                println!("  {n}\t{b}");
            }
            let jobs: usize = rows
                .iter()
                .filter(|(n, _)| n.starts_with("job_"))
                .map(|(_, b)| *b)
                .sum();
            println!("  job_sum\t{jobs}");
        }
    }
    println!("BUDGET\t{TOOLS_LIST_BYTE_BUDGET}");
}
