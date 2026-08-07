//! Bounded HTTP readiness probe for scheduler job handlers.
//!
//! Uses the same destination policy as downloads (SSRF-safe by default).

use std::time::{Duration, Instant};

use futures_util::StreamExt;
use reqwest::{header, Client, Method};
use serde_json::json;

use crate::policy::{DestinationPolicy, NetworkPolicy};

const DEFAULT_TIMEOUT_MS: u64 = 5_000;
const MAX_TIMEOUT_MS: u64 = 15_000;
/// GET responses are discarded after this many bytes (status still reported).
const MAX_PROBE_BODY_BYTES: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpProbeMethod {
    Head,
    Get,
}

impl HttpProbeMethod {
    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_uppercase().as_str() {
            "HEAD" => Some(Self::Head),
            "GET" => Some(Self::Get),
            _ => None,
        }
    }

    fn as_reqwest(self) -> Method {
        match self {
            Self::Head => Method::HEAD,
            Self::Get => Method::GET,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Head => "HEAD",
            Self::Get => "GET",
        }
    }
}

/// Probe `url` with HEAD (default) or GET under [`NetworkPolicy`].
///
/// `timeout_ms` defaults to 5000 and is capped at 15000. Redirects are followed
/// manually and re-validated each hop. Response bodies are not returned.
pub async fn http_probe(
    url: &str,
    method: HttpProbeMethod,
    timeout_ms: Option<u64>,
    network: NetworkPolicy,
) -> Result<serde_json::Value, String> {
    let timeout_ms = timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS);
    let destinations = DestinationPolicy::new(network.clone());
    let started = Instant::now();
    let overall = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        let mut current = url.to_owned();
        let mut redirects = 0usize;
        for hop in 0..=network.max_redirects {
            let destination = destinations
                .resolve_and_validate(&current)
                .await
                .map_err(|error| error.message)?;
            let request_url = destination.url().clone();
            let host = request_url
                .host_str()
                .ok_or_else(|| "URL must include a host".to_owned())?;
            let chosen = *destination
                .addresses()
                .first()
                .ok_or_else(|| "destination resolved to no addresses".to_owned())?;
            let client = Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .resolve(host, chosen)
                .build()
                .map_err(|_| "HTTP client initialization failed".to_owned())?;
            let response = client
                .request(method.as_reqwest(), request_url.clone())
                .header(header::USER_AGENT, "bobby-browser-http-probe/0.6")
                .send()
                .await
                .map_err(|error| format!("HTTP request failed: {error}"))?;
            let status = response.status();
            if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                if hop == network.max_redirects {
                    return Err("redirect limit exceeded".to_owned());
                }
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| "redirect location is invalid".to_owned())?;
                current = request_url
                    .join(location)
                    .map_err(|_| "redirect location is invalid".to_owned())?
                    .to_string();
                redirects += 1;
                continue;
            }
            // Drain a tiny body cap so the connection can close cleanly on GET.
            if method == HttpProbeMethod::Get {
                let mut drained = 0usize;
                let mut stream = response.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = chunk.map_err(|_| "HTTP transfer was interrupted".to_owned())?;
                    drained = drained.saturating_add(chunk.len());
                    if drained > MAX_PROBE_BODY_BYTES {
                        break;
                    }
                }
            }
            let ok = status.is_success();
            return Ok(json!({
                "ok": ok,
                "status": status.as_u16(),
                "finalUrl": request_url.as_str(),
                "method": method.as_str(),
                "elapsedMs": started.elapsed().as_millis() as u64,
                "redirects": redirects
            }));
        }
        Err("redirect limit exceeded".to_owned())
    })
    .await
    .map_err(|_| format!("http_probe timed out after {timeout_ms}ms"))?;
    overall
}

const DEFAULT_FETCH_BODY_BYTES: usize = 4_096;
const MAX_FETCH_BODY_BYTES: usize = 16_384;

/// GET `url` and return a truncated UTF-8 body under [`NetworkPolicy`].
///
/// Agents use this instead of opening a browser session to inspect health JSON /
/// small API responses. `timeout_ms` defaults to 5000 (cap 15000).
/// `max_body_bytes` defaults to 4096 (cap 16384). Optional `contains` requires
/// the body substring for `ok` (in addition to HTTP success).
pub async fn http_fetch(
    url: &str,
    timeout_ms: Option<u64>,
    max_body_bytes: Option<usize>,
    contains: Option<&str>,
    network: NetworkPolicy,
) -> Result<serde_json::Value, String> {
    let timeout_ms = timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS).min(MAX_TIMEOUT_MS);
    let max_body_bytes = max_body_bytes
        .unwrap_or(DEFAULT_FETCH_BODY_BYTES)
        .clamp(1, MAX_FETCH_BODY_BYTES);
    let destinations = DestinationPolicy::new(network.clone());
    let started = Instant::now();
    let overall = tokio::time::timeout(Duration::from_millis(timeout_ms), async {
        let mut current = url.to_owned();
        let mut redirects = 0usize;
        for hop in 0..=network.max_redirects {
            let destination = destinations
                .resolve_and_validate(&current)
                .await
                .map_err(|error| error.message)?;
            let request_url = destination.url().clone();
            let host = request_url
                .host_str()
                .ok_or_else(|| "URL must include a host".to_owned())?;
            let chosen = *destination
                .addresses()
                .first()
                .ok_or_else(|| "destination resolved to no addresses".to_owned())?;
            let client = Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .resolve(host, chosen)
                .build()
                .map_err(|_| "HTTP client initialization failed".to_owned())?;
            let response = client
                .request(Method::GET, request_url.clone())
                .header(header::USER_AGENT, "bobby-browser-http-fetch/0.6")
                .send()
                .await
                .map_err(|error| format!("HTTP request failed: {error}"))?;
            let status = response.status();
            let content_type = response
                .headers()
                .get(header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("")
                .to_owned();
            if matches!(status.as_u16(), 301 | 302 | 303 | 307 | 308) {
                if hop == network.max_redirects {
                    return Err("redirect limit exceeded".to_owned());
                }
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|value| value.to_str().ok())
                    .ok_or_else(|| "redirect location is invalid".to_owned())?;
                current = request_url
                    .join(location)
                    .map_err(|_| "redirect location is invalid".to_owned())?
                    .to_string();
                redirects += 1;
                continue;
            }

            let mut raw = Vec::new();
            let mut truncated = false;
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| "HTTP transfer was interrupted".to_owned())?;
                let room = max_body_bytes.saturating_sub(raw.len());
                if room == 0 {
                    truncated = true;
                    break;
                }
                if chunk.len() > room {
                    raw.extend_from_slice(&chunk[..room]);
                    truncated = true;
                    break;
                }
                raw.extend_from_slice(&chunk);
            }
            let bytes = raw.len();
            let body = String::from_utf8_lossy(&raw).into_owned();
            let contains_matched = contains.map(|needle| body.contains(needle));
            let ok = status.is_success() && contains_matched.unwrap_or(true);
            let mut result = json!({
                "ok": ok,
                "status": status.as_u16(),
                "finalUrl": request_url.as_str(),
                "method": "GET",
                "contentType": content_type,
                "body": body,
                "bytes": bytes,
                "truncated": truncated,
                "elapsedMs": started.elapsed().as_millis() as u64,
                "redirects": redirects
            });
            if let Some(matched) = contains_matched {
                result["containsMatched"] = json!(matched);
            }
            return Ok(result);
        }
        Err("redirect limit exceeded".to_owned())
    })
    .await
    .map_err(|_| format!("http_fetch timed out after {timeout_ms}ms"))?;
    overall
}

const DEFAULT_WAIT_TIMEOUT_MS: u64 = 30_000;
const MAX_WAIT_TIMEOUT_MS: u64 = 60_000;
const DEFAULT_INTERVAL_MS: u64 = 1_000;
const MAX_INTERVAL_MS: u64 = 10_000;

/// Poll until a successful probe (or body match) or the wait budget expires.
///
/// `timeout_ms` is the overall wait budget (default 30000, cap 60000).
/// `interval_ms` is the delay between failed attempts (default 1000, cap 10000).
/// `probe_timeout_ms` is forwarded to each attempt.
/// When `contains` is set, each attempt is [`http_fetch`] (GET + truncated body)
/// and success requires HTTP success plus the substring; otherwise each attempt
/// is [`http_probe`].
pub async fn http_wait(
    url: &str,
    method: HttpProbeMethod,
    timeout_ms: Option<u64>,
    interval_ms: Option<u64>,
    probe_timeout_ms: Option<u64>,
    contains: Option<&str>,
    max_body_bytes: Option<usize>,
    network: NetworkPolicy,
) -> Result<serde_json::Value, String> {
    let wait_ms = timeout_ms
        .unwrap_or(DEFAULT_WAIT_TIMEOUT_MS)
        .min(MAX_WAIT_TIMEOUT_MS);
    let interval_ms = interval_ms
        .unwrap_or(DEFAULT_INTERVAL_MS)
        .clamp(50, MAX_INTERVAL_MS);
    let started = Instant::now();
    let deadline = started + Duration::from_millis(wait_ms);
    let mut attempts = 0u64;
    // Last non-success outcome from the most recent attempt.
    #[allow(unused_assignments)]
    let mut last_outcome: Option<Result<serde_json::Value, String>> = None;
    let body_gate = contains.is_some();

    loop {
        attempts += 1;
        let attempt = if body_gate {
            http_fetch(
                url,
                probe_timeout_ms,
                max_body_bytes,
                contains,
                network.clone(),
            )
            .await
        } else {
            http_probe(url, method, probe_timeout_ms, network.clone()).await
        };
        match attempt {
            Ok(sample) => {
                let ok = sample
                    .get("ok")
                    .and_then(|value| value.as_bool())
                    .unwrap_or(false);
                if ok {
                    let key = if body_gate { "fetch" } else { "probe" };
                    return Ok(json!({
                        "ok": true,
                        "attempts": attempts,
                        "elapsedMs": started.elapsed().as_millis() as u64,
                        key: sample,
                    }));
                }
                last_outcome = Some(Ok(sample));
            }
            Err(error) => {
                last_outcome = Some(Err(error));
            }
        }

        let now = Instant::now();
        if now >= deadline {
            break;
        }
        let sleep_for = Duration::from_millis(interval_ms).min(deadline - now);
        tokio::time::sleep(sleep_for).await;
    }

    let mut detail = json!({
        "ok": false,
        "attempts": attempts,
        "elapsedMs": started.elapsed().as_millis() as u64,
        "timedOut": true,
    });
    match last_outcome {
        Some(Ok(sample)) => {
            let key = if body_gate { "lastFetch" } else { "lastProbe" };
            detail[key] = sample;
        }
        Some(Err(error)) => detail["lastError"] = json!(error),
        None => {}
    }
    Err(format!(
        "http_wait timed out after {wait_ms}ms: {}",
        serde_json::to_string(&detail).unwrap_or_else(|_| "{}".to_owned())
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_loopback_by_default() {
        let err = http_probe(
            "http://127.0.0.1:9/",
            HttpProbeMethod::Head,
            Some(1_000),
            NetworkPolicy::default(),
        )
        .await
        .expect_err("loopback must be denied");
        assert!(
            err.to_lowercase().contains("denied")
                || err.to_lowercase().contains("private")
                || err.to_lowercase().contains("loopback")
                || err.to_lowercase().contains("policy")
                || err.to_lowercase().contains("not allowed")
                || err.to_lowercase().contains("not permitted"),
            "unexpected deny message: {err}"
        );
    }

    #[tokio::test]
    async fn rejects_non_http_scheme() {
        let err = http_probe(
            "ftp://example.com/",
            HttpProbeMethod::Head,
            Some(1_000),
            NetworkPolicy::default(),
        )
        .await
        .expect_err("ftp must fail");
        assert!(!err.is_empty());
    }

    #[tokio::test]
    async fn wait_rejects_loopback_by_default() {
        let err = http_wait(
            "http://127.0.0.1:9/",
            HttpProbeMethod::Head,
            Some(200),
            Some(50),
            Some(50),
            None,
            None,
            NetworkPolicy::default(),
        )
        .await
        .expect_err("loopback must be denied");
        assert!(
            err.to_lowercase().contains("timed out") || err.to_lowercase().contains("denied"),
            "unexpected deny message: {err}"
        );
    }

    #[tokio::test]
    async fn wait_with_contains_rejects_loopback_by_default() {
        let err = http_wait(
            "http://127.0.0.1:9/",
            HttpProbeMethod::Get,
            Some(200),
            Some(50),
            Some(50),
            Some("ready"),
            Some(256),
            NetworkPolicy::default(),
        )
        .await
        .expect_err("loopback must be denied");
        assert!(
            err.to_lowercase().contains("timed out") || err.to_lowercase().contains("denied"),
            "unexpected deny message: {err}"
        );
    }
}
