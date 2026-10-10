//! An MCP connection that ends closes the sessions it opened and had not
//! closed, and only those.

use std::{sync::Arc, time::Duration as StdDuration};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use interface_core::{
    AuthorityStore, CapabilityHandle, EventStore, InterfaceResult, RuntimeInterface,
};
use mcp_gateway::{ArtifactResources, Server};
use page_runtime::PageRuntime;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use serde_json::{json, Value};
use session_manager::SessionManager;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream},
    sync::{Mutex, Notify},
    task::JoinHandle,
};
use types::{Capability, PrincipalId, SessionId};
use uuid::uuid;
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};
use workflow_journal::JsonlJournal;

/// Forwards every required method to the real runtime, records deletes, and
/// can hold `create_session` after the session exists.
struct Spy {
    inner: Arc<AuthenticatedRuntime>,
    deletes: Mutex<Vec<SessionId>>,
    created: Arc<Notify>,
    release: Option<Arc<Notify>>,
}

#[async_trait]
impl RuntimeInterface for Spy {
    async fn runtime_info(
        &self,
        ctx: types::RequestContext,
    ) -> InterfaceResult<types::RuntimeInfo> {
        self.inner.runtime_info(ctx).await
    }
    async fn list_sessions(
        &self,
        ctx: types::RequestContext,
    ) -> InterfaceResult<Vec<types::SessionState>> {
        self.inner.list_sessions(ctx).await
    }
    async fn delete_session(
        &self,
        ctx: types::RequestContext,
        session: SessionId,
    ) -> InterfaceResult<()> {
        self.deletes.lock().await.push(session.clone());
        self.inner.delete_session(ctx, session).await
    }
    async fn create_session(
        &self,
        ctx: types::RequestContext,
        req: types::CreateSessionRequest,
    ) -> InterfaceResult<types::SessionState> {
        let session = self.inner.create_session(ctx, req).await?;
        self.created.notify_one();
        if let Some(release) = &self.release {
            release.notified().await;
        }
        Ok(session)
    }
    async fn open_page(
        &self,
        ctx: types::RequestContext,
        req: types::OpenPageRequest,
    ) -> InterfaceResult<types::PageState> {
        self.inner.open_page(ctx, req).await
    }
    async fn submit(
        &self,
        ctx: types::RequestContext,
        envelope: types::CommandEnvelope,
    ) -> InterfaceResult<types::CommandOutcome> {
        self.inner.submit(ctx, envelope).await
    }
    async fn checkpoint(
        &self,
        ctx: types::RequestContext,
        checkpoint: types::WorkflowCheckpoint,
        evidence: Vec<types::Evidence>,
    ) -> InterfaceResult<types::WorkflowCheckpoint> {
        self.inner.checkpoint(ctx, checkpoint, evidence).await
    }
    async fn resolve_command_evidence(
        &self,
        ctx: types::RequestContext,
        command_ids: Vec<types::CommandId>,
    ) -> InterfaceResult<Vec<types::Evidence>> {
        self.inner.resolve_command_evidence(ctx, command_ids).await
    }
    async fn recover(
        &self,
        ctx: types::RequestContext,
        workflow: types::WorkflowId,
    ) -> InterfaceResult<types::RecoveryDecision> {
        self.inner.recover(ctx, workflow).await
    }
    async fn recovery_status(
        &self,
        ctx: types::RequestContext,
        workflow: types::WorkflowId,
    ) -> InterfaceResult<types::RecoveryStatus> {
        self.inner.recovery_status(ctx, workflow).await
    }
    async fn submit_with_auto_checkpoint(
        &self,
        ctx: types::RequestContext,
        envelope: types::CommandEnvelope,
    ) -> InterfaceResult<(types::CommandOutcome, Option<types::CheckpointId>)> {
        self.inner.submit_with_auto_checkpoint(ctx, envelope).await
    }
    async fn workflows_for_session(
        &self,
        ctx: types::RequestContext,
        session: SessionId,
        limit: usize,
    ) -> InterfaceResult<Vec<types::WorkflowId>> {
        self.inner.workflows_for_session(ctx, session, limit).await
    }
}

struct Rig {
    handle: CapabilityHandle,
    /// Same `RuntimeService` state as every connection, observed from outside.
    observer: AuthenticatedRuntime,
    spy: Arc<Spy>,
    server: Arc<Server>,
    service: RuntimeService,
    _scratch: tempfile::TempDir,
}

/// A worker that opens pages without a browser, so `workflow_start` without a
/// url completes.
struct NoBrowserWorker {
    profile: std::path::PathBuf,
}

#[async_trait]
impl BrowserWorker for NoBrowserWorker {
    fn worker_id(&self) -> types::WorkerId {
        types::WorkerId::new()
    }

    fn profile_dir(&self) -> &std::path::Path {
        &self.profile
    }

    async fn close(&self) -> Result<(), types::CommandError> {
        Ok(())
    }

    fn tabs(&self) -> Option<&dyn worker_pool::TabsEngine> {
        Some(self)
    }

    fn navigation(&self) -> Option<&dyn worker_pool::NavigationEngine> {
        Some(self)
    }

    fn observation(&self) -> Option<&dyn worker_pool::ObservationEngine> {
        Some(self)
    }

    fn input(&self) -> Option<&dyn worker_pool::InputEngine> {
        Some(self)
    }
}

#[async_trait]
impl worker_pool::TabsEngine for NoBrowserWorker {
    async fn open_page(&self, _: types::PageId) -> Result<(), types::CommandError> {
        Ok(())
    }
}

#[async_trait]
impl worker_pool::NavigationEngine for NoBrowserWorker {
    async fn navigate(
        &self,
        _: &types::PageId,
        _: &types::NavigateCommand,
    ) -> Result<Vec<types::Evidence>, types::CommandError> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl worker_pool::ObservationEngine for NoBrowserWorker {
    async fn inspect(
        &self,
        _: &types::PageId,
        _: &types::InspectCommand,
    ) -> Result<Vec<types::Evidence>, types::CommandError> {
        Ok(Vec::new())
    }
}

#[async_trait]
impl worker_pool::InputEngine for NoBrowserWorker {
    async fn click(
        &self,
        _: &types::PageId,
        _: &types::ClickCommand,
    ) -> Result<Vec<types::Evidence>, types::CommandError> {
        Ok(Vec::new())
    }

    async fn type_text(
        &self,
        _: &types::PageId,
        _: &types::TypeTextCommand,
    ) -> Result<Vec<types::Evidence>, types::CommandError> {
        Ok(Vec::new())
    }
}

struct NoBrowserFactory;

#[async_trait]
impl WorkerFactory for NoBrowserFactory {
    async fn launch(
        &self,
        session_id: &SessionId,
    ) -> Result<Arc<dyn BrowserWorker>, types::CommandError> {
        Ok(Arc::new(NoBrowserWorker {
            profile: std::path::PathBuf::from(format!("/profiles/{}", session_id.0)),
        }))
    }
}

async fn rig(release: Option<Arc<Notify>>) -> Rig {
    let authority = AuthorityStore::with_capacity(1);
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid!("10000000-0000-0000-0000-0000000000d1")),
            Capability::ALL.to_vec(),
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let scratch = tempfile::tempdir().unwrap();
    let journal = Arc::new(
        JsonlJournal::open(scratch.path().join("journal.jsonl"))
            .await
            .unwrap(),
    );
    let workers = Arc::new(WorkerPool::new(4, Arc::new(NoBrowserFactory)));
    let service = RuntimeService::new(
        SessionManager::new(workers.clone()),
        PageRuntime::new(journal, workers),
    );
    let spy = Arc::new(Spy {
        inner: Arc::new(AuthenticatedRuntime::new(service.clone(), handle.clone())),
        deletes: Mutex::new(Vec::new()),
        created: Arc::new(Notify::new()),
        release,
    });
    let server = Arc::new(Server::for_interface(
        spy.clone(),
        handle.clone(),
        EventStore::new(1024),
        ArtifactResources::default(),
    ));
    Rig {
        observer: AuthenticatedRuntime::new(service.clone(), handle.clone()),
        handle,
        spy,
        server,
        service,
        _scratch: scratch,
    }
}

impl Rig {
    async fn listed(&self) -> Vec<SessionId> {
        self.observer
            .list_sessions(self.handle.context(Utc::now() + Duration::minutes(1), None))
            .await
            .unwrap()
            .into_iter()
            .map(|session| session.id)
            .collect()
    }

    /// A second connection of the same principal over the same runtime.
    fn other_connection(&self) -> Server {
        Server::for_interface(
            Arc::new(AuthenticatedRuntime::new(
                self.service.clone(),
                self.handle.clone(),
            )),
            self.handle.clone(),
            EventStore::new(1024),
            ArtifactResources::default(),
        )
    }

    fn connect(&self) -> Connection {
        let (client, server_side) = tokio::io::duplex(64 * 1024);
        let (server_read, server_write) = tokio::io::split(server_side);
        let (client_read, client_write) = tokio::io::split(client);
        let server = Arc::clone(&self.server);
        let task = tokio::spawn(async move { server.serve(server_read, server_write).await });
        Connection {
            reader: BufReader::new(client_read),
            writer: client_write,
            task,
        }
    }
}

type ClientRead = tokio::io::ReadHalf<DuplexStream>;
type ClientWrite = tokio::io::WriteHalf<DuplexStream>;

struct Connection {
    reader: BufReader<ClientRead>,
    writer: ClientWrite,
    task: JoinHandle<std::io::Result<()>>,
}

impl Connection {
    async fn send(&mut self, message: Value) {
        let mut line = serde_json::to_vec(&message).unwrap();
        line.push(b'\n');
        self.writer.write_all(&line).await.unwrap();
    }

    async fn recv(&mut self) -> Value {
        let mut line = String::new();
        tokio::time::timeout(StdDuration::from_secs(10), self.reader.read_line(&mut line))
            .await
            .expect("response arrives")
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    async fn initialize(&mut self) {
        self.send(
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},
            "clientInfo":{"name":"test","version":"1"}}}),
        )
        .await;
        self.recv().await;
        self.send(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
            .await;
    }

    async fn call(&mut self, id: u64, name: &str, arguments: Value) -> Value {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":"tools/call",
            "params":{"name":name,"arguments":arguments}}))
            .await;
        loop {
            let message = self.recv().await;
            if message["id"] == json!(id) {
                return message;
            }
        }
    }

    fn session_id(response: &Value, field: &str) -> SessionId {
        serde_json::from_value(response["result"]["structuredContent"][field].clone())
            .unwrap_or_else(|_| panic!("{field} in {response}"))
    }

    /// Ends the connection the way a departing agent host does: EOF on input.
    async fn end(mut self) {
        self.writer.shutdown().await.unwrap();
        let _ = tokio::time::timeout(StdDuration::from_secs(5), self.task)
            .await
            .expect("serve returns on EOF");
    }
}

async fn eventually_empty(rig: &Rig) {
    for _ in 0..100 {
        if rig.listed().await.is_empty() {
            return;
        }
        tokio::time::sleep(StdDuration::from_millis(50)).await;
    }
    panic!("sessions still registered: {:?}", rig.listed().await);
}

#[tokio::test]
async fn session_create_is_closed_when_the_connection_ends() {
    let rig = rig(None).await;
    let mut connection = rig.connect();
    connection.initialize().await;
    connection
        .call(2, "session_create", json!({"profile":"default"}))
        .await;
    assert_eq!(rig.listed().await.len(), 1);

    connection.end().await;

    assert!(rig.listed().await.is_empty());
}

#[tokio::test]
async fn workflow_start_session_is_closed_when_the_connection_ends() {
    let rig = rig(None).await;
    let mut connection = rig.connect();
    connection.initialize().await;
    let started = connection
        .call(2, "workflow_start", json!({"profile":"default"}))
        .await;
    assert_eq!(
        started["result"]["structuredContent"]["status"],
        json!("completed"),
        "{started}"
    );
    assert_eq!(rig.listed().await.len(), 1);

    connection.end().await;

    assert!(rig.listed().await.is_empty());
}

#[tokio::test]
async fn a_session_the_agent_closed_is_not_deleted_again() {
    let rig = rig(None).await;
    let mut connection = rig.connect();
    connection.initialize().await;
    let created = connection
        .call(2, "session_create", json!({"profile":"default"}))
        .await;
    let session_id = Connection::session_id(&created, "id");
    connection
        .call(3, "session_close", json!({"sessionId": session_id}))
        .await;
    assert_eq!(rig.spy.deletes.lock().await.len(), 1);

    connection.end().await;

    assert_eq!(
        rig.spy.deletes.lock().await.len(),
        1,
        "disconnect cleanup deleted a session the agent already closed"
    );
}

#[tokio::test]
async fn a_session_another_connection_opened_survives() {
    let rig = rig(None).await;
    let other = rig.other_connection();
    other
        .handle_message(
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},
            "clientInfo":{"name":"other","version":"1"}}}),
        )
        .await;
    other
        .handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;
    let foreign = other
        .handle_message(json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"session_create","arguments":{"profile":"other"}}}))
        .await
        .unwrap();
    let foreign_id = Connection::session_id(&foreign, "id");

    let mut connection = rig.connect();
    connection.initialize().await;
    connection
        .call(2, "session_create", json!({"profile":"default"}))
        .await;
    assert_eq!(rig.listed().await.len(), 2);

    connection.end().await;

    assert_eq!(rig.listed().await, vec![foreign_id]);
}

#[tokio::test]
async fn a_dropped_serve_future_still_closes_the_sessions() {
    let rig = rig(None).await;
    let (client, server_side) = tokio::io::duplex(64 * 1024);
    let (server_read, server_write) = tokio::io::split(server_side);
    let (client_read, mut client_write) = tokio::io::split(client);
    let server = Arc::clone(&rig.server);
    let serving = tokio::spawn(async move {
        // The broker drops the serve future when the peer is gone and the
        // grace elapses; the input is still open here, as it is for a peer
        // that vanished without EOF.
        let _ = tokio::time::timeout(
            StdDuration::from_millis(600),
            server.serve(server_read, server_write),
        )
        .await;
    });
    for message in [
        json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
            "protocolVersion":"2025-11-25","capabilities":{},
            "clientInfo":{"name":"test","version":"1"}}}),
        json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
        json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"session_create","arguments":{"profile":"default"}}}),
    ] {
        let mut line = serde_json::to_vec(&message).unwrap();
        line.push(b'\n');
        client_write.write_all(&line).await.unwrap();
    }
    let mut created = false;
    for _ in 0..100 {
        if rig.listed().await.len() == 1 {
            created = true;
            break;
        }
        tokio::time::sleep(StdDuration::from_millis(20)).await;
    }
    assert!(created, "session_create ran");
    serving.await.unwrap();

    eventually_empty(&rig).await;
    drop((client_write, client_read));
}

#[tokio::test]
async fn a_create_that_completes_after_the_connection_ended_is_deleted() {
    let release = Arc::new(Notify::new());
    let rig = rig(Some(Arc::clone(&release))).await;
    let mut connection = rig.connect();
    connection.initialize().await;
    connection
        .send(json!({"jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"session_create","arguments":{"profile":"default"}}}))
        .await;
    rig.spy.created.notified().await;
    assert_eq!(
        rig.listed().await.len(),
        1,
        "session exists, create is held"
    );

    // The held request cannot finish; serve returns without it.
    connection.writer.shutdown().await.unwrap();
    tokio::time::timeout(StdDuration::from_secs(5), &mut connection.task)
        .await
        .expect("serve returns")
        .unwrap()
        .ok();

    release.notify_one();

    eventually_empty(&rig).await;
}
