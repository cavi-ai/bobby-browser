use super::*;
use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};
use types::{AttemptId, SessionId, WorkerId, WorkflowId};
use worker_pool::{BrowserWorker, WorkerFactory, WorkerLease, WorkerPool};

pub(super) struct Fixture {
    pub envelope: CommandEnvelope,
    pub lease: WorkerLease,
    pub worker: Arc<Worker>,
    _pool: WorkerPool,
}

impl Fixture {
    pub async fn new() -> Self {
        let worker = Arc::new(Worker {
            inspection: std::sync::Mutex::new(Vec::new()),
            waits: AtomicUsize::new(0),
        });
        let pool = WorkerPool::new(1, Arc::new(Factory(worker.clone())));
        let session_id = SessionId::new();
        let lease = pool.lease(session_id.clone()).await.unwrap();
        Self {
            envelope: CommandEnvelope {
                schema_version: CommandEnvelope::SCHEMA_VERSION,
                command_id: CommandId::new(),
                workflow_id: WorkflowId::new(),
                attempt_id: AttemptId::new(),
                session_id,
                page_id: Some(types::PageId::new()),
                deadline: chrono::Utc::now() + chrono::Duration::seconds(30),
                command: RuntimeCommand::Primitive(PrimitiveCommand::Inspect(
                    InspectCommand::default(),
                )),
            },
            lease,
            worker,
            _pool: pool,
        }
    }

    pub async fn verify(
        &self,
        family: &dyn CommandVerifier,
        command: &PrimitiveCommand,
        evidence: Vec<Evidence>,
    ) -> Result<Vec<Evidence>, CommandError> {
        family
            .verify(
                VerificationContext {
                    envelope: &self.envelope,
                    lease: &self.lease,
                    typed_on: None,
                },
                command,
                evidence,
            )
            .await
    }
}

pub(super) fn command(kind: &str, input: serde_json::Value) -> PrimitiveCommand {
    serde_json::from_value(serde_json::json!({"kind":kind,"input":input})).unwrap()
}

pub(super) fn evidence(value: serde_json::Value) -> Evidence {
    serde_json::from_value(value).unwrap()
}

struct Factory(Arc<Worker>);
#[async_trait::async_trait]
impl WorkerFactory for Factory {
    async fn launch(&self, _: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(self.0.clone())
    }
}

pub(super) struct Worker {
    pub inspection: std::sync::Mutex<Vec<Evidence>>,
    pub waits: AtomicUsize,
}

#[async_trait::async_trait]
impl BrowserWorker for Worker {
    fn worker_id(&self) -> WorkerId {
        WorkerId::new()
    }
    fn profile_dir(&self) -> &Path {
        Path::new("verifier-fixture")
    }
    fn observation(&self) -> Option<&dyn worker_pool::ObservationEngine> {
        Some(self)
    }
    fn wait_provider(&self) -> Option<&dyn worker_pool::WaitProvider> {
        Some(self)
    }
    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }
}

#[async_trait::async_trait]
impl worker_pool::ObservationEngine for Worker {
    async fn inspect(
        &self,
        _: &types::PageId,
        _: &InspectCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(self.inspection.lock().unwrap().clone())
    }
}

impl worker_pool::WaitProvider for Worker {
    fn observer<'a>(
        &'a self,
        _: &'a types::PageId,
    ) -> Box<dyn worker_pool::wait::WaitObserver + 'a> {
        Box::new(self)
    }
}

#[async_trait::async_trait]
impl worker_pool::wait::WaitObserver for &Worker {
    async fn observe(
        &self,
        _: &types::WaitCondition,
    ) -> Result<worker_pool::wait::WaitObservation, CommandError> {
        self.waits.fetch_add(1, Ordering::SeqCst);
        Err(verification_error("fixture settle timeout"))
    }
}
