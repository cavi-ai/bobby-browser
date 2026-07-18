#![cfg(unix)]

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use artifact_store::ArtifactStore;
use async_trait::async_trait;
use chrono::{Duration, Utc};
use interface_core::{
    ArtifactOwnershipLimits, ArtifactReader, AuthorityStore, EventStore, SessionOwnershipRegistry,
};
use mcp_gateway::{ArtifactResources, Server};
use page_runtime::PageRuntime;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use session_manager::SessionManager;
use types::{
    AttemptId, Capability, CaptureScreenshotCommand, ClickCommand, CommandEnvelope, CommandError,
    CommandId, ErrorLayer, Evidence, InspectCommand, NavigateCommand, PageId, PrimitiveCommand,
    PrincipalId, SessionId, TypeTextCommand, WorkerId, WorkflowId,
};
use uuid::uuid;
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};
use workflow_journal::JsonlJournal;

const ONE_PIXEL_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137,
];

struct ScreenshotWorker {
    id: WorkerId,
    profile: PathBuf,
    session_id: SessionId,
    artifacts: ArtifactStore,
}

#[async_trait]
impl BrowserWorker for ScreenshotWorker {
    fn worker_id(&self) -> WorkerId {
        self.id.clone()
    }
    fn profile_dir(&self) -> &Path {
        &self.profile
    }
    async fn open_page(&self, _: PageId) -> Result<(), CommandError> {
        Ok(())
    }
    async fn navigate(
        &self,
        _: &PageId,
        _: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![])
    }
    async fn inspect(&self, _: &PageId, _: &InspectCommand) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![])
    }
    async fn click(&self, _: &PageId, _: &ClickCommand) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![])
    }
    async fn type_text(
        &self,
        _: &PageId,
        _: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![])
    }
    async fn capture_screenshot(
        &self,
        page_id: &PageId,
        _: &CaptureScreenshotCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        let record = self
            .artifacts
            .put_png(&self.session_id, page_id, ONE_PIXEL_PNG)
            .await
            .map_err(|_| CommandError {
                code: types::ErrorCode::BrowserCommandFailed,
                message: "fixture artifact write failed".to_owned(),
                layer: ErrorLayer::Driver,
                retryable: false,
            })?;
        Ok(vec![Evidence::Screenshot {
            artifact_id: record.artifact_id,
            media_type: record.media_type,
            width: record.width,
            height: record.height,
            bytes: record.bytes,
            sha256: record.sha256,
        }])
    }
    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }
}

struct ScreenshotFactory {
    profile: PathBuf,
    artifacts: ArtifactStore,
}

#[async_trait]
impl WorkerFactory for ScreenshotFactory {
    async fn launch(&self, session_id: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(Arc::new(ScreenshotWorker {
            id: WorkerId::new(),
            profile: self.profile.clone(),
            session_id: session_id.clone(),
            artifacts: self.artifacts.clone(),
        }))
    }
}

async fn fixture() -> (Server, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let artifacts = ArtifactStore::new(root.path().join("artifacts"), 4096, 4096);
    let journal = Arc::new(
        JsonlJournal::open(root.path().join("journal.jsonl"))
            .await
            .unwrap(),
    );
    let workers = Arc::new(WorkerPool::new(
        1,
        Arc::new(ScreenshotFactory {
            profile: root.path().join("profile"),
            artifacts: artifacts.clone(),
        }),
    ));
    let runtime = RuntimeService::new(
        SessionManager::new(workers.clone()),
        PageRuntime::new(journal, workers),
    );
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000026")),
            [
                Capability::SessionRead,
                Capability::SessionWrite,
                Capability::PageWrite,
                Capability::BrowserMutate,
                Capability::ArtifactRead,
                Capability::ArtifactCapture,
            ],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let (ownership, recorder) = SessionOwnershipRegistry::bounded(4);
    let reader = ArtifactReader::new(
        artifacts,
        ownership,
        4096,
        ArtifactOwnershipLimits {
            max_records: 8,
            max_bytes: 32 * 1024,
        },
    )
    .unwrap();
    let authenticated = Arc::new(AuthenticatedRuntime::with_session_ownership(
        runtime, handle, recorder,
    ));
    let server = Server::production(
        authenticated,
        EventStore::new(8),
        ArtifactResources::new(reader, 8),
    );
    initialize(&server).await;
    (server, root)
}

async fn initialize(server: &Server) {
    server
        .handle_message(request(
            1,
            "initialize",
            json!({
                "protocolVersion":"2025-11-25", "capabilities":{},
                "clientInfo":{"name":"test","version":"1"}
            }),
        ))
        .await;
    server
        .handle_message(json!({
            "jsonrpc":"2.0","method":"notifications/initialized","params":{}
        }))
        .await;
}

fn request(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}

async fn capture(server: &Server) -> String {
    let session = server
        .handle_message(request(
            2,
            "tools/call",
            json!({
                "name":"session_create","arguments":{"profile":"fixture"}
            }),
        ))
        .await
        .unwrap();
    let session_id: SessionId =
        serde_json::from_value(session["result"]["structuredContent"]["id"].clone()).unwrap();
    let page = server
        .handle_message(request(
            3,
            "tools/call",
            json!({
                "name":"page_open","arguments":{"sessionId":session_id}
            }),
        ))
        .await
        .unwrap();
    let page_id: PageId =
        serde_json::from_value(page["result"]["structuredContent"]["id"].clone()).unwrap();
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id,
        page_id: Some(page_id),
        deadline: Utc::now() + Duration::seconds(30),
        command: PrimitiveCommand::CaptureScreenshot(CaptureScreenshotCommand {
            mode: types::ScreenshotMode::Viewport,
        }),
    };
    let captured = server
        .handle_message(request(
            4,
            "tools/call",
            json!({
                "name":"command_execute","arguments":{"envelope":envelope}
            }),
        ))
        .await
        .unwrap();
    let link = captured["result"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["type"] == "resource_link")
        .expect("production registration emits a resource link");
    link["uri"].as_str().unwrap().to_owned()
}

#[tokio::test]
async fn production_constructor_registers_runtime_artifact_then_lists_and_reads_it() {
    let (server, _root) = fixture().await;
    let uri = capture(&server).await;
    let listed = server
        .handle_message(request(5, "resources/list", json!({})))
        .await
        .unwrap();
    assert_eq!(listed["result"]["resources"][0]["uri"], uri);
    let read = server
        .handle_message(request(6, "resources/read", json!({"uri":uri})))
        .await
        .unwrap();
    assert_eq!(read["result"]["contents"][0]["mimeType"], "image/png");
    assert!(
        read["result"]["contents"][0]["blob"]
            .as_str()
            .unwrap()
            .len()
            < 128
    );
}

#[tokio::test]
async fn resource_uris_reject_paths_queries_fragments_and_authority_changes() {
    let (server, _root) = fixture().await;
    let uri = capture(&server).await;
    let artifact_id = uri.strip_prefix("artifact://").unwrap();
    for (index, invalid) in [
        format!("artifact://{artifact_id}/child"),
        format!("artifact://{artifact_id}?session=other"),
        format!("artifact://{artifact_id}#fragment"),
        format!("artifact://other@{artifact_id}"),
        "file:///etc/passwd".to_owned(),
    ]
    .into_iter()
    .enumerate()
    {
        let response = server
            .handle_message(request(
                10 + index as u64,
                "resources/read",
                json!({"uri":invalid}),
            ))
            .await
            .unwrap();
        assert_eq!(response["error"]["code"], -32602, "{response}");
    }
}
