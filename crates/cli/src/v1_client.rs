//! Shared blocking HTTP client for bobby `/v1/*` CLI surfaces (jobs, openshell).
//!
//! `bobby` runs under `#[tokio::main]`; reqwest's blocking client builds its own
//! runtime and panics if constructed on a worker thread. Isolate on a plain OS thread.

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{Duration as ChronoDuration, Utc};
use types::CURRENT_INTERFACE_VERSION;
use uuid::Uuid;

const REQUEST_TIMEOUT_SECS: u64 = 10;
const DEADLINE_MINUTES: i64 = 2;

#[derive(Debug, Clone)]
pub struct V1Request {
    pub method: reqwest::Method,
    pub url: String,
    pub bearer: String,
    pub body: Option<serde_json::Value>,
    pub idempotency_key: Option<String>,
}

#[derive(Debug, Clone)]
pub struct V1Response {
    pub status: reqwest::StatusCode,
    pub body: String,
}

pub fn v1_url(base_url: &str, path: &str) -> Result<String> {
    let base = base_url.trim_end_matches('/');
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    Ok(format!("{base}{path}"))
}

/// Perform a `/v1` request off the Tokio runtime; returns status + raw body text.
pub fn v1_request(options: V1Request) -> Result<V1Response> {
    match std::thread::spawn(move || v1_request_blocking(options)).join() {
        Ok(result) => result,
        Err(_) => anyhow::bail!("v1 HTTP thread panicked"),
    }
}

fn v1_request_blocking(options: V1Request) -> Result<V1Response> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .no_proxy()
        .build()
        .context("failed to build /v1 HTTP client")?;

    let correlation_id = Uuid::new_v4();
    let deadline = (Utc::now() + ChronoDuration::minutes(DEADLINE_MINUTES)).to_rfc3339();

    let mut builder = client
        .request(options.method.clone(), &options.url)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", options.bearer),
        )
        .header("x-interface-version", CURRENT_INTERFACE_VERSION)
        .header("x-correlation-id", correlation_id.to_string())
        .header("x-deadline", deadline);

    if let Some(key) = options.idempotency_key.as_deref() {
        builder = builder.header("idempotency-key", key);
    }

    let response = if let Some(body) = options.body {
        builder
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
    } else {
        builder.send()
    }
    .with_context(|| format!("{} {}", options.method, options.url))?;

    let status = response.status();
    let body = response
        .text()
        .with_context(|| format!("failed to read response body from {}", options.url))?;

    Ok(V1Response { status, body })
}

/// Parse a successful `/v1` JSON body; empty / 204 → `{}`.
pub fn parse_json_body(status: reqwest::StatusCode, text: &str) -> Result<serde_json::Value> {
    if status == reqwest::StatusCode::NO_CONTENT || text.trim().is_empty() {
        return Ok(serde_json::json!({}));
    }
    serde_json::from_str(text).context("/v1 response is not valid JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn v1_url_joins_base_and_path() {
        assert_eq!(
            v1_url("http://127.0.0.1:7777", "/v1/jobs").unwrap(),
            "http://127.0.0.1:7777/v1/jobs"
        );
        assert_eq!(
            v1_url("http://127.0.0.1:7777/", "v1/jobs/abc").unwrap(),
            "http://127.0.0.1:7777/v1/jobs/abc"
        );
    }

    #[test]
    fn parse_json_body_empty_and_nocontent() {
        assert_eq!(
            parse_json_body(reqwest::StatusCode::NO_CONTENT, "").unwrap(),
            serde_json::json!({})
        );
        assert_eq!(
            parse_json_body(reqwest::StatusCode::OK, "   ").unwrap(),
            serde_json::json!({})
        );
        assert_eq!(
            parse_json_body(reqwest::StatusCode::OK, r#"{"ok":true}"#).unwrap()["ok"],
            true
        );
    }
}
