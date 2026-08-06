use std::io;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::post;
use axum::{Json, Router};
use serde_json::Value;

use crate::auth;
use crate::upstream::{ExtractInput, ProposeInput, Upstream, UpstreamError};
use crate::validate::{validate_extract, validate_proposal, ValidateError};
use crate::wire::{ExtractRequest, ProposeRequest};

/// Upstream provider type for the vision proxy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpstreamKind {
    OpenAi,
    Ollama,
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

fn bad_request(message: &str) -> Response {
    (
        StatusCode::BAD_REQUEST,
        Json(serde_json::json!({ "error": message })),
    )
        .into_response()
}

fn bad_gateway(message: impl Into<String>) -> Response {
    (
        StatusCode::BAD_GATEWAY,
        Json(serde_json::json!({ "error": message.into() })),
    )
        .into_response()
}

async fn handle_vision(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    if !auth::authorize(&headers, &state.bearer_token) {
        return StatusCode::UNAUTHORIZED.into_response();
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

    let input = ProposeInput {
        purpose: request.purpose,
        intent_kind: request.intent_kind,
        stuck: request.stuck,
        screenshot_png_b64: request.screenshot_png,
        context: request.context,
    };

    let proposal = match state.upstream.propose(input).await {
        Ok(proposal) => proposal,
        Err(error) => return bad_gateway(upstream_error_message(error)),
    };

    if let Err(error) = validate_proposal(&proposal) {
        return bad_gateway(validate_error_message(error));
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
        Err(error) => return bad_gateway(upstream_error_message(error)),
    };

    if let Err(error) = validate_extract(&response) {
        return bad_gateway(validate_error_message(error));
    }

    Json(response).into_response()
}

fn upstream_error_message(error: UpstreamError) -> String {
    error.to_string()
}

fn validate_error_message(error: ValidateError) -> String {
    error.to_string()
}

pub fn router(state: AppState) -> Router {
    let path = state.path.clone();
    Router::new()
        .route(&path, post(handle_vision))
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
