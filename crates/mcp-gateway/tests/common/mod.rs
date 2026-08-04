//! Shared live-runtime harness for gateway integration tests.
//!
//! `RuntimeService::default()` has no journal, no worker pool, and no
//! recovery coordinator, so every command dispatched through it fails
//! downstream — tests built on it can only ever exercise auth and schema
//! validation. This harness wires the real `RuntimeService` (journal,
//! worker pool, recovery coordinator) over a fake worker that returns real
//! evidence per command kind, so tests assert actual outcomes.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use interface_core::CapabilityHandle;
use mcp_gateway::Server;
use page_runtime::{PageRuntime, RecoveryCoordinator};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use session_manager::SessionManager;
use tempfile::TempDir;
use types::{
    ClickCommand, CommandError, Evidence, InspectCommand, ListPagesCommand, NavigateCommand,
    PageEvidence, PageId, SessionId, TypeTextCommand, WorkerId,
};
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};
use workflow_journal::JsonlJournal;

/// A fake worker that returns the evidence variant each command verifies
/// against, so outcomes reach `completed` instead of failing verification.
pub struct LiveWorker {
    id: WorkerId,
    profile: PathBuf,
}

#[async_trait::async_trait]
impl BrowserWorker for LiveWorker {
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
        Ok(vec![Evidence::Navigation {
            url: command.url.clone(),
            title: "live-harness".into(),
        }])
    }

    async fn inspect(
        &self,
        _: &PageId,
        command: &InspectCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Inspection {
            selector: command.selector.clone(),
            url: "https://live-harness.test/".into(),
            title: "live-harness".into(),
            text: "live-harness-text".into(),
            html: None,
        }])
    }

    async fn click(
        &self,
        _: &PageId,
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Element {
            selector: command.selector.clone(),
            text: None,
        }])
    }

    async fn type_text(
        &self,
        _: &PageId,
        command: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Element {
            selector: command.selector.clone(),
            text: Some(command.value.clone()),
        }])
    }

    async fn list_pages(&self, _: &ListPagesCommand) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Pages {
            pages: vec![PageEvidence {
                page_id: PageId::new(),
                url: "https://live-harness.test/".into(),
                title: "live-harness".into(),
            }],
        }])
    }

    async fn collect_candidates(
        &self,
        _: &PageId,
        _: &types::TargetSpec,
    ) -> Result<Vec<dom_engine::Candidate>, CommandError> {
        // No DOM behind the fake: intents that must resolve a target fail
        // with the engine's own domain error (e.g. targetNotFound), which is
        // exactly the deterministic terminal outcome tests assert against.
        Ok(Vec::new())
    }

    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }
}

pub struct LiveFactory;

#[async_trait::async_trait]
impl WorkerFactory for LiveFactory {
    async fn launch(&self, session_id: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(Arc::new(LiveWorker {
            id: WorkerId::new(),
            profile: PathBuf::from(format!("/profiles/{}", session_id.0)),
        }))
    }
}

/// A gateway server over a fully wired runtime, plus the journal every
/// command writes and the tempdir holding it (dropped when the caller drops
/// it).
pub struct LiveServer {
    pub server: Server,
    pub journal: Arc<JsonlJournal>,
    _root: TempDir,
}

pub async fn live_server(handle: CapabilityHandle) -> LiveServer {
    let root = tempfile::tempdir().expect("harness tempdir");
    let journal = Arc::new(
        JsonlJournal::open(root.path().join("journal.jsonl"))
            .await
            .expect("harness journal opens"),
    );
    let workers = Arc::new(WorkerPool::new(4, Arc::new(LiveFactory)));
    let checkpoints = checkpoint_store::CheckpointStore::open(root.path().join("checkpoints"))
        .await
        .expect("harness checkpoint store opens");
    let runtime = RuntimeService::with_recovery(
        SessionManager::new(workers.clone()),
        PageRuntime::new(journal.clone(), workers),
        RecoveryCoordinator::new(checkpoints),
    );
    LiveServer {
        server: Server::new(Arc::new(AuthenticatedRuntime::new(runtime, handle))),
        journal,
        _root: root,
    }
}

pub async fn initialize(server: &Server) {
    server
        .handle_message(serde_json::json!({
            "jsonrpc":"2.0","id":1,"method":"initialize",
            "params":{"protocolVersion":"2025-11-25","capabilities":{},
                      "clientInfo":{"name":"harness","version":"1"}}
        }))
        .await;
    server
        .handle_message(serde_json::json!({
            "jsonrpc":"2.0","method":"notifications/initialized","params":{}
        }))
        .await;
}

pub fn request(id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
    serde_json::json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}

/// Create a session and open a page through the real runtime; both ids are
/// owned by the caller's principal and safe to put in envelopes.
pub async fn create_session_and_page(server: &Server, next_id: &mut u64) -> (SessionId, PageId) {
    *next_id += 1;
    let created = server
        .handle_message(request(
            *next_id,
            "tools/call",
            serde_json::json!({"name":"session_create","arguments":{"profile":"harness"}}),
        ))
        .await
        .expect("session_create answered");
    let session_id = created["result"]["structuredContent"]["id"]
        .as_str()
        .expect("session id")
        .to_owned();
    *next_id += 1;
    let opened = server
        .handle_message(request(
            *next_id,
            "tools/call",
            serde_json::json!({
                "name":"page_open",
                "arguments":{"sessionId":session_id}
            }),
        ))
        .await
        .expect("page_open answered");
    let page_id = opened["result"]["structuredContent"]["id"]
        .as_str()
        .expect("page id")
        .to_owned();
    (
        SessionId(uuid::Uuid::parse_str(&session_id).expect("session uuid")),
        PageId(uuid::Uuid::parse_str(&page_id).expect("page uuid")),
    )
}

pub async fn execute_envelope(
    server: &Server,
    next_id: &mut u64,
    envelope: &types::CommandEnvelope,
) -> serde_json::Value {
    *next_id += 1;
    server
        .handle_message(request(
            *next_id,
            "tools/call",
            serde_json::json!({
                "name":"command_execute",
                "arguments":{"envelope":envelope}
            }),
        ))
        .await
        .expect("command_execute answered")
}

/// The harness DOM has no candidates, so a target-resolving intent must fail
/// with the engine's own deterministic domain outcome -- proof the envelope
/// executed through the real runtime rather than merely passing validation.
pub fn assert_intent_domain_failure(
    response: &serde_json::Value,
    envelope: &types::CommandEnvelope,
    intent_kind: &str,
) {
    let content = &response["result"]["structuredContent"];
    assert!(response["error"].is_null(), "{response}");
    assert_eq!(
        content["commandId"],
        envelope.command_id.0.to_string(),
        "{response}"
    );
    assert_eq!(content["status"], "failed", "{response}");
    assert_eq!(response["result"]["isError"], true, "{response}");
    let record = content["evidence"]
        .as_array()
        .expect("evidence is an array")
        .iter()
        .find(|item| item["kind"] == "intentExecution")
        .unwrap_or_else(|| panic!("no intentExecution evidence: {response}"))["record"]
        .clone();
    assert_eq!(record["intentKind"], intent_kind, "{response}");
    assert_eq!(record["resolutionPath"], "deterministic", "{response}");
    assert_eq!(record["candidates"], serde_json::json!([]), "{response}");
    let verification = record["verification"].as_str().unwrap_or_default();
    assert!(
        verification.starts_with("target"),
        "expected a target-resolution failure, got {verification}: {response}"
    );
}
