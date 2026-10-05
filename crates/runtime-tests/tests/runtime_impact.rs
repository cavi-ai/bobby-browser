use std::net::SocketAddr;
use std::time::Duration;

use broker::{
    serve_listener,
    testing::{app_with_shared_control, issue_bearer},
    SharedRuntimeControl,
};
use checkpoint_store::CheckpointStore;
use chrono::Utc;
use futures_util::StreamExt;
use page_runtime::{PageRuntime, RecoveryCoordinator};
use sdk_core::RuntimeService;
use session_manager::SessionManager;
use tokio_tungstenite::tungstenite::{client::IntoClientRequest, http::HeaderValue};
use types::{
    AttemptId, CheckpointId, CommandClass, CreateSessionRequest, ExecutionPolicy, OpenPageRequest,
    PageId, SessionId, WorkflowCheckpoint, WorkflowId,
};
use uuid::Uuid;

const SECRET: &str = "owner-stop-secret-0123456789";
const PRINCIPAL: Uuid = Uuid::from_u128(0x77);

struct Rig {
    address: SocketAddr,
    owner_id: Uuid,
    runtime: RuntimeService,
    bearer: String,
    client: reqwest::Client,
    _checkpoints: tempfile::TempDir,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn rig() -> Rig {
    let dir = tempfile::tempdir().unwrap();
    let store = CheckpointStore::open(dir.path().join("checkpoints"))
        .await
        .unwrap();
    let runtime = RuntimeService::with_recovery(
        SessionManager::default(),
        PageRuntime::default(),
        RecoveryCoordinator::new(store),
    );
    let owner_id = Uuid::new_v4();
    let control = SharedRuntimeControl::new(owner_id, SECRET.into(), |_| Ok(()));
    let (app, _authority, admin) = app_with_shared_control(4, runtime.clone(), &control).await;
    let bearer = issue_bearer(
        &app,
        &admin,
        PRINCIPAL,
        &["session:read", "session:write", "page:read", "page:write"],
    )
    .await;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let server = tokio::spawn(serve_listener(listener, app, 64));
    Rig {
        address,
        owner_id,
        runtime,
        bearer,
        client: reqwest::Client::new(),
        _checkpoints: dir,
        server,
    }
}

impl Rig {
    async fn impact(&self, secret: Option<&str>) -> reqwest::Response {
        let mut request = self
            .client
            .get(format!("http://{}/_bobby/runtime/impact", self.address));
        if let Some(secret) = secret {
            request = request.header("x-bobby-owner-stop", secret);
        }
        request.send().await.unwrap()
    }

    async fn impact_json(&self) -> serde_json::Value {
        let response = self.impact(Some(SECRET)).await;
        assert_eq!(response.status(), 200);
        response.json().await.unwrap()
    }
}

#[tokio::test]
async fn impact_requires_the_owner_stop_secret() {
    let rig = rig().await;
    let missing = rig.impact(None).await;
    assert_eq!(missing.status(), 401);
    assert!(missing.bytes().await.unwrap().is_empty());
    let wrong = rig.impact(Some("not-the-secret")).await;
    assert_eq!(wrong.status(), 401);
    assert!(wrong.bytes().await.unwrap().is_empty());
    let body = rig.impact_json().await;
    assert_eq!(body["ownerId"], rig.owner_id.to_string());
}

#[tokio::test]
async fn identity_route_body_is_unchanged() {
    let rig = rig().await;
    let response = rig
        .client
        .get(format!("http://{}/_bobby/runtime", rig.address))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.text().await.unwrap(),
        format!(r#"{{"ownerId":"{}"}}"#, rig.owner_id)
    );
}

#[tokio::test]
async fn impact_lists_sessions_pages_and_recoverable_workflows() {
    let rig = rig().await;
    let paged = rig
        .runtime
        .create_session(CreateSessionRequest {
            profile: "paged".into(),
            proxy: None,
            execution_policy: ExecutionPolicy::default(),
            zigzagzig: false,
        })
        .await
        .unwrap();
    let page = rig
        .runtime
        .pages
        .open(OpenPageRequest {
            session_id: paged.id.clone(),
        })
        .await;
    rig.runtime
        .sessions
        .add_page(&paged.id, page.id.clone())
        .await
        .unwrap();
    rig.runtime
        .pages
        .set_url(
            &page.id,
            "https://user:hunter2@example.test/cart?token=secret#frag".into(),
            "complete",
        )
        .await
        .unwrap();
    let saved = rig
        .runtime
        .create_session(CreateSessionRequest {
            profile: "saved".into(),
            proxy: None,
            execution_policy: ExecutionPolicy::default(),
            zigzagzig: false,
        })
        .await
        .unwrap();
    let workflow_id = WorkflowId::new();
    rig.runtime
        .checkpoint(
            checkpoint(&saved.id, &workflow_id, "https://example.test/"),
            Vec::new(),
        )
        .await
        .unwrap();

    let body = rig.impact_json().await;
    println!("{body}");
    assert_eq!(body["connections"], 0);
    assert_eq!(body["inFlightCommands"], 0);
    assert!(chrono::DateTime::parse_from_rfc3339(body["takenAt"].as_str().unwrap()).is_ok());
    let sessions = body["sessions"].as_array().unwrap();
    assert_eq!(sessions.len(), 2);
    let find = |profile: &str| {
        sessions
            .iter()
            .find(|session| session["profile"] == profile)
            .unwrap()
    };
    let paged_view = find("paged");
    assert_eq!(paged_view["sessionId"], paged.id.0.to_string());
    assert_eq!(paged_view["pages"][0]["pageId"], page.id.0.to_string());
    assert_eq!(paged_view["pages"][0]["url"], "https://example.test/cart");
    assert_eq!(paged_view["recoverableWorkflows"], serde_json::json!([]));
    assert!(paged_view["createdAt"].is_string() && paged_view["lastUsedAt"].is_string());
    let saved_view = find("saved");
    assert_eq!(saved_view["pages"], serde_json::json!([]));
    assert_eq!(
        saved_view["recoverableWorkflows"],
        serde_json::json!([workflow_id.0.to_string()])
    );
}

#[tokio::test]
async fn impact_omits_the_url_of_a_page_with_no_known_url() {
    let rig = rig().await;
    let session = rig
        .runtime
        .create_session(CreateSessionRequest {
            profile: "blank".into(),
            proxy: None,
            execution_policy: ExecutionPolicy::default(),
            zigzagzig: false,
        })
        .await
        .unwrap();
    let page = rig
        .runtime
        .pages
        .open(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await;
    rig.runtime
        .sessions
        .add_page(&session.id, page.id.clone())
        .await
        .unwrap();
    let body = rig.impact_json().await;
    let page_view = &body["sessions"][0]["pages"][0];
    assert_eq!(page_view["pageId"], page.id.0.to_string());
    assert!(page_view.get("url").is_none());
}

#[tokio::test]
async fn connections_count_live_gateway_sockets() {
    let rig = rig().await;
    assert_eq!(rig.impact_json().await["connections"], 0);
    let mut request = format!("ws://{}/v1/gateway/mcp", rig.address)
        .into_client_request()
        .unwrap();
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", rig.bearer)).unwrap(),
    );
    let (mut socket, _) = tokio_tungstenite::connect_async(request).await.unwrap();
    assert_eq!(settled_connections(&rig, 1).await, 1);
    // The requesting HTTP call itself is not a gateway connection.
    assert_eq!(rig.impact_json().await["connections"], 1);
    drop(socket.next());
    drop(socket);
    assert_eq!(settled_connections(&rig, 0).await, 0);
}

async fn settled_connections(rig: &Rig, want: u64) -> u64 {
    let mut seen = u64::MAX;
    for _ in 0..200 {
        seen = rig.impact_json().await["connections"].as_u64().unwrap();
        if seen == want {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    seen
}

fn checkpoint(session_id: &SessionId, workflow_id: &WorkflowId, url: &str) -> WorkflowCheckpoint {
    WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: CheckpointId::new(),
        workflow_id: workflow_id.clone(),
        attempt_id: AttemptId::new(),
        session_id: session_id.clone(),
        page_id: PageId::new(),
        restart_url: url.into(),
        current_url: url.into(),
        cursor: None,
        boundary_command_id: None,
        recovery_class: CommandClass::Reconciliable,
        invariants: Vec::new(),
        replayable_inputs: Vec::new(),
        evidence: Vec::new(),
        recovery_history: Vec::new(),
        recovery_receipts: Vec::new(),
        created_at: Utc::now(),
    }
}
