use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use cookie::Cookie;
use encoding_rs::{Encoding, UTF_8};
use futures_util::StreamExt;
use reqwest::header::{self, HeaderMap};
use reqwest::{Client, StatusCode};
use sha2::{Digest, Sha256};
use types::{
    CommandError, DownloadUrlCommand, ErrorCode, ErrorLayer, Evidence, ExecutionReason,
    InspectCommand,
};
use url::Url;

use crate::document::inspect_document;
use crate::state::{HttpCookie, HttpStateSnapshot, ResponseStateDelta};
use crate::{DestinationPolicy, NetworkPolicy};

pub struct DirectHttpExecutor {
    network: NetworkPolicy,
    destinations: DestinationPolicy,
}

pub struct HttpMeta {
    pub final_url: String,
    pub status: u16,
    pub redirect_chain: Vec<String>,
    pub bytes: u64,
    pub sha256: String,
    pub elapsed_ms: u64,
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
            network,
        }
    }

    pub async fn inspect(
        &self,
        snapshot: &HttpStateSnapshot,
        command: &InspectCommand,
    ) -> Result<HttpCandidate, CommandError> {
        let response = self
            .execute(snapshot, &snapshot.current_url, self.network.max_body_bytes)
            .await?;
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
        let evidence = match inspect_document(response.url.as_str(), &body, command) {
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
        if command.max_bytes == 0 || command.max_bytes > self.network.max_download_bytes as u64 {
            return Err(policy_error(
                "download byte limit is outside the configured range",
            ));
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
            redirects.push(url.to_string());
            let host = url
                .host_str()
                .ok_or_else(|| policy_error("URL must include a host"))?;
            let chosen = *destination
                .addresses()
                .first()
                .ok_or_else(|| policy_error("destination resolved to no addresses"))?;
            let client = Client::builder()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_millis(self.network.request_timeout_ms))
                .resolve(host, chosen)
                .build()
                .map_err(|_| transfer_error("HTTP client initialization failed"))?;
            let mut request = client
                .get(url.clone())
                .header(header::USER_AGENT, &snapshot.user_agent)
                .header(header::ACCEPT_LANGUAGE, &snapshot.language);
            let cookies = cookie_header(snapshot, &state, &url);
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
            collect_state(response.headers(), &url, &mut state);
            if response.status().is_redirection() {
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
            final_url: self.url.to_string(),
            status: self.status.as_u16(),
            redirect_chain: self.redirects.clone(),
            bytes: self.body.len() as u64,
            sha256: format!("{:x}", Sha256::digest(&self.body)),
            elapsed_ms: self.elapsed_ms,
        }
    }
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

fn cookie_header(snapshot: &HttpStateSnapshot, state: &ResponseStateDelta, url: &Url) -> String {
    let host = url.host_str().unwrap_or_default();
    snapshot
        .cookies
        .iter()
        .chain(state.cookies.iter())
        .filter(|cookie| {
            host == cookie.domain.trim_start_matches('.')
                || host.ends_with(&format!(".{}", cookie.domain.trim_start_matches('.')))
        })
        .filter(|cookie| {
            url.path().starts_with(&cookie.path) && (!cookie.secure || url.scheme() == "https")
        })
        .map(|cookie| format!("{}={}", cookie.name, cookie.value))
        .collect::<Vec<_>>()
        .join("; ")
}

fn collect_state(headers: &HeaderMap, url: &Url, state: &mut ResponseStateDelta) {
    for value in headers.get_all(header::SET_COOKIE) {
        let Ok(raw) = value.to_str() else { continue };
        let Ok(cookie) = Cookie::parse(raw.to_owned()) else {
            continue;
        };
        state.cookies.push(HttpCookie {
            name: cookie.name().to_owned(),
            value: cookie.value().to_owned(),
            domain: cookie
                .domain()
                .unwrap_or_else(|| url.host_str().unwrap_or_default())
                .to_owned(),
            path: cookie.path().unwrap_or("/").to_owned(),
            secure: cookie.secure().unwrap_or(false),
            http_only: cookie.http_only().unwrap_or(false),
            same_site: cookie.same_site().map(|v| format!("{v:?}")),
            expires_unix: None,
            priority: None,
            source_scheme: Some(url.scheme().to_owned()),
            source_port: url.port_or_known_default().map(i64::from),
            partition_key: None,
        });
    }
    let mut validators = BTreeMap::new();
    if let Some(value) = headers.get(header::ETAG).and_then(|v| v.to_str().ok()) {
        validators.insert(url.to_string(), value.to_owned());
    }
    if let Some(value) = headers
        .get(header::LAST_MODIFIED)
        .and_then(|v| v.to_str().ok())
    {
        validators.insert(url.to_string(), value.to_owned());
    }
    state.cache_validators.extend(validators);
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
    headers
        .get(header::CONTENT_DISPOSITION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.split(';').find_map(|part| {
                part.trim()
                    .strip_prefix("filename=")
                    .map(|name| name.trim_matches('"').to_owned())
            })
        })
        .or_else(|| {
            url.path_segments()
                .and_then(|mut parts| parts.next_back())
                .filter(|part| !part.is_empty())
                .map(str::to_owned)
        })
        .unwrap_or_else(|| "download".to_owned())
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
fn transfer_error(message: impl Into<String>) -> CommandError {
    error(ErrorCode::HttpTransferFailed, message, true)
}
fn too_large(limit: usize) -> CommandError {
    error(
        ErrorCode::HttpResponseTooLarge,
        format!("HTTP response exceeded configured limit of {limit} bytes"),
        false,
    )
}
