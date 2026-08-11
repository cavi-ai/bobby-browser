use std::sync::Arc;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::data_collector::VisionDataCollector;
use crate::upstream::{ExtractInput, ProposeInput, Upstream, UpstreamError};
use crate::validate::{validate_extract, validate_proposal};
use crate::wire::{ExtractResponse, ProposeResponse};

/// Default Ollama endpoint.
pub const DEFAULT_BASE_URL: &str = "http://127.0.0.1:11434";
pub const DEFAULT_MODEL: &str = "llava:7b";

const PROPOSE_SYSTEM: &str = "You are a vision assistant for a browser automation agent. Return ONLY valid JSON. When candidates are listed, select only by zero-based index: clickCandidate for locate/submitAndVerify/follow/dismissObstruction, typeIntoCandidate for fill/type, extractFromCandidate for extract. Candidate actions contain only kind and index; never emit typed or extracted values. Without candidates, click/typeText/extractValue remain supported. Click coordinates are CSS pixels relative to the screenshot image. Do not include markdown fences, comments, or text outside JSON.";

const EXTRACT_SYSTEM: &str = "Return only JSON {\"value\": <json matching the caller schema>}.";

/// Ollama-compatible upstream that talks to a local Ollama instance.
pub struct OllamaUpstream {
    client: reqwest::Client,
    model: String,
    base_url: String,
    data_collector: Option<Arc<VisionDataCollector>>,
}

impl OllamaUpstream {
    pub fn new(model: String, base_url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            model,
            base_url,
            data_collector: None,
        }
    }

    pub fn with_data_collector(mut self, collector: Arc<VisionDataCollector>) -> Self {
        self.data_collector = Some(collector);
        self
    }

    fn completions_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{base}/v1/chat/completions")
    }

    async fn chat_json(
        &self,
        system: &str,
        user_text: &str,
        image_b64: Option<&str>,
    ) -> Result<Value, UpstreamError> {
        let user_content = if let Some(b64) = image_b64 {
            vec![
                json!({ "type": "text", "text": user_text }),
                json!({
                    "type": "image_url",
                    "image_url": {
                        "url": format!("data:image/png;base64,{b64}")
                    }
                }),
            ]
        } else {
            vec![json!({ "type": "text", "text": user_text })]
        };

        let body = ChatRequest {
            model: &self.model,
            messages: vec![
                ChatMessage {
                    role: "system",
                    content: MessageContent::Text(system),
                },
                ChatMessage {
                    role: "user",
                    content: MessageContent::Parts(user_content),
                },
            ],
        };

        let response = self
            .client
            .post(self.completions_url())
            .json(&body)
            .timeout(std::time::Duration::from_secs(60))
            .send()
            .await
            .map_err(|e| UpstreamError::Transport(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(UpstreamError::Rejected(format!(
                "Ollama returned {status}: {text}"
            )));
        }

        let completion: ChatCompletion = response
            .json()
            .await
            .map_err(|e| UpstreamError::Invalid(format!("Ollama response parse failed: {e}")))?;

        let content = completion
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .ok_or_else(|| UpstreamError::Invalid("Ollama returned no message content".into()))?;

        parse_json_content(&content)
    }
}

#[async_trait]
impl Upstream for OllamaUpstream {
    async fn propose(&self, input: ProposeInput) -> Result<ProposeResponse, UpstreamError> {
        let result = self.do_propose(&input).await;

        // Log for training data collection
        if let Some(collector) = &self.data_collector {
            match &result {
                Ok(response) => {
                    collector.log_proposal(
                        input.screenshot_png_b64.clone(),
                        &input,
                        Some(response.clone()),
                        None, // journey — set by runtime
                        None, // step — set by runtime
                        None, // success — set by runtime
                        None, // error_message — set by runtime
                        None, // run_id — set by runtime
                        Some(self.model.clone()),
                    );
                }
                Err(_) => {
                    collector.log_proposal(
                        input.screenshot_png_b64.clone(),
                        &input,
                        None,
                        None,
                        None,
                        None,
                        Some("upstream error".into()),
                        None,
                        Some(self.model.clone()),
                    );
                }
            }
        }

        result
    }

    async fn extract(&self, input: ExtractInput) -> Result<ExtractResponse, UpstreamError> {
        let schema = serde_json::to_string(&input.schema)
            .map_err(|e| UpstreamError::Invalid(format!("schema serialization failed: {e}")))?;
        let purpose = input.purpose.as_deref().unwrap_or("");
        let user_text = format!(
            "extract structured value from page content.\npurpose: {purpose}\nschema: {schema}\ncontent:\n{}",
            input.content
        );
        let value = self.chat_json(EXTRACT_SYSTEM, &user_text, None).await?;
        let response: ExtractResponse = serde_json::from_value(value).map_err(|e| {
            UpstreamError::Invalid(format!("extract JSON does not match wire shape: {e}"))
        })?;
        validate_extract(&response).map_err(|e| UpstreamError::Invalid(e.to_string()))?;
        Ok(response)
    }
}

impl OllamaUpstream {
    async fn do_propose(&self, input: &ProposeInput) -> Result<ProposeResponse, UpstreamError> {
        let mut user_text = format!(
            "purpose: {}\nintentKind: {}\nstuck: {}",
            input.purpose, input.intent_kind, input.stuck
        );
        if let Some(context) = &input.context {
            if let Some(url) = &context.url {
                user_text.push_str(&format!("\nurl: {url}"));
            }
            if !context.candidates.is_empty() {
                user_text.push_str("\ncandidates:");
                for candidate in &context.candidates {
                    user_text.push_str(&format!(
                        "\n- {} \"{}\"{}",
                        candidate.role,
                        candidate.name,
                        candidate
                            .ordinal
                            .map(|ordinal| format!(" (#{ordinal})"))
                            .unwrap_or_default()
                    ));
                }
            }
            if !context.recent_command_kinds.is_empty() {
                user_text.push_str(&format!(
                    "\nrecentCommands: {}",
                    context.recent_command_kinds.join(", ")
                ));
            }
        }
        let value = self
            .chat_json(PROPOSE_SYSTEM, &user_text, Some(&input.screenshot_png_b64))
            .await?;
        let proposal: ProposeResponse = serde_json::from_value(value).map_err(|e| {
            UpstreamError::Invalid(format!("proposal JSON does not match wire shape: {e}"))
        })?;
        validate_proposal(&proposal).map_err(|e| UpstreamError::Invalid(e.to_string()))?;
        Ok(proposal)
    }
}

fn parse_json_content(content: &str) -> Result<Value, UpstreamError> {
    let trimmed = content.trim();
    let json_str = strip_markdown_fences(trimmed);
    serde_json::from_str(json_str)
        .map_err(|e| UpstreamError::Invalid(format!("model content is not valid JSON: {e}")))
}

fn strip_markdown_fences(s: &str) -> &str {
    let s = s.strip_prefix("```json").unwrap_or(s);
    let s = s.strip_prefix("```").unwrap_or(s);
    s.strip_suffix("```").unwrap_or(s).trim()
}

#[derive(Serialize)]
struct ChatRequest<'a> {
    model: &'a str,
    messages: Vec<ChatMessage<'a>>,
}

#[derive(Serialize)]
struct ChatMessage<'a> {
    role: &'a str,
    content: MessageContent<'a>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum MessageContent<'a> {
    Text(&'a str),
    Parts(Vec<Value>),
}

#[derive(Deserialize)]
struct ChatCompletion {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatChoiceMessage,
}

#[derive(Deserialize)]
struct ChatChoiceMessage {
    content: Option<String>,
}

/// Build an Ollama upstream from environment variables.
///
/// Required env vars:
/// - `VISION_OLLAMA_MODEL` (optional, defaults to `llava:7b`)
/// - `VISION_OLLAMA_BASE_URL` (optional, defaults to `http://127.0.0.1:11434`)
pub fn ollama_upstream_from_env() -> Result<OllamaUpstream, OllamaConfigError> {
    let model = std::env::var("VISION_OLLAMA_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    if model.is_empty() {
        return Err(OllamaConfigError::MissingModel);
    }
    let base_url =
        std::env::var("VISION_OLLAMA_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
    Ok(OllamaUpstream::new(model, base_url))
}

#[derive(Debug, thiserror::Error)]
pub enum OllamaConfigError {
    #[error("VISION_OLLAMA_MODEL is missing or empty")]
    MissingModel,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_fences_from_json_block() {
        let raw = "```json\n{\"a\":1}\n```";
        assert_eq!(strip_markdown_fences(raw), "{\"a\":1}");
    }

    #[test]
    fn parse_raw_json_content() {
        let v =
            parse_json_content(r#"{"confidence":0.5,"action":{"kind":"click","x":1.0,"y":2.0}}"#)
                .unwrap();
        assert_eq!(v["confidence"], serde_json::json!(0.5));
    }

    #[test]
    fn prompt_requires_index_only_intent_compatible_candidate_actions() {
        for required in [
            "clickCandidate",
            "typeIntoCandidate",
            "extractFromCandidate",
            "zero-based index",
            "never emit typed or extracted values",
        ] {
            assert!(PROPOSE_SYSTEM.contains(required), "missing {required}");
        }
    }
}
