use axum::{
    body::{Body, Bytes},
    extract::{rejection::BytesRejection, Extension, Path, RawQuery, State},
    http::{header::CONTENT_TYPE, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use interface_core::{Event, EventGap};
use serde::{de::DeserializeOwned, Deserialize};
use types::{
    CommandEnvelope, CommandOutcome, CorrelationId, Evidence, InterfaceErrorCode,
    InterfaceOperation, OpenPageRequest, RecoveryDecision, WorkflowCheckpoint, WorkflowId,
};
use uuid::Uuid;

use crate::{
    auth::{authorize_boundary, interface_error, AuthenticatedRequest, ProtocolError},
    AppState,
};

pub(crate) fn protected_router() -> Router<AppState> {
    Router::new()
        .route("/v1/runtime", get(runtime_info))
        .route("/v1/sessions", get(list_sessions).post(create_session))
        .route("/v1/pages", post(open_page))
        .route("/v1/commands", post(submit_command))
        .route("/v1/checkpoints", post(checkpoint))
        .route("/v1/recovery/{workflow}", post(recover))
        .route("/v1/events", get(events))
        .route("/v1/artifacts/{id}", get(artifact))
}

pub(crate) async fn healthz() -> Json<serde_json::Value> {
    Json(serde_json::json!({ "ok": true }))
}

async fn runtime_info(
    Extension(request): Extension<AuthenticatedRequest>,
) -> Result<Json<types::RuntimeInfo>, ProtocolError> {
    request
        .runtime
        .runtime_info(request.context)
        .await
        .map(Json)
        .map_err(ProtocolError::from)
}

async fn list_sessions(
    Extension(request): Extension<AuthenticatedRequest>,
) -> Result<Json<Vec<types::SessionState>>, ProtocolError> {
    request
        .runtime
        .list_sessions(request.context)
        .await
        .map(Json)
        .map_err(ProtocolError::from)
}

async fn create_session(
    Extension(request): Extension<AuthenticatedRequest>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<types::SessionState>, ProtocolError> {
    let input = parse_json(body, &request.context.correlation_id)?;
    request
        .runtime
        .create_session(request.context, input)
        .await
        .map(Json)
        .map_err(ProtocolError::from)
}

async fn open_page(
    Extension(request): Extension<AuthenticatedRequest>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<types::PageState>, ProtocolError> {
    let input: OpenPageRequest = parse_json(body, &request.context.correlation_id)?;
    request
        .runtime
        .open_page(request.context, input)
        .await
        .map(Json)
        .map_err(ProtocolError::from)
}

async fn submit_command(
    State(state): State<AppState>,
    Extension(request): Extension<AuthenticatedRequest>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ProtocolError> {
    let envelope: CommandEnvelope = parse_json(body, &request.context.correlation_id)?;
    if envelope.deadline <= Utc::now() || envelope.deadline > request.context.deadline {
        return Err(ProtocolError::from(interface_error(
            InterfaceErrorCode::InvalidRequest,
            "command deadline must be live and no later than the request deadline",
            request.context.correlation_id,
            None,
        )));
    }
    let outcome = request
        .runtime
        .submit(request.context.clone(), envelope)
        .await
        .map_err(ProtocolError::from)?;
    let payload = serde_json::to_value(&outcome).unwrap_or_else(|_| {
        serde_json::json!({
            "error": "command outcome could not be serialized"
        })
    });
    state
        .events
        .append(Event::new("command.outcome", payload))
        .await;
    Ok(outcome_response(outcome))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CheckpointRequest {
    checkpoint: WorkflowCheckpoint,
    #[serde(default)]
    evidence: Vec<Evidence>,
}

async fn checkpoint(
    Extension(request): Extension<AuthenticatedRequest>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<WorkflowCheckpoint>, ProtocolError> {
    let input: CheckpointRequest = parse_json(body, &request.context.correlation_id)?;
    request
        .runtime
        .checkpoint(request.context, input.checkpoint, input.evidence)
        .await
        .map(Json)
        .map_err(ProtocolError::from)
}

async fn recover(
    Path(workflow): Path<String>,
    Extension(request): Extension<AuthenticatedRequest>,
) -> Result<Response, ProtocolError> {
    if workflow.len() > 64 {
        return Err(ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            request.context.correlation_id,
        ));
    }
    let workflow = WorkflowId(Uuid::parse_str(&workflow).map_err(|_| {
        ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            request.context.correlation_id.clone(),
        )
    })?);
    let decision = request
        .runtime
        .recover(request.context, workflow)
        .await
        .map_err(ProtocolError::from)?;
    let status = if matches!(decision, RecoveryDecision::NeedsReconciliation { .. }) {
        StatusCode::CONFLICT
    } else {
        StatusCode::OK
    };
    Ok((status, Json(decision)).into_response())
}

async fn events(
    State(state): State<AppState>,
    RawQuery(raw_query): RawQuery,
    Extension(request): Extension<AuthenticatedRequest>,
) -> Result<Response, ProtocolError> {
    let (after, limit) = parse_event_query(raw_query.as_deref(), &state, &request)?;
    authorize_boundary(&request, InterfaceOperation::SubscribeEvents)?;
    let wait = (request.context.deadline - Utc::now())
        .to_std()
        .map_err(|_| deadline_error(&request.context.correlation_id))?;
    match tokio::time::timeout(wait, state.events.read_after(after.into(), limit)).await {
        Ok(Ok(batch)) => Ok(Json(batch).into_response()),
        Ok(Err(gap)) => Ok(event_gap_response(gap)),
        Err(_) => Err(deadline_error(&request.context.correlation_id)),
    }
}

async fn artifact(
    State(state): State<AppState>,
    Path(artifact_id): Path<String>,
    RawQuery(raw_query): RawQuery,
    headers: HeaderMap,
    Extension(request): Extension<AuthenticatedRequest>,
) -> Result<Response, ProtocolError> {
    if raw_query.as_deref().is_some_and(|query| !query.is_empty()) {
        return Err(ProtocolError::from(interface_error(
            InterfaceErrorCode::InvalidRequest,
            "artifact ownership is determined by the authenticated boundary",
            request.context.correlation_id,
            None,
        )));
    }
    if artifact_id.is_empty()
        || artifact_id.len() > 128
        || !artifact_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return Err(ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            request.context.correlation_id,
        ));
    }
    if headers.get_all("range").iter().next().is_some() {
        return Err(ProtocolError::from(interface_error(
            InterfaceErrorCode::UnsupportedOperation,
            "artifact ranges are not supported",
            request.context.correlation_id,
            None,
        )));
    }
    authorize_boundary(&request, InterfaceOperation::ReadArtifact)?;
    let Some(content) = state
        .artifacts
        .read(&request.handle, &request.context, &artifact_id)
        .await
        .map_err(ProtocolError::from)?
    else {
        return Err(ProtocolError::from(interface_error(
            InterfaceErrorCode::NotFound,
            "artifact was not found",
            request.context.correlation_id,
            None,
        )));
    };
    let media_type = HeaderValue::from_str(&content.media_type).map_err(|_| {
        ProtocolError::from(interface_error(
            InterfaceErrorCode::Internal,
            "artifact media type is invalid",
            request.context.correlation_id,
            None,
        ))
    })?;
    let mut response = Response::new(Body::from(content.bytes));
    response.headers_mut().insert(CONTENT_TYPE, media_type);
    Ok(response)
}

fn parse_json<T: DeserializeOwned>(
    body: Result<Bytes, BytesRejection>,
    correlation_id: &CorrelationId,
) -> Result<T, ProtocolError> {
    let bytes = body.map_err(|_| ProtocolError::oversized(correlation_id.clone()))?;
    serde_json::from_slice(&bytes).map_err(|_| {
        ProtocolError::from(interface_error(
            InterfaceErrorCode::InvalidRequest,
            "request body is not valid JSON for this operation",
            correlation_id.clone(),
            None,
        ))
    })
}

fn parse_event_query(
    raw: Option<&str>,
    state: &AppState,
    request: &AuthenticatedRequest,
) -> Result<(u64, usize), ProtocolError> {
    let raw = raw.unwrap_or_default();
    if raw.len() > 1024 {
        return Err(ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            request.context.correlation_id.clone(),
        ));
    }
    let mut after = None;
    let mut limit = None;
    for (key, value) in url::form_urlencoded::parse(raw.as_bytes()) {
        match key.as_ref() {
            "after" if after.is_none() => {
                after = Some(value.parse::<u64>().map_err(|_| {
                    ProtocolError::invalid_with(
                        InterfaceErrorCode::InvalidRequest,
                        request.context.correlation_id.clone(),
                    )
                })?)
            }
            "limit" if limit.is_none() => {
                limit = Some(value.parse::<usize>().map_err(|_| {
                    ProtocolError::invalid_with(
                        InterfaceErrorCode::InvalidRequest,
                        request.context.correlation_id.clone(),
                    )
                })?)
            }
            _ => {
                return Err(ProtocolError::invalid_with(
                    InterfaceErrorCode::InvalidRequest,
                    request.context.correlation_id.clone(),
                ))
            }
        }
    }
    let limit = limit.unwrap_or(state.interface.max_event_batch);
    if limit == 0 || limit > state.interface.max_event_batch {
        return Err(ProtocolError::from(interface_error(
            InterfaceErrorCode::InvalidRequest,
            "event limit is outside the configured bound",
            request.context.correlation_id.clone(),
            None,
        )));
    }
    Ok((after.unwrap_or(0), limit))
}

fn event_gap_response(gap: EventGap) -> Response {
    (StatusCode::CONFLICT, Json(gap)).into_response()
}

fn deadline_error(correlation_id: &CorrelationId) -> ProtocolError {
    ProtocolError::from(interface_error(
        InterfaceErrorCode::DeadlineExceeded,
        "request deadline exceeded",
        correlation_id.clone(),
        None,
    ))
}

fn outcome_response(outcome: CommandOutcome) -> Response {
    let (status, retry_after_ms) = match &outcome {
        CommandOutcome::Completed { .. } | CommandOutcome::Restarted { .. } => {
            (StatusCode::OK, None)
        }
        CommandOutcome::RetryableFailure { .. } => (StatusCode::SERVICE_UNAVAILABLE, None),
        CommandOutcome::NeedsReconciliation { .. } => (StatusCode::CONFLICT, None),
        CommandOutcome::PolicyDenied { .. } => (StatusCode::FORBIDDEN, None),
        CommandOutcome::ResourceExhausted { retry_after_ms, .. } => {
            (StatusCode::TOO_MANY_REQUESTS, Some(*retry_after_ms))
        }
        CommandOutcome::Failed { error, .. } if error.code == types::ErrorCode::InvalidRequest => {
            (StatusCode::UNPROCESSABLE_ENTITY, None)
        }
        CommandOutcome::Failed { .. } => (StatusCode::INTERNAL_SERVER_ERROR, None),
    };
    let mut response = (status, Json(outcome)).into_response();
    if let Some(milliseconds) = retry_after_ms {
        let seconds = milliseconds.saturating_add(999) / 1_000;
        if let Ok(value) = HeaderValue::from_str(&seconds.max(1).to_string()) {
            response.headers_mut().insert("retry-after", value);
        }
    }
    response
}
