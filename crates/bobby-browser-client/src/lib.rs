//! Typed HTTP client for a running `bobby serve` instance.
//!
//! Mirrors `@bobby-browser/sdk` (`BrowserRuntimeClient`) for Rust callers.

use chrono::{Duration as ChronoDuration, Utc};
use reqwest::{Client, Method};
use serde::de::DeserializeOwned;
use serde::Serialize;
use std::time::Duration;
use thiserror::Error;
use types::{
    CommandEnvelope, CommandOutcome, CreateSessionRequest, OpenPageRequest, PageState, RuntimeInfo,
    SessionId, SessionState, CURRENT_INTERFACE_VERSION,
};
use uuid::Uuid;

/// Errors returned by [`BrowserRuntimeClient`].
#[derive(Debug, Error)]
pub enum ClientError {
    #[error("transport error: {0}")]
    Transport(String),
    #[error("HTTP {status}: {message}")]
    Http { status: u16, message: String },
    #[error("protocol error: {0}")]
    Protocol(String),
}

impl ClientError {
    fn redact(self, bearer: &str) -> Self {
        match self {
            Self::Transport(message) => Self::Transport(message.replace(bearer, "")),
            Self::Http { status, message } => Self::Http {
                status,
                message: message.replace(bearer, ""),
            },
            Self::Protocol(message) => Self::Protocol(message.replace(bearer, "")),
        }
    }
}

/// Options for a single request.
#[derive(Debug, Clone, Default)]
pub struct RequestOptions {
    pub timeout: Option<Duration>,
    pub correlation_id: Option<String>,
    pub idempotency_key: Option<String>,
}

/// Authenticated client for `/v1/*`.
#[derive(Debug, Clone)]
pub struct BrowserRuntimeClient {
    base_url: String,
    bearer_token: String,
    http: Client,
    default_timeout: Duration,
}

impl BrowserRuntimeClient {
    /// Create a client. `base_url` is the broker origin (trailing `/` and `/v1` are stripped).
    pub fn new(
        base_url: impl Into<String>,
        bearer_token: impl Into<String>,
    ) -> Result<Self, ClientError> {
        let bearer_token = bearer_token.into();
        if bearer_token.is_empty() {
            return Err(ClientError::Protocol(
                "bearerToken must not be empty".into(),
            ));
        }
        let base_url = normalize_base_url(base_url.into());
        if base_url.is_empty() {
            return Err(ClientError::Protocol("baseUrl must not be empty".into()));
        }
        let http = Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|error| ClientError::Transport(error.to_string()))?;
        Ok(Self {
            base_url,
            bearer_token,
            http,
            default_timeout: Duration::from_secs(30),
        })
    }

    pub async fn runtime_info(
        &self,
        options: Option<RequestOptions>,
    ) -> Result<RuntimeInfo, ClientError> {
        self.json(Method::GET, "/v1/runtime", None::<()>, options)
            .await
    }

    pub async fn create_session(
        &self,
        input: &CreateSessionRequest,
        options: Option<RequestOptions>,
    ) -> Result<SessionState, ClientError> {
        self.json(Method::POST, "/v1/sessions", Some(input), options)
            .await
    }

    pub async fn list_sessions(
        &self,
        options: Option<RequestOptions>,
    ) -> Result<Vec<SessionState>, ClientError> {
        self.json(Method::GET, "/v1/sessions", None::<()>, options)
            .await
    }

    pub async fn delete_session(
        &self,
        session_id: &SessionId,
        options: Option<RequestOptions>,
    ) -> Result<(), ClientError> {
        self.empty(
            Method::DELETE,
            &format!("/v1/sessions/{}", session_id.0),
            options,
        )
        .await
    }

    pub async fn open_page(
        &self,
        input: &OpenPageRequest,
        options: Option<RequestOptions>,
    ) -> Result<PageState, ClientError> {
        self.json(Method::POST, "/v1/pages", Some(input), options)
            .await
    }

    pub async fn submit(
        &self,
        input: &CommandEnvelope,
        options: Option<RequestOptions>,
    ) -> Result<CommandOutcome, ClientError> {
        self.json(Method::POST, "/v1/commands", Some(input), options)
            .await
    }

    async fn empty(
        &self,
        method: Method,
        path: &str,
        options: Option<RequestOptions>,
    ) -> Result<(), ClientError> {
        let options = options.unwrap_or_default();
        let timeout = options.timeout.unwrap_or(self.default_timeout);
        let deadline =
            Utc::now() + ChronoDuration::from_std(timeout).unwrap_or(ChronoDuration::seconds(30));
        let correlation = options
            .correlation_id
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base_url))
            .timeout(timeout)
            .header("authorization", format!("Bearer {}", self.bearer_token))
            .header("x-interface-version", CURRENT_INTERFACE_VERSION)
            .header("x-correlation-id", correlation)
            .header("x-deadline", deadline.to_rfc3339());
        if let Some(key) = options.idempotency_key {
            request = request.header("idempotency-key", key);
        }

        let response = request.send().await.map_err(|error| {
            ClientError::Transport(error.to_string()).redact(&self.bearer_token)
        })?;
        let status = response.status();
        if status == reqwest::StatusCode::NO_CONTENT || status.is_success() {
            return Ok(());
        }
        let text = response.text().await.unwrap_or_default();
        Err(ClientError::Http {
            status: status.as_u16(),
            message: text,
        }
        .redact(&self.bearer_token))
    }

    async fn json<B, T>(
        &self,
        method: Method,
        path: &str,
        body: Option<B>,
        options: Option<RequestOptions>,
    ) -> Result<T, ClientError>
    where
        B: Serialize,
        T: DeserializeOwned,
    {
        let options = options.unwrap_or_default();
        let timeout = options.timeout.unwrap_or(self.default_timeout);
        let deadline =
            Utc::now() + ChronoDuration::from_std(timeout).unwrap_or(ChronoDuration::seconds(30));
        let correlation = options
            .correlation_id
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        let mut request = self
            .http
            .request(method, format!("{}{path}", self.base_url))
            .timeout(timeout)
            .header("authorization", format!("Bearer {}", self.bearer_token))
            .header("x-interface-version", CURRENT_INTERFACE_VERSION)
            .header("x-correlation-id", correlation)
            .header("x-deadline", deadline.to_rfc3339());
        if let Some(key) = options.idempotency_key {
            request = request.header("idempotency-key", key);
        }
        if let Some(body) = body {
            request = request
                .header("content-type", "application/json")
                .json(&body);
        }

        let response = request.send().await.map_err(|error| {
            ClientError::Transport(error.to_string()).redact(&self.bearer_token)
        })?;
        let status = response.status();
        let text = response.text().await.map_err(|error| {
            ClientError::Transport(error.to_string()).redact(&self.bearer_token)
        })?;
        if !status.is_success() {
            return Err(ClientError::Http {
                status: status.as_u16(),
                message: text,
            }
            .redact(&self.bearer_token));
        }
        serde_json::from_str(&text).map_err(|error| {
            ClientError::Protocol(format!("invalid JSON body: {error}")).redact(&self.bearer_token)
        })
    }
}

fn normalize_base_url(value: String) -> String {
    let trimmed = value.trim_end_matches('/').to_string();
    if let Some(stripped) = trimmed.strip_suffix("/v1") {
        stripped.trim_end_matches('/').to_string()
    } else {
        trimmed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::Body;
    use axum::extract::Request;
    use axum::http::HeaderMap;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use axum::routing::get;
    use axum::Router;
    use serde_json::json;
    use tokio::net::TcpListener;

    async fn runtime_handler(headers: HeaderMap) -> impl IntoResponse {
        assert!(headers
            .get("authorization")
            .and_then(|v| v.to_str().ok())
            .is_some_and(|v| v == "Bearer test-token"));
        assert_eq!(
            headers
                .get("x-interface-version")
                .and_then(|v| v.to_str().ok()),
            Some(CURRENT_INTERFACE_VERSION)
        );
        assert!(headers.get("x-correlation-id").is_some());
        assert!(headers.get("x-deadline").is_some());
        (
            StatusCode::OK,
            [(axum::http::header::CONTENT_TYPE, "application/json")],
            json!({
                "version": env!("CARGO_PKG_VERSION"),
                "capabilities": ["session:read"],
                "active_sessions": 0,
                "queued_jobs": 0,
                "uptime_ms": 1,
            })
            .to_string(),
        )
    }

    #[tokio::test]
    async fn runtime_info_sends_required_headers() {
        let app = Router::new().route("/v1/runtime", get(runtime_handler));
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });

        let client = BrowserRuntimeClient::new(format!("http://{addr}/v1"), "test-token").unwrap();
        let info = client.runtime_info(None).await.unwrap();
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(info.active_sessions, 0);
    }

    #[tokio::test]
    async fn normalize_strips_v1_suffix() {
        assert_eq!(
            normalize_base_url("http://127.0.0.1:7777/v1/".into()),
            "http://127.0.0.1:7777"
        );
    }

    #[tokio::test]
    async fn rejects_empty_bearer() {
        let err = BrowserRuntimeClient::new("http://127.0.0.1:7777", "").unwrap_err();
        assert!(matches!(err, ClientError::Protocol(_)));
    }

    #[allow(dead_code)]
    async fn _unused_request(_req: Request<Body>) {}
}
