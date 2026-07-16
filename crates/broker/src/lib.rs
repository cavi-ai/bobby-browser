use std::net::SocketAddr;

use axum::{extract::State, routing::{get, post}, Json, Router};
use config::AppConfig;
use sdk_core::RuntimeService;
use types::{CreateSessionRequest, NavigationRequest, OpenPageRequest};

#[derive(Clone, Default)]
pub struct AppState {
    pub runtime: RuntimeService,
}

pub async fn serve(config: AppConfig) {
    let state = AppState::default();
    let app = Router::new()
        .route("/healthz", get(healthz))
        .route("/runtime", get(runtime_info))
        .route("/sessions", get(list_sessions).post(create_session))
        .route("/pages", post(open_page))
        .route("/navigate", post(navigate))
        .with_state(state);

    let addr: SocketAddr = format!("{}:{}", config.server.host, config.server.port)
        .parse()
        .expect("valid socket address");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("bind listener");

    axum::serve(listener, app).await.expect("serve axum app");
}

async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

async fn runtime_info(State(state): State<AppState>) -> Json<types::RuntimeInfo> {
    Json(state.runtime.runtime_info().await)
}

async fn list_sessions(State(state): State<AppState>) -> Json<Vec<types::SessionState>> {
    Json(state.runtime.list_sessions().await)
}

async fn create_session(
    State(state): State<AppState>,
    Json(req): Json<CreateSessionRequest>,
) -> Json<types::SessionState> {
    Json(state.runtime.create_session(req).await)
}

async fn open_page(
    State(state): State<AppState>,
    Json(req): Json<OpenPageRequest>,
) -> Result<Json<types::PageState>, Json<serde_json::Value>> {
    state
        .runtime
        .open_page(req)
        .await
        .map(Json)
        .map_err(|e| Json(serde_json::json!({ "error": e.to_string() })))
}

async fn navigate(
    State(state): State<AppState>,
    Json(req): Json<NavigationRequest>,
) -> Result<Json<types::NavigationResult>, Json<serde_json::Value>> {
    state
        .runtime
        .navigate(req)
        .await
        .map(Json)
        .map_err(|e| Json(serde_json::json!({ "error": e.to_string() })))
}
