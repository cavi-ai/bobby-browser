//! A control drawn on a canvas has no accessible name, so the accessibility
//! tree cannot offer it. With `[vision].detector_model`, the vision window
//! also offers on-screen controls named by the text inside them, and the
//! runtime clicks the one the provider picks at its center.
#![cfg(target_os = "macos")]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{Duration, Utc};
use config::{NodeConfig, NodeKind};
use sdk_core::RuntimeService;
use test_site::{FixtureSite, Route};
use types::{
    AttemptId, CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest, ExecutionPolicy,
    NavigateCommand, OpenPageRequest, PrimitiveCommand, RuntimeCommand, WaitUntil, WorkflowId,
};

const PAGE: &str = r##"<!doctype html><title>Report</title>
<style>body{font-family:sans-serif;margin:24px}button{font-size:16px;margin-right:8px}</style>
<main><h1>Quarterly report</h1>
<p><button type="button">Save draft</button><button type="button">Cancel</button></p>
<canvas id="export" width="160" height="48" style="cursor:pointer"></canvas></main>
<script>
const canvas = document.getElementById("export");
const pen = canvas.getContext("2d");
pen.fillStyle = "#1d4ed8";
pen.beginPath();
pen.roundRect(0, 0, 160, 48, 8);
pen.fill();
pen.fillStyle = "#ffffff";
pen.font = "600 20px sans-serif";
pen.textAlign = "center";
pen.textBaseline = "middle";
pen.fillText("Export", 80, 24);
canvas.addEventListener("click", () => { location.hash = "exported"; });
</script>"##;

/// Request bodies a loopback vision node received.
type Received = Arc<Mutex<Vec<serde_json::Value>>>;

/// A loopback text-tier vision node that picks the offered candidate named
/// "Export", and answers under the confidence floor when none is offered.
async fn export_picking_node(received: Received) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let app = axum::Router::new()
        .route(
            "/propose",
            axum::routing::get(|| async {
                let mut headers = axum::http::HeaderMap::new();
                headers.insert("x-bobby-vision-text", "ready".parse().unwrap());
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
                                .position(|candidate| candidate["name"] == "Export")
                        });
                    received.lock().unwrap().push(body);
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

fn config(root: &Path, endpoint: &str, detector_model: Option<PathBuf>) -> config::AppConfig {
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
        vision: config::VisionConfig {
            detector_model,
            ..config::VisionConfig::default()
        },
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
        credentials: Vec::new(),
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

/// Opens `url` in a session with `visionAssist` and follows "Export" to `#exported`.
async fn follow_export(
    endpoint: &str,
    url: &str,
    detector_model: Option<PathBuf>,
) -> CommandOutcome {
    let root = tempfile::tempdir().unwrap();
    let runtime = RuntimeService::build(&config(root.path(), endpoint, detector_model))
        .await
        .unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "screen-controls".into(),
            proxy: None,
            execution_policy: ExecutionPolicy {
                vision_assist: true,
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
    let follow: RuntimeCommand = serde_json::from_value(serde_json::json!({
        "kind": "intent",
        "input": {"kind": "follow", "input": {
            "purpose": "Export",
            "expectedDestination": {
                "condition": {"kind": "url", "matcher": {"kind": "contains", "value": "#exported"}},
                "timeoutMs": 3000,
            },
        }},
    }))
    .unwrap();
    let navigate = RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
        url: url.into(),
        wait_until: WaitUntil::Interactive,
        timeout_ms: 10_000,
    }));
    let mut last = None;
    for command in [navigate, follow] {
        let envelope = CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(60),
            command,
        };
        last = Some(runtime.submit_with_vision_capability(envelope, true).await);
    }
    last.unwrap()
}

fn offered_names(received: &Received) -> Vec<String> {
    received
        .lock()
        .unwrap()
        .iter()
        .flat_map(|body| {
            body["context"]["candidates"]
                .as_array()
                .cloned()
                .unwrap_or_default()
        })
        .filter_map(|candidate| candidate["name"].as_str().map(str::to_owned))
        .collect()
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium and BOBBY_DETECTOR_MODEL"]
async fn a_canvas_control_is_followed_by_its_on_screen_name() {
    let model = PathBuf::from(
        std::env::var("BOBBY_DETECTOR_MODEL")
            .expect("BOBBY_DETECTOR_MODEL names the ONNX detector"),
    );
    let site = FixtureSite::spawn(vec![("/report", Route::Html(PAGE.into()))]).await;
    let url = site.url("/report");

    // The accessibility tree has no "Export", so the provider is never offered it.
    let received = Received::default();
    let endpoint = export_picking_node(Arc::clone(&received)).await;
    let outcome = follow_export(&endpoint, &url, None).await;
    assert!(
        matches!(outcome, CommandOutcome::Failed { .. }),
        "{outcome:?}"
    );
    assert!(!offered_names(&received).contains(&"Export".to_owned()));

    // The detector finds the drawn button and its text names it.
    let received = Received::default();
    let endpoint = export_picking_node(Arc::clone(&received)).await;
    let outcome = follow_export(&endpoint, &url, Some(model)).await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    assert!(offered_names(&received).contains(&"Export".to_owned()));
}
