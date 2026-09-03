#![allow(dead_code)]

use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};

use async_trait::async_trait;
use chrono::Utc;
use interface_core::{InterfaceResult, RuntimeInterface};
use tokio::sync::Notify;
use types::{
    CommandEnvelope, CommandOutcome, CreateSessionRequest, Evidence, OpenPageRequest, PageId,
    PageMode, PageState, RecoveryDecision, RequestContext, RuntimeInfo, SessionId, SessionState,
    WorkflowCheckpoint, WorkflowId,
};

pub struct StaticRuntime {
    pub sessions: Vec<SessionState>,
}

pub struct PageCreatingRuntime {
    pub session: SessionState,
}

pub struct AutoSessionRuntime {
    session: Mutex<Option<SessionState>>,
}

impl AutoSessionRuntime {
    pub fn empty() -> Arc<Self> {
        Arc::new(Self {
            session: Mutex::new(None),
        })
    }
}

#[async_trait]
impl RuntimeInterface for PageCreatingRuntime {
    async fn runtime_info(&self, _: RequestContext) -> InterfaceResult<RuntimeInfo> {
        unreachable!()
    }
    async fn list_sessions(&self, _: RequestContext) -> InterfaceResult<Vec<SessionState>> {
        Ok(vec![self.session.clone()])
    }
    async fn recovery_status(
        &self,
        _: RequestContext,
        _: WorkflowId,
    ) -> InterfaceResult<types::RecoveryStatus> {
        unreachable!()
    }
    async fn workflows_for_session(
        &self,
        _: RequestContext,
        _: types::SessionId,
        _: usize,
    ) -> InterfaceResult<Vec<WorkflowId>> {
        unreachable!()
    }
    async fn submit_with_auto_checkpoint(
        &self,
        _: RequestContext,
        _: types::CommandEnvelope,
    ) -> InterfaceResult<(types::CommandOutcome, types::CheckpointId)> {
        unreachable!()
    }
    async fn delete_session(&self, _: RequestContext, _: types::SessionId) -> InterfaceResult<()> {
        unreachable!()
    }

    async fn create_session(
        &self,
        _: RequestContext,
        _: CreateSessionRequest,
    ) -> InterfaceResult<SessionState> {
        unreachable!()
    }
    async fn open_page(
        &self,
        _: RequestContext,
        request: OpenPageRequest,
    ) -> InterfaceResult<PageState> {
        Ok(PageState {
            id: PageId::new(),
            session_id: request.session_id,
            url: None,
            mode: PageMode::Document,
            ready_state: "created".into(),
            pending_requests: 0,
        })
    }
    async fn submit(
        &self,
        _: RequestContext,
        _: CommandEnvelope,
    ) -> InterfaceResult<CommandOutcome> {
        unreachable!()
    }
    async fn checkpoint(
        &self,
        _: RequestContext,
        _: WorkflowCheckpoint,
        _: Vec<Evidence>,
    ) -> InterfaceResult<WorkflowCheckpoint> {
        unreachable!()
    }
    async fn resolve_command_evidence(
        &self,
        _: RequestContext,
        _: Vec<types::CommandId>,
    ) -> InterfaceResult<Vec<Evidence>> {
        unreachable!()
    }
    async fn recover(&self, _: RequestContext, _: WorkflowId) -> InterfaceResult<RecoveryDecision> {
        unreachable!()
    }
}

#[async_trait]
impl RuntimeInterface for AutoSessionRuntime {
    async fn runtime_info(&self, _: RequestContext) -> InterfaceResult<RuntimeInfo> {
        unreachable!()
    }
    async fn list_sessions(&self, _: RequestContext) -> InterfaceResult<Vec<SessionState>> {
        Ok(self
            .session
            .lock()
            .expect("session lock")
            .clone()
            .into_iter()
            .collect())
    }
    async fn recovery_status(
        &self,
        _: RequestContext,
        _: WorkflowId,
    ) -> InterfaceResult<types::RecoveryStatus> {
        unreachable!()
    }
    async fn workflows_for_session(
        &self,
        _: RequestContext,
        _: types::SessionId,
        _: usize,
    ) -> InterfaceResult<Vec<WorkflowId>> {
        unreachable!()
    }
    async fn submit_with_auto_checkpoint(
        &self,
        _: RequestContext,
        _: types::CommandEnvelope,
    ) -> InterfaceResult<(types::CommandOutcome, types::CheckpointId)> {
        unreachable!()
    }
    async fn delete_session(&self, _: RequestContext, _: types::SessionId) -> InterfaceResult<()> {
        unreachable!()
    }

    async fn create_session(
        &self,
        _: RequestContext,
        request: CreateSessionRequest,
    ) -> InterfaceResult<SessionState> {
        let now = Utc::now();
        let session = SessionState {
            id: SessionId::new(),
            profile: request.profile,
            proxy: request.proxy,
            page_ids: Vec::new(),
            created_at: now,
            last_used_at: now,
            execution_policy: request.execution_policy,
            zigzagzig: request.zigzagzig,
        };
        *self.session.lock().expect("session lock") = Some(session.clone());
        Ok(session)
    }
    async fn open_page(
        &self,
        _: RequestContext,
        request: OpenPageRequest,
    ) -> InterfaceResult<PageState> {
        let page = PageId::new();
        let mut session = self.session.lock().expect("session lock");
        if let Some(session) = session.as_mut() {
            session.page_ids.push(page.clone());
        }
        Ok(PageState {
            id: page,
            session_id: request.session_id,
            url: None,
            mode: PageMode::Document,
            ready_state: "created".into(),
            pending_requests: 0,
        })
    }
    async fn submit(
        &self,
        _: RequestContext,
        _: CommandEnvelope,
    ) -> InterfaceResult<CommandOutcome> {
        unreachable!()
    }
    async fn checkpoint(
        &self,
        _: RequestContext,
        _: WorkflowCheckpoint,
        _: Vec<Evidence>,
    ) -> InterfaceResult<WorkflowCheckpoint> {
        unreachable!()
    }
    async fn resolve_command_evidence(
        &self,
        _: RequestContext,
        _: Vec<types::CommandId>,
    ) -> InterfaceResult<Vec<Evidence>> {
        unreachable!()
    }
    async fn recover(&self, _: RequestContext, _: WorkflowId) -> InterfaceResult<RecoveryDecision> {
        unreachable!()
    }
}

pub struct BlockingRuntime {
    active: AtomicUsize,
    peak: AtomicUsize,
    entered: Notify,
    release: Notify,
}

impl BlockingRuntime {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            active: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
            entered: Notify::new(),
            release: Notify::new(),
        })
    }

    pub async fn wait_for_active(&self, expected: usize) {
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while self.active.load(Ordering::Acquire) < expected {
                self.entered.notified().await;
            }
        })
        .await
        .unwrap();
    }

    pub fn release_all(&self) {
        self.release.notify_waiters();
    }

    pub fn peak(&self) -> usize {
        self.peak.load(Ordering::Acquire)
    }

    async fn block(&self) -> Vec<SessionState> {
        let active = self.active.fetch_add(1, Ordering::AcqRel) + 1;
        self.peak.fetch_max(active, Ordering::AcqRel);
        self.entered.notify_waiters();
        self.release.notified().await;
        self.active.fetch_sub(1, Ordering::AcqRel);
        Vec::new()
    }
}

#[async_trait]
impl RuntimeInterface for StaticRuntime {
    async fn runtime_info(&self, _: RequestContext) -> InterfaceResult<RuntimeInfo> {
        unreachable!()
    }
    async fn list_sessions(&self, _: RequestContext) -> InterfaceResult<Vec<SessionState>> {
        Ok(self.sessions.clone())
    }
    async fn recovery_status(
        &self,
        _: RequestContext,
        _: WorkflowId,
    ) -> InterfaceResult<types::RecoveryStatus> {
        unreachable!()
    }
    async fn workflows_for_session(
        &self,
        _: RequestContext,
        _: types::SessionId,
        _: usize,
    ) -> InterfaceResult<Vec<WorkflowId>> {
        unreachable!()
    }
    async fn submit_with_auto_checkpoint(
        &self,
        _: RequestContext,
        _: types::CommandEnvelope,
    ) -> InterfaceResult<(types::CommandOutcome, types::CheckpointId)> {
        unreachable!()
    }
    async fn delete_session(&self, _: RequestContext, _: types::SessionId) -> InterfaceResult<()> {
        unreachable!()
    }

    async fn create_session(
        &self,
        _: RequestContext,
        _: CreateSessionRequest,
    ) -> InterfaceResult<SessionState> {
        unreachable!()
    }
    async fn open_page(&self, _: RequestContext, _: OpenPageRequest) -> InterfaceResult<PageState> {
        unreachable!()
    }
    async fn submit(
        &self,
        _: RequestContext,
        _: CommandEnvelope,
    ) -> InterfaceResult<CommandOutcome> {
        unreachable!()
    }
    async fn checkpoint(
        &self,
        _: RequestContext,
        _: WorkflowCheckpoint,
        _: Vec<Evidence>,
    ) -> InterfaceResult<WorkflowCheckpoint> {
        unreachable!()
    }
    async fn resolve_command_evidence(
        &self,
        _: RequestContext,
        _: Vec<types::CommandId>,
    ) -> InterfaceResult<Vec<Evidence>> {
        unreachable!()
    }
    async fn recover(&self, _: RequestContext, _: WorkflowId) -> InterfaceResult<RecoveryDecision> {
        unreachable!()
    }
}

#[async_trait]
impl RuntimeInterface for BlockingRuntime {
    async fn runtime_info(&self, _: RequestContext) -> InterfaceResult<RuntimeInfo> {
        unreachable!()
    }
    async fn list_sessions(&self, _: RequestContext) -> InterfaceResult<Vec<SessionState>> {
        Ok(self.block().await)
    }
    async fn recovery_status(
        &self,
        _: RequestContext,
        _: WorkflowId,
    ) -> InterfaceResult<types::RecoveryStatus> {
        unreachable!()
    }
    async fn workflows_for_session(
        &self,
        _: RequestContext,
        _: types::SessionId,
        _: usize,
    ) -> InterfaceResult<Vec<WorkflowId>> {
        unreachable!()
    }
    async fn submit_with_auto_checkpoint(
        &self,
        _: RequestContext,
        _: types::CommandEnvelope,
    ) -> InterfaceResult<(types::CommandOutcome, types::CheckpointId)> {
        unreachable!()
    }
    async fn delete_session(&self, _: RequestContext, _: types::SessionId) -> InterfaceResult<()> {
        unreachable!()
    }

    async fn create_session(
        &self,
        _: RequestContext,
        _: CreateSessionRequest,
    ) -> InterfaceResult<SessionState> {
        unreachable!()
    }
    async fn open_page(&self, _: RequestContext, _: OpenPageRequest) -> InterfaceResult<PageState> {
        unreachable!()
    }
    async fn submit(
        &self,
        _: RequestContext,
        _: CommandEnvelope,
    ) -> InterfaceResult<CommandOutcome> {
        unreachable!()
    }
    async fn checkpoint(
        &self,
        _: RequestContext,
        _: WorkflowCheckpoint,
        _: Vec<Evidence>,
    ) -> InterfaceResult<WorkflowCheckpoint> {
        unreachable!()
    }
    async fn resolve_command_evidence(
        &self,
        _: RequestContext,
        _: Vec<types::CommandId>,
    ) -> InterfaceResult<Vec<Evidence>> {
        unreachable!()
    }
    async fn recover(&self, _: RequestContext, _: WorkflowId) -> InterfaceResult<RecoveryDecision> {
        unreachable!()
    }
}
