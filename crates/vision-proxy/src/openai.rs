use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::upstream::{ExtractInput, ProposeInput, Upstream, UpstreamError};
use crate::validate::{validate_extract, validate_proposal};
use crate::wire::{ExtractResponse, ProposeResponse};

pub const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
pub const DEFAULT_MODEL: &str = "gpt-4o";

const PROPOSE_SYSTEM: &str = "Return only JSON matching bobby propose response: \
{\"confidence\":0..1,\"action\":{\"kind\":\"click\"|\"typeText\"|\"extractValue\",...}}. \
Click coordinates are CSS pixels of the screenshot.";

const EXTRACT_SYSTEM: &str = "Return only JSON {\"value\": <json matching the caller schema>}.";

pub struct OpenAiUpstream {
    client: reqwest::Client,
    api_key: String,
    model: String,
    base_url: String,
}

impl OpenAiUpstream {
    pub fn new(api_key: String, model: String, base_url: String) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key,
            model,
            base_url,
        }
    }

    fn completions_url(&self) -> String {
        let base = self.base_url.trim_end_matches('/');
        format!("{base}/chat/completions")
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
            response_format: ResponseFormat {
                format_type: "json_object",
            },
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
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .map_err(|e| UpstreamError::Transport(e.to_string()))?;

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(UpstreamError::Rejected(format!(
                "OpenAI returned {status}: {text}"
            )));
        }

        let completion: ChatCompletion = response
            .json()
            .await
            .map_err(|e| UpstreamError::Invalid(format!("OpenAI response parse failed: {e}")))?;

        let content = completion
            .choices
            .into_iter()
            .next()
            .and_then(|c| c.message.content)
            .ok_or_else(|| UpstreamError::Invalid("OpenAI returned no message content".into()))?;

        parse_json_content(&content)
    }
}

#[async_trait]
impl Upstream for OpenAiUpstream {
    async fn propose(&self, input: ProposeInput) -> Result<ProposeResponse, UpstreamError> {
        let user_text = format!(
            "purpose: {}\nintentKind: {}\nstuck: {}",
            input.purpose, input.intent_kind, input.stuck
        );
        let value = self
            .chat_json(PROPOSE_SYSTEM, &user_text, Some(&input.screenshot_png_b64))
            .await?;
        let proposal: ProposeResponse = serde_json::from_value(value).map_err(|e| {
            UpstreamError::Invalid(format!("proposal JSON does not match wire shape: {e}"))
        })?;
        validate_proposal(&proposal).map_err(|e| UpstreamError::Invalid(e.to_string()))?;
        Ok(proposal)
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
    response_format: ResponseFormat,
    messages: Vec<ChatMessage<'a>>,
}

#[derive(Serialize)]
struct ResponseFormat {
    #[serde(rename = "type")]
    format_type: &'static str,
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

#[derive(Debug, thiserror::Error)]
pub enum OpenAiConfigError {
    #[error("OPENAI_API_KEY is missing or empty")]
    MissingApiKey,
}

pub fn openai_upstream_from_env() -> Result<OpenAiUpstream, OpenAiConfigError> {
    let api_key = std::env::var("OPENAI_API_KEY").map_err(|_| OpenAiConfigError::MissingApiKey)?;
    if api_key.is_empty() {
        return Err(OpenAiConfigError::MissingApiKey);
    }
    let model = std::env::var("OPENAI_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
    let base_url =
        std::env::var("OPENAI_BASE_URL").unwrap_or_else(|_| DEFAULT_BASE_URL.to_string());
    Ok(OpenAiUpstream::new(api_key, model, base_url))
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
}
