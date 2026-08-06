//! MCP job tools against an in-process scheduler.

use std::sync::Arc;

use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use mcp_gateway::{InProcessJobPort, Server, Toolset};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::json;
use types::{Capability, PrincipalId};
use uuid::uuid;

async fn server_with_jobs() -> Server {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000061")),
            Capability::ALL.to_vec(),
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let (port, scheduler) = InProcessJobPort::memory();
    tokio::spawn(async move {
        let _ = scheduler.run().await;
    });
    let server = Server::new(Arc::new(AuthenticatedRuntime::new(
        RuntimeService::default(),
        handle,
    )))
    .with_startup_toolset(Toolset::Verify)
    .with_jobs(Arc::new(port));
    server
        .handle_message(json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-11-25","capabilities":{},
                      "clientInfo":{"name":"jobs-mcp","version":"1"}}
        }))
        .await
        .unwrap();
    server
        .handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;
    server
}

#[tokio::test]
async fn job_submit_status_and_cancel_round_trip() {
    let server = server_with_jobs().await;
    let listed = server
        .handle_message(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .await
        .unwrap();
    let names: Vec<_> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["name"].as_str().unwrap().to_owned())
        .collect();
    assert!(names.contains(&"job_submit".to_owned()));
    assert!(names.contains(&"job_status".to_owned()));
    assert!(names.contains(&"job_cancel".to_owned()));

    let submitted = server
        .handle_message(json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"job_submit","arguments":{"name":"echo","payload":{"ok":true}}}
        }))
        .await
        .unwrap();
    assert!(
        submitted["result"]["isError"] != true,
        "submit failed: {submitted}"
    );
    let job_id = submitted["result"]["structuredContent"]["jobId"]
        .as_str()
        .expect("jobId")
        .to_owned();

    let status = server
        .handle_message(json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"job_status","arguments":{"jobId":job_id}}
        }))
        .await
        .unwrap();
    assert_eq!(
        status["result"]["structuredContent"]["name"],
        "echo",
        "{status}"
    );

    let cancelled = server
        .handle_message(json!({
            "jsonrpc":"2.0","id":5,"method":"tools/call",
            "params":{"name":"job_cancel","arguments":{"jobId":job_id}}
        }))
        .await
        .unwrap();
    assert_eq!(
        cancelled["result"]["structuredContent"]["cancelled"],
        true,
        "{cancelled}"
    );
}
