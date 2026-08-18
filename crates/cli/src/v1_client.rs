//! Shared blocking HTTP client for bobby `/v1/*` CLI surfaces (jobs, openshell).
//!
//! `bobby` runs under `#[tokio::main]`; reqwest's blocking client builds its own
//! runtime and panics if constructed on a worker thread. Isolate on a plain OS thread.

use std::io::Read;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{Duration as ChronoDuration, Utc};
use types::CURRENT_INTERFACE_VERSION;
use uuid::Uuid;

const REQUEST_TIMEOUT_SECS: u64 = 10;
const DEADLINE_MINUTES: i64 = 2;
const MAX_RESPONSE_BODY_BYTES: usize = 8 * 1024 * 1024;

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
    v1_request_with_limits(
        options,
        Duration::from_secs(REQUEST_TIMEOUT_SECS),
        ChronoDuration::minutes(DEADLINE_MINUTES),
    )
}

/// `v1_request` with caller-chosen limits: long-running commands (a vision
/// solve loop waits on a local model per round) outgrow the 10s default.
pub fn v1_request_with_limits(
    options: V1Request,
    request_timeout: Duration,
    deadline: ChronoDuration,
) -> Result<V1Response> {
    match std::thread::spawn(move || v1_request_blocking(options, request_timeout, deadline)).join()
    {
        Ok(result) => result,
        Err(_) => anyhow::bail!("v1 HTTP thread panicked"),
    }
}

fn v1_request_blocking(
    options: V1Request,
    request_timeout: Duration,
    deadline: ChronoDuration,
) -> Result<V1Response> {
    let client = reqwest::blocking::Client::builder()
        .timeout(request_timeout)
        .no_proxy()
        .build()
        .context("failed to build /v1 HTTP client")?;

    let correlation_id = Uuid::new_v4();
    let deadline = (Utc::now() + deadline).to_rfc3339();

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
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BODY_BYTES as u64)
    {
        bail!(
            "response body exceeds {MAX_RESPONSE_BODY_BYTES} bytes from {}",
            options.url
        );
    }
    let mut bounded = response.take((MAX_RESPONSE_BODY_BYTES + 1) as u64);
    let mut bytes = Vec::new();
    bounded
        .read_to_end(&mut bytes)
        .with_context(|| format!("failed to read response body from {}", options.url))?;
    if bytes.len() > MAX_RESPONSE_BODY_BYTES {
        bail!(
            "response body exceeds {MAX_RESPONSE_BODY_BYTES} bytes from {}",
            options.url
        );
    }
    let body = String::from_utf8(bytes)
        .with_context(|| format!("response body from {} is not UTF-8", options.url))?;

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
    use std::io::{Read, Write};
    use std::net::TcpListener;

    fn serve_once(response: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let handle = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = [0_u8; 4096];
            let _ = stream.read(&mut request);
            let _ = stream.write_all(&response);
        });
        (format!("http://{address}/v1/test"), handle)
    }

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

    #[test]
    fn rejects_response_advertised_above_the_body_limit() {
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 16777216\r\n\r\n".to_vec();
        let (url, server) = serve_once(response);

        let error = v1_request(V1Request {
            method: reqwest::Method::GET,
            url,
            bearer: "test-token".into(),
            body: None,
            idempotency_key: None,
        })
        .unwrap_err();
        server.join().unwrap();

        assert!(
            error.to_string().contains("response body exceeds"),
            "unexpected error: {error:#}"
        );
    }

    #[test]
    fn rejects_chunked_response_that_crosses_the_body_limit() {
        let oversized = vec![b'x'; MAX_RESPONSE_BODY_BYTES + 1];
        let mut response = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
            oversized.len()
        )
        .into_bytes();
        response.extend_from_slice(&oversized);
        response.extend_from_slice(b"\r\n0\r\n\r\n");
        let (url, server) = serve_once(response);

        let error = v1_request(V1Request {
            method: reqwest::Method::GET,
            url,
            bearer: "test-token".into(),
            body: None,
            idempotency_key: None,
        })
        .unwrap_err();
        server.join().unwrap();

        assert!(
            error.to_string().contains("response body exceeds"),
            "unexpected error: {error:#}"
        );
    }
}
