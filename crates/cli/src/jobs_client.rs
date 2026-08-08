use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};
use serde_json::json;

use crate::v1_client::{self, V1Request};

const CLIENT_TOKEN_ENV: &str = "AUTOMATION_RUNTIME_TOKEN";

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
    v1_client::v1_url(base_url, path)
}

/// Prefer `--payload-file` when set; otherwise parse `--payload` JSON.
pub fn resolve_submit_payload(
    payload: &str,
    payload_file: Option<&Path>,
) -> Result<serde_json::Value> {
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
    let response = v1_client::v1_request(V1Request {
        method: options.method.clone(),
        url: options.url.clone(),
        bearer: options.bearer,
        body: options.body,
        idempotency_key: options.idempotency_key,
    })?;

    if !response.status.is_success() {
        eprintln!("jobs request failed: HTTP {}", response.status);
        if !response.body.is_empty() {
            eprintln!("{}", response.body);
        }
        std::process::exit(1);
    }

    if response.body.trim().is_empty() {
        return Ok(());
    }

    match serde_json::from_str::<serde_json::Value>(&response.body) {
        Ok(value) => {
            println!("{}", serde_json::to_string_pretty(&value)?);
        }
        Err(_) => {
            println!("{}", response.body);
        }
    }
    Ok(())
}

pub struct SubmitJobOptions<'a> {
    pub name: &'a str,
    pub payload: serde_json::Value,
    pub priority: JobPriorityArg,
    pub max_retries: u32,
    pub timeout_ms: Option<u64>,
    pub idempotency_key: Option<String>,
}

pub fn submit_job(base_url: &str, bearer: String, options: SubmitJobOptions<'_>) -> Result<()> {
    let url = jobs_url(base_url, "/v1/jobs")?;
    let body = build_submit_body(
        options.name,
        options.payload,
        options.priority,
        options.max_retries,
        options.timeout_ms,
    );
    jobs_request(JobsRequestOptions {
        method: reqwest::Method::POST,
        url,
        bearer,
        body: Some(body),
        idempotency_key: options.idempotency_key,
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
        let value = resolve_submit_payload(r#"{"from":"flag"}"#, Some(file.path())).unwrap();
        assert_eq!(value["from"], "file");
    }

    #[test]
    fn resolve_submit_payload_uses_flag_when_no_file() {
        let value = resolve_submit_payload(r#"{"from":"flag"}"#, None).unwrap();
        assert_eq!(value["from"], "flag");
    }

    #[test]
    fn build_submit_body_omits_timeout_when_unset() {
        let body = build_submit_body("echo", json!({}), JobPriorityArg::Normal, 3, None);
        assert_eq!(body["name"], "echo");
        assert_eq!(body["priority"], "normal");
        assert_eq!(body["maxRetries"], 3);
        assert!(body.get("timeoutMs").is_none());
    }

    #[test]
    fn build_submit_body_includes_timeout_when_set() {
        let body = build_submit_body("echo", json!({"k":1}), JobPriorityArg::High, 1, Some(5000));
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
