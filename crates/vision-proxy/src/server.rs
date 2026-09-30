use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde_json::Value;

use crate::auth;
use crate::upstream::{ExtractInput, ProposeInput, Upstream, UpstreamError};
use crate::validate::{validate_extract, validate_proposal_for_request, ValidateError};
use crate::wire::{ExtractRequest, ProposeRequest};

/// Upstream provider type for the vision proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamKind {
    OpenAi,
    Ollama,
    Mlx,
}

#[derive(Clone, Debug)]
pub struct ProxyConfig {
    pub bind: SocketAddr,
    pub path: String,
    pub bearer_token: String,
    pub upstream_kind: UpstreamKind,
}

#[derive(Clone)]
pub struct AppState {
    pub path: String,
    pub bearer_token: String,
    pub upstream: Arc<dyn Upstream>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    Propose,
    Extract,
}

fn classify(v: &Value) -> Option<Kind> {
    let has_shot = v.get("screenshotPng").is_some();
    let has_extract = v.get("schema").is_some() && v.get("content").is_some();
    match (has_shot, has_extract) {
        (true, false) => Some(Kind::Propose),
        (false, true) => Some(Kind::Extract),
        _ => None,
    }
}

fn error_response(
    status: StatusCode,
    code: &str,
    kind: &str,
    message: impl Into<String>,
) -> Response {
    (
        status,
        Json(serde_json::json!({
            "error": {
                "code": code,
                "kind": kind,
                "message": bound_diagnostic(message.into()),
                "retryable": status.is_server_error(),
            }
        })),
    )
        .into_response()
}

fn bad_request(message: &str) -> Response {
    error_response(
        StatusCode::BAD_REQUEST,
        "visionInvalidRequest",
        "request",
        message,
    )
}

fn bad_gateway(error: UpstreamError) -> Response {
    let (code, kind, message) = upstream_error_diagnostic(error);
    error_response(StatusCode::BAD_GATEWAY, code, kind, message)
}

fn validation_failed(error: ValidateError) -> Response {
    error_response(
        StatusCode::BAD_GATEWAY,
        "visionInvalidModelReply",
        "invalid-model-reply",
        error.to_string(),
    )
}

fn bound_diagnostic(message: String) -> String {
    message.chars().take(512).collect()
}

async fn handle_vision(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !auth::authorize(&headers, &state.bearer_token) {
        return error_response(
            StatusCode::UNAUTHORIZED,
            "visionAuthRejected",
            "auth",
            "missing or invalid bearer token",
        );
    }

    let kind = match classify(&body) {
        Some(kind) => kind,
        None => return bad_request("ambiguous or unsupported request body"),
    };

    match kind {
        Kind::Propose => handle_propose(&state, body).await,
        Kind::Extract => handle_extract(&state, body).await,
    }
}

async fn handle_propose(state: &AppState, body: Value) -> Response {
    let request: ProposeRequest = match serde_json::from_value(body) {
        Ok(request) => request,
        Err(error) => return bad_request(&error.to_string()),
    };

    let intent_kind = request.intent_kind.clone();
    let candidate_count = request
        .context
        .as_ref()
        .map_or(0, |context| context.candidates.len());
    let input = ProposeInput {
        purpose: request.purpose,
        intent_kind: request.intent_kind,
        stuck: request.stuck,
        screenshot_png_b64: request.screenshot_png,
        corpus_screenshot_png_b64: request.corpus_screenshot_png,
        context: request.context,
    };

    let proposal = match state.upstream.propose(input).await {
        Ok(proposal) => proposal,
        Err(error) => return bad_gateway(error),
    };

    if let Err(error) = validate_proposal_for_request(&proposal, &intent_kind, candidate_count) {
        return validation_failed(error);
    }

    Json(proposal).into_response()
}

async fn handle_extract(state: &AppState, body: Value) -> Response {
    let request: ExtractRequest = match serde_json::from_value(body) {
        Ok(request) => request,
        Err(error) => return bad_request(&error.to_string()),
    };

    let input = ExtractInput {
        schema: request.schema,
        content: request.content,
        purpose: request.purpose,
    };

    let response = match state.upstream.extract(input).await {
        Ok(response) => response,
        Err(error) => return bad_gateway(error),
    };

    if let Err(error) = validate_extract(&response) {
        return validation_failed(error);
    }

    Json(response).into_response()
}

fn upstream_error_diagnostic(error: UpstreamError) -> (&'static str, &'static str, String) {
    match error {
        UpstreamError::Transport(message) => ("visionUpstreamTransport", "transport", message),
        UpstreamError::Rejected(message) => {
            ("visionUpstreamRejected", "upstream-rejected", message)
        }
        UpstreamError::Invalid(_) => (
            "visionInvalidModelReply",
            "invalid-model-reply",
            "upstream returned an invalid model reply".to_string(),
        ),
    }
}

async fn handle_status() -> impl IntoResponse {
    ([("x-bobby-vision", "ready")], StatusCode::OK)
}

pub fn router(state: AppState) -> Router {
    let path = state.path.clone();
    Router::new()
        .route(&path, get(handle_status).post(handle_vision))
        .with_state(state)
}

pub async fn serve(config: ProxyConfig, upstream: Arc<dyn Upstream>) -> io::Result<()> {
    let state = AppState {
        path: config.path,
        bearer_token: config.bearer_token,
        upstream,
    };
    let app = router(state);
    let listener = tokio::net::TcpListener::bind(config.bind).await?;
    axum::serve(listener, app).await?;
    Ok(())
}
