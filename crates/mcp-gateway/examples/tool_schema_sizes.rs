use std::sync::Arc;

use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use mcp_gateway::{InProcessJobPort, Server, Toolset};
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
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
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
        .unwrap();
    server
        .handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;
    let response = server
        .handle_message(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .await
        .unwrap();
    let tools = response["result"]["tools"].as_array().unwrap();
    let mut rows: Vec<(String, usize)> = tools
        .iter()
        .map(|t| {
            let name = t["name"].as_str().unwrap_or("").to_owned();
            let bytes = serde_json::to_vec(t).unwrap().len();
            (name, bytes)
        })
        .collect();
    rows.sort_by_key(|(_, bytes)| std::cmp::Reverse(*bytes));
    let frame = serde_json::to_vec(&response).unwrap().len();
    (frame, rows)
}

#[tokio::main]
async fn main() {
    let (full_frame, full_rows) = list_for(Toolset::Full).await;
    let (verify_frame, verify_rows) = list_for(Toolset::Verify).await;
    let (explore_frame, _) = list_for(Toolset::Explore).await;
    let (act_frame, _) = list_for(Toolset::Act).await;
    let (intent_frame, _) = list_for(Toolset::Intent).await;
    println!("PHASE\tFRAME\tCOUNT");
    println!("full\t{full_frame}\t{}", full_rows.len());
    println!("verify\t{verify_frame}\t{}", verify_rows.len());
    println!("explore\t{explore_frame}");
    println!("act\t{act_frame}");
    println!("intent\t{intent_frame}");
    println!("BUDGET\t{}", 128 * 1024);
    println!("HEADROOM_FULL\t{}", 128 * 1024 - full_frame);
    println!("---FULL---");
    for (n, b) in &full_rows {
        println!("{n}\t{b}");
    }
    println!("---JOBS_IN_VERIFY---");
    let mut job_sum = 0usize;
    for (n, b) in &verify_rows {
        if n.starts_with("job_") {
            println!("{n}\t{b}");
            job_sum += b;
        }
    }
    println!("JOB_SUM_VERIFY\t{job_sum}");
    println!("FULL_PLUS_JOBS_EST\t{}", full_frame + job_sum);
}
