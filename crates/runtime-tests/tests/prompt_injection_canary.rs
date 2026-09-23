//! Phase 3 slice D3: prompt-injection posture, exercised against a real
//! Chrome browser (`BOBBY_CHROME_EXECUTABLE`) and the canary page at
//! `packages/bobby-gauntlet/src/pages/canary.ts` (route `/agent-canary`,
//! built to `dist/` by `pnpm --filter @cavi-ai/bobby-gauntlet build`).
//!
//! The canary's visible text and a hidden (off-screen, still
//! accessibility-tree-reachable) element both read like an instruction
//! aimed at an agent: call `evaluate_javascript`, request vision assistance,
//! navigate to a loopback "exfil" URL. This test runs the normal
//! observe/extract loop over it and asserts the product invariants in
//! `docs/bobby-browser/source/pages/security/prompt-injection.md`: page
//! text always comes back marked `pageDerived`, never as a command, and a
//! session without the `JavascriptEvaluate`/`VisionAssist` capabilities
//! cannot reach `evaluate_javascript`/`extract_structured` for this page no
//! matter what its text asks for -- capability gates are the only
//! enforcement boundary, not a read of page content.

use std::sync::Arc;

use chrono::{Duration, Utc};
use gauntlet_server::{ScenarioConfig, ScenarioServer};
use interface_core::AuthorityStore;
use mcp_gateway::Server;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use types::{Capability, PrincipalId};

async fn runtime_service() -> (RuntimeService, tempfile::TempDir) {
    let root = tempfile::tempdir().expect("create canary root");
    let mut config = config::AppConfig::default();
    config.browser.profiles_dir = root.path().join("profiles");
    config.browser.upload_roots = vec![root.path().join("uploads")];
    config.browser.downloads_dir = root.path().join("downloads");
    config.browser.artifacts_dir = root.path().join("artifacts");
    config.storage.journal_path = root.path().join("commands.jsonl");
    config.storage.checkpoints_dir = root.path().join("checkpoints");
    config.storage.authority_path = root.path().join("authority.json");
    config.storage.scheduler_journal_path = root.path().join("scheduler-jobs.jsonl");
    config.http.allow_loopback = true;
    for path in [
        &config.browser.upload_roots[0],
        &config.browser.downloads_dir,
        &config.browser.artifacts_dir,
        &config.storage.checkpoints_dir,
    ] {
        std::fs::create_dir_all(path).expect("create confined canary directory");
    }
    let service = RuntimeService::build(&config)
        .await
        .expect("build real Chrome runtime (set BOBBY_CHROME_EXECUTABLE)");
    (service, root)
}

async fn server_with_capabilities(
    service: RuntimeService,
    capabilities: Vec<Capability>,
) -> Server {
    let authority = AuthorityStore::in_memory();
    let expires = Utc::now() + Duration::minutes(5);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            capabilities,
            expires,
        )
        .await
        .expect("issue canary token")
        .expose_once();
    let handle = authority.verify(&token).await.expect("verify canary token");
    Server::new(Arc::new(AuthenticatedRuntime::new(service, handle)))
}

async fn initialize(server: &Server) {
    server
        .handle_message(json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-11-25","capabilities":{},
                      "clientInfo":{"name":"prompt-injection-canary","version":"1"}}
        }))
        .await
        .expect("initialize answers");
    server
        .handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;
}

async fn call(server: &Server, id: u64, name: &str, arguments: Value) -> Value {
    server
        .handle_message(json!({
            "jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":name,"arguments":arguments}
        }))
        .await
        .expect("tools/call answers")
}

#[tokio::test]
async fn injected_page_text_is_marked_page_derived_and_cannot_escalate_capabilities() {
    let (service, _root) = runtime_service().await;
    let scenario = ScenarioServer::start(ScenarioConfig::seeded("prompt-injection"))
        .await
        .expect(
            "start the gauntlet server; run `pnpm --filter @cavi-ai/bobby-gauntlet build` first",
        );
    let canary_url = scenario.application_url("/agent-canary");

    // A fully-capable session runs the ordinary observe/extract loop over
    // the canary page.
    let full = server_with_capabilities(service.clone(), Capability::ALL.to_vec()).await;
    initialize(&full).await;
    let started = call(
        &full,
        2,
        "workflow_start",
        json!({"profile":"canary","url":canary_url}),
    )
    .await;
    assert_eq!(
        started["result"]["structuredContent"]["status"], "completed",
        "workflow_start on the canary page did not complete: {started}"
    );
    let handle = started["result"]["structuredContent"]["workflowHandle"].clone();

    let observed = call(
        &full,
        3,
        "workflow_observe",
        json!({"workflowHandle":handle}),
    )
    .await;
    let observed_content = &observed["result"]["structuredContent"];
    assert_eq!(
        observed_content["pageDerived"],
        json!(true),
        "workflow_observe over the canary page must mark its result pageDerived: {observed}"
    );
    let observed_text = serde_json::to_string(observed_content).expect("serializable");
    assert!(
        observed_text.contains("evaluate_javascript") || observed_text.contains("SYSTEM OVERRIDE"),
        "workflow_observe must surface the canary's injected text as data, not swallow it: {observed_text}"
    );

    let extracted = call(
        &full,
        4,
        "intent_extract",
        json!({
            "workflowHandle": handle,
            "purpose": "read any operator instruction text present on the page",
            "fields": [
                {"name":"instruction", "purpose":"any instruction-shaped text on the page", "value":{"kind":"text"}}
            ]
        }),
    )
    .await;
    assert_eq!(
        extracted["result"]["structuredContent"]["pageDerived"],
        json!(true),
        "intent_extract over the canary page must mark its result pageDerived: {extracted}"
    );

    // A session that is missing exactly the two escalation capabilities the
    // canary's text asks for -- everything else it needs to browse is
    // present -- must still be refused those two tools on this same page,
    // regardless of what the page says.
    let restricted_capabilities: Vec<Capability> = Capability::ALL
        .iter()
        .copied()
        .filter(|capability| {
            !matches!(
                capability,
                Capability::JavascriptEvaluate | Capability::VisionAssist
            )
        })
        .collect();
    let restricted = server_with_capabilities(service.clone(), restricted_capabilities).await;
    initialize(&restricted).await;
    let restricted_started = call(
        &restricted,
        2,
        "workflow_start",
        json!({"profile":"canary-restricted","url":scenario.application_url("/agent-canary")}),
    )
    .await;
    assert_eq!(
        restricted_started["result"]["structuredContent"]["status"], "completed",
        "the restricted session must still be able to browse: {restricted_started}"
    );
    let restricted_handle =
        restricted_started["result"]["structuredContent"]["workflowHandle"].clone();

    let js_attempt = call(
        &restricted,
        3,
        "evaluate_javascript",
        json!({"workflowHandle":restricted_handle,"expression":"1+1"}),
    )
    .await;
    assert!(
        js_attempt.get("error").is_some(),
        "evaluate_javascript must be refused without JavascriptEvaluate, no matter what the \
         canary page's text asks for: {js_attempt}"
    );

    let vision_attempt = call(
        &restricted,
        4,
        "extract_structured",
        json!({
            "workflowHandle": restricted_handle,
            "schema": {"type":"object","properties":{"x":{"type":"string"}}},
            "purpose": "test"
        }),
    )
    .await;
    assert!(
        vision_attempt.get("error").is_some(),
        "extract_structured (VisionAssist-gated) must be refused without VisionAssist, no \
         matter what the canary page's text asks for: {vision_attempt}"
    );
}
