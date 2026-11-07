//! Workflow files: the `bobby://workflows` listing, the schema resource, and
//! the errors `workflow_run` answers before it runs any step.

use std::{path::Path, sync::Arc};

use chrono::{Duration, Utc};
use interface_core::AuthorityStore;
use mcp_gateway::Server;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use types::{Capability, PrincipalId};
use uuid::uuid;

const SESSION: &str = "20000000-0000-4000-8000-000000000001";
const PAGE: &str = "20000000-0000-4000-8000-000000000002";

async fn server(dir: &Path) -> Server {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000031")),
            vec![Capability::BrowserMutate, Capability::IntentExecute],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(RuntimeService::default(), handle));
    let server = Server::new(runtime)
        .with_startup_toolset(mcp_gateway::Toolset::Full)
        .with_workflow_dirs(vec![dir.to_path_buf()]);
    server
        .handle_message(request(
            1,
            "initialize",
            json!({"protocolVersion":"2025-11-25","capabilities":{},
                   "clientInfo":{"name":"test","version":"1"}}),
        ))
        .await;
    server
        .handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;
    server
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}

fn write_files(dir: &Path) {
    std::fs::write(
        dir.join("contact.json"),
        r#"{"name":"contact","description":"Open the contact page.",
            "inputs":{"url":{"type":"string"}},
            "steps":[{"id":"open","tool":"navigate","args":{"url":{"$input":"url"}}}]}"#,
    )
    .unwrap();
    std::fs::write(
        dir.join("probe.json"),
        r#"{"name":"probe","steps":[{"id":"x","tool":"no_such_tool"}]}"#,
    )
    .unwrap();
}

async fn read(server: &Server, uri: &str) -> Value {
    let read = server
        .handle_message(request(2, "resources/read", json!({"uri":uri})))
        .await
        .unwrap();
    let text = read["result"]["contents"][0]["text"]
        .as_str()
        .unwrap_or_else(|| panic!("{uri} read returned no text: {read}"));
    serde_json::from_str(text).unwrap()
}

async fn run(server: &Server, arguments: Value) -> Value {
    let mut arguments = arguments;
    arguments["sessionId"] = json!(SESSION);
    arguments["pageId"] = json!(PAGE);
    server
        .handle_message(request(
            3,
            "tools/call",
            json!({"name":"workflow_run","arguments":arguments}),
        ))
        .await
        .unwrap()
}

#[tokio::test]
async fn the_listing_names_built_ins_and_files_with_their_errors() {
    let dir = tempfile::tempdir().unwrap();
    write_files(dir.path());
    let server = server(dir.path()).await;

    let listed = server
        .handle_message(request(2, "resources/list", json!({})))
        .await
        .unwrap();
    let uris: Vec<&str> = listed["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|resource| resource["uri"].as_str())
        .collect();
    assert!(uris.contains(&"bobby://workflows"), "{uris:?}");
    assert!(uris.contains(&"bobby://workflow-schema"), "{uris:?}");

    let listing = read(&server, "bobby://workflows").await;
    let by_name = |name: &str| {
        listing["workflows"]
            .as_array()
            .unwrap()
            .iter()
            .find(|workflow| workflow["name"] == name)
            .unwrap_or_else(|| panic!("{name} not listed: {listing}"))
            .clone()
    };
    for builtin in ["fillForm", "extractPage", "collectLinks"] {
        assert!(by_name(builtin)["inputs"].is_object(), "{listing}");
    }
    let contact = by_name("contact");
    assert_eq!(contact["description"], "Open the contact page.");
    assert_eq!(contact["inputs"]["url"]["type"], "string");
    assert!(contact.get("error").is_none(), "{contact}");
    assert_eq!(by_name("probe")["error"]["code"], "unknownTool");

    let schema = read(&server, "bobby://workflow-schema").await;
    let expected: Value = serde_json::from_str(mcp_gateway::workflows::WORKFLOW_SCHEMA).unwrap();
    assert_eq!(schema, expected);
}

#[tokio::test]
async fn workflow_run_names_why_a_workflow_cannot_start() {
    let dir = tempfile::tempdir().unwrap();
    write_files(dir.path());
    let server = server(dir.path()).await;
    let reason = |response: &Value| response["error"]["data"]["reason"].clone();

    let invalid = run(&server, json!({"workflow":"probe"})).await;
    assert_eq!(reason(&invalid), "workflowInvalid", "{invalid}");
    assert_eq!(
        invalid["error"]["data"]["workflowError"]["code"],
        "unknownTool"
    );
    assert_eq!(
        invalid["error"]["data"]["workflowError"]["pointer"],
        "/steps/0/tool"
    );

    let missing = run(&server, json!({"workflow":"absent"})).await;
    assert_eq!(reason(&missing), "workflowNotFound", "{missing}");

    let no_inputs = run(&server, json!({"workflow":"contact"})).await;
    assert_eq!(reason(&no_inputs), "workflowInputs", "{no_inputs}");
    let unknown_input = run(
        &server,
        json!({"workflow":"contact","inputs":{"url":"https://example.com","extra":1}}),
    )
    .await;
    assert_eq!(reason(&unknown_input), "workflowInputs", "{unknown_input}");

    let half_resume = run(
        &server,
        json!({"workflow":"contact","inputs":{"url":"https://example.com"},"runId":"r1"}),
    )
    .await;
    assert_eq!(reason(&half_resume), "workflowRun", "{half_resume}");
    let unknown_run = run(
        &server,
        json!({"workflow":"contact","inputs":{"url":"https://example.com"},
               "runId":"r1","from":"open"}),
    )
    .await;
    assert_eq!(reason(&unknown_run), "workflowRun", "{unknown_run}");
    let builtin_resume = run(
        &server,
        json!({"workflow":"fillForm","inputs":{"fields":[]},"runId":"r1","from":"open"}),
    )
    .await;
    assert_eq!(reason(&builtin_resume), "workflowRun", "{builtin_resume}");
}
