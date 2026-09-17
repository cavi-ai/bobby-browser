use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::extract::{Multipart, Path as AxumPath, Query, State};
use axum::http::{header, HeaderMap, HeaderValue, Request, Response, StatusCode};
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

#[async_trait]
trait RecaptchaVerifier: Send + Sync {
    async fn verify(&self, token: &str) -> Result<bool, String>;
}

struct GoogleRecaptchaVerifier {
    client: reqwest::Client,
    secret: String,
}

#[derive(Deserialize)]
struct GoogleRecaptchaResponse {
    success: bool,
}

#[async_trait]
impl RecaptchaVerifier for GoogleRecaptchaVerifier {
    async fn verify(&self, token: &str) -> Result<bool, String> {
        let response = self
            .client
            .post("https://www.google.com/recaptcha/api/siteverify")
            .form(&[("secret", self.secret.as_str()), ("response", token)])
            .send()
            .await
            .map_err(|error| error.to_string())?
            .error_for_status()
            .map_err(|error| error.to_string())?
            .json::<GoogleRecaptchaResponse>()
            .await
            .map_err(|error| error.to_string())?;
        Ok(response.success)
    }
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
    pub consent: Option<String>,
    pub session_email: Option<String>,
    pub mfa_code: String,
    pub mfa_completions: u64,
    pub three_ds_code: String,
    pub three_ds_completions: u64,
    pub billing_address: Option<BillingAddress>,
    pub billing_period: Option<BillingPeriod>,
    pub charge: Option<ChargeRecord>,
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

pub const OPERATOR_EMAIL: &str = "maya@northstar.example";
pub const OPERATOR_PASSWORD: &str = "atlas-ops-2026";
pub const MFA_CODE: &str = "246813";
pub const THREE_DS_CODE: &str = "391726";
const SESSION_COOKIE: &str = "northstar-session";
const CONSENT_COOKIE: &str = "northstar-consent";
const ATLAS_ROW: usize = 24;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BillingAddress {
    pub street: String,
    pub city: String,
    pub postal_code: String,
    pub label: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct BillingPeriod {
    pub start: String,
    pub end: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ChargeRecord {
    pub plan: String,
    pub amount_cents: u64,
    pub address: BillingAddress,
    pub period: BillingPeriod,
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
    consent: Option<String>,
    session_email: Option<String>,
    mfa_pending: bool,
    mfa_completions: u64,
    card_token: Option<String>,
    three_ds_completions: u64,
    billing_address: Option<BillingAddress>,
    billing_period: Option<BillingPeriod>,
    charge: Option<ChargeRecord>,
}

struct SharedState {
    run_id: String,
    public_config: PublicRunConfig,
    recaptcha_verifier: Option<Arc<dyn RecaptchaVerifier>>,
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
        let verifier = config.recaptcha.as_ref().map(|recaptcha| {
            Arc::new(GoogleRecaptchaVerifier {
                client: reqwest::Client::builder()
                    .timeout(std::time::Duration::from_secs(8))
                    .build()
                    .expect("reCAPTCHA HTTP client configuration is valid"),
                secret: recaptcha.secret.clone(),
            }) as Arc<dyn RecaptchaVerifier>
        });
        Self::start_inner(config, verifier).await
    }

    #[cfg(test)]
    async fn start_with_verifier(
        config: ScenarioConfig,
        verifier: Arc<dyn RecaptchaVerifier>,
    ) -> TestResult<Self> {
        Self::start_inner(config, Some(verifier)).await
    }

    async fn start_inner(
        config: ScenarioConfig,
        recaptcha_verifier: Option<Arc<dyn RecaptchaVerifier>>,
    ) -> TestResult<Self> {
        let dist = repository_root().join("packages/bobby-gauntlet/dist");
        if !dist.join("index.html").is_file() || !dist.join("app.js").is_file() {
            return Err("built Northstar application is missing; run pnpm --filter @cavi-ai/bobby-gauntlet build".into());
        }
        let run_id = format!("run-{}", sanitize(&config.seed));
        let state = Arc::new(SharedState {
            run_id,
            public_config: config.public_config(),
            recaptcha_verifier,
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
                consent: None,
                session_email: None,
                mfa_pending: false,
                mfa_completions: 0,
                card_token: None,
                three_ds_completions: 0,
                billing_address: None,
                billing_period: None,
                charge: None,
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
            .route("/level-two-checkpoint", get(level_two_checkpoint))
            .route(
                "/api/integrations/ledger-cloud/complete",
                post(complete_authorization),
            )
            .route("/api/reports", post(create_report))
            .route("/api/reports/latest", get(latest_report))
            .route("/api/reports/{id}", get(report_state))
            .route("/api/reports/{id}/download", get(download_report))
            .route("/api/consent", get(consent_state).post(set_consent))
            .route("/api/session", get(session_state))
            .route("/api/session/login", post(login))
            .route("/api/session/mfa", post(verify_mfa))
            .route("/api/billing/addresses", get(billing_addresses))
            .route("/api/billing/tokenize", post(tokenize_card))
            .route("/api/billing/3ds", post(verify_three_ds))
            .route("/api/billing/charge", post(charge_atlas))
            .route("/pay/card", get(card_frame))
            .route("/pay/3ds", get(three_ds_frame))
            // Tool-neutral verification surface for out-of-process drivers:
            // the same state `snapshot()` and `request_log()` expose
            // in-process, as JSON over HTTP.
            .route("/__gauntlet/snapshot", get(gauntlet_snapshot))
            .route("/__gauntlet/request-log", get(gauntlet_request_log))
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
        snapshot_of(&state)
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
                    "uploadedSha256": inner.uploaded.as_ref().map(|bytes| hex::encode(Sha256::digest(bytes))),
                    "uploadedCustomerId": inner.uploaded_customer_id,
                    "uploadedFilename": inner.uploaded_filename,
                    "uploadedMediaType": inner.uploaded_media_type,
                    "previewConfirmations": inner.preview_confirmations,
                    "authorizationGrants": inner.authorization_grants,
                    "reportGenerations": inner.report_generations,
                    "consent": inner.consent,
                    "sessionEmail": inner.session_email,
                    "mfaCode": MFA_CODE,
                    "mfaCompletions": inner.mfa_completions,
                    "threeDsCode": THREE_DS_CODE,
                    "threeDsCompletions": inner.three_ds_completions,
                    "billingAddress": inner.billing_address,
                    "billingPeriod": inner.billing_period,
                    "charge": inner.charge,
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

fn cookie_value(headers: &HeaderMap, name: &str) -> Option<String> {
    let raw = headers.get(header::COOKIE)?.to_str().ok()?;
    raw.split(';').find_map(|part| {
        let (key, value) = part.trim().split_once('=')?;
        (key == name).then(|| value.to_string())
    })
}

fn require_session(
    headers: &HeaderMap,
    state: &SharedState,
) -> Result<(), (StatusCode, Json<Value>)> {
    require_run(headers, state)?;
    if cookie_value(headers, SESSION_COOKIE).as_deref() == Some("operator") {
        Ok(())
    } else {
        Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({ "code": "unauthenticated", "message": "Sign in to continue." })),
        ))
    }
}

fn set_cookie(name: &str, value: &str) -> HeaderValue {
    HeaderValue::from_str(&format!("{name}={value}; Path=/; SameSite=Lax"))
        .expect("cookie header is valid")
}

async fn record(state: &SharedState, value: impl Into<String>) {
    state.inner.lock().await.requests.push(value.into());
}

async fn dashboard(State(state): State<Arc<SharedState>>, headers: HeaderMap) -> impl IntoResponse {
    if let Err(error) = require_session(&headers, &state) {
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
    if let Err(error) = require_session(&headers, &state) {
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
    let query = query.q.as_deref().unwrap_or_default();
    Json(catalog(&inner, query)).into_response()
}

async fn customer(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    AxumPath(id): AxumPath<String>,
) -> impl IntoResponse {
    if let Err(error) = require_session(&headers, &state) {
        return error.into_response();
    }
    record(&state, format!("GET /api/customers/{id}")).await;
    let inner = state.inner.lock().await;
    match customer_by_id(&inner, &id) {
        Some(value) => Json(value).into_response(),
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "not_found", "message": "Customer not found." })),
        )
            .into_response(),
    }
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
    if let Err(error) = require_session(&headers, &state) {
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

fn decoy_name(index: usize) -> String {
    match index {
        8 => "Atlas Maritime".into(),
        11 => "Atlas Analytics".into(),
        _ => format!("Helios Freight {index:02}"),
    }
}

fn catalog(state: &RunState, query: &str) -> Vec<Value> {
    let needle = query.trim().to_ascii_lowercase();
    (0..40)
        .filter_map(|index| {
            let row = if index == ATLAS_ROW {
                customer_json(state)
            } else {
                json!({
                    "id": format!("cus_decoy_{index:02}"),
                    "name": decoy_name(index),
                    "email": format!("ops-{index}@decoy.example"),
                    "company": decoy_name(index),
                    "joinedAt": "2025-11-02",
                    "priority": "normal",
                    "status": if index == 3 { "paused" } else { "active" }
                })
            };
            let name = row["name"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase();
            let email = row["email"]
                .as_str()
                .unwrap_or_default()
                .to_ascii_lowercase();
            (needle.is_empty() || name.contains(&needle) || email.contains(&needle)).then_some(row)
        })
        .collect()
}

fn customer_by_id(state: &RunState, id: &str) -> Option<Value> {
    catalog(state, "")
        .into_iter()
        .find(|row| row["id"].as_str() == Some(id))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct OnboardingBody {
    #[serde(flatten)]
    record: OnboardingRecord,
    recaptcha_response: Option<String>,
}

async fn onboard(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<OnboardingBody>,
) -> impl IntoResponse {
    if let Err(error) = require_session(&headers, &state) {
        return error.into_response();
    }
    record(&state, "POST /api/onboarding").await;
    if let Some(verifier) = &state.recaptcha_verifier {
        let Some(token) = body
            .recaptcha_response
            .as_deref()
            .filter(|token| !token.trim().is_empty())
        else {
            return (
                StatusCode::UNPROCESSABLE_ENTITY,
                Json(json!({ "code": "recaptcha_required", "message": "Complete the reCAPTCHA challenge." })),
            )
                .into_response();
        };
        match verifier.verify(token).await {
            Ok(true) => {}
            Ok(false) => {
                return (
                    StatusCode::UNPROCESSABLE_ENTITY,
                    Json(json!({ "code": "recaptcha_failed", "message": "The reCAPTCHA response was rejected." })),
                )
                    .into_response();
            }
            Err(_) => {
                return (
                    StatusCode::BAD_GATEWAY,
                    Json(json!({ "code": "recaptcha_unavailable", "message": "reCAPTCHA verification is temporarily unavailable." })),
                )
                    .into_response();
            }
        }
    }
    let mut inner = state.inner.lock().await;
    if inner.reject_postal_remaining && body.record.postal_code != "10001" {
        inner.reject_postal_remaining = false;
        return (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "code": "postal_rejected", "message": "Review the highlighted field.", "fields": { "postalCode": "Use 10001 for this account." } }))).into_response();
    }
    inner.onboarding_records += 1;
    inner.onboarding = Some(body.record);
    Json(json!({ "id": "onb_atlas_01", "status": "complete" })).into_response()
}

async fn upload_document(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    mut multipart: Multipart,
) -> impl IntoResponse {
    if let Err(error) = require_session(&headers, &state) {
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
    let digest = hex::encode(Sha256::digest(&bytes));
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

async fn level_two_checkpoint() -> Html<&'static str> {
    Html(
        r#"<!doctype html><html><head><title>Level 2 checkpoint</title></head><body><main><h1>Account checkpoint</h1><p>Confirm the onboarding details in the original window.</p><button type="button" onclick="window.close()">Return to onboarding</button></main></body></html>"#,
    )
}

async fn confirm_preview(
    State(state): State<Arc<SharedState>>,
    AxumPath(_id): AxumPath<String>,
) -> impl IntoResponse {
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    state.inner.lock().await.preview_confirmations += 1;
    state.preview_confirmed.notify_waiters();
    Json(json!({ "status": "confirmed" })).into_response()
}

async fn integration_state(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(error) = require_session(&headers, &state) {
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
        r#"<!doctype html><title>Ledger Cloud authorization</title><main><h1>Authorize Ledger Cloud</h1><button id="authorize" type="button">Authorize account</button><p role="status"></p></main><script>document.querySelector('#authorize').addEventListener('click', async () => { await fetch('/api/integrations/ledger-cloud/complete', {method:'POST',credentials:'include',headers:{'content-type':'application/json','x-northstar-run':sessionStorage.getItem('northstar.run') ?? new URLSearchParams(location.search).get('run') ?? ''},body:'{"code":"approved"}'}); document.querySelector('[role=status]').textContent='Authorization complete'; window.opener?.postMessage({type:'northstar.authorization.complete'}, location.origin); window.close(); });</script>"#,
    )
}

async fn complete_authorization(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(error) = require_session(&headers, &state) {
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
    if let Err(error) = require_session(&headers, &state) {
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
    if let Err(error) = require_session(&headers, &state) {
        return error.into_response();
    }
    Json(json!({ "id": id, "status": "complete", "filename": "atlas-operations.csv", "mediaType": "text/csv", "downloadUrl": "/api/reports/rep_atlas_01/download", "sha256": hex::encode(Sha256::digest(b"customer,priority\nAtlas Labs,high\n")) })).into_response()
}

async fn latest_report(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(error) = require_session(&headers, &state) {
        return error.into_response();
    }
    if state.inner.lock().await.report_generations == 0 {
        return (
            StatusCode::NOT_FOUND,
            Json(json!({ "code": "report_not_found", "message": "No report has been generated." })),
        )
            .into_response();
    }
    Json(json!({ "id": "rep_atlas_01", "status": "complete", "filename": "atlas-operations.csv", "mediaType": "text/csv", "downloadUrl": "/api/reports/rep_atlas_01/download", "sha256": hex::encode(Sha256::digest(b"customer,priority\nAtlas Labs,high\n")) })).into_response()
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
            return bytes_response(StatusCode::INTERNAL_SERVER_ERROR, "text/plain", Vec::new());
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

fn snapshot_of(state: &RunState) -> ScenarioSnapshot {
    ScenarioSnapshot {
        atlas_priority: state.atlas_priority.clone(),
        priority_updates: state.priority_updates,
        onboarding_records: state.onboarding_records,
        onboarding: state.onboarding.clone(),
        uploaded_sha256: state
            .uploaded
            .as_ref()
            .map(|bytes| hex::encode(Sha256::digest(bytes))),
        uploaded_customer_id: state.uploaded_customer_id.clone(),
        uploaded_filename: state.uploaded_filename.clone(),
        uploaded_media_type: state.uploaded_media_type.clone(),
        preview_confirmations: state.preview_confirmations,
        authorization_grants: state.authorization_grants,
        report_generations: state.report_generations,
        consent: state.consent.clone(),
        session_email: state.session_email.clone(),
        mfa_code: MFA_CODE.into(),
        mfa_completions: state.mfa_completions,
        three_ds_code: THREE_DS_CODE.into(),
        three_ds_completions: state.three_ds_completions,
        billing_address: state.billing_address.clone(),
        billing_period: state.billing_period.clone(),
        charge: state.charge.clone(),
    }
}

#[derive(Deserialize)]
struct ConsentBody {
    choice: String,
}

async fn consent_state(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(error) = require_run(&headers, &state) {
        return error.into_response();
    }
    let from_cookie = cookie_value(&headers, CONSENT_COOKIE)
        .filter(|choice| choice == "accept" || choice == "reject");
    Json(json!({ "consent": from_cookie })).into_response()
}

async fn set_consent(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<ConsentBody>,
) -> impl IntoResponse {
    if let Err(error) = require_run(&headers, &state) {
        return error.into_response();
    }
    if body.choice != "accept" && body.choice != "reject" {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "code": "invalid_consent", "message": "Choose accept or reject." })),
        )
            .into_response();
    }
    let mut inner = state.inner.lock().await;
    inner.consent = Some(body.choice.clone());
    inner
        .requests
        .push(format!("POST /api/consent {}", body.choice));
    let mut response = Json(json!({ "consent": body.choice })).into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, set_cookie(CONSENT_COOKIE, &body.choice));
    response
}

#[derive(Deserialize)]
struct LoginBody {
    email: String,
    password: String,
}

async fn session_state(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
) -> impl IntoResponse {
    if let Err(error) = require_run(&headers, &state) {
        return error.into_response();
    }
    let cookie_ok = cookie_value(&headers, SESSION_COOKIE).as_deref() == Some("operator");
    let inner = state.inner.lock().await;
    Json(json!({
        "authenticated": cookie_ok && inner.session_email.is_some(),
        "email": cookie_ok.then(|| inner.session_email.clone()).flatten(),
        "mfaPending": cookie_ok && inner.mfa_pending,
    }))
    .into_response()
}

async fn login(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<LoginBody>,
) -> impl IntoResponse {
    if let Err(error) = require_run(&headers, &state) {
        return error.into_response();
    }
    if cookie_value(&headers, CONSENT_COOKIE).is_none()
        && state.inner.lock().await.consent.is_none()
    {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "code": "consent_required", "message": "Review cookie preferences first." })),
        )
            .into_response();
    }
    if body.email != OPERATOR_EMAIL || body.password != OPERATOR_PASSWORD {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "code": "invalid_credentials", "message": "Check the operator email and password." })),
        )
            .into_response();
    }
    let mut inner = state.inner.lock().await;
    inner.mfa_pending = true;
    inner.session_email = None;
    inner.requests.push("POST /api/session/login".into());
    Json(json!({ "status": "mfaRequired" })).into_response()
}

#[derive(Deserialize)]
struct MfaBody {
    code: String,
}

async fn verify_mfa(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<MfaBody>,
) -> impl IntoResponse {
    if let Err(error) = require_run(&headers, &state) {
        return error.into_response();
    }
    let mut inner = state.inner.lock().await;
    if !inner.mfa_pending {
        return (
            StatusCode::CONFLICT,
            Json(
                json!({ "code": "mfa_not_pending", "message": "Sign in before entering a code." }),
            ),
        )
            .into_response();
    }
    if body.code.trim() != MFA_CODE {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "code": "invalid_mfa", "message": "That code is not valid." })),
        )
            .into_response();
    }
    inner.mfa_pending = false;
    inner.session_email = Some(OPERATOR_EMAIL.into());
    inner.mfa_completions += 1;
    inner.requests.push("POST /api/session/mfa".into());
    let mut response =
        Json(json!({ "authenticated": true, "email": OPERATOR_EMAIL })).into_response();
    response
        .headers_mut()
        .append(header::SET_COOKIE, set_cookie(SESSION_COOKIE, "operator"));
    response
}

#[derive(Deserialize)]
struct AddressQuery {
    q: Option<String>,
}

fn atlas_address() -> BillingAddress {
    BillingAddress {
        street: "100 Federal Street".into(),
        city: "Boston".into(),
        postal_code: "02110".into(),
        label: "Atlas Labs Boston".into(),
    }
}

fn trap_address() -> BillingAddress {
    BillingAddress {
        street: "1 Atlantic Avenue".into(),
        city: "Boston".into(),
        postal_code: "02210".into(),
        label: "Boston HQ".into(),
    }
}

async fn billing_addresses(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Query(query): Query<AddressQuery>,
) -> impl IntoResponse {
    if let Err(error) = require_session(&headers, &state) {
        return error.into_response();
    }
    let needle = query.q.unwrap_or_default().to_ascii_lowercase();
    if needle.trim().len() < 3 {
        return Json(Vec::<BillingAddress>::new()).into_response();
    }
    let options = [trap_address(), atlas_address()];
    Json(
        options
            .into_iter()
            .filter(|address| {
                address.label.to_ascii_lowercase().contains(&needle)
                    || address.street.to_ascii_lowercase().contains(&needle)
                    || address.city.to_ascii_lowercase().contains(&needle)
            })
            .collect::<Vec<_>>(),
    )
    .into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct CardBody {
    number: String,
    expiry: String,
    cvc: String,
}

async fn tokenize_card(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<CardBody>,
) -> impl IntoResponse {
    if let Err(error) = require_session(&headers, &state) {
        return error.into_response();
    }
    if body.number.replace(' ', "") != "4242424242424242"
        || body.expiry != "12/28"
        || body.cvc != "123"
    {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "code": "invalid_card", "message": "Use the Atlas test card." })),
        )
            .into_response();
    }
    let mut inner = state.inner.lock().await;
    inner.card_token = Some("tok_atlas".into());
    inner.requests.push("POST /api/billing/tokenize".into());
    Json(json!({ "token": "tok_atlas", "threeDsRequired": true })).into_response()
}

#[derive(Deserialize)]
struct ThreeDsBody {
    code: String,
}

async fn verify_three_ds(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<ThreeDsBody>,
) -> impl IntoResponse {
    if let Err(error) = require_session(&headers, &state) {
        return error.into_response();
    }
    let mut inner = state.inner.lock().await;
    if inner.card_token.as_deref() != Some("tok_atlas") {
        return (
            StatusCode::CONFLICT,
            Json(json!({ "code": "card_required", "message": "Tokenize a card first." })),
        )
            .into_response();
    }
    if body.code.trim() != THREE_DS_CODE {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "code": "invalid_3ds", "message": "That challenge code is not valid." })),
        )
            .into_response();
    }
    inner.three_ds_completions += 1;
    inner.requests.push("POST /api/billing/3ds".into());
    Json(json!({ "status": "authenticated", "token": "tok_atlas" })).into_response()
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ChargeBody {
    plan: String,
    period: BillingPeriod,
    address: BillingAddress,
}

async fn charge_atlas(
    State(state): State<Arc<SharedState>>,
    headers: HeaderMap,
    Json(body): Json<ChargeBody>,
) -> impl IntoResponse {
    if let Err(error) = require_session(&headers, &state) {
        return error.into_response();
    }
    let mut inner = state.inner.lock().await;
    if inner.three_ds_completions == 0 {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({ "code": "3ds_required", "message": "Complete 3-D Secure before charging." })),
        )
            .into_response();
    }
    if body.period.start != "2026-01-06" || body.period.end != "2026-01-20" {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "code": "invalid_period", "message": "Bill 6 Jan 2026 through 20 Jan 2026." })),
        )
            .into_response();
    }
    if body.address.postal_code != "02110" {
        return (
            StatusCode::UNPROCESSABLE_ENTITY,
            Json(json!({ "code": "invalid_address", "message": "Use the Atlas Labs Boston address." })),
        )
            .into_response();
    }
    let charge = ChargeRecord {
        plan: body.plan,
        amount_cents: 8400,
        address: body.address.clone(),
        period: body.period.clone(),
    };
    inner.billing_address = Some(body.address);
    inner.billing_period = Some(body.period);
    inner.charge = Some(charge.clone());
    inner.requests.push("POST /api/billing/charge".into());
    Json(charge).into_response()
}

async fn card_frame() -> Html<&'static str> {
    Html(
        r#"<!doctype html><title>Card</title><main><form aria-label="Card details"><label>Card number<input id="card-number" aria-label="Card number" name="number" autocomplete="cc-number"></label><label>Expiry<input id="card-expiry" aria-label="Expiry" name="expiry" autocomplete="cc-exp" placeholder="MM/YY"></label><label>CVC<input id="card-cvc" aria-label="CVC" name="cvc" autocomplete="cc-csc"></label><button id="save-card" type="submit">Save card</button></form><p role="status"></p></main><script>
const run = sessionStorage.getItem('northstar.run') ?? new URLSearchParams(location.search).get('run') ?? '';
document.querySelector('form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const body = { number: document.querySelector('[name=number]').value, expiry: document.querySelector('[name=expiry]').value, cvc: document.querySelector('[name=cvc]').value };
  const response = await fetch('/api/billing/tokenize', { method: 'POST', credentials: 'include', headers: { 'content-type': 'application/json', 'x-northstar-run': run }, body: JSON.stringify(body) });
  const payload = await response.json();
  document.querySelector('[role=status]').textContent = response.ok ? 'Card saved' : (payload.message ?? 'Card rejected');
  if (response.ok) window.parent.postMessage({ type: 'northstar.card.tokenized', token: payload.token }, location.origin);
});
</script>"#,
    )
}

async fn three_ds_frame() -> Html<&'static str> {
    Html(
        r#"<!doctype html><title>3-D Secure</title><main><h1>Confirm this payment</h1><form aria-label="3-D Secure challenge"><label>Challenge code<input id="challenge-code" aria-label="Challenge code" name="code" inputmode="numeric"></label><button id="verify-payment" type="submit">Verify payment</button></form><p role="status"></p></main><script>
const run = sessionStorage.getItem('northstar.run') ?? new URLSearchParams(location.search).get('run') ?? '';
document.querySelector('form').addEventListener('submit', async (event) => {
  event.preventDefault();
  const response = await fetch('/api/billing/3ds', { method: 'POST', credentials: 'include', headers: { 'content-type': 'application/json', 'x-northstar-run': run }, body: JSON.stringify({ code: document.querySelector('[name=code]').value }) });
  const payload = await response.json();
  document.querySelector('[role=status]').textContent = response.ok ? 'Payment authenticated' : (payload.message ?? 'Challenge failed');
  if (response.ok) window.parent.postMessage({ type: 'northstar.3ds.complete' }, location.origin);
});
</script>"#,
    )
}

async fn gauntlet_snapshot(State(state): State<Arc<SharedState>>) -> Json<ScenarioSnapshot> {
    let state = state.inner.lock().await;
    Json(snapshot_of(&state))
}

async fn gauntlet_request_log(State(state): State<Arc<SharedState>>) -> Json<Vec<String>> {
    Json(state.inner.lock().await.requests.clone())
}

fn repository_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(Path::parent)
        .expect("gauntlet-server is nested beneath repository root")
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
    use super::{
        GauntletLevel, LevelTwoTrapPlan, RecaptchaVerifier, ScenarioConfig, ScenarioServer,
        MFA_CODE, OPERATOR_EMAIL, OPERATOR_PASSWORD, THREE_DS_CODE,
    };
    use async_trait::async_trait;
    use std::sync::Arc;

    struct TokenVerifier;

    #[async_trait]
    impl RecaptchaVerifier for TokenVerifier {
        async fn verify(&self, token: &str) -> Result<bool, String> {
            match token {
                "accepted-token" => Ok(true),
                "unavailable-token" => Err("verification service unavailable".into()),
                _ => Ok(false),
            }
        }
    }

    async fn operator_headers(server: &ScenarioServer) -> reqwest::header::HeaderMap {
        let client = reqwest::Client::new();
        let run = server.run_id();
        client
            .post(format!("{}/api/consent", server.base_url()))
            .header("x-northstar-run", run)
            .json(&serde_json::json!({ "choice": "accept" }))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        client
            .post(format!("{}/api/session/login", server.base_url()))
            .header("x-northstar-run", run)
            .header("cookie", "northstar-consent=accept")
            .json(&serde_json::json!({ "email": OPERATOR_EMAIL, "password": OPERATOR_PASSWORD }))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let mfa = client
            .post(format!("{}/api/session/mfa", server.base_url()))
            .header("x-northstar-run", run)
            .header("cookie", "northstar-consent=accept")
            .json(&serde_json::json!({ "code": MFA_CODE }))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let cookie = mfa
            .headers()
            .get_all(reqwest::header::SET_COOKIE)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .find(|value| value.starts_with("northstar-session="))
            .and_then(|value| value.split(';').next())
            .expect("session cookie");
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert("x-northstar-run", run.parse().unwrap());
        headers.insert(
            reqwest::header::COOKIE,
            format!("northstar-consent=accept; {cookie}")
                .parse()
                .unwrap(),
        );
        headers
    }

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
        let headers = operator_headers(&server).await;
        let response = reqwest::Client::new()
            .patch(format!(
                "{}/api/customers/cus_atlas/priority",
                server.base_url()
            ))
            .headers(headers)
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
        assert_eq!(state.session_email.as_deref(), Some(OPERATOR_EMAIL));
        assert_eq!(state.mfa_completions, 1);
        assert_eq!(state.consent.as_deref(), Some("accept"));
    }

    #[tokio::test]
    async fn level_two_verifies_recaptcha_before_mutating_onboarding_state() {
        let mut config =
            ScenarioConfig::level_two("recaptcha-boundary", "site-test", "secret-test").unwrap();
        config.reject_postal_once = false;
        let server = ScenarioServer::start_with_verifier(config, Arc::new(TokenVerifier))
            .await
            .unwrap();
        let client = reqwest::Client::new();
        let headers = operator_headers(&server).await;
        let record = serde_json::json!({
            "fullName": "Maya Chen",
            "email": "maya@atlas.example",
            "companyName": "Atlas Labs",
            "postalCode": "10001",
            "plan": "growth",
            "billingCycle": "annual"
        });

        for (token, status, code) in [
            (
                None,
                reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                "recaptcha_required",
            ),
            (
                Some("rejected-token"),
                reqwest::StatusCode::UNPROCESSABLE_ENTITY,
                "recaptcha_failed",
            ),
            (
                Some("unavailable-token"),
                reqwest::StatusCode::BAD_GATEWAY,
                "recaptcha_unavailable",
            ),
        ] {
            let mut body = record.clone();
            if let Some(token) = token {
                body["recaptchaResponse"] = serde_json::Value::String(token.into());
            }
            let response = client
                .post(format!("{}/api/onboarding", server.base_url()))
                .headers(headers.clone())
                .json(&body)
                .send()
                .await
                .unwrap();
            assert_eq!(response.status(), status);
            assert_eq!(
                response.json::<serde_json::Value>().await.unwrap()["code"],
                code
            );
            assert_eq!(server.snapshot().await.onboarding_records, 0);
        }

        let mut accepted = record;
        accepted["recaptchaResponse"] = serde_json::Value::String("accepted-token".into());
        let response = client
            .post(format!("{}/api/onboarding", server.base_url()))
            .headers(headers)
            .json(&accepted)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(server.snapshot().await.onboarding_records, 1);
    }

    #[tokio::test]
    async fn charge_requires_3ds_and_the_atlas_boston_address() {
        let server = ScenarioServer::start(ScenarioConfig::seeded("billing"))
            .await
            .unwrap();
        let client = reqwest::Client::new();
        let headers = operator_headers(&server).await;
        let period = serde_json::json!({ "start": "2026-01-06", "end": "2026-01-20" });
        let address = serde_json::json!({
            "street": "100 Federal Street",
            "city": "Boston",
            "postalCode": "02110",
            "label": "Atlas Labs Boston"
        });
        let blocked = client
            .post(format!("{}/api/billing/charge", server.base_url()))
            .headers(headers.clone())
            .json(&serde_json::json!({ "plan": "growth", "period": period, "address": address }))
            .send()
            .await
            .unwrap();
        assert_eq!(blocked.status(), reqwest::StatusCode::FORBIDDEN);

        client
            .post(format!("{}/api/billing/tokenize", server.base_url()))
            .headers(headers.clone())
            .json(&serde_json::json!({ "number": "4242424242424242", "expiry": "12/28", "cvc": "123" }))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        client
            .post(format!("{}/api/billing/3ds", server.base_url()))
            .headers(headers.clone())
            .json(&serde_json::json!({ "code": THREE_DS_CODE }))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        client
            .post(format!("{}/api/billing/charge", server.base_url()))
            .headers(headers)
            .json(&serde_json::json!({ "plan": "growth", "period": period, "address": address }))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap();
        let snapshot = server.snapshot().await;
        assert_eq!(snapshot.three_ds_completions, 1);
        assert_eq!(snapshot.charge.as_ref().unwrap().amount_cents, 8400);
        assert_eq!(
            snapshot.billing_address.as_ref().unwrap().postal_code,
            "02110"
        );
    }

    #[tokio::test]
    async fn payment_frames_include_card_and_challenge_fields() {
        let server = ScenarioServer::start(ScenarioConfig::seeded("frames"))
            .await
            .unwrap();
        let client = reqwest::Client::new();
        let card = client
            .get(format!("{}/pay/card", server.base_url()))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(card.contains("id=\"card-number\""), "{card}");
        assert!(card.contains("Card number"), "{card}");
        let challenge = client
            .get(format!("{}/pay/3ds", server.base_url()))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .text()
            .await
            .unwrap();
        assert!(challenge.contains("Challenge code"), "{challenge}");
    }
}
