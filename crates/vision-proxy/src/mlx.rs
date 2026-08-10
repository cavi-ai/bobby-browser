use async_trait::async_trait;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::data_collector::VisionDataCollector;
use crate::upstream::{ExtractInput, ProposeInput, Upstream, UpstreamError};
use crate::validate::{validate_extract, validate_proposal};
use crate::wire::{ExtractResponse, ProposeResponse};

/// Default loopback endpoint of the canonical Python vision server.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:9101";
pub const DEFAULT_MODEL: &str = "mlx-community/Qwen2.5-VL-3B-Instruct-4bit";

/// Pass-through upstream for the canonical Python vision server
/// (`scripts/vision-mlx/vision_server.py`). The server already speaks
/// Bobby's wire format — propose/extract bodies are forwarded verbatim —
/// so this upstream is a thin authenticated proxy, unlike the Ollama
/// backend which translates chat-completions payloads.
pub struct MlxUpstream {
    client: reqwest::Client,
    base_url: String,
    timeout: std::time::Duration,
    data_collector: Option<Arc<VisionDataCollector>>,
}

impl MlxUpstream {
    pub fn new(base_url: String) -> Self {
        Self::with_timeout(base_url, std::time::Duration::from_secs(60))
    }

    pub fn with_timeout(base_url: String, timeout: std::time::Duration) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.trim_end_matches('/').to_string(),
            timeout,
            data_collector: None,
        }
    }

    pub fn with_data_collector(mut self, collector: Arc<VisionDataCollector>) -> Self {
        self.data_collector = Some(collector);
        self
    }

    async fn post_json(&self, path: &str, body: &Value) -> Result<Value, UpstreamError> {
        let url = format!("{}{}", self.base_url, path);
        let response = self
            .client
            .post(&url)
            .json(body)
            .timeout(self.timeout)
            .send()
            .await
            .map_err(|e| UpstreamError::Transport(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(UpstreamError::Rejected(format!(
                "mlx server returned {status}: {text}"
            )));
        }

        response
            .json()
            .await
            .map_err(|e| UpstreamError::Invalid(format!("mlx server response parse failed: {e}")))
    }
}

#[async_trait]
impl Upstream for MlxUpstream {
    async fn propose(&self, input: ProposeInput) -> Result<ProposeResponse, UpstreamError> {
        let body = json!({
            "purpose": input.purpose,
            "intentKind": input.intent_kind,
            "stuck": input.stuck,
            "screenshotPng": input.screenshot_png_b64,
            "context": input.context,
        });
        let result = self.do_propose(&input, &body).await;

        if let Some(collector) = &self.data_collector {
            match &result {
                Ok(response) => collector.log_proposal(
                    input.screenshot_png_b64.clone(),
                    &input,
                    Some(response.clone()),
                    None,
                    None,
                    None,
                    None,
                    None,
                    Some(DEFAULT_MODEL.to_string()),
                ),
                Err(_) => collector.log_proposal(
                    input.screenshot_png_b64.clone(),
                    &input,
                    None,
                    None,
                    None,
                    None,
                    Some("upstream error".into()),
                    None,
                    Some(DEFAULT_MODEL.to_string()),
                ),
            }
        }

        result
    }

    async fn extract(&self, input: ExtractInput) -> Result<ExtractResponse, UpstreamError> {
        let body = json!({
            "schema": input.schema,
            "content": input.content,
            "purpose": input.purpose,
        });
        let value = self.post_json("/extract", &body).await?;
        let response: ExtractResponse = serde_json::from_value(value).map_err(|e| {
            UpstreamError::Invalid(format!("extract JSON does not match wire shape: {e}"))
        })?;
        validate_extract(&response).map_err(|e| UpstreamError::Invalid(e.to_string()))?;
        Ok(response)
    }
}

impl MlxUpstream {
    async fn do_propose(
        &self,
        _input: &ProposeInput,
        body: &Value,
    ) -> Result<ProposeResponse, UpstreamError> {
        let value = self.post_json("/propose", body).await?;
        let proposal: ProposeResponse = serde_json::from_value(value).map_err(|e| {
            UpstreamError::Invalid(format!("proposal JSON does not match wire shape: {e}"))
        })?;
        validate_proposal(&proposal).map_err(|e| UpstreamError::Invalid(e.to_string()))?;
        Ok(proposal)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MlxConfigError {
    #[error("VISION_MLX_BASE_URL is missing or empty")]
    MissingBaseUrl,
}

/// Build an MLX upstream from environment variables.
pub fn mlx_upstream_from_env() -> Result<MlxUpstream, MlxConfigError> {
    let base_url =
        std::env::var("VISION_MLX_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
    if base_url.is_empty() {
        return Err(MlxConfigError::MissingBaseUrl);
    }
    Ok(MlxUpstream::new(base_url))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn propose_round_trip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut request = [0_u8; 8192];
            let _ = socket.read(&mut request).await.unwrap();
            let body = br#"{"confidence":0.9,"action":{"kind":"click","x":12.0,"y":34.0}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                String::from_utf8_lossy(body),
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let upstream = MlxUpstream::new(format!("http://{address}"));
        let proposal = upstream
            .propose(ProposeInput {
                purpose: "Continue".into(),
                intent_kind: "locate".into(),
                stuck: "targetMissing".into(),
                screenshot_png_b64: "png".into(),
                context: None,
            })
            .await
            .unwrap();
        assert_eq!(proposal.confidence, 0.9);
        assert!(matches!(
            proposal.action,
            crate::wire::VisionAction::Click { x: 12.0, y: 34.0 }
        ));
    }

    #[tokio::test]
    async fn extract_round_trip() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut request = [0_u8; 8192];
            let _ = socket.read(&mut request).await.unwrap();
            let body = br#"{"value":{"title":"Example"}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                String::from_utf8_lossy(body),
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let upstream = MlxUpstream::new(format!("http://{address}"));
        let response = upstream
            .extract(ExtractInput {
                schema: serde_json::json!({"type": "object"}),
                content: "Example".into(),
                purpose: None,
            })
            .await
            .unwrap();
        assert_eq!(response.value["title"], serde_json::json!("Example"));
    }

    #[test]
    fn env_default_base_url() {
        std::env::remove_var("VISION_MLX_BASE_URL");
        let upstream = mlx_upstream_from_env().unwrap();
        assert_eq!(upstream.base_url, DEFAULT_BASE_URL);
    }
}
