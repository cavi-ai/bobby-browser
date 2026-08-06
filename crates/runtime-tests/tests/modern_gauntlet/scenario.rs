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
use tokio::sync::{Mutex, Notify};

type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum GauntletLevel {
    One,
    Two,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq, Default)]
#[serde(rename_all = "camelCase")]
pub struct LevelTwoTrapPlan {
    pub extra_modal: bool,
    pub extra_popup: bool,
    pub reversed_identity_fields: bool,
    pub delayed_control_ms: u64,
}

impl LevelTwoTrapPlan {
    fn seeded(seed: &str) -> Self {
        let digest = Sha256::digest(seed.as_bytes());
        Self {
            extra_modal: true,
            extra_popup: true,
            reversed_identity_fields: digest[0] & 1 == 1,
            delayed_control_ms: 150 + u64::from(digest[1]),
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecaptchaConfig {
    site_key: String,
    secret: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PublicRunConfig {
    level: u8,
    seed: String,
    traps: LevelTwoTrapPlan,
    recaptcha_site_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ScenarioConfig {
    pub seed: String,
    pub reject_postal_once: bool,
    pub level: GauntletLevel,
    pub traps: LevelTwoTrapPlan,
    pub recaptcha: Option<RecaptchaConfig>,
}

impl ScenarioConfig {
    pub fn seeded(seed: impl Into<String>) -> Self {
        Self {
            seed: seed.into(),
            reject_postal_once: true,
            level: GauntletLevel::One,
            traps: LevelTwoTrapPlan::default(),
            recaptcha: None,
        }
    }

    pub fn level_two(
        seed: impl Into<String>,
        site_key: impl Into<String>,
        secret: impl Into<String>,
    ) -> TestResult<Self> {
        let seed = seed.into();
        let site_key = site_key.into();
        let secret = secret.into();
        if site_key.trim().is_empty() || secret.trim().is_empty() {
            return Err("Level 2 requires non-empty reCAPTCHA site key and secret".into());
        }
        Ok(Self {
            traps: LevelTwoTrapPlan::seeded(&seed),
            seed,
            reject_postal_once: true,
            level: GauntletLevel::Two,
            recaptcha: Some(RecaptchaConfig { site_key, secret }),
        })
    }

    pub fn public_config(&self) -> PublicRunConfig {
        PublicRunConfig {
            level: match self.level {
                GauntletLevel::One => 1,
                GauntletLevel::Two => 2,
            },
            seed: self.seed.clone(),
            traps: self.traps.clone(),
            recaptcha_site_key: self
                .recaptcha
                .as_ref()
                .map(|config| config.site_key.clone()),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ScenarioSnapshot {
    pub atlas_priority: String,
    pub priority_updates: u64,
    pub onboarding_records: u64,
    pub onboarding: Option<OnboardingRecord>,
    pub uploaded_sha256: Option<String>,
    pub uploaded_customer_id: Option<String>,
    pub uploaded_filename: Option<String>,
    pub uploaded_media_type: Option<String>,
    pub preview_confirmations: u64,
    pub authorization_grants: u64,
    pub report_generations: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct OnboardingRecord {
    pub full_name: String,
    pub email: String,
    pub company_name: String,
    pub postal_code: String,
    pub plan: String,
    pub billing_cycle: String,
}

#[derive(Debug)]
struct RunState {
    atlas_priority: String,
    priority_updates: u64,
    onboarding_records: u64,
    onboarding: Option<OnboardingRecord>,
    reject_postal_remaining: bool,
    uploaded: Option<Vec<u8>>,
    uploaded_customer_id: Option<String>,
    uploaded_filename: Option<String>,
    uploaded_media_type: Option<String>,
    preview_confirmations: u64,
    connected: bool,
    authorization_grants: u64,
    report_generations: u64,
    requests: Vec<String>,
}

#[derive(Debug)]
struct SharedState {
    run_id: String,
    public_config: PublicRunConfig,
    recaptcha: Option<RecaptchaConfig>,
    dist: PathBuf,
    inner: Mutex<RunState>,
    report_generated: Notify,
    preview_confirmed: Notify,
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
            public_config: config.public_config(),
            recaptcha: config.recaptcha.clone(),
            dist,
            inner: Mutex::new(RunState {
                atlas_priority: "normal".into(),
                priority_updates: 0,
                onboarding_records: 0,
                onboarding: None,
                reject_postal_remaining: config.reject_postal_once,
                uploaded: None,
                uploaded_customer_id: None,
                uploaded_filename: None,
                uploaded_media_type: None,
                preview_confirmations: 0,
                connected: false,
                authorization_grants: 0,
                report_generations: 0,
                requests: Vec::new(),
            }),
            report_generated: Notify::new(),
            preview_confirmed: Notify::new(),
        });
        let app = Router::new()
            .route("/api/run-config", get(run_config))
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
            .route("/api/reports/latest", get(latest_report))
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
        format!(
            "{}{}?run={}&level={}",
            self.base_url(),
            path,
            self.run_id(),
            self.state.public_config.level
        )
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
            onboarding: state.onboarding.clone(),
            uploaded_sha256: state
                .uploaded
                .as_ref()
                .map(|bytes| format!("{:x}", Sha256::digest(bytes))),
            uploaded_customer_id: state.uploaded_customer_id.clone(),
            uploaded_filename: state.uploaded_filename.clone(),
            uploaded_media_type: state.uploaded_media_type.clone(),
            preview_confirmations: state.preview_confirmations,
            authorization_grants: state.authorization_grants,
            report_generations: state.report_generations,
        }
    }

    pub async fn request_log(&self) -> Vec<String> {
        self.state.inner.lock().await.requests.clone()
    }

    pub async fn wait_for_report_generation(&self) -> TestResult<()> {
        let notified = self.state.report_generated.notified();
        if self.state.inner.lock().await.report_generations == 1 {
            return Ok(());
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), notified)
            .await
            .map_err(|_| "report generation was not observed within 10 seconds")?;
        Ok(())
    }

    pub async fn wait_for_preview_confirmation(&self) -> TestResult<()> {
        let notified = self.state.preview_confirmed.notified();
        if self.state.inner.lock().await.preview_confirmations == 1 {
            return Ok(());
        }
        tokio::time::timeout(std::time::Duration::from_secs(10), notified)
            .await
            .map_err(|_| "preview confirmation was not observed within 10 seconds")?;
        Ok(())
    }
}

async fn run_config(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(error) = require_run(&headers, &state) {
        return error.into_response();
    }
    Json(state.public_config.clone()).into_response()
}

impl Drop for ScenarioServer {
    fn drop(&mut self) {
        if let Ok(inner) = self.state.inner.try_lock() {
            let directory = repository_root()
                .join("target/modern-gauntlet-artifacts/server")
                .join(&self.state.run_id);
            if std::fs::create_dir_all(&directory).is_ok() {
                let snapshot = json!({
                    "atlasPriority": inner.atlas_priority,
                    "priorityUpdates": inner.priority_updates,
                    "onboardingRecords": inner.onboarding_records,
                    "onboarding": inner.onboarding,
                    "uploadedSha256": inner.uploaded.as_ref().map(|bytes| format!("{:x}", Sha256::digest(bytes))),
                    "uploadedCustomerId": inner.uploaded_customer_id,
                    "uploadedFilename": inner.uploaded_filename,
                    "uploadedMediaType": inner.uploaded_media_type,
                    "previewConfirmations": inner.preview_confirmations,
                    "authorizationGrants": inner.authorization_grants,
                    "reportGenerations": inner.report_generations,
                });
                if let Ok(bytes) = serde_json::to_vec_pretty(&snapshot) {
                    let _ = std::fs::write(directory.join("server-state.json"), bytes);
                }
                if let Ok(bytes) = serde_json::to_vec_pretty(&inner.requests) {
                    let _ = std::fs::write(directory.join("request-log.json"), bytes);
                }
            }
        }
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

type OnboardingBody = OnboardingRecord;

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
    inner.onboarding = Some(body);
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
    let mut inner = state.inner.lock().await;
    inner.uploaded = Some(bytes);
    inner.uploaded_customer_id = customer_id.clone();
    inner.uploaded_filename = filename.clone();
    inner.uploaded_media_type = media_type.clone();
    Json(json!({ "id": "doc_atlas_01", "customerId": customer_id.unwrap_or_else(|| "cus_atlas".into()), "filename": filename.unwrap_or_else(|| "document.txt".into()), "mediaType": media_type.unwrap_or_else(|| "application/octet-stream".into()), "sha256": digest, "previewUrl": "/api/documents/doc_atlas_01/preview" })).into_response()
}

async fn document_preview(AxumPath(id): AxumPath<String>) -> Html<String> {
    Html(format!(
        r#"<!doctype html><title>Document preview</title><main><h1>Approved customer document</h1><p>Document {id}</p><form method="post" action="/api/documents/{id}/confirm"><button id="confirm-preview" type="submit" aria-label="Confirm document preview">Confirm document</button></form></main>"#
    ))
}

async fn confirm_preview(
    State(state): State<Arc<SharedState>>,
    AxumPath(_id): AxumPath<String>,
) -> impl IntoResponse {
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    state.inner.lock().await.preview_confirmations += 1;
    state.preview_confirmed.notify_waiters();
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
        r#"<!doctype html><title>Ledger Cloud authorization</title><main><h1>Authorize Ledger Cloud</h1><button id="authorize" type="button">Authorize account</button><p role="status"></p></main><script>document.querySelector('#authorize').addEventListener('click', async () => { await fetch('/api/integrations/ledger-cloud/complete', {method:'POST',headers:{'content-type':'application/json','x-northstar-run':sessionStorage.getItem('northstar.run') ?? new URLSearchParams(location.search).get('run') ?? ''},body:'{"code":"approved"}'}); document.querySelector('[role=status]').textContent='Authorization complete'; window.opener?.postMessage({type:'northstar.authorization.complete'}, location.origin); window.close(); });</script>"#,
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
        state.report_generated.notify_waiters();
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

async fn latest_report(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(error) = require_run(&headers, &state) {
        return error.into_response();
    }
    if state.inner.lock().await.report_generations == 0 {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "report_not_found", "message": "No report has been generated." })),
        )
            .into_response();
    }
    Json(json!({ "id": "rep_atlas_01", "status": "complete", "filename": "atlas-operations.csv", "mediaType": "text/csv", "downloadUrl": "/api/reports/rep_atlas_01/download", "sha256": format!("{:x}", Sha256::digest(b"customer,priority\nAtlas Labs,high\n")) })).into_response()
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
    use super::{GauntletLevel, LevelTwoTrapPlan, ScenarioConfig, ScenarioServer};

    #[test]
    fn level_one_is_the_compatible_default() {
        let config = ScenarioConfig::seeded("atlas");
        assert_eq!(config.level, GauntletLevel::One);
        assert!(config.recaptcha.is_none());
    }

    #[test]
    fn level_two_traps_are_seeded_and_public_config_never_contains_the_secret() {
        let first = ScenarioConfig::level_two("atlas", "site-test", "secret-canary").unwrap();
        let second = ScenarioConfig::level_two("atlas", "site-test", "secret-canary").unwrap();
        assert_eq!(first.traps, second.traps);
        assert_ne!(first.traps, LevelTwoTrapPlan::default());
        let public = serde_json::to_string(&first.public_config()).unwrap();
        assert!(public.contains("site-test"));
        assert!(!public.contains("secret-canary"));
    }

    #[test]
    fn level_two_rejects_missing_recaptcha_configuration() {
        assert!(ScenarioConfig::level_two("atlas", "", "secret").is_err());
        assert!(ScenarioConfig::level_two("atlas", "site", "").is_err());
    }

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
