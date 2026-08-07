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
}
