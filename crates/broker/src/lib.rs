use std::net::SocketAddr;
use std::sync::Arc;

use async_trait::async_trait;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use config::AppConfig;
use sdk_core::RuntimeService;
use types::{
    CommandEnvelope, CommandOutcome, CreateSessionRequest, NavigationRequest, OpenPageRequest,
    PageState, RuntimeError, RuntimeInfo, SessionState,
};

#[async_trait]
pub trait RuntimeApi: Send + Sync {
    async fn runtime_info(&self) -> RuntimeInfo;
    async fn list_sessions(&self) -> Vec<SessionState>;
    async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<SessionState, RuntimeError>;
    async fn open_page(&self, request: OpenPageRequest) -> Result<PageState, RuntimeError>;
    async fn navigate(
        &self,
        request: NavigationRequest,
    ) -> Result<types::NavigationResult, RuntimeError>;
    async fn submit(&self, envelope: CommandEnvelope) -> CommandOutcome;
}

#[async_trait]
impl RuntimeApi for RuntimeService {
    async fn runtime_info(&self) -> RuntimeInfo {
        self.runtime_info().await
    }

    async fn list_sessions(&self) -> Vec<SessionState> {
        self.list_sessions().await
    }

    async fn create_session(
        &self,
        request: CreateSessionRequest,
    ) -> Result<SessionState, RuntimeError> {
        self.create_session(request).await
    }

    async fn open_page(&self, request: OpenPageRequest) -> Result<PageState, RuntimeError> {
        self.open_page(request).await
    }

    async fn navigate(
        &self,
        request: NavigationRequest,
    ) -> Result<types::NavigationResult, RuntimeError> {
        self.navigate(request).await
    }

    async fn submit(&self, envelope: CommandEnvelope) -> CommandOutcome {
        self.submit(envelope).await
    }
}

#[derive(Clone)]
pub struct AppState {
    runtime: Arc<dyn RuntimeApi>,
}

impl AppState {
    pub fn new(runtime: Arc<dyn RuntimeApi>) -> Self {
        Self { runtime }
    }
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .route("/runtime", get(runtime_info))
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/pages", post(open_page))
        .route("/navigate", post(navigate))
        .route("/commands", post(submit_command))
        .with_state(state)
}

pub async fn serve(config: AppConfig) -> anyhow::Result<()> {
    let runtime = Arc::new(RuntimeService::build(&config).await?);
    let app = router(AppState::new(runtime));
    let addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

async fn runtime_info(State(state): State<AppState>) -> Json<RuntimeInfo> {
    Json(state.runtime.runtime_info().await)
}

async fn list_sessions(State(state): State<AppState>) -> Json<Vec<SessionState>> {
    Json(state.runtime.list_sessions().await)
}

async fn create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateSessionRequest>,
) -> Result<Json<SessionState>, ApiError> {
    state
        .runtime
        .create_session(req)
        .await
        .map(Json)
        .map_err(ApiError)
}

async fn open_page(
    State(state): State<AppState>,
    Json(req): Json<OpenPageRequest>,
) -> Result<Json<PageState>, ApiError> {
    state
        .runtime
        .open_page(req)
        .await
        .map(Json)
        .map_err(ApiError)
}

async fn navigate(
    State(state): State<AppState>,
    Json(req): Json<NavigationRequest>,
) -> Result<Json<types::NavigationResult>, ApiError> {
    state
        .runtime
        .navigate(req)
        .await
        .map(Json)
        .map_err(ApiError)
}

async fn submit_command(
    State(state): State<AppState>,
    Json(envelope): Json<CommandEnvelope>,
) -> Response {
    let outcome = state.runtime.submit(envelope).await;
    let status = outcome_status(&outcome);
    (status, Json(outcome)).into_response()
}

fn outcome_status(outcome: &CommandOutcome) -> StatusCode {
    match outcome {
        CommandOutcome::Completed { .. } | CommandOutcome::Restarted { .. } => StatusCode::OK,
        CommandOutcome::RetryableFailure { .. } => StatusCode::SERVICE_UNAVAILABLE,
        CommandOutcome::NeedsReconciliation { .. } => StatusCode::CONFLICT,
        CommandOutcome::PolicyDenied { .. } => StatusCode::FORBIDDEN,
        CommandOutcome::ResourceExhausted { .. } => StatusCode::TOO_MANY_REQUESTS,
        CommandOutcome::Failed { error, .. } if error.code == types::ErrorCode::InvalidRequest => {
            StatusCode::UNPROCESSABLE_ENTITY
        }
        CommandOutcome::Failed { .. } => StatusCode::INTERNAL_SERVER_ERROR,
    }
}

struct ApiError(RuntimeError);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        let status = match &self.0 {
            RuntimeError::NotFound(_) => StatusCode::NOT_FOUND,
            RuntimeError::InvalidRequest(_) => StatusCode::UNPROCESSABLE_ENTITY,
            RuntimeError::Internal(_) => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (
            status,
            Json(serde_json::json!({ "error": self.0.to_string() })),
        )
            .into_response()
    }
}
