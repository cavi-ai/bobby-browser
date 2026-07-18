#![cfg(unix)]

use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use artifact_store::ArtifactStore;
use async_trait::async_trait;
use chrono::{Duration, Utc};
use interface_core::{
    ArtifactOwnershipLimits, ArtifactReader, AuthorityStore, EventStore, SessionOwnershipAuthority,
};
use mcp_gateway::{ArtifactResources, Server};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use types::{Capability, PageId, PrincipalId, SessionId};
use uuid::uuid;

#[derive(Default)]
struct Ownership(RwLock<HashMap<SessionId, PrincipalId>>);

#[derive(Clone)]
struct ArtifactOutcomeRuntime {
    artifact_id: String,
}

#[async_trait]
impl interface_core::RuntimeInterface for ArtifactOutcomeRuntime {
    async fn runtime_info(
        &self,
        _: types::RequestContext,
    ) -> interface_core::InterfaceResult<types::RuntimeInfo> {
        unreachable!()
    }
    async fn list_sessions(
        &self,
        _: types::RequestContext,
    ) -> interface_core::InterfaceResult<Vec<types::SessionState>> {
        unreachable!()
    }
    async fn create_session(
        &self,
        _: types::RequestContext,
        _: types::CreateSessionRequest,
    ) -> interface_core::InterfaceResult<types::SessionState> {
        unreachable!()
    }
    async fn open_page(
        &self,
        _: types::RequestContext,
        _: types::OpenPageRequest,
    ) -> interface_core::InterfaceResult<types::PageState> {
        unreachable!()
    }
    async fn submit(
        &self,
        _: types::RequestContext,
        envelope: types::CommandEnvelope,
    ) -> interface_core::InterfaceResult<types::CommandOutcome> {
        Ok(types::CommandOutcome::Completed {
            command_id: envelope.command_id,
            evidence: vec![types::Evidence::Screenshot {
                artifact_id: self.artifact_id.clone(),
                media_type: "text/plain".to_owned(),
                width: 1,
                height: 1,
                bytes: 12,
                sha256: "0".repeat(64),
            }],
        })
    }
    async fn checkpoint(
        &self,
        _: types::RequestContext,
        _: types::WorkflowCheckpoint,
        _: Vec<types::Evidence>,
    ) -> interface_core::InterfaceResult<types::WorkflowCheckpoint> {
        unreachable!()
    }
    async fn recover(
        &self,
        _: types::RequestContext,
        _: types::WorkflowId,
    ) -> interface_core::InterfaceResult<types::RecoveryDecision> {
        unreachable!()
    }
}

impl SessionOwnershipAuthority for Ownership {
    fn owns_session(&self, principal: &PrincipalId, session: &SessionId) -> bool {
        self.0
            .read()
            .unwrap()
            .get(session)
            .is_some_and(|owner| owner == principal)
    }
}

async fn fixture() -> (Server, String, tempfile::TempDir) {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(root.path(), 4096, 4096);
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000026")),
            [
                Capability::SessionRead,
                Capability::ArtifactRead,
                Capability::ArtifactCapture,
            ],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let context = handle.context(Utc::now() + Duration::minutes(1), None);
    let session = SessionId::new();
    let ownership = Arc::new(Ownership::default());
    ownership
        .0
        .write()
        .unwrap()
        .insert(session.clone(), context.principal_id.clone());
    let reader = ArtifactReader::new(
        store.clone(),
        ownership,
        4096,
        ArtifactOwnershipLimits {
            max_records: 8,
            max_bytes: 32 * 1024,
        },
    )
    .unwrap();
    let record = store
        .put(
            &session,
            &PageId::new(),
            "text/plain",
            "txt",
            b"trusted text",
            4096,
        )
        .await
        .unwrap();
    let reference = reader
        .register(&handle, &context, &session, &record)
        .await
        .unwrap();
    let artifact_id = reference.artifact_id().to_owned();
    let resources = ArtifactResources::new(reader, 8);
    resources
        .register_trusted(session, reference)
        .await
        .unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(
        RuntimeService::default(),
        handle.clone(),
    ));
    let server = Server::new(runtime, handle).with_boundaries(EventStore::new(8), resources);
    initialize(&server).await;
    (server, artifact_id, root)
}

async fn initialize(server: &Server) {
    server
        .handle_message(request(
            1,
            "initialize",
            json!({
                "protocolVersion":"2025-11-25","capabilities":{},
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

#[tokio::test]
async fn trusted_artifacts_list_and_read_in_deterministic_resource_form() {
    let (server, artifact_id, _root) = fixture().await;
    let listed = server
        .handle_message(request(2, "resources/list", json!({})))
        .await
        .unwrap();
    assert_eq!(
        listed["result"]["resources"][0]["uri"],
        format!("artifact://{artifact_id}")
    );

    let read = server
        .handle_message(request(
            3,
            "resources/read",
            json!({"uri":format!("artifact://{artifact_id}")}),
        ))
        .await
        .unwrap();
    assert_eq!(read["result"]["contents"][0]["text"], "trusted text");
    assert_eq!(read["result"]["contents"][0]["mimeType"], "text/plain");
}

#[tokio::test]
async fn resource_uris_reject_paths_queries_fragments_and_authority_changes() {
    let (server, artifact_id, _root) = fixture().await;
    for (index, uri) in [
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
                json!({"uri":uri}),
            ))
            .await
            .unwrap();
        assert_eq!(response["error"]["code"], -32602, "{response}");
    }
}

#[tokio::test]
async fn command_results_link_only_artifacts_present_in_the_trusted_catalog() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(root.path(), 4096, 4096);
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-000000000027")),
            [
                Capability::BrowserMutate,
                Capability::ArtifactRead,
                Capability::ArtifactCapture,
            ],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let context = handle.context(Utc::now() + Duration::minutes(1), None);
    let session = SessionId::new();
    let ownership = Arc::new(Ownership::default());
    ownership
        .0
        .write()
        .unwrap()
        .insert(session.clone(), context.principal_id.clone());
    let reader = ArtifactReader::new(
        store.clone(),
        ownership,
        4096,
        ArtifactOwnershipLimits {
            max_records: 8,
            max_bytes: 32 * 1024,
        },
    )
    .unwrap();
    let record = store
        .put(
            &session,
            &PageId::new(),
            "text/plain",
            "txt",
            b"trusted text",
            4096,
        )
        .await
        .unwrap();
    let reference = reader
        .register(&handle, &context, &session, &record)
        .await
        .unwrap();
    let artifact_id = reference.artifact_id().to_owned();
    let resources = ArtifactResources::new(reader, 8);
    resources
        .register_trusted(session.clone(), reference)
        .await
        .unwrap();
    let server = Server::new(
        Arc::new(ArtifactOutcomeRuntime {
            artifact_id: artifact_id.clone(),
        }),
        handle,
    )
    .with_boundaries(EventStore::new(8), resources);
    initialize(&server).await;
    let envelope = types::CommandEnvelope {
        schema_version: types::CommandEnvelope::SCHEMA_VERSION,
        command_id: types::CommandId::new(),
        workflow_id: types::WorkflowId::new(),
        attempt_id: types::AttemptId::new(),
        session_id: session,
        page_id: None,
        deadline: Utc::now() + Duration::seconds(30),
        command: types::PrimitiveCommand::ListPages(types::ListPagesCommand),
    };
    let response = server
        .handle_message(request(
            50,
            "tools/call",
            json!({"name":"command_execute","arguments":{"envelope":envelope}}),
        ))
        .await
        .unwrap();
    let links = response["result"]["content"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["type"] == "resource_link")
        .collect::<Vec<_>>();
    assert_eq!(links.len(), 1, "{response}");
    assert_eq!(links[0]["uri"], format!("artifact://{artifact_id}"));
}
