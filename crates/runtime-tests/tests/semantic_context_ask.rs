//! `context_ask` answers a description that does not share the control's
//! wording from the observed controls' labels, when the session's vision
//! provider answers labels without a screenshot.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{Duration, Utc};
use config::{NodeConfig, NodeKind};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use test_site::{FixtureSite, Route};
use types::{
    AccessibilitySnapshotCommand, AttemptId, Capability, CommandEnvelope, CommandId,
    CommandOutcome, CreateSessionRequest, ExecutionPolicy, NavigateCommand, OpenPageRequest,
    PrimitiveCommand, RuntimeCommand, WaitUntil, WorkflowId,
};

const PAGE: &str = r#"<!doctype html><title>Account</title><main>
<a href="/help">Help center</a>
<form><label for="user">Username</label><input id="user">
<button type="button">Log in</button></form></main>"#;
const DESCRIPTION: &str = "sign in button";

/// Request bodies a loopback vision node received.
type Received = Arc<Mutex<Vec<serde_json::Value>>>;

/// A loopback vision node that picks the offered candidate named "Log in".
/// With `text_tier`, its status says it answers labels without a screenshot.
async fn labelled_vision_node(text_tier: bool, received: Received) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route(
            "/propose",
            axum::routing::get(move || async move {
                let mut headers = axum::http::HeaderMap::new();
                if text_tier {
                    headers.insert("x-bobby-vision-text", "ready".parse().unwrap());
                }
                headers
            })
            .post(move |axum::Json(body): axum::Json<serde_json::Value>| {
                let received = Arc::clone(&received);
                async move {
                    let index = body["context"]["candidates"]
                        .as_array()
                        .and_then(|candidates| {
                            candidates
                                .iter()
                                .position(|candidate| candidate["name"] == "Log in")
                        });
                    received.lock().unwrap().push(body);
                    // A window without the control gets an answer under the floor.
                    axum::Json(serde_json::json!({
                        "confidence": if index.is_some() { 0.9 } else { 0.1 },
                        "action": {"kind": "clickCandidate", "index": index.unwrap_or(0)},
                    }))
                }
            }),
        )
        .layer(axum::extract::DefaultBodyLimit::disable());
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    format!("http://{address}/propose")
}

fn chrome_executable() -> PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

fn config(root: &Path, endpoint: &str) -> config::AppConfig {
    config::AppConfig {
        http: config::HttpConfig {
            allow_loopback: true,
            ..config::HttpConfig::default()
        },
        server: config::ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            shutdown_timeout_ms: 10_000,
        },
        browser: config::BrowserConfig {
            executable: Some(chrome_executable()),
            profiles_dir: root.join("profiles"),
            headless: true,
            max_active: 1,
            upload_roots: vec![],
            downloads_dir: root.join("downloads"),
            artifacts_dir: root.join("artifacts"),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
            max_js_result_bytes: 64 * 1024,
            max_js_timeout_ms: 30_000,
        },
        storage: config::StorageConfig {
            journal_path: root.join("commands.jsonl"),
            checkpoints_dir: root.join("checkpoints"),
            authority_path: root.join("authority.json"),
            scheduler_journal_path: root.join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
        context: config::ContextConfig::default(),
        nodes: BTreeMap::from([(
            "vision".to_owned(),
            NodeConfig {
                kind: NodeKind::Vision,
                endpoint_url: endpoint.to_owned(),
                token_env: None,
                timeout_ms: 15_000,
            },
        )]),
    }
}

const CAPABILITIES: [Capability; 7] = [
    Capability::SessionRead,
    Capability::SessionWrite,
    Capability::PageRead,
    Capability::PageWrite,
    Capability::BrowserMutate,
    Capability::IntentExecute,
    Capability::VisionAssist,
];

/// Observes the page in a session with `vision_assist`, then asks for `description`.
async fn ask_after_a_snapshot(
    endpoint: &str,
    url: &str,
    vision_assist: bool,
    description: &str,
) -> Option<types::ContextAnswer> {
    let root = tempfile::tempdir().unwrap();
    let runtime = RuntimeService::build(&config(root.path(), endpoint))
        .await
        .unwrap();
    let authority = interface_core::AuthorityStore::in_memory();
    let handle = authority
        .issue(
            types::PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            CAPABILITIES,
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let handle = authority.verify(&handle).await.unwrap();
    let authed = AuthenticatedRuntime::new(runtime.clone(), handle);
    let ctx = types::RequestContext::new_for_test(
        authed.capability_handle().principal_id().clone(),
        CAPABILITIES,
        Utc::now() + Duration::minutes(5),
    );
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "semantic-context-ask".into(),
            proxy: None,
            execution_policy: ExecutionPolicy {
                vision_assist,
                ..ExecutionPolicy::default()
            },
            zigzagzig: false,
        })
        .await
        .unwrap();
    let page = runtime
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();
    for command in [
        PrimitiveCommand::Navigate(NavigateCommand {
            url: url.into(),
            wait_until: WaitUntil::Interactive,
            timeout_ms: 10_000,
        }),
        PrimitiveCommand::AccessibilitySnapshot(AccessibilitySnapshotCommand {
            max_nodes: None,
            target: None,
        }),
    ] {
        let outcome = runtime
            .submit(CommandEnvelope {
                schema_version: CommandEnvelope::SCHEMA_VERSION,
                command_id: CommandId::new(),
                workflow_id: WorkflowId::new(),
                attempt_id: AttemptId::new(),
                session_id: session.id.clone(),
                page_id: Some(page.id.clone()),
                deadline: Utc::now() + Duration::seconds(30),
                command: RuntimeCommand::Primitive(command),
            })
            .await;
        assert!(
            matches!(outcome, CommandOutcome::Completed { .. }),
            "{outcome:?}"
        );
    }
    interface_core::RuntimeInterface::context_ask(
        &authed,
        ctx,
        session.id,
        page.id,
        description.into(),
    )
    .await
    .unwrap()
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn a_described_control_is_answered_from_labels_without_a_screenshot() {
    let site = FixtureSite::spawn(vec![("/account", Route::Html(PAGE.into()))]).await;
    let url = site.url("/account");
    let received = Received::default();
    let text_tier = labelled_vision_node(true, Arc::clone(&received)).await;
    let image_only = labelled_vision_node(false, Arc::clone(&received)).await;

    // The control's own name answers from the snapshot without a provider.
    let answer = ask_after_a_snapshot(&text_tier, &url, false, "Log in")
        .await
        .expect("the snapshot answers the control's exact name");
    assert_eq!(answer.target.role, "button");
    assert_eq!(answer.source, None);

    // Neither a session without vision assist nor a provider that needs
    // screenshots is asked.
    let answer = ask_after_a_snapshot(&text_tier, &url, false, DESCRIPTION).await;
    assert!(answer.is_none(), "{answer:?}");
    let answer = ask_after_a_snapshot(&image_only, &url, true, DESCRIPTION).await;
    assert!(answer.is_none(), "{answer:?}");
    assert!(received.lock().unwrap().is_empty());

    let answer = ask_after_a_snapshot(&text_tier, &url, true, DESCRIPTION)
        .await
        .unwrap_or_else(|| panic!("no answer; provider saw {:?}", received.lock().unwrap()));
    assert_eq!(
        (
            answer.target.role.as_str(),
            answer.target.accessible_name.as_str()
        ),
        ("button", "Log in")
    );
    assert_eq!(serde_json::to_value(answer.source).unwrap(), "semantic");
    assert!(answer.confidence >= 0.75);
    let received = received.lock().unwrap();
    assert_eq!(received.len(), 1);
    assert!(
        received[0].get("screenshotPng").is_none(),
        "{}",
        received[0]
    );
    assert_eq!(received[0]["purpose"], "Locate the sign in button");
    assert!(
        received[0]["context"].get("url").is_none(),
        "{}",
        received[0]
    );
}
