use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use broker::{router, AppState};
use chrono::{Duration, SecondsFormat, Utc};
use config::InterfaceConfig;
use interface_core::{AuthorityStore, InterfaceResult, RuntimeInterface};
use tower::ServiceExt;
use types::{
    AttemptId, Capability, CommandEnvelope, CommandError, CommandId, CommandOutcome, ErrorCode,
    ErrorLayer, Evidence, InspectCommand, OpenPageRequest, PageState, PrimitiveCommand,
    PrincipalId, RecoveryDecision, RequestContext, RuntimeCommand, RuntimeInfo, SessionState,
    WorkflowCheckpoint, WorkflowId, CURRENT_INTERFACE_VERSION,
};
use uuid::Uuid;

struct FakeRuntime {
    outcome: CommandOutcome,
}

#[async_trait]
impl RuntimeInterface for FakeRuntime {
    async fn runtime_info(&self, _: RequestContext) -> InterfaceResult<RuntimeInfo> {
        unreachable!()
    }

    async fn list_sessions(&self, _: RequestContext) -> InterfaceResult<Vec<SessionState>> {
        unreachable!()
    }

    async fn recovery_status(
        &self,
        _: RequestContext,
        _: WorkflowId,
    ) -> InterfaceResult<types::RecoveryStatus> {
        unreachable!()
    }
    async fn delete_session(&self, _: RequestContext, _: types::SessionId) -> InterfaceResult<()> {
        unreachable!()
    }

    async fn create_session(
        &self,
        _: RequestContext,
        _: types::CreateSessionRequest,
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
        Ok(self.outcome.clone())
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
        _: Vec<CommandId>,
    ) -> InterfaceResult<Vec<Evidence>> {
        unreachable!()
    }

    async fn recover(&self, _: RequestContext, _: WorkflowId) -> InterfaceResult<RecoveryDecision> {
        unreachable!()
    }
}

fn error(code: ErrorCode) -> CommandError {
    CommandError {
        code,
        message: "test outcome".into(),
        layer: ErrorLayer::Workflow,
        retryable: false,
    }
}

fn envelope() -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: types::SessionId::new(),
        page_id: Some(types::PageId::new()),
        deadline: Utc::now() + Duration::minutes(1),
        command: RuntimeCommand::Primitive(PrimitiveCommand::Inspect(InspectCommand::default())),
    }
}

async fn response_for(outcome: CommandOutcome) -> (StatusCode, serde_json::Value, Option<u64>) {
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(Uuid::from_u128(99)),
            [Capability::BrowserMutate],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let app = router(AppState::new(
        Arc::new(authority),
        move |_| {
            Arc::new(FakeRuntime {
                outcome: outcome.clone(),
            })
        },
        InterfaceConfig::default(),
    ));
    let response = app
        .oneshot(
            Request::post("/v1/commands")
                .header("authorization", format!("Bearer {token}"))
                .header("x-interface-version", CURRENT_INTERFACE_VERSION)
                .header("x-correlation-id", "10000000-0000-0000-0000-000000000099")
                .header(
                    "x-deadline",
                    (Utc::now() + Duration::minutes(2))
                        .to_rfc3339_opts(SecondsFormat::Millis, true),
                )
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&envelope()).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let retry_after = response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.parse::<u64>().ok());
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap(), retry_after)
}

#[tokio::test]
async fn command_outcomes_map_to_stable_http_statuses() {
    let id = CommandId::new();
    let cases = vec![
        (
            CommandOutcome::Completed {
                command_id: id.clone(),
                evidence: vec![],
            },
            StatusCode::OK,
            "completed",
            None,
        ),
        (
            CommandOutcome::RetryableFailure {
                command_id: id.clone(),
                error: error(ErrorCode::BrowserCommandFailed),
            },
            StatusCode::SERVICE_UNAVAILABLE,
            "retryableFailure",
            Some(1),
        ),
        (
            CommandOutcome::NeedsReconciliation {
                command_id: id.clone(),
                error: error(ErrorCode::BrowserCommandFailed),
                evidence: vec![],
            },
            StatusCode::CONFLICT,
            "needsReconciliation",
            None,
        ),
        (
            CommandOutcome::PolicyDenied {
                command_id: id.clone(),
                error: error(ErrorCode::PolicyDenied),
            },
            StatusCode::FORBIDDEN,
            "policyDenied",
            None,
        ),
        (
            CommandOutcome::ResourceExhausted {
                command_id: id.clone(),
                error: error(ErrorCode::ResourceExhausted),
                retry_after_ms: 50,
            },
            StatusCode::TOO_MANY_REQUESTS,
            "resourceExhausted",
            Some(1),
        ),
        (
            CommandOutcome::Failed {
                command_id: id.clone(),
                error: error(ErrorCode::InvalidRequest),
                evidence: vec![],
            },
            StatusCode::UNPROCESSABLE_ENTITY,
            "failed",
            None,
        ),
        (
            CommandOutcome::Restarted {
                command_id: id.clone(),
                prior_attempt_id: AttemptId::new(),
                attempt_id: AttemptId::new(),
                reason: "checkpoint could not be reconciled".into(),
                evidence: vec![],
            },
            StatusCode::OK,
            "restarted",
            None,
        ),
        (
            CommandOutcome::Failed {
                command_id: id,
                error: error(ErrorCode::Internal),
                evidence: vec![],
            },
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed",
            None,
        ),
    ];

    for (outcome, expected_status, expected_tag, expected_retry_after) in cases {
        let (status, body, retry_after) = response_for(outcome).await;
        assert_eq!(status, expected_status);
        assert_eq!(body["status"], expected_tag);
        assert_eq!(retry_after, expected_retry_after);
    }
}
