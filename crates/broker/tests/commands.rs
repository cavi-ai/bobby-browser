use std::sync::Arc;

use async_trait::async_trait;
use axum::body::{to_bytes, Body};
use axum::http::{Request, StatusCode};
use broker::{router, AppState, RuntimeApi};
use chrono::{Duration, Utc};
use tower::ServiceExt;
use types::{
    AttemptId, CommandEnvelope, CommandError, CommandId, CommandOutcome, CreateSessionRequest,
    ErrorCode, ErrorLayer, InspectCommand, NavigationRequest, OpenPageRequest, PageState,
    PrimitiveCommand, RuntimeError, RuntimeInfo, SessionState, WorkflowId,
};

struct FakeRuntime {
    outcome: CommandOutcome,
}

#[async_trait]
impl RuntimeApi for FakeRuntime {
    async fn runtime_info(&self) -> RuntimeInfo {
        RuntimeInfo {
            version: "test".into(),
            capabilities: vec![],
            active_sessions: 0,
            queued_jobs: 0,
            uptime_ms: 0,
        }
    }

    async fn list_sessions(&self) -> Vec<SessionState> {
        vec![]
    }

    async fn create_session(&self, _: CreateSessionRequest) -> Result<SessionState, RuntimeError> {
        Err(RuntimeError::Internal("not used by command test".into()))
    }

    async fn open_page(&self, _: OpenPageRequest) -> Result<PageState, RuntimeError> {
        Err(RuntimeError::Internal("not used by command test".into()))
    }

    async fn navigate(
        &self,
        _: NavigationRequest,
    ) -> Result<types::NavigationResult, RuntimeError> {
        Err(RuntimeError::Internal("not used by command test".into()))
    }

    async fn submit(&self, _: CommandEnvelope) -> CommandOutcome {
        self.outcome.clone()
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
        schema_version: 1,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: types::SessionId::new(),
        page_id: Some(types::PageId::new()),
        deadline: Utc::now() + Duration::minutes(1),
        command: PrimitiveCommand::Inspect(InspectCommand::default()),
    }
}

async fn response_for(outcome: CommandOutcome) -> (StatusCode, serde_json::Value) {
    let app = router(AppState::new(Arc::new(FakeRuntime { outcome })));
    let response = app
        .oneshot(
            Request::post("/commands")
                .header("content-type", "application/json")
                .body(Body::from(serde_json::to_vec(&envelope()).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&body).unwrap())
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
        ),
        (
            CommandOutcome::RetryableFailure {
                command_id: id.clone(),
                error: error(ErrorCode::BrowserCommandFailed),
            },
            StatusCode::SERVICE_UNAVAILABLE,
            "retryableFailure",
        ),
        (
            CommandOutcome::NeedsReconciliation {
                command_id: id.clone(),
                error: error(ErrorCode::BrowserCommandFailed),
                evidence: vec![],
            },
            StatusCode::CONFLICT,
            "needsReconciliation",
        ),
        (
            CommandOutcome::PolicyDenied {
                command_id: id.clone(),
                error: error(ErrorCode::PolicyDenied),
            },
            StatusCode::FORBIDDEN,
            "policyDenied",
        ),
        (
            CommandOutcome::ResourceExhausted {
                command_id: id.clone(),
                error: error(ErrorCode::ResourceExhausted),
                retry_after_ms: 50,
            },
            StatusCode::TOO_MANY_REQUESTS,
            "resourceExhausted",
        ),
        (
            CommandOutcome::Failed {
                command_id: id.clone(),
                error: error(ErrorCode::InvalidRequest),
            },
            StatusCode::UNPROCESSABLE_ENTITY,
            "failed",
        ),
        (
            CommandOutcome::Restarted {
                command_id: id.clone(),
                prior_attempt_id: AttemptId::new(),
                attempt_id: AttemptId::new(),
                reason: "checkpoint could not be reconciled".into(),
            },
            StatusCode::OK,
            "restarted",
        ),
        (
            CommandOutcome::Failed {
                command_id: id,
                error: error(ErrorCode::Internal),
            },
            StatusCode::INTERNAL_SERVER_ERROR,
            "failed",
        ),
    ];

    for (outcome, expected_status, expected_tag) in cases {
        let (status, body) = response_for(outcome).await;
        assert_eq!(status, expected_status);
        assert_eq!(body["status"], expected_tag);
    }
}
