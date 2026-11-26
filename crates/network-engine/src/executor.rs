use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use cookie::Cookie;
use encoding_rs::{Encoding, UTF_8};
use futures_util::StreamExt;
use reqwest::header::{self, HeaderMap};
use reqwest::{Client, StatusCode};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tokio::time::timeout;
use types::{
    CommandError, DownloadUrlCommand, ErrorCode, ErrorLayer, Evidence, ExecutionReason,
    InspectCommand,
};
use url::Url;

use crate::document::inspect_document;
use crate::eligibility::download_limit_error;
use crate::state::{HttpCookie, HttpStateSnapshot, ResponseStateDelta};
use crate::{DestinationPolicy, NetworkPolicy};

#[derive(Clone)]
pub struct DirectHttpExecutor {
    network: NetworkPolicy,
    destinations: DestinationPolicy,
    permits: Arc<Semaphore>,
}

pub struct HttpMeta {
    pub final_url: String,
    pub status: u16,
    pub redirect_chain: Vec<String>,
    pub bytes: u64,
    pub sha256: String,
    pub elapsed_ms: u64,
    pub content_type: String,
}

pub enum HttpCandidate {
    Inspection {
        evidence: Evidence,
        state: ResponseStateDelta,
        meta: HttpMeta,
    },
    Download {
        bytes: Vec<u8>,
        filename: String,
        media_type: String,
        state: ResponseStateDelta,
        meta: HttpMeta,
    },
    FallbackRequired(ExecutionReason),
}

struct BoundedResponse {
    url: Url,
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
    state: ResponseStateDelta,
    redirects: Vec<String>,
    elapsed_ms: u64,
}

impl DirectHttpExecutor {
    pub fn new(network: NetworkPolicy) -> Self {
        Self {
            destinations: DestinationPolicy::new(network.clone()),
            permits: Arc::new(Semaphore::new(network.max_concurrent_requests)),
            network,
        }
    }

    pub async fn inspect(
        &self,
        snapshot: &HttpStateSnapshot,
        command: &InspectCommand,
    ) -> Result<HttpCandidate, CommandError> {
        timeout(
            Duration::from_millis(self.network.request_timeout_ms),
            async {
                let _permit = self
                    .permits
                    .acquire()
                    .await
                    .map_err(|_| transfer_error("HTTP executor is unavailable"))?;
                self.inspect_permitted(snapshot, command).await
            },
        )
        .await
        .map_err(|_| deadline_error())?
    }

    async fn inspect_permitted(
        &self,
        snapshot: &HttpStateSnapshot,
        command: &InspectCommand,
    ) -> Result<HttpCandidate, CommandError> {
        let response = self
            .execute(snapshot, &snapshot.current_url, self.network.max_body_bytes)
            .await?;
        if matches!(response.status.as_u16(), 204 | 205 | 304) || response.body.is_empty() {
            return Ok(HttpCandidate::FallbackRequired(
                ExecutionReason::StateConflict,
            ));
        }
        let media_type = content_type(&response.headers);
        if !matches!(
            media_type.as_str(),
            "text/html" | "application/xhtml+xml" | "text/plain"
        ) {
            return Ok(HttpCandidate::FallbackRequired(
                ExecutionReason::UnsupportedContentType,
            ));
        }
        let body = decode_text(&response.body, &response.headers)
            .ok_or_else(|| transfer_error("response text decoding was incomplete"))?;
        let safe_final_url = journal_safe_url(&response.url);
        let evidence = match inspect_document(&safe_final_url, &body, command) {
            Ok(evidence) => evidence,
            Err(reason) => return Ok(HttpCandidate::FallbackRequired(reason)),
        };
        let meta = response.meta();
        Ok(HttpCandidate::Inspection {
            evidence,
            state: response.state,
            meta,
        })
    }

    pub async fn download(
        &self,
        snapshot: &HttpStateSnapshot,
        command: &DownloadUrlCommand,
    ) -> Result<HttpCandidate, CommandError> {
        timeout(
            Duration::from_millis(self.network.request_timeout_ms),
            async {
                let _permit = self
                    .permits
                    .acquire()
                    .await
                    .map_err(|_| transfer_error("HTTP executor is unavailable"))?;
                self.download_permitted(snapshot, command).await
            },
        )
        .await
        .map_err(|_| deadline_error())?
    }

    async fn download_permitted(
        &self,
        snapshot: &HttpStateSnapshot,
        command: &DownloadUrlCommand,
    ) -> Result<HttpCandidate, CommandError> {
        if command.max_bytes == 0 || command.max_bytes > self.network.max_download_bytes as u64 {
            return Err(download_limit_error(self.network.max_download_bytes));
        }
        let limit = usize::try_from(command.max_bytes).unwrap_or(usize::MAX);
        let response = self.execute(snapshot, &command.url, limit).await?;
        let media_type = content_type(&response.headers);
        if command
            .expected_content_type
            .as_deref()
            .is_some_and(|expected| expected != media_type)
        {
            return Ok(HttpCandidate::FallbackRequired(
                ExecutionReason::UnsupportedContentType,
            ));
        }
        let filename = filename(&response.headers, &response.url);
        let meta = response.meta();
        Ok(HttpCandidate::Download {
            bytes: response.body,
            filename,
            media_type,
            state: response.state,
            meta,
        })
    }

    async fn execute(
        &self,
        snapshot: &HttpStateSnapshot,
        input: &str,
        limit: usize,
    ) -> Result<BoundedResponse, CommandError> {
        let started = Instant::now();
        let mut current = input.to_owned();
        let mut redirects = Vec::new();
        let mut state = ResponseStateDelta::default();
        for hop in 0..=self.network.max_redirects {
            let destination = self.destinations.resolve_and_validate(&current).await?;
            let url = destination.url().clone();
            redirects.push(journal_safe_url(&url));
            let host = url
                .host_str()
                .ok_or_else(|| policy_error("URL must include a host"))?;
            let chosen = *destination
                .addresses()
                .first()
                .ok_or_else(|| policy_error("destination resolved to no addresses"))?;
            let client = Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .resolve(host, chosen)
                .build()
                .map_err(|_| transfer_error("HTTP client initialization failed"))?;
            let mut request = client
                .get(url.clone())
                .header(header::USER_AGENT, &snapshot.user_agent)
                .header(header::ACCEPT_LANGUAGE, &snapshot.language);
            let cookies = cookie_header(snapshot, &state, &url)?;
            if !cookies.is_empty() {
                request = request.header(header::COOKIE, cookies);
            }
            if let Some(validator) = snapshot.cache_validators.get(url.as_str()) {
                request = request.header(header::IF_NONE_MATCH, validator);
            }
            let response = request
                .send()
                .await
                .map_err(|_| transfer_error("HTTP request failed"))?;
            bound_headers(response.headers(), self.network.max_header_bytes)?;
            collect_state(response.headers(), &url, &mut state)?;
            if matches!(response.status().as_u16(), 301 | 302 | 303 | 307 | 308) {
                if hop == self.network.max_redirects {
                    return Err(transfer_error("redirect limit exceeded"));
                }
                let location = response
                    .headers()
                    .get(header::LOCATION)
                    .and_then(|v| v.to_str().ok())
                    .ok_or_else(|| transfer_error("redirect location is invalid"))?;
                current = url
                    .join(location)
                    .map_err(|_| transfer_error("redirect location is invalid"))?
                    .to_string();
                continue;
            }
            let status = response.status();
            let headers = response.headers().clone();
            let mut body = Vec::new();
            let mut stream = response.bytes_stream();
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.map_err(|_| transfer_error("HTTP transfer was interrupted"))?;
                if body.len().saturating_add(chunk.len()) > limit {
                    return Err(too_large(limit));
                }
                body.extend_from_slice(&chunk);
            }
            return Ok(BoundedResponse {
                url,
                status,
                headers,
                body,
                state,
                redirects,
                elapsed_ms: started.elapsed().as_millis() as u64,
            });
        }
        Err(transfer_error("redirect limit exceeded"))
    }
}

impl BoundedResponse {
    fn meta(&self) -> HttpMeta {
        HttpMeta {
            final_url: journal_safe_url(&self.url),
            status: self.status.as_u16(),
            redirect_chain: self.redirects.clone(),
            bytes: self.body.len() as u64,
            sha256: hex::encode(Sha256::digest(&self.body)),
            elapsed_ms: self.elapsed_ms,
            content_type: content_type(&self.headers),
        }
    }
}

fn journal_safe_url(url: &Url) -> String {
    let mut safe = url.clone();
    let _ = safe.set_username("");
    let _ = safe.set_password(None);
    safe.set_query(None);
    safe.set_fragment(None);
    safe.to_string()
}

fn bound_headers(headers: &HeaderMap, limit: usize) -> Result<(), CommandError> {
    let size = headers
        .iter()
        .map(|(name, value)| name.as_str().len() + value.as_bytes().len() + 4)
        .sum::<usize>();
    if size > limit {
        Err(too_large(limit))
    } else {
        Ok(())
    }
}

fn cookie_header(
    snapshot: &HttpStateSnapshot,
    state: &ResponseStateDelta,
    url: &Url,
) -> Result<String, CommandError> {
    let host = url.host_str().unwrap_or_default();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| transfer_error("system clock is invalid"))?
        .as_secs_f64();
    let top_level = Url::parse(&snapshot.current_url)
        .map_err(|_| policy_error("cookie top-level site is invalid"))?;
    let mut jar = BTreeMap::new();
    for cookie in snapshot.cookies.iter().chain(state.cookies.iter()) {
        if cookie
            .expires_unix
            .is_some_and(|expiry| !expiry.is_finite())
        {
            return Err(policy_error("cookie expiry cannot be evaluated safely"));
        }
        let partition = cookie
            .partition_key
            .as_ref()
            .map(|key| (key.top_level_site.clone(), key.has_cross_site_ancestor));
        jar.insert(
            (
                cookie.name.clone(),
                cookie.domain.trim_start_matches('.').to_ascii_lowercase(),
                cookie.path.clone(),
                partition,
            ),
            cookie,
        );
    }
    let mut sent = Vec::new();
    for cookie in jar.into_values() {
        if cookie.same_site.as_deref().is_some_and(|value| {
            value.eq_ignore_ascii_case("strict") || value.eq_ignore_ascii_case("lax")
        }) && (url.scheme() != top_level.scheme()
            || url.host_str().map(str::to_ascii_lowercase)
                != top_level.host_str().map(str::to_ascii_lowercase))
        {
            return Err(equivalence_error(
                "schemeful SameSite request context cannot be proven equivalent",
            ));
        }
        if cookie
            .same_site
            .as_deref()
            .is_some_and(|value| value.eq_ignore_ascii_case("none"))
            && (!cookie.secure || url.scheme() != "https")
        {
            return Err(equivalence_error(
                "SameSite=None cookie requires Secure HTTPS",
            ));
        }
        if cookie.expires_unix.is_some_and(|expiry| expiry <= now)
            || (cookie.secure && url.scheme() != "https")
        {
            continue;
        }
        let domain = cookie.domain.trim_start_matches('.');
        let domain_matches = if cookie.host_only {
            host.eq_ignore_ascii_case(domain)
        } else {
            host.eq_ignore_ascii_case(domain)
                || host
                    .to_ascii_lowercase()
                    .ends_with(&format!(".{}", domain.to_ascii_lowercase()))
        };
        let request_path = url.path();
        let path_matches = request_path == cookie.path
            || (request_path.starts_with(&cookie.path)
                && (cookie.path.ends_with('/')
                    || request_path.as_bytes().get(cookie.path.len()) == Some(&b'/')));
        if !domain_matches || !path_matches {
            continue;
        }
        if let Some(source_port) = cookie.source_port {
            let request_port = url.port_or_known_default().map(i64::from);
            if source_port == -1 || request_port != Some(source_port) {
                return Err(equivalence_error(
                    "cookie source port does not match the request URL",
                ));
            }
        }
        if let Some(partition) = &cookie.partition_key {
            let site = Url::parse(&partition.top_level_site)
                .map_err(|_| policy_error("cookie partition cannot be evaluated safely"))?;
            if partition.has_cross_site_ancestor
                || site.scheme() != top_level.scheme()
                || site.host_str() != top_level.host_str()
            {
                continue;
            }
        }
        sent.push(format!("{}={}", cookie.name, cookie.value));
    }
    Ok(sent.join("; "))
}

fn collect_state(
    headers: &HeaderMap,
    url: &Url,
    state: &mut ResponseStateDelta,
) -> Result<(), CommandError> {
    for value in headers.get_all(header::SET_COOKIE) {
        let Ok(raw) = value.to_str() else { continue };
        let Ok(cookie) = Cookie::parse(raw.to_owned()) else {
            continue;
        };
        if cookie.partitioned() == Some(true) {
            return Err(policy_error(
                "partitioned response cookie cannot be represented safely",
            ));
        }
        if cookie.domain().is_some() {
            return Err(equivalence_error(
                "response Domain cookies require browser fallback",
            ));
        }
        let response_cookie = HttpCookie {
            name: cookie.name().to_owned(),
            value: cookie.value().to_owned(),
            domain: cookie
                .domain()
                .unwrap_or_else(|| url.host_str().unwrap_or_default())
                .to_owned(),
            host_only: cookie.domain().is_none(),
            path: cookie.path().unwrap_or("/").to_owned(),
            secure: cookie.secure().unwrap_or(false),
            http_only: cookie.http_only().unwrap_or(false),
            same_site: cookie.same_site().map(|v| format!("{v:?}")),
            expires_unix: cookie
                .expires_datetime()
                .map(|expiry| expiry.unix_timestamp() as f64),
            priority: None,
            source_scheme: Some(
                if url.scheme() == "https" {
                    "Secure"
                } else {
                    "NonSecure"
                }
                .to_owned(),
            ),
            source_port: url.port_or_known_default().map(i64::from),
            partition_key: None,
        };
        state.cookies.retain(|existing| {
            existing.name != response_cookie.name
                || !existing
                    .domain
                    .trim_start_matches('.')
                    .eq_ignore_ascii_case(response_cookie.domain.trim_start_matches('.'))
                || existing.path != response_cookie.path
                || !same_partition(existing, &response_cookie)
        });
        state.cookies.push(response_cookie);
    }
    let mut validators = BTreeMap::new();
    if let Some(value) = headers.get(header::ETAG).and_then(|v| v.to_str().ok()) {
        validators.insert(journal_safe_url(url), value.to_owned());
    }
    if let Some(value) = headers
        .get(header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
    {
        validators.insert(journal_safe_url(url), value.to_owned());
    }
    state.cache_validators.extend(validators);
    Ok(())
}

fn same_partition(left: &HttpCookie, right: &HttpCookie) -> bool {
    match (&left.partition_key, &right.partition_key) {
        (None, None) => true,
        (Some(left), Some(right)) => {
            left.top_level_site == right.top_level_site
                && left.has_cross_site_ancestor == right.has_cross_site_ancestor
        }
        _ => false,
    }
}

fn content_type(headers: &HeaderMap) -> String {
    headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("application/octet-stream")
        .split(';')
        .next()
        .unwrap_or_default()
        .trim()
        .to_ascii_lowercase()
}

fn decode_text(bytes: &[u8], headers: &HeaderMap) -> Option<String> {
    let charset = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.split(';')
                .find_map(|part| part.trim().strip_prefix("charset="))
        })
        .unwrap_or("utf-8");
    let encoding = Encoding::for_label(charset.as_bytes()).unwrap_or(UTF_8);
    let (text, _, malformed) = encoding.decode(bytes);
    (!malformed).then(|| text.into_owned())
}

fn filename(headers: &HeaderMap, url: &Url) -> String {
    let supplied = headers
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            let parts = v.split(';').map(str::trim).collect::<Vec<_>>();
            parts
                .iter()
                .find_map(|part| {
                    part.strip_prefix("filename*=UTF-8''")
                        .and_then(percent_decode)
                })
                .or_else(|| {
                    parts.iter().find_map(|part| {
                        part.trim()
                            .strip_prefix("filename=")
                            .map(|name| name.trim_matches('"').to_owned())
                    })
                })
        })
        .or_else(|| {
            url.path_segments()
                .and_then(|mut parts| parts.next_back())
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
        });
    supplied
        .and_then(sanitize_filename)
        .unwrap_or_else(|| "download.bin".to_owned())
}

fn percent_decode(input: &str) -> Option<String> {
    let mut bytes = Vec::with_capacity(input.len());
    let mut chars = input.as_bytes().iter().copied();
    while let Some(byte) = chars.next() {
        if byte == b'%' {
            let hi = chars.next()?;
            let lo = chars.next()?;
            let hex = |b: u8| match b {
                b'0'..=b'9' => Some(b - b'0'),
                b'a'..=b'f' => Some(b - b'a' + 10),
                b'A'..=b'F' => Some(b - b'A' + 10),
                _ => None,
            };
            bytes.push(hex(hi)? * 16 + hex(lo)?);
        } else {
            bytes.push(byte);
        }
    }
    String::from_utf8(bytes).ok()
}

fn sanitize_filename(name: String) -> Option<String> {
    let base = name
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches([' ', '.'])
        .to_ascii_uppercase();
    let reserved = matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || base
            .strip_prefix("COM")
            .is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"))
        || base
            .strip_prefix("LPT")
            .is_some_and(|n| matches!(n, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9"));
    if name.is_empty()
        || name.len() > 255
        || name == "."
        || name == ".."
        || name.starts_with('/')
        || name.starts_with('\\')
        || name.contains('/')
        || name.contains('\\')
        || name.contains(':')
        || name.ends_with(['.', ' '])
        || reserved
        || name.chars().any(char::is_control)
    {
        None
    } else {
        Some(name)
    }
}

#[cfg(test)]
#[allow(clippy::items_after_test_module)]
mod cookie_context_tests {
    use super::*;
    use crate::state::{HttpCookie, HttpStateSnapshot};

    fn snapshot(
        current_url: &str,
        same_site: &str,
        secure: bool,
        source_port: Option<i64>,
    ) -> HttpStateSnapshot {
        HttpStateSnapshot {
            version: 1,
            current_url: current_url.into(),
            cache_validators: BTreeMap::new(),
            user_agent: "test".into(),
            language: "en".into(),
            cookies: vec![HttpCookie {
                name: "sid".into(),
                value: "secret".into(),
                domain: "example.com".into(),
                host_only: true,
                path: "/".into(),
                secure,
                http_only: true,
                same_site: Some(same_site.into()),
                expires_unix: None,
                priority: None,
                source_scheme: None,
                source_port,
                partition_key: None,
            }],
        }
    }

    #[test]
    fn strict_and_lax_fail_closed_across_scheme_or_host() {
        for mode in ["Strict", "Lax"] {
            let state = snapshot("https://example.com/", mode, false, None);
            assert_eq!(
                cookie_header(
                    &state,
                    &ResponseStateDelta::default(),
                    &Url::parse("http://example.com/").unwrap()
                )
                .unwrap_err()
                .code,
                ErrorCode::HttpEquivalenceUnproven
            );
            assert_eq!(
                cookie_header(
                    &state,
                    &ResponseStateDelta::default(),
                    &Url::parse("https://other.example/").unwrap()
                )
                .unwrap_err()
                .code,
                ErrorCode::HttpEquivalenceUnproven
            );
        }
    }

    #[test]
    fn same_host_schemeful_cookie_is_sent_and_source_port_must_match() {
        let state = snapshot("https://example.com/", "Strict", true, Some(443));
        assert_eq!(
            cookie_header(
                &state,
                &ResponseStateDelta::default(),
                &Url::parse("https://example.com/path").unwrap()
            )
            .unwrap(),
            "sid=secret"
        );
        let mismatch = snapshot("https://example.com/", "Lax", true, Some(8443));
        assert_eq!(
            cookie_header(
                &mismatch,
                &ResponseStateDelta::default(),
                &Url::parse("https://example.com/").unwrap()
            )
            .unwrap_err()
            .code,
            ErrorCode::HttpEquivalenceUnproven
        );
    }

    #[test]
    fn same_site_none_requires_secure_https() {
        for (secure, url) in [
            (false, "https://example.com/"),
            (true, "http://example.com/"),
        ] {
            let state = snapshot("https://example.com/", "None", secure, None);
            assert_eq!(
                cookie_header(
                    &state,
                    &ResponseStateDelta::default(),
                    &Url::parse(url).unwrap()
                )
                .unwrap_err()
                .code,
                ErrorCode::HttpEquivalenceUnproven
            );
        }
    }
}

fn error(code: ErrorCode, message: impl Into<String>, retryable: bool) -> CommandError {
    CommandError {
        code,
        message: message.into(),
        layer: ErrorLayer::Network,
        retryable,
    }
}
fn policy_error(message: impl Into<String>) -> CommandError {
    error(ErrorCode::NetworkPolicyDenied, message, false)
}
fn equivalence_error(message: impl Into<String>) -> CommandError {
    error(ErrorCode::HttpEquivalenceUnproven, message, false)
}
fn transfer_error(message: impl Into<String>) -> CommandError {
    error(ErrorCode::HttpTransferFailed, message, true)
}
fn deadline_error() -> CommandError {
    error(
        ErrorCode::DeadlineExceeded,
        "HTTP operation deadline exceeded",
        true,
    )
}
fn too_large(limit: usize) -> CommandError {
    error(
        ErrorCode::HttpResponseTooLarge,
        format!("HTTP response exceeded configured limit of {limit} bytes"),
        false,
    )
}
