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
use sha2::{Digest, Sha256};
use types::{
    AttemptId, Capability, CaptureScreenshotCommand, ClickAndWaitForDownloadCommand, ClickCommand,
    ClosePageCommand, CommandEnvelope, CommandError, CommandId, ErrorLayer, Evidence,
    InspectCommand, ListPagesCommand, NavigateCommand, PageId, PrimitiveCommand, PrincipalId,
    RuntimeCommand, SessionId, TypeTextCommand, WorkerId, WorkflowId,
};
use uuid::uuid;
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};
use workflow_journal::JsonlJournal;

const ONE_PIXEL_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137,
];
const DOWNLOAD_BYTES: &[u8] = b"private fixture download";

struct ScreenshotWorker {
    id: WorkerId,
    profile: PathBuf,
    session_id: SessionId,
    artifacts: ArtifactStore,
    download_path: PathBuf,
    screenshot_count: usize,
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
        command: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        if command.url.ends_with("/fail") {
            return Err(fixture_error("fixture navigation failed"));
        }
        Ok(vec![Evidence::Navigation {
            url: command.url.clone(),
            title: "Fixture page".to_owned(),
        }])
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
        let mut evidence = Vec::new();
        for _ in 0..self.screenshot_count {
            let record = self
                .artifacts
                .put_png(&self.session_id, page_id, ONE_PIXEL_PNG)
                .await
                .map_err(|_| fixture_error("fixture artifact write failed"))?;
            evidence.push(Evidence::Screenshot {
                artifact_id: record.artifact_id,
                media_type: record.media_type,
                width: record.width,
                height: record.height,
                bytes: record.bytes,
                sha256: record.sha256,
            });
        }
        Ok(evidence)
    }
    async fn click_and_wait_for_download(
        &self,
        _: &PageId,
        _: &ClickAndWaitForDownloadCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Download {
            filename: "fixture.txt".to_owned(),
            path: self.download_path.to_string_lossy().into_owned(),
            bytes: DOWNLOAD_BYTES.len() as u64,
            sha256: hex::encode(Sha256::digest(DOWNLOAD_BYTES)),
            saved_to: None,
        }])
    }
    async fn close_page_command(
        &self,
        command: &ClosePageCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Page {
            page_id: command.page_id.clone(),
            url: "about:blank".to_owned(),
            title: "Fixture page".to_owned(),
        }])
    }
    async fn list_pages(&self, _: &ListPagesCommand) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Pages { pages: vec![] }])
    }
    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }
}

struct ScreenshotFactory {
    profile: PathBuf,
    artifacts: ArtifactStore,
    downloads: PathBuf,
    screenshot_count: usize,
}

#[async_trait]
impl WorkerFactory for ScreenshotFactory {
    async fn launch(&self, session_id: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        let download_dir = self.downloads.join(session_id.0.to_string());
        tokio::fs::create_dir_all(&download_dir)
            .await
            .map_err(|_| fixture_error("fixture download directory failed"))?;
        let download_path = download_dir.join("fixture.txt");
        tokio::fs::write(&download_path, DOWNLOAD_BYTES)
            .await
            .map_err(|_| fixture_error("fixture download write failed"))?;
        Ok(Arc::new(ScreenshotWorker {
            id: WorkerId::new(),
            profile: self.profile.clone(),
            session_id: session_id.clone(),
            artifacts: self.artifacts.clone(),
            download_path,
            screenshot_count: self.screenshot_count,
        }))
    }
}

async fn fixture() -> (Server, tempfile::TempDir) {
    fixture_with(1, 8).await
}

async fn fixture_with(
    screenshot_count: usize,
    catalog_entries: usize,
) -> (Server, tempfile::TempDir) {
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
            downloads: root.path().join("downloads"),
            screenshot_count,
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
                Capability::FileDownload,
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
        artifacts.clone(),
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
        ArtifactResources::production(
            reader,
            artifacts,
            root.path().join("downloads"),
            4096,
            catalog_entries,
        ),
    );
    initialize(&server).await;
    (server, root)
}

fn fixture_error(message: &str) -> CommandError {
    CommandError {
        code: types::ErrorCode::BrowserCommandFailed,
        message: message.to_owned(),
        layer: ErrorLayer::Driver,
        retryable: false,
    }
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

async fn create_session_page(server: &Server) -> (SessionId, PageId) {
    let session_id = create_session(server).await;
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
    (session_id, page_id)
}

async fn create_session(server: &Server) -> SessionId {
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
    serde_json::from_value(session["result"]["structuredContent"]["id"].clone()).unwrap()
}

#[tokio::test]
async fn page_open_accepts_a_url_and_returns_the_navigation_outcome() {
    let (server, _root) = fixture().await;
    let session_id = create_session(&server).await;

    let response = server
        .handle_message(request(
            3,
            "tools/call",
            json!({
                "name":"page_open","arguments":{
                    "sessionId":session_id,
                    "url":"https://example.test/jobs"
                }
            }),
        ))
        .await
        .unwrap();

    assert!(response.get("error").is_none(), "{response}");
    assert!(response["result"]["structuredContent"]["id"].is_string());
    assert_eq!(
        response["result"]["structuredContent"]["navigationOutcome"]["status"],
        json!("completed")
    );
    assert_eq!(
        response["result"]["structuredContent"]["navigationOutcome"]["evidence"][0]["url"],
        json!("https://example.test/jobs")
    );
    assert_eq!(
        response["result"]["structuredContent"]["url"],
        json!("https://example.test/jobs"),
        "page url must reflect the completed navigation, not the pre-navigation open: {response}"
    );
    assert_eq!(
        response["result"]["structuredContent"]["ready_state"],
        json!("interactive"),
        "page ready_state must reflect the completed navigation: {response}"
    );
}

#[tokio::test]
async fn page_open_closes_the_new_page_when_initial_navigation_fails() {
    let (server, _root) = fixture().await;
    let session_id = create_session(&server).await;

    let response = server
        .handle_message(request(
            3,
            "tools/call",
            json!({
                "name":"page_open","arguments":{
                    "sessionId":session_id,
                    "url":"https://example.test/fail"
                }
            }),
        ))
        .await
        .unwrap();

    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(
        response["result"]["structuredContent"]["navigationOutcome"]["status"],
        json!("failed")
    );
    assert_eq!(
        response["result"]["structuredContent"]["cleanupOutcome"]["status"],
        json!("completed")
    );
    assert_eq!(
        response["result"]["structuredContent"]["pageClosed"],
        json!(true)
    );
}

#[tokio::test]
async fn command_execute_list_pages_succeeds_without_a_page_id() {
    let (server, _root) = fixture().await;
    let session_id = create_session(&server).await;

    let response = server
        .handle_message(request(
            3,
            "tools/call",
            json!({
                "name":"command_execute","arguments":{"envelope":{
                    "schemaVersion":2,
                    "commandId":"10000000-0000-0000-0000-000000000121",
                    "workflowId":"10000000-0000-0000-0000-000000000122",
                    "attemptId":"10000000-0000-0000-0000-000000000123",
                    "sessionId":session_id,
                    "pageId":null,
                    "deadline":(Utc::now() + Duration::seconds(270)).to_rfc3339(),
                    "command":{"kind":"primitive","input":{"kind":"listPages","input":null}}
                }}
            }),
        ))
        .await
        .unwrap();

    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(
        response["result"]["structuredContent"]["status"],
        json!("completed"),
        "{response}"
    );
    assert_eq!(
        response["result"]["structuredContent"]["evidence"][0]["kind"],
        json!("pages")
    );
}

async fn execute_screenshot(server: &Server) -> Value {
    let (session_id, page_id) = create_session_page(server).await;
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id,
        page_id: Some(page_id),
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Primitive(PrimitiveCommand::CaptureScreenshot(
            CaptureScreenshotCommand {
                mode: types::ScreenshotMode::Viewport,
            },
        )),
    };
    server
        .handle_message(request(
            4,
            "tools/call",
            json!({
                "name":"command_execute","arguments":{"envelope":envelope}
            }),
        ))
        .await
        .unwrap()
}

async fn capture(server: &Server) -> String {
    let captured = execute_screenshot(server).await;
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
    let listed_uris: Vec<String> = listed["result"]["resources"]
        .as_array()
        .unwrap()
        .iter()
        .map(|resource| resource["uri"].as_str().unwrap().to_owned())
        .collect();
    assert!(
        listed_uris.contains(&uri),
        "captured artifact not listed: {listed_uris:?}"
    );
    for expected in [
        "bobby://capabilities",
        "bobby://failure-taxonomy",
        "bobby://intents",
        "bobby://primitives",
        "bobby://job-handlers",
    ] {
        assert!(
            listed_uris.contains(&expected.to_owned()),
            "{expected} not listed alongside the captured artifact: {listed_uris:?}"
        );
    }
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

#[tokio::test]
async fn download_paths_are_replaced_by_authenticated_resources_and_never_exposed() {
    let (server, root) = fixture().await;
    let (session_id, page_id) = create_session_page(&server).await;
    let envelope = CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id,
        page_id: Some(page_id),
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Primitive(PrimitiveCommand::ClickAndWaitForDownload(
            ClickAndWaitForDownloadCommand {
                selector: "#download".to_owned(),
                target: None,
                timeout_ms: 1_000,
            },
        )),
    };
    let response = server
        .handle_message(request(
            30,
            "tools/call",
            json!({"name":"command_execute","arguments":{"envelope":envelope}}),
        ))
        .await
        .unwrap();
    let serialized = response.to_string();
    assert!(
        !serialized.contains(&root.path().to_string_lossy().into_owned()),
        "{response}"
    );
    let path = response["result"]["structuredContent"]["evidence"][0]["path"]
        .as_str()
        .expect("download path replacement");
    assert!(path.starts_with("artifact://"), "{response}");
    assert!(response["result"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .any(|item| item["type"] == "resource_link" && item["uri"] == path));
    let read = server
        .handle_message(request(31, "resources/read", json!({"uri":path})))
        .await
        .unwrap();
    assert_eq!(
        read["result"]["contents"][0]["mimeType"],
        "application/octet-stream"
    );
}

#[tokio::test]
async fn artifact_admission_failure_after_completed_command_never_becomes_retryable_error() {
    let (server, _root) = fixture_with(2, 1).await;
    let response = execute_screenshot(&server).await;
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(response["result"]["isError"], false, "{response}");
    assert_eq!(
        response["result"]["structuredContent"]["status"],
        "completed"
    );
    let diagnostic = &response["result"]["structuredContent"]["artifactRegistration"];
    assert_eq!(diagnostic["status"], "partial", "{response}");
    assert_eq!(diagnostic["reconciliationRequired"], true);
    assert!(diagnostic["commandId"].is_string());
    assert_eq!(diagnostic["retryable"], false);
}
