use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::json;
use types::CURRENT_INTERFACE_VERSION;
use uuid::Uuid;

const CLIENT_TOKEN_ENV: &str = "AUTOMATION_RUNTIME_TOKEN";
const REQUEST_TIMEOUT_SECS: u64 = 10;
const DEADLINE_MINUTES: i64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum JobPriorityArg {
    Low,
    Normal,
    High,
    Critical,
}

impl JobPriorityArg {
    pub fn as_wire(self) -> &'static str {
        match self {
            Self::Low => "low",
            Self::Normal => "normal",
            Self::High => "high",
            Self::Critical => "critical",
        }
    }
}

/// Resolve bearer for `bobby jobs`: `--token`, then `AUTOMATION_RUNTIME_TOKEN`,
/// then bootstrap dotenv. Never print the returned value.
pub fn resolve_jobs_auth(token_override: Option<String>, bootstrap_path: &Path) -> Result<String> {
    if let Some(token) = token_override.filter(|value| !value.is_empty()) {
        return Ok(token);
    }
    if let Ok(token) = std::env::var(CLIENT_TOKEN_ENV) {
        if !token.is_empty() {
            return Ok(token);
        }
    }
    if bootstrap_path.exists() {
        return crate::bootstrap_local::load_bootstrap_bearer(bootstrap_path);
    }
    bail!(
        "no bearer token: pass --token, set {CLIENT_TOKEN_ENV}, or provide bootstrap at {}",
        bootstrap_path.display()
    )
}

pub fn resolve_jobs_base_url(base_url: Option<String>, config: &config::AppConfig) -> String {
    base_url.unwrap_or_else(|| format!("http://{}:{}", config.server.host, config.server.port))
}

pub fn jobs_url(base_url: &str, path: &str) -> Result<String> {
    let base = base_url.trim_end_matches('/');
    let path = if path.starts_with('/') {
        path.to_owned()
    } else {
        format!("/{path}")
    };
    Ok(format!("{base}{path}"))
}

/// Prefer `--payload-file` when set; otherwise parse `--payload` JSON.
pub fn resolve_submit_payload(payload: &str, payload_file: Option<&Path>) -> Result<serde_json::Value> {
    if let Some(path) = payload_file {
        let contents = std::fs::read_to_string(path)
            .with_context(|| format!("failed to read payload file {}", path.display()))?;
        return serde_json::from_str(&contents)
            .with_context(|| format!("payload file {} is not valid JSON", path.display()));
    }
    serde_json::from_str(payload).context("--payload is not valid JSON")
}

pub fn build_submit_body(
    name: &str,
    payload: serde_json::Value,
    priority: JobPriorityArg,
    max_retries: u32,
    timeout_ms: Option<u64>,
) -> serde_json::Value {
    let mut body = json!({
        "name": name,
        "payload": payload,
        "priority": priority.as_wire(),
        "maxRetries": max_retries,
    });
    if let Some(timeout_ms) = timeout_ms {
        body["timeoutMs"] = json!(timeout_ms);
    }
    body
}

pub struct JobsRequestOptions {
    pub method: reqwest::Method,
    pub url: String,
    pub bearer: String,
    pub body: Option<serde_json::Value>,
    pub idempotency_key: Option<String>,
}

pub fn jobs_request(options: JobsRequestOptions) -> Result<()> {
    // `bobby` runs under `#[tokio::main]`; reqwest's blocking client builds its own
    // runtime and panics if constructed on a worker thread. Isolate on a plain OS thread.
    match std::thread::spawn(move || jobs_request_blocking(options)).join() {
        Ok(result) => result,
        Err(_) => anyhow::bail!("jobs HTTP thread panicked"),
    }
}

fn jobs_request_blocking(options: JobsRequestOptions) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .no_proxy()
        .build()
        .context("failed to build jobs HTTP client")?;

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
    let text = response
        .text()
        .with_context(|| format!("failed to read response body from {}", options.url))?;

    if !status.is_success() {
        eprintln!("jobs request failed: HTTP {status}");
        if !text.is_empty() {
            eprintln!("{text}");
        }
        std::process::exit(1);
    }

    if text.trim().is_empty() {
        return Ok(());
    }

    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => {
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        Err(_) => {
            println!("{text}");
        }
    }
    Ok(())
}

pub fn submit_job(
    base_url: &str,
    bearer: String,
    name: &str,
    payload: serde_json::Value,
    priority: JobPriorityArg,
    max_retries: u32,
    timeout_ms: Option<u64>,
    idempotency_key: Option<String>,
) -> Result<()> {
    let url = jobs_url(base_url, "/v1/jobs")?;
    let body = build_submit_body(name, payload, priority, max_retries, timeout_ms);
    jobs_request(JobsRequestOptions {
        method: reqwest::Method::POST,
        url,
        bearer,
        body: Some(body),
        idempotency_key,
    })
}

pub fn job_status(base_url: &str, bearer: String, job_id: &str) -> Result<()> {
    let url = jobs_url(base_url, &format!("/v1/jobs/{job_id}"))?;
    jobs_request(JobsRequestOptions {
        method: reqwest::Method::GET,
        url,
        bearer,
        body: None,
        idempotency_key: None,
    })
}

pub fn cancel_job(base_url: &str, bearer: String, job_id: &str) -> Result<()> {
    let url = jobs_url(base_url, &format!("/v1/jobs/{job_id}"))?;
    jobs_request(JobsRequestOptions {
        method: reqwest::Method::DELETE,
        url,
        bearer,
        body: None,
        idempotency_key: None,
    })
}

pub fn load_config_for_jobs(config: Option<PathBuf>) -> Result<config::AppConfig> {
    let config_path = crate::resolve_config_path(config);
    config::AppConfig::load(&config_path)
        .with_context(|| format!("failed to load config from {}", config_path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::NamedTempFile;

    #[test]
    fn jobs_url_joins_base_and_path() {
        assert_eq!(
            jobs_url("http://127.0.0.1:7777", "/v1/jobs").unwrap(),
            "http://127.0.0.1:7777/v1/jobs"
        );
        assert_eq!(
            jobs_url("http://127.0.0.1:7777/", "v1/jobs/abc").unwrap(),
            "http://127.0.0.1:7777/v1/jobs/abc"
        );
    }

    #[test]
    fn resolve_submit_payload_prefers_file() {
        let mut file = NamedTempFile::new().unwrap();
        write!(file, r#"{{"from":"file"}}"#).unwrap();
        let value =
            resolve_submit_payload(r#"{"from":"flag"}"#, Some(file.path())).unwrap();
        assert_eq!(value["from"], "file");
    }

    #[test]
    fn resolve_submit_payload_uses_flag_when_no_file() {
        let value = resolve_submit_payload(r#"{"from":"flag"}"#, None).unwrap();
        assert_eq!(value["from"], "flag");
    }

    #[test]
    fn build_submit_body_omits_timeout_when_unset() {
        let body = build_submit_body(
            "echo",
            json!({}),
            JobPriorityArg::Normal,
            3,
            None,
        );
        assert_eq!(body["name"], "echo");
        assert_eq!(body["priority"], "normal");
        assert_eq!(body["maxRetries"], 3);
        assert!(body.get("timeoutMs").is_none());
    }

    #[test]
    fn build_submit_body_includes_timeout_when_set() {
        let body = build_submit_body(
            "echo",
            json!({"k":1}),
            JobPriorityArg::High,
            1,
            Some(5000),
        );
        assert_eq!(body["timeoutMs"], 5000);
        assert_eq!(body["priority"], "high");
    }

    #[test]
    fn resolve_jobs_base_url_uses_override() {
        let config = config::AppConfig::default();
        assert_eq!(
            resolve_jobs_base_url(Some("http://example:9".into()), &config),
            "http://example:9"
        );
        let default = resolve_jobs_base_url(None, &config);
        assert!(default.contains(&config.server.port.to_string()));
    }
}
