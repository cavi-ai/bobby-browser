use axum::{
    body::{to_bytes, Body, Bytes},
    extract::{rejection::BytesRejection, Extension, Path, Request, State},
    http::{
        header::{CONTENT_LENGTH, CONTENT_TYPE, TRANSFER_ENCODING},
        HeaderMap, HeaderValue, Method, StatusCode,
    },
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use chrono::Utc;
use interface_core::{canonical_sha256, Event, EventGap, IdempotencyReservation};
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use task_scheduler::{Job, JobConfig, JobError, JobId, JobPriority, JobStatus};
use types::{
    CommandEnvelope, CommandOutcome, CorrelationId, InterfaceErrorCode, InterfaceOperation,
    OpenPageRequest, PrincipalId, RecoveryDecision, WorkflowCheckpoint, WorkflowId,
};
use uuid::Uuid;

use crate::{
    auth::{authorize_boundary, interface_error, AuthenticatedRequest, ProtocolError},
    jobs::JobSubmitOutcome,
    AppState,
};

pub(crate) fn protected_router() -> Router<AppState> {
    Router::new()
        .route("/v1/runtime", get(runtime_info))
        .route("/v1/sessions", get(list_sessions).post(create_session))
        .route(
            "/v1/sessions/{session}",
            axum::routing::delete(delete_session),
        )
        .route("/v1/pages", post(open_page))
        .route(
            "/v1/sessions/{session}/pages/{page}/forms",
            get(form_snapshot),
        )
        .route("/v1/commands", post(submit_command))
        .route("/v1/context/ask", get(context_ask))
        .route("/v1/context/site/{key}", get(context_site))
        .route("/v1/checkpoints", post(checkpoint))
        .route(
            "/v1/recovery/{workflow}",
            post(recover).get(recovery_status),
        )
        .route("/v1/events", get(events))
        .route("/v1/artifacts/{id}", get(artifact))
        .route("/v1/jobs", post(submit_job))
        .route("/v1/jobs/{job}", get(get_job).delete(cancel_job))
        .route("/v1/principals", post(issue_principal))
        .route(
            "/v1/principals/{principal}",
            axum::routing::delete(revoke_principal),
        )
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

#[derive(Clone)]
struct ContextAskQuery {
    session: types::SessionId,
    page: types::PageId,
    description: String,
}

async fn context_ask(
    Extension(request): Extension<AuthenticatedRequest>,
    Extension(query): Extension<ContextAskQuery>,
) -> Result<Json<serde_json::Value>, ProtocolError> {
    request
        .runtime
        .authorize_operation(request.context.clone(), InterfaceOperation::ReadContext)
        .await
        .map_err(ProtocolError::from)?;
    let answer = request
        .runtime
        .context_ask(
            request.context,
            query.session,
            query.page,
            query.description,
        )
        .await
        .map_err(ProtocolError::from)?;
    // `None` is an answer, not a failure — same contract as MCP context_ask.
    Ok(Json(serde_json::json!({ "answer": answer })))
}

async fn context_site(
    Path(key): Path<String>,
    Extension(request): Extension<AuthenticatedRequest>,
) -> Result<Json<serde_json::Value>, ProtocolError> {
    let site = request
        .runtime
        .context_site(request.context, key)
        .await
        .map_err(ProtocolError::from)?;
    Ok(Json(serde_json::json!({ "site": site })))
}

async fn form_snapshot(
    Path((session, page)): Path<(String, String)>,
    Extension(query): Extension<FormSnapshotQuery>,
    Extension(request): Extension<AuthenticatedRequest>,
) -> Result<Json<types::FormSnapshot>, ProtocolError> {
    let session = types::SessionId(Uuid::parse_str(&session).map_err(|_| {
        ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            request.context.correlation_id.clone(),
        )
    })?);
    let page = types::PageId(Uuid::parse_str(&page).map_err(|_| {
        ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            request.context.correlation_id.clone(),
        )
    })?);
    request
        .runtime
        .form_snapshot(request.context, session, page, query.max_controls)
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
        .submit(request.context.clone(), envelope.clone())
        .await
        .map_err(ProtocolError::from)?;
    state
        .artifacts
        .admit_outcome(&request.handle, &request.context, &envelope, &outcome)
        .await
        .map_err(|_| {
            ProtocolError::from(interface_error(
                InterfaceErrorCode::ResourceExhausted,
                "artifact catalog capacity exhausted",
                request.context.correlation_id.clone(),
                None,
            ))
        })?;
    let payload = serde_json::to_value(&outcome).unwrap_or_else(|_| {
        serde_json::json!({
            "error": "command outcome could not be serialized"
        })
    });
    state
        .events
        .append_for(
            request.context.principal_id.clone(),
            Event::new("command.outcome", payload),
        )
        .await;
    Ok(outcome_response(outcome))
}

/// Callers name commands whose evidence the runtime already recorded; they
/// never hand evidence in. `deny_unknown_fields` rejects an `evidence` key at
/// the boundary rather than persisting a checkpoint nothing verified.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CheckpointRequest {
    checkpoint: WorkflowCheckpoint,
    #[serde(default)]
    evidence_refs: Vec<types::CommandId>,
}

/// Must match `mcp_gateway::schema::MAX_EVIDENCE_ITEMS`: both surfaces resolve
/// through the same journal and bound the work the same way.
const MAX_EVIDENCE_REFS: usize = 128;

async fn checkpoint(
    Extension(request): Extension<AuthenticatedRequest>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Json<WorkflowCheckpoint>, ProtocolError> {
    let input: CheckpointRequest = parse_json(body, &request.context.correlation_id)?;
    if input.evidence_refs.len() > MAX_EVIDENCE_REFS {
        return Err(ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            request.context.correlation_id,
        ));
    }
    // Resolve before checkpointing: `resolve_command_evidence` checks each
    // command's owning session, so a reference to another principal's command
    // fails here rather than contributing evidence.
    let evidence = request
        .runtime
        .resolve_command_evidence(request.context.clone(), input.evidence_refs)
        .await?;
    request
        .runtime
        .checkpoint(request.context, input.checkpoint, evidence)
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
    Extension(query): Extension<EventQuery>,
    Extension(request): Extension<AuthenticatedRequest>,
) -> Result<Response, ProtocolError> {
    authorize_boundary(&request, InterfaceOperation::SubscribeEvents)?;
    if query.stream {
        return Ok(event_stream_response(
            state.events.clone(),
            request.context.principal_id.clone(),
            query.after,
            query.limit,
        ));
    }
    let wait = (request.context.deadline - Utc::now())
        .to_std()
        .map_err(|_| deadline_error(&request.context.correlation_id))?;
    match tokio::time::timeout(
        wait,
        state.events.read_after_for(
            &request.context.principal_id,
            query.after.into(),
            query.limit,
        ),
    )
    .await
    {
        Ok(Ok(batch)) => Ok(Json(batch).into_response()),
        Ok(Err(gap)) => Ok(event_gap_response(
            gap,
            request.context.correlation_id.clone(),
        )),
        Err(_) => Err(deadline_error(&request.context.correlation_id)),
    }
}

const EVENT_STREAM_HEARTBEAT: std::time::Duration = std::time::Duration::from_secs(15);

/// Server-sent-event stream of the principal-scoped event history. Each event's
/// `id` is its cursor, so clients resume with `after` or `Last-Event-ID`. A
/// cursor gap is terminal: an `event.gap` notification is emitted and the
/// stream closes, matching the 409 poll path.
fn event_stream_response(
    store: interface_core::EventStore,
    principal: types::PrincipalId,
    after: u64,
    limit: usize,
) -> Response {
    use axum::response::sse::Event as SseEvent;
    use futures_util::stream;

    enum Read {
        Events(std::collections::VecDeque<interface_core::Event>),
        Gap(interface_core::EventGap),
        Heartbeat,
    }
    struct Cursor {
        after: u64,
        limit: usize,
        pending: std::collections::VecDeque<interface_core::Event>,
        done: bool,
    }

    let stream = stream::unfold(
        (
            store,
            principal,
            Cursor {
                after,
                limit,
                pending: Default::default(),
                done: false,
            },
        ),
        |(store, principal, mut cursor)| async move {
            loop {
                if cursor.done {
                    return None;
                }
                if let Some(event) = cursor.pending.pop_front() {
                    cursor.after = event.cursor.0;
                    let sse = SseEvent::default()
                        .id(event.cursor.0.to_string())
                        .event(event.kind.clone())
                        .json_data(&event.payload)
                        .unwrap_or_else(|_| SseEvent::default().data(""));
                    return Some((
                        Ok::<_, std::convert::Infallible>(sse),
                        (store, principal, cursor),
                    ));
                }
                let outcome = {
                    let read = store.read_after_for(&principal, cursor.after.into(), cursor.limit);
                    tokio::pin!(read);
                    match tokio::time::timeout(EVENT_STREAM_HEARTBEAT, read).await {
                        Ok(Ok(batch)) => Read::Events(batch.events.into()),
                        Ok(Err(gap)) => Read::Gap(gap),
                        Err(_) => Read::Heartbeat,
                    }
                };
                match outcome {
                    Read::Events(events) => {
                        cursor.pending = events;
                    }
                    Read::Gap(gap) => {
                        cursor.done = true;
                        let sse = SseEvent::default()
                            .event("event.gap")
                            .json_data(gap)
                            .unwrap_or_else(|_| SseEvent::default().data(""));
                        return Some((Ok(sse), (store, principal, cursor)));
                    }
                    Read::Heartbeat => {
                        return Some((
                            Ok(SseEvent::default().comment("keep-alive")),
                            (store, principal, cursor),
                        ));
                    }
                }
            }
        },
    );
    axum::response::sse::Sse::new(stream).into_response()
}

async fn artifact(
    State(state): State<AppState>,
    Path(artifact_id): Path<String>,
    headers: HeaderMap,
    Extension(request): Extension<AuthenticatedRequest>,
) -> Result<Response, ProtocolError> {
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

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct IssuePrincipalRequest {
    principal_id: Uuid,
    capabilities: Vec<types::Capability>,
    expires_at: chrono::DateTime<Utc>,
}

/// Longest lifetime an interface-issued principal token may be granted.
const MAX_TOKEN_TTL_DAYS: i64 = 90;

async fn issue_principal(
    State(state): State<AppState>,
    Extension(request): Extension<AuthenticatedRequest>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ProtocolError> {
    authorize_boundary(&request, InterfaceOperation::IssuePrincipal)?;
    let input: IssuePrincipalRequest = parse_json(body, &request.context.correlation_id)?;
    if input.capabilities.is_empty() {
        return Err(ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            request.context.correlation_id.clone(),
        ));
    }
    if input
        .capabilities
        .contains(&types::Capability::AuthorityAdmin)
    {
        return Err(ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            request.context.correlation_id.clone(),
        ));
    }
    if !input
        .capabilities
        .iter()
        .all(|capability| request.context.capabilities.contains(*capability))
    {
        return Err(ProtocolError::from(interface_error(
            InterfaceErrorCode::InvalidRequest,
            "issued capabilities must not exceed the issuer's",
            request.context.correlation_id.clone(),
            None,
        )));
    }
    if input.expires_at > Utc::now() + chrono::Duration::days(MAX_TOKEN_TTL_DAYS) {
        return Err(ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            request.context.correlation_id.clone(),
        ));
    }
    let issued = state
        .authority
        .issue(
            PrincipalId::from_uuid(input.principal_id),
            input.capabilities.clone(),
            input.expires_at,
        )
        .await
        .map_err(ProtocolError::from)?;
    tracing::info!("principal.issued");
    Ok((
        StatusCode::CREATED,
        Json(serde_json::json!({
            "bearer": issued.expose_once(),
            "principalId": input.principal_id,
            "capabilities": input.capabilities,
            "expiresAt": input.expires_at,
        })),
    )
        .into_response())
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SubmitJobRequest {
    name: String,
    #[serde(default)]
    payload: serde_json::Value,
    #[serde(default)]
    priority: JobPriorityDto,
    #[serde(default = "default_max_retries")]
    max_retries: u32,
    timeout_ms: Option<u64>,
}

fn default_max_retries() -> u32 {
    3
}

#[derive(Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
enum JobPriorityDto {
    Low,
    #[default]
    Normal,
    High,
    Critical,
}

impl From<JobPriorityDto> for JobPriority {
    fn from(value: JobPriorityDto) -> Self {
        match value {
            JobPriorityDto::Low => JobPriority::Low,
            JobPriorityDto::Normal => JobPriority::Normal,
            JobPriorityDto::High => JobPriority::High,
            JobPriorityDto::Critical => JobPriority::Critical,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JobSubmitResponse {
    job_id: String,
    status: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct JobStatusResponse {
    id: String,
    name: String,
    priority: String,
    status: String,
    payload: serde_json::Value,
    created_at: chrono::DateTime<Utc>,
    started_at: Option<chrono::DateTime<Utc>>,
    completed_at: Option<chrono::DateTime<Utc>>,
    retry_count: u32,
    max_retries: u32,
    result: Option<serde_json::Value>,
    error: Option<String>,
    timeout_ms: Option<u64>,
    correlation_id: Option<String>,
}

fn status_wire(status: &JobStatus) -> String {
    match status {
        JobStatus::Pending => "pending",
        JobStatus::Running => "running",
        JobStatus::Completed => "completed",
        JobStatus::Failed => "failed",
        JobStatus::Cancelled => "cancelled",
    }
    .to_string()
}

fn priority_wire(priority: &JobPriority) -> String {
    match priority {
        JobPriority::Low => "low",
        JobPriority::Normal => "normal",
        JobPriority::High => "high",
        JobPriority::Critical => "critical",
    }
    .to_string()
}

fn job_status_response(job: Job) -> JobStatusResponse {
    JobStatusResponse {
        id: job.id.0,
        name: job.name,
        priority: priority_wire(&job.priority),
        status: status_wire(&job.status),
        payload: job.payload,
        created_at: job.created_at,
        started_at: job.started_at,
        completed_at: job.completed_at,
        retry_count: job.retry_count,
        max_retries: job.max_retries,
        result: job.result.map(|r| {
            serde_json::json!({
                "jobId": r.job_id.0,
                "success": r.success,
                "output": r.output,
                "error": r.error,
                "completedAt": r.completed_at,
            })
        }),
        error: job.error,
        timeout_ms: job.timeout_ms,
        correlation_id: job.correlation_id,
    }
}

fn job_error(err: JobError, correlation_id: CorrelationId) -> ProtocolError {
    match err {
        JobError::NotFound(_) => ProtocolError::from(interface_error(
            InterfaceErrorCode::InvalidRequest,
            "job not found",
            correlation_id,
            None,
        )),
        JobError::QueueFull => ProtocolError::from(interface_error(
            InterfaceErrorCode::ResourceExhausted,
            "job queue is full",
            correlation_id,
            None,
        )),
        JobError::Execution(message) if message.contains("already finished") => {
            ProtocolError::from(interface_error(
                InterfaceErrorCode::InvalidRequest,
                &message,
                correlation_id,
                None,
            ))
        }
        other => ProtocolError::from(interface_error(
            InterfaceErrorCode::InvalidRequest,
            &other.to_string(),
            correlation_id,
            None,
        )),
    }
}

async fn dispatch_submit_job(
    state: &AppState,
    request: &AuthenticatedRequest,
    input: SubmitJobRequest,
) -> Result<JobSubmitOutcome, ProtocolError> {
    let mut config = JobConfig::new(input.name, input.payload)
        .with_priority(input.priority.into())
        .with_max_retries(input.max_retries);
    if let Some(timeout_ms) = input.timeout_ms {
        config = config.with_timeout(std::time::Duration::from_millis(timeout_ms));
    }
    config = config.with_correlation_id(request.context.correlation_id.as_uuid().to_string());
    config = config.with_owner(request.context.principal_id.clone());
    let id = state
        .scheduler
        .submit(config)
        .await
        .map_err(|e| job_error(e, request.context.correlation_id.clone()))?;
    let status = state
        .scheduler
        .get_job(&id)
        .await
        .map(|job| job.status)
        .unwrap_or(JobStatus::Pending);
    Ok(JobSubmitOutcome { job_id: id, status })
}

async fn submit_job(
    State(state): State<AppState>,
    Extension(request): Extension<AuthenticatedRequest>,
    body: Result<Bytes, BytesRejection>,
) -> Result<Response, ProtocolError> {
    authorize_boundary(&request, InterfaceOperation::SubmitJob)?;
    let raw = match body {
        Ok(bytes) => bytes,
        Err(_) => {
            return Err(ProtocolError::invalid_with(
                InterfaceErrorCode::InvalidRequest,
                request.context.correlation_id.clone(),
            ))
        }
    };
    let input: SubmitJobRequest = serde_json::from_slice(&raw).map_err(|_| {
        ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            request.context.correlation_id.clone(),
        )
    })?;
    if input.name.trim().is_empty() {
        return Err(ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            request.context.correlation_id.clone(),
        ));
    }

    let outcome = if let Some(key) = request.context.idempotency_key.clone() {
        let digest = canonical_sha256(&input).map_err(|_| {
            ProtocolError::invalid_with(
                InterfaceErrorCode::InvalidRequest,
                request.context.correlation_id.clone(),
            )
        })?;
        let reservation = state
            .job_idempotency
            .reserve(
                request.context.principal_id.clone(),
                key,
                InterfaceOperation::SubmitJob,
                digest,
                Utc::now(),
                request.context.deadline,
                request.context.correlation_id.clone(),
            )
            .await
            .map_err(ProtocolError::from)?;
        match reservation {
            IdempotencyReservation::Replay(outcome) => outcome,
            IdempotencyReservation::Acquired(permit) => {
                match dispatch_submit_job(&state, &request, input).await {
                    Ok(outcome) => {
                        state
                            .job_idempotency
                            .finish(permit, outcome.clone(), Utc::now())
                            .await
                            .map_err(ProtocolError::from)?;
                        outcome
                    }
                    Err(error) => {
                        state.job_idempotency.abandon(permit).await;
                        return Err(error);
                    }
                }
            }
        }
    } else {
        dispatch_submit_job(&state, &request, input).await?
    };

    Ok((
        StatusCode::CREATED,
        Json(JobSubmitResponse {
            job_id: outcome.job_id.0,
            status: status_wire(&outcome.status),
        }),
    )
        .into_response())
}

async fn get_job(
    State(state): State<AppState>,
    Extension(request): Extension<AuthenticatedRequest>,
    Path(job): Path<String>,
) -> Result<Json<JobStatusResponse>, ProtocolError> {
    authorize_boundary(&request, InterfaceOperation::ReadJob)?;
    let id = JobId(job);
    let Some(job) = state.scheduler.get_job(&id).await else {
        return Err(job_error(
            JobError::NotFound(id),
            request.context.correlation_id.clone(),
        ));
    };
    // Ownership is indistinguishable from absence: another principal's job
    // answers NotFound, never its payload or existence. Jobs persisted
    // before ownership (owner: None) stay readable by any job:read holder.
    if job
        .owner
        .as_ref()
        .is_some_and(|owner| *owner != request.context.principal_id)
    {
        return Err(job_error(
            JobError::NotFound(id),
            request.context.correlation_id.clone(),
        ));
    }
    Ok(Json(job_status_response(job)))
}

async fn cancel_job(
    State(state): State<AppState>,
    Extension(request): Extension<AuthenticatedRequest>,
    Path(job): Path<String>,
) -> Result<StatusCode, ProtocolError> {
    authorize_boundary(&request, InterfaceOperation::CancelJob)?;
    let id = JobId(job);
    // Same ownership gate as get_job: cancel is a read of another
    // principal's work queue otherwise.
    match state.scheduler.get_job(&id).await {
        Some(job)
            if job
                .owner
                .as_ref()
                .is_some_and(|owner| *owner != request.context.principal_id) =>
        {
            return Err(job_error(
                JobError::NotFound(id),
                request.context.correlation_id.clone(),
            ));
        }
        _ => {}
    }
    state
        .scheduler
        .cancel_job(&id)
        .await
        .map_err(|e| job_error(e, request.context.correlation_id.clone()))?;
    Ok(StatusCode::NO_CONTENT)
}

async fn recovery_status(
    Path(workflow): Path<String>,
    Extension(request): Extension<AuthenticatedRequest>,
) -> Result<Json<types::RecoveryStatus>, ProtocolError> {
    let workflow = WorkflowId(Uuid::parse_str(&workflow).map_err(|_| {
        ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            request.context.correlation_id.clone(),
        )
    })?);
    request
        .runtime
        .recovery_status(request.context, workflow)
        .await
        .map(Json)
        .map_err(ProtocolError::from)
}

async fn delete_session(
    Path(session): Path<String>,
    Extension(request): Extension<AuthenticatedRequest>,
) -> Result<Response, ProtocolError> {
    let session = types::SessionId(Uuid::parse_str(&session).map_err(|_| {
        ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            request.context.correlation_id.clone(),
        )
    })?);
    request
        .runtime
        .delete_session(request.context, session)
        .await
        .map_err(ProtocolError::from)?;
    Ok(StatusCode::NO_CONTENT.into_response())
}

async fn revoke_principal(
    Path(principal): Path<String>,
    Extension(request): Extension<AuthenticatedRequest>,
    State(state): State<AppState>,
) -> Result<Response, ProtocolError> {
    authorize_boundary(&request, InterfaceOperation::RevokePrincipal)?;
    let principal = PrincipalId::from_uuid(Uuid::parse_str(&principal).map_err(|_| {
        ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            request.context.correlation_id.clone(),
        )
    })?);
    state
        .authority
        .revoke(&principal)
        .await
        .map_err(ProtocolError::from)?;
    tracing::info!("principal.revoked");
    Ok(StatusCode::NO_CONTENT.into_response())
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

#[derive(Clone, Copy)]
pub(crate) struct EventQuery {
    after: u64,
    limit: usize,
    stream: bool,
}

#[derive(Clone, Copy, Default)]
struct FormSnapshotQuery {
    max_controls: Option<u32>,
}

fn parse_context_ask_query(
    query: Option<&str>,
    correlation_id: &types::CorrelationId,
) -> Result<ContextAskQuery, ProtocolError> {
    let invalid =
        || ProtocolError::invalid_with(InterfaceErrorCode::InvalidRequest, correlation_id.clone());
    let pairs =
        url::form_urlencoded::parse(query.ok_or_else(invalid)?.as_bytes()).collect::<Vec<_>>();
    if pairs.len() != 3 {
        return Err(invalid());
    }
    let mut session = None;
    let mut page = None;
    let mut description = None;
    for (key, value) in pairs {
        match key.as_ref() {
            "sessionId" => session = Some(Uuid::parse_str(&value).map_err(|_| invalid())?),
            "pageId" => page = Some(Uuid::parse_str(&value).map_err(|_| invalid())?),
            "description" if !value.is_empty() && value.len() <= 256 => {
                description = Some(value.into_owned())
            }
            _ => return Err(invalid()),
        }
    }
    Ok(ContextAskQuery {
        session: types::SessionId(session.ok_or_else(invalid)?),
        page: types::PageId(page.ok_or_else(invalid)?),
        description: description.ok_or_else(invalid)?,
    })
}

pub(crate) async fn validate_request_boundary(
    state: &AppState,
    request: &mut Request,
) -> Result<(), ProtocolError> {
    let authenticated = request
        .extensions()
        .get::<AuthenticatedRequest>()
        .expect("authentication inserts trusted request context");
    let correlation_id = authenticated.context.correlation_id.clone();
    let remaining = (authenticated.context.deadline - Utc::now())
        .to_std()
        .map_err(|_| deadline_error(&correlation_id))?;
    let body_deadline = tokio::time::Instant::now() + remaining;
    let path = request.uri().path();
    let bodyful = matches!(
        (request.method(), path),
        (&Method::POST, "/v1/sessions")
            | (&Method::POST, "/v1/pages")
            | (&Method::POST, "/v1/commands")
            | (&Method::POST, "/v1/checkpoints")
            | (&Method::POST, "/v1/principals")
            | (&Method::POST, "/v1/jobs")
    );

    if path == "/v1/events" {
        let query = parse_event_query(request.uri().query(), state, &correlation_id)?;
        request.extensions_mut().insert(query);
    } else if path.starts_with("/v1/sessions/") && path.ends_with("/forms") {
        let mut parsed = FormSnapshotQuery::default();
        if let Some(query) = request.uri().query() {
            let pairs = url::form_urlencoded::parse(query.as_bytes()).collect::<Vec<_>>();
            if pairs.len() != 1 || pairs[0].0 != "maxControls" {
                return Err(ProtocolError::invalid_with(
                    InterfaceErrorCode::InvalidRequest,
                    correlation_id,
                ));
            }
            let value = pairs[0].1.parse::<u32>().ok();
            if value.is_none_or(|value| !(1..=512).contains(&value)) {
                return Err(ProtocolError::invalid_with(
                    InterfaceErrorCode::InvalidRequest,
                    correlation_id,
                ));
            }
            parsed.max_controls = value;
        }
        request.extensions_mut().insert(parsed);
    } else if path == "/v1/context/ask" {
        let parsed = parse_context_ask_query(request.uri().query(), &correlation_id)?;
        request.extensions_mut().insert(parsed);
    } else if request.uri().query().is_some() {
        return Err(ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            correlation_id,
        ));
    }

    if !bodyful && declared_body(request.headers()) {
        return Err(ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            correlation_id,
        ));
    }

    let body = std::mem::replace(request.body_mut(), Body::empty());
    let bytes = match tokio::time::timeout_at(
        body_deadline,
        to_bytes(body, state.interface.max_request_bytes),
    )
    .await
    {
        Err(_) => return Err(deadline_error(&correlation_id)),
        Ok(Ok(bytes)) => bytes,
        Ok(Err(_)) if bodyful => return Err(ProtocolError::oversized(correlation_id)),
        Ok(Err(_)) => {
            return Err(ProtocolError::invalid_with(
                InterfaceErrorCode::InvalidRequest,
                correlation_id,
            ))
        }
    };
    if !bodyful && !bytes.is_empty() {
        return Err(ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            correlation_id,
        ));
    }
    *request.body_mut() = Body::from(bytes);
    Ok(())
}

fn declared_body(headers: &HeaderMap) -> bool {
    if headers.get_all(TRANSFER_ENCODING).iter().next().is_some() {
        return true;
    }
    let mut lengths = headers.get_all(CONTENT_LENGTH).iter();
    let Some(length) = lengths.next() else {
        return false;
    };
    if lengths.next().is_some() {
        return true;
    }
    length
        .to_str()
        .ok()
        .and_then(|value| value.parse::<usize>().ok())
        != Some(0)
}

fn parse_event_query(
    raw: Option<&str>,
    state: &AppState,
    correlation_id: &CorrelationId,
) -> Result<EventQuery, ProtocolError> {
    let raw = raw.unwrap_or_default();
    if raw.len() > 1024 || !valid_query_encoding(raw) {
        return Err(ProtocolError::invalid_with(
            InterfaceErrorCode::InvalidRequest,
            correlation_id.clone(),
        ));
    }
    let mut after = None;
    let mut limit = None;
    let mut stream = None;
    for (key, value) in url::form_urlencoded::parse(raw.as_bytes()) {
        match key.as_ref() {
            "stream" if stream.is_none() => stream = Some(matches!(value.as_ref(), "1" | "true")),
            "after" if after.is_none() => {
                after = Some(value.parse::<u64>().map_err(|_| {
                    ProtocolError::invalid_with(
                        InterfaceErrorCode::InvalidRequest,
                        correlation_id.clone(),
                    )
                })?)
            }
            "limit" if limit.is_none() => {
                limit = Some(value.parse::<usize>().map_err(|_| {
                    ProtocolError::invalid_with(
                        InterfaceErrorCode::InvalidRequest,
                        correlation_id.clone(),
                    )
                })?)
            }
            _ => {
                return Err(ProtocolError::invalid_with(
                    InterfaceErrorCode::InvalidRequest,
                    correlation_id.clone(),
                ))
            }
        }
    }
    let limit = limit.unwrap_or(state.interface.max_event_batch);
    if limit == 0 || limit > state.interface.max_event_batch {
        return Err(ProtocolError::from(interface_error(
            InterfaceErrorCode::InvalidRequest,
            "event limit is outside the configured bound",
            correlation_id.clone(),
            None,
        )));
    }
    Ok(EventQuery {
        after: after.unwrap_or(0),
        limit,
        stream: stream.unwrap_or(false),
    })
}

fn valid_query_encoding(raw: &str) -> bool {
    let bytes = raw.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len()
                || !bytes[index + 1].is_ascii_hexdigit()
                || !bytes[index + 2].is_ascii_hexdigit()
            {
                return false;
            }
            index += 3;
        } else {
            index += 1;
        }
    }
    true
}

fn event_gap_response(gap: EventGap, correlation_id: CorrelationId) -> Response {
    let error = interface_error(
        InterfaceErrorCode::InvalidRequest,
        "event history has a cursor gap",
        correlation_id,
        None,
    );
    (
        StatusCode::CONFLICT,
        Json(serde_json::json!({ "error": error, "gap": gap })),
    )
        .into_response()
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
    // Default backoff for 503 retryable command failures when the outcome has
    // no explicit `retry_after_ms` (unlike ResourceExhausted).
    const RETRYABLE_FAILURE_RETRY_AFTER_MS: u64 = 1_000;
    let (status, retry_after_ms) = match &outcome {
        CommandOutcome::Completed { .. } | CommandOutcome::Restarted { .. } => {
            (StatusCode::OK, None)
        }
        CommandOutcome::RetryableFailure { .. } => (
            StatusCode::SERVICE_UNAVAILABLE,
            Some(RETRYABLE_FAILURE_RETRY_AFTER_MS),
        ),
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
