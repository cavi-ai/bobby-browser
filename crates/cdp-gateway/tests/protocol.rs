use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};

use async_trait::async_trait;
use cdp_gateway::{CdpConnection, CdpErrorCode, CdpEvent, CdpRequest, MethodRegistry};
use chrono::{Duration, Utc};
use interface_core::{AuthorityStore, InterfaceResult, RuntimeInterface};
use serde_json::json;
use types::{
    Capability, CommandEnvelope, CommandOutcome, CreateSessionRequest, Evidence, OpenPageRequest,
    PageState, PrincipalId, RecoveryDecision, RequestContext, RuntimeInfo, SessionState,
    WorkflowCheckpoint, WorkflowId,
};

#[derive(Default)]
struct RecordingRuntime(AtomicUsize);

#[async_trait]
impl RuntimeInterface for RecordingRuntime {
    async fn runtime_info(&self, _: RequestContext) -> InterfaceResult<RuntimeInfo> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("not needed")
    }
    async fn list_sessions(&self, _: RequestContext) -> InterfaceResult<Vec<SessionState>> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(vec![])
    }
    async fn create_session(
        &self,
        _: RequestContext,
        _: CreateSessionRequest,
    ) -> InterfaceResult<SessionState> {
        panic!("not needed")
    }
    async fn open_page(&self, _: RequestContext, _: OpenPageRequest) -> InterfaceResult<PageState> {
        panic!("not needed")
    }
    async fn submit(
        &self,
        _: RequestContext,
        _: CommandEnvelope,
    ) -> InterfaceResult<CommandOutcome> {
        self.0.fetch_add(1, Ordering::SeqCst);
        panic!("unsupported must not forward")
    }
    async fn checkpoint(
        &self,
        _: RequestContext,
        _: WorkflowCheckpoint,
        _: Vec<Evidence>,
    ) -> InterfaceResult<WorkflowCheckpoint> {
        panic!("not needed")
    }
    async fn recover(&self, _: RequestContext, _: WorkflowId) -> InterfaceResult<RecoveryDecision> {
        panic!("not needed")
    }
}

#[tokio::test]
async fn unsupported_methods_are_explicit_and_never_forwarded() {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [Capability::SessionRead],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap();
    let handle = authority.verify(&token.expose_once()).await.unwrap();
    let runtime = Arc::new(RecordingRuntime::default());
    let connection = CdpConnection::new(handle, runtime.clone(), MethodRegistry::compiled());
    let response = connection
        .dispatch(CdpRequest::new(7, "SystemInfo.getProcessInfo", json!({})))
        .await;
    assert_eq!(
        response.error().unwrap().code,
        CdpErrorCode::MethodNotFound as i32
    );
    assert_eq!(runtime.0.load(Ordering::SeqCst), 0);
}

#[test]
fn frames_and_request_ids_are_bounded() {
    assert_eq!(cdp_gateway::MAX_FRAME_BYTES, 1024 * 1024);
    assert_eq!(cdp_gateway::MAX_IN_FLIGHT_REQUESTS, 128);
    assert_eq!(cdp_gateway::MAX_QUEUED_EVENTS, 1024);
    assert!(cdp_gateway::parse_frame(&vec![b'x'; cdp_gateway::MAX_FRAME_BYTES + 1]).is_err());
    assert!(
        cdp_gateway::parse_frame(br#"{"id":0,"method":"Browser.getVersion","params":{}}"#).is_err()
    );
}

#[tokio::test]
async fn event_queue_fails_closed_at_the_declared_bound() {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [Capability::SessionRead],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap();
    let connection = CdpConnection::new(
        authority.verify(&token.expose_once()).await.unwrap(),
        Arc::new(RecordingRuntime::default()),
        MethodRegistry::compiled(),
    );
    assert!(connection
        .queue_event(CdpEvent {
            method: "Target.targetDestroyed".into(),
            params: json!({}),
            session_id: None,
        })
        .await
        .is_err());
    for index in 0..cdp_gateway::MAX_QUEUED_EVENTS {
        connection
            .queue_event(CdpEvent {
                method: "Target.targetDestroyed".into(),
                params: json!({"targetId": format!("target-{index}")}),
                session_id: None,
            })
            .await
            .unwrap();
    }
    assert!(connection
        .queue_event(CdpEvent {
            method: "Target.targetDestroyed".into(),
            params: json!({"targetId": "overflow"}),
            session_id: None,
        })
        .await
        .is_err());
}

#[test]
fn manifest_and_handlers_are_bijective_and_have_no_wildcards() {
    let registry = MethodRegistry::compiled();
    registry.validate().unwrap();
    assert!(registry.methods().all(|method| !method.name.contains('*')));
    assert!(registry
        .methods()
        .all(|method| registry.has_handler(&method.name)));
    assert_eq!(registry.method_count(), registry.handler_count());
    assert!(registry.events().all(|event| event.capability().is_some()));
    assert!(registry
        .events()
        .all(|event| registry.has_event_translator(&event.name)));
    assert_eq!(registry.event_count(), registry.event_translator_count());
}
