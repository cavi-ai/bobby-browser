use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Multipart, Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, Request, Response, StatusCode};
use axum::response::{Html, IntoResponse};
use axum::routing::{get, patch, post};
use axum::{Json, Router};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Clone)]
pub struct ScenarioConfig {
    pub seed: String,
    pub reject_postal_once: bool,
    pub report_interrupt: bool,
}

impl ScenarioConfig {
    pub fn seeded(seed: impl Into<String>) -> Self {
        Self {
            seed: seed.into(),
            reject_postal_once: true,
            report_interrupt: false,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioSnapshot {
    pub atlas_priority: String,
    pub priority_updates: u64,
    pub onboarding_records: u64,
    pub uploaded_sha256: Option<String>,
    pub preview_confirmations: u64,
    pub authorization_grants: u64,
    pub report_generations: u64,
}

#[derive(Debug)]
struct RunState {
    atlas_priority: String,
    priority_updates: u64,
    onboarding_records: u64,
    reject_postal_remaining: bool,
    uploaded: Option<Vec<u8>>,
    preview_confirmations: u64,
    connected: bool,
    authorization_grants: u64,
    report_generations: u64,
    requests: Vec<String>,
}

#[derive(Debug)]
struct SharedState {
    run_id: String,
    dist: PathBuf,
    inner: Mutex<RunState>,
}

pub struct ScenarioServer {
    address: SocketAddr,
    state: Arc<SharedState>,
    task: tokio::task::JoinHandle<()>,
}

impl ScenarioServer {
    pub async fn start(config: ScenarioConfig) -> TestResult<Self> {
        let dist = repository_root().join("packages/bobby-gauntlet/dist");
        if !dist.join("index.html").is_file() || !dist.join("app.js").is_file() {
            return Err("built Northstar application is missing; run pnpm --filter @cavi-ai/bobby-gauntlet build".into());
        }
        let run_id = format!("run-{}", sanitize(&config.seed));
        let state = Arc::new(SharedState {
            run_id,
            dist,
            inner: Mutex::new(RunState {
                atlas_priority: "normal".into(),
                priority_updates: 0,
                onboarding_records: 0,
                reject_postal_remaining: config.reject_postal_once,
                uploaded: None,
                preview_confirmations: 0,
                connected: false,
                authorization_grants: 0,
                report_generations: 0,
                requests: Vec::new(),
            }),
        });
        let app = Router::new()
            .route("/api/dashboard", get(dashboard))
            .route("/api/customers", get(customers))
            .route("/api/customers/{id}", get(customer))
            .route("/api/customers/{id}/priority", patch(update_priority))
            .route("/api/onboarding", post(onboard))
            .route("/api/documents", post(upload_document))
            .route("/api/documents/{id}/preview", get(document_preview))
            .route("/api/documents/{id}/confirm", post(confirm_preview))
            .route("/api/integrations/ledger-cloud", get(integration_state))
            .route("/authorize/ledger-cloud", get(authorize_page))
            .route(
                "/api/integrations/ledger-cloud/complete",
                post(complete_authorization),
            )
            .route("/api/reports", post(create_report))
            .route("/api/reports/{id}", get(report_state))
            .route("/api/reports/{id}/download", get(download_report))
            .fallback(get(static_file))
            .with_state(Arc::clone(&state));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let task = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        Ok(Self {
            address,
            state,
            task,
        })
    }

    pub fn base_url(&self) -> String {
        format!("http://{}", self.address)
    }

    pub fn application_url(&self, path: &str) -> String {
        format!("{}{}?run={}", self.base_url(), path, self.run_id())
    }

    pub fn run_id(&self) -> &str {
        &self.state.run_id
    }

    pub async fn snapshot(&self) -> ScenarioSnapshot {
        let state = self.state.inner.lock().await;
        ScenarioSnapshot {
            atlas_priority: state.atlas_priority.clone(),
            priority_updates: state.priority_updates,
            onboarding_records: state.onboarding_records,
            uploaded_sha256: state
                .uploaded
                .as_ref()
                .map(|bytes| format!("{:x}", Sha256::digest(bytes))),
            preview_confirmations: state.preview_confirmations,
            authorization_grants: state.authorization_grants,
            report_generations: state.report_generations,
        }
    }

    pub async fn request_log(&self) -> Vec<String> {
        self.state.inner.lock().await.requests.clone()
    }
}

impl Drop for ScenarioServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

fn require_run(headers: &HeaderMap, state: &SharedState) -> Result<(), (StatusCode, Json<Value>)> {
    if headers
        .get("x-northstar-run")
        .and_then(|value| value.to_str().ok())
        == Some(&state.run_id)
    {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "code": "invalid_run", "message": "A valid run identity is required." })),
        ))
    }
}

async fn record(state: &SharedState, value: impl Into<String>) {
    state.inner.lock().await.requests.push(value.into());
}

async fn dashboard(State(state): State<Arc<SharedState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(error) = require_run(&headers, &state) {
        return error.into_response();
    }
    record(&state, "GET /api/dashboard").await;
    Json(json!({ "activeCustomers": 48, "pendingOnboarding": 6, "documentsProcessed": 127, "reportsReady": 9 })).into_response()
}

#[derive(Deserialize)]
struct CustomerQuery {
    q: Option<String>,
}

async fn customers(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Query(query): Query<CustomerQuery>,
) -> impl IntoResponse {
    if let Err(error) = require_run(&headers, &state) {
        return error.into_response();
    }
    record(
        &state,
        format!(
            "GET /api/customers?q={}",
            query.q.as_deref().unwrap_or_default()
        ),
    )
    .await;
    let inner = state.inner.lock().await;
    let matches = query
        .q
        .as_deref()
        .is_none_or(|value| value.is_empty() || "atlas labs".contains(&value.to_ascii_lowercase()));
    let values = if matches {
        vec![customer_json(&inner)]
    } else {
        Vec::new()
    };
    Json(values).into_response()
}

async fn customer(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> impl IntoResponse {
    if let Err(error) = require_run(&headers, &state) {
        return error.into_response();
    }
    record(&state, format!("GET /api/customers/{id}")).await;
    if id != "cus_atlas" {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "not_found", "message": "Customer not found." })),
        )
            .into_response();
    }
    let inner = state.inner.lock().await;
    Json(customer_json(&inner)).into_response()
}

#[derive(Deserialize)]
struct PriorityBody {
    priority: String,
}

async fn update_priority(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
    Json(body): Json<PriorityBody>,
) -> impl IntoResponse {
    if let Err(error) = require_run(&headers, &state) {
        return error.into_response();
    }
    if id != "cus_atlas" || !["low", "normal", "high"].contains(&body.priority.as_str()) {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "code": "invalid_priority", "message": "Choose a valid priority." })),
        )
            .into_response();
    }
    let mut inner = state.inner.lock().await;
    inner
        .requests
        .push(format!("PATCH /api/customers/{id}/priority"));
    inner.atlas_priority = body.priority;
    inner.priority_updates += 1;
    Json(customer_json(&inner)).into_response()
}

fn customer_json(state: &RunState) -> Value {
    json!({ "id": "cus_atlas", "name": "Atlas Labs", "email": "ops@atlas.example", "company": "Atlas Labs", "joinedAt": "2026-01-15", "priority": state.atlas_priority, "status": "active" })
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OnboardingBody {
    postal_code: String,
}

async fn onboard(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<OnboardingBody>,
) -> impl IntoResponse {
    if let Err(error) = require_run(&headers, &state) {
        return error.into_response();
    }
    let mut inner = state.inner.lock().await;
    inner.requests.push("POST /api/onboarding".into());
    if inner.reject_postal_remaining && body.postal_code != "10001" {
        inner.reject_postal_remaining = false;
        return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "code": "postal_rejected", "message": "Review the highlighted field.", "fields": { "postalCode": "Use 10001 for this account." } }))).into_response();
    }
    inner.onboarding_records += 1;
    Json(json!({ "id": "onb_atlas_01", "status": "complete" })).into_response()
}

async fn upload_document(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> impl IntoResponse {
    if let Err(error) = require_run(&headers, &state) {
        return error.into_response();
    }
    let mut customer_id = None;
    let mut filename = None;
    let mut media_type = None;
    let mut bytes = None;
    while let Ok(Some(field)) = multipart.next_field().await {
        match field.name() {
            Some("customerId") => customer_id = field.text().await.ok(),
            Some("document") => {
                filename = field.file_name().map(ToOwned::to_owned);
                media_type = field.content_type().map(ToOwned::to_owned);
                bytes = field.bytes().await.ok().map(|value| value.to_vec());
            }
            _ => {}
        }
    }
    let Some(bytes) = bytes else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "code": "missing_document", "message": "Choose a document." })),
        )
            .into_response();
    };
    let digest = format!("{:x}", Sha256::digest(&bytes));
    state.inner.lock().await.uploaded = Some(bytes);
    Json(json!({ "id": "doc_atlas_01", "customerId": customer_id.unwrap_or_else(|| "cus_atlas".into()), "filename": filename.unwrap_or_else(|| "document.txt".into()), "mediaType": media_type.unwrap_or_else(|| "application/octet-stream".into()), "sha256": digest, "previewUrl": "/api/documents/doc_atlas_01/preview" })).into_response()
}

async fn document_preview(AxumPath(id): AxumPath<String>) -> Html<String> {
    Html(format!(
        r#"<!doctype html><title>Document preview</title><main><h1>Approved customer document</h1><p>Document {id}</p><form method="post" action="/api/documents/{id}/confirm"><button type="submit" aria-label="Confirm document preview">Confirm document</button></form></main>"#
    ))
}

async fn confirm_preview(
    State(state): State<Arc<SharedState>>,
    AxumPath(_id): AxumPath<String>,
) -> impl IntoResponse {
    state.inner.lock().await.preview_confirmations += 1;
    Html("<!doctype html><title>Document confirmed</title><p role=status>Document confirmed</p>")
}

async fn integration_state(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(error) = require_run(&headers, &state) {
        return error.into_response();
    }
    let inner = state.inner.lock().await;
    if inner.connected {
        Json(json!({ "connected": true, "identity": "finance@atlas.example" })).into_response()
    } else {
        Json(json!({ "connected": false, "authorizationUrl": "/authorize/ledger-cloud" }))
            .into_response()
    }
}

async fn authorize_page() -> Html<&'static str> {
    Html(
        r#"<!doctype html><title>Ledger Cloud authorization</title><main><h1>Authorize Ledger Cloud</h1><button id="authorize" type="button">Authorize account</button><p role="status"></p></main><script>document.querySelector('#authorize').addEventListener('click', async () => { await fetch('/api/integrations/ledger-cloud/complete', {method:'POST',headers:{'content-type':'application/json','x-northstar-run':sessionStorage.getItem('northstar.run') ?? new URLSearchParams(location.search).get('run') ?? ''},body:'{"code":"approved"}'}); document.querySelector('[role=status]').textContent='Authorization complete'; window.opener?.postMessage({type:'northstar.authorization.complete'}, location.origin); });</script>"#,
    )
}

async fn complete_authorization(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(error) = require_run(&headers, &state) {
        return error.into_response();
    }
    let mut inner = state.inner.lock().await;
    if !inner.connected {
        inner.connected = true;
        inner.authorization_grants += 1;
    }
    Json(json!({ "connected": true, "identity": "finance@atlas.example" })).into_response()
}

async fn create_report(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(error) = require_run(&headers, &state) {
        return error.into_response();
    }
    let mut inner = state.inner.lock().await;
    if inner.report_generations == 0 {
        inner.report_generations = 1;
    }
    Json(json!({ "id": "rep_atlas_01", "status": "pending" })).into_response()
}

async fn report_state(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> impl IntoResponse {
    if let Err(error) = require_run(&headers, &state) {
        return error.into_response();
    }
    Json(json!({ "id": id, "status": "complete", "filename": "atlas-operations.csv", "mediaType": "text/csv", "downloadUrl": "/api/reports/rep_atlas_01/download", "sha256": format!("{:x}", Sha256::digest(b"customer,priority\nAtlas Labs,high\n")) })).into_response()
}

async fn download_report() -> Response<Body> {
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/csv")
        .header(
            header::CONTENT_DISPOSITION,
            "attachment; filename=atlas-operations.csv",
        )
        .body(Body::from("customer,priority\nAtlas Labs,high\n"))
        .expect("report response")
}

async fn static_file(
    State(state): State<Arc<SharedState>>,
    request: Request<Body>,
) -> Response<Body> {
    let relative = request.uri().path().trim_start_matches('/');
    if relative.split('/').any(|segment| segment == "..") {
        return bytes_response(StatusCode::BAD_REQUEST, "text/plain", b"bad path".to_vec());
    }
    let requested = if relative == "app.js" || relative == "app.css" {
        state.dist.join(relative)
    } else {
        state.dist.join("index.html")
    };
    let canonical_root = match tokio::fs::canonicalize(&state.dist).await {
        Ok(path) => path,
        Err(_) => {
            return bytes_response(StatusCode::INTERNAL_SERVER_ERROR, "text/plain", Vec::new())
        }
    };
    let canonical = match tokio::fs::canonicalize(requested).await {
        Ok(path) if path.starts_with(canonical_root) => path,
        _ => return bytes_response(StatusCode::NOT_FOUND, "text/plain", Vec::new()),
    };
    let content_type = match canonical.extension().and_then(|value| value.to_str()) {
        Some("html") => "text/html; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        _ => "application/octet-stream",
    };
    match tokio::fs::read(canonical).await {
        Ok(bytes) => bytes_response(StatusCode::OK, content_type, bytes),
        Err(_) => bytes_response(StatusCode::NOT_FOUND, "text/plain", Vec::new()),
    }
}

fn bytes_response(
    status: StatusCode,
    content_type: &'static str,
    bytes: Vec<u8>,
) -> Response<Body> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "no-store")
        .body(Body::from(bytes))
        .expect("static response")
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("runtime-tests is nested beneath repository root")
        .to_path_buf()
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() {
                character
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{ScenarioConfig, ScenarioServer};

    #[tokio::test]
    async fn priority_mutation_is_run_scoped_and_counted_once() {
        let server = ScenarioServer::start(ScenarioConfig::seeded("customer-update"))
            .await
            .unwrap();
        let response = reqwest::Client::new()
            .patch(format!(
                "{}/api/customers/cus_atlas/priority",
                server.base_url()
            ))
            .header("x-northstar-run", server.run_id())
            .json(&serde_json::json!({ "priority": "high" }))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);

        let state = server.snapshot().await;
        assert_eq!(state.atlas_priority, "high");
        assert_eq!(state.priority_updates, 1);
    }
}
