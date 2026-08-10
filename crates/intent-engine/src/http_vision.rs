use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use types::{CommandError, ErrorCode, ErrorLayer};

use crate::{StuckKind, VisionAction, VisionAssist, VisionProposal, VisionProposeRequest};

const MAX_RESPONSE_BYTES: usize = 64 * 1024;
const MAX_TEXT_BYTES: usize = 4096;

/// Vision-assist provider over HTTP. The endpoint receives the intent
/// context plus a base64 PNG and returns a confidence-scored action.
/// Any transport, schema, or confidence-contract failure raises
/// `VisionAssistFailed`, which declines the escalation rather than
/// acting on an unverifiable proposal.
pub struct HttpVisionAssist {
    client: reqwest::Client,
    endpoint: String,
    bearer: Option<String>,
    timeout: std::time::Duration,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ProposeBody<'a> {
    purpose: &'a str,
    intent_kind: &'a str,
    stuck: &'a str,
    screenshot_png: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    context: Option<crate::VisionPromptContext>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ProposalBody {
    confidence: f32,
    action: ActionBody,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
enum ActionBody {
    Click { x: f64, y: f64 },
    TypeText { text: String },
    ExtractValue { value: String },
    ClickCandidate { index: u32 },
}

impl HttpVisionAssist {
    pub fn new(
        endpoint: String,
        bearer: Option<String>,
        timeout: std::time::Duration,
    ) -> Result<Self, CommandError> {
        let parsed = url::Url::parse(&endpoint)
            .map_err(|_| provider_error("vision endpoint URL is invalid"))?;
        let loopback = matches!(
            parsed.host_str(),
            Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
        );
        let secure = parsed.scheme() == "https";
        let allowed = secure || (parsed.scheme() == "http" && loopback);
        if !allowed {
            return Err(provider_error(
                "vision endpoint must be https, or http only on loopback",
            ));
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .map_err(|_| provider_error("vision HTTP client failed to initialize"))?;
        Ok(Self {
            client,
            endpoint,
            bearer,
            timeout,
        })
    }
}

/// Structured extraction over the same provider endpoint: the page's
/// bounded text content plus the caller's JSON schema go out; the provider
/// returns one JSON value that is then validated against the schema by the
/// runtime before it becomes evidence.
#[async_trait]
pub trait StructuredExtractor: Send + Sync {
    async fn extract_structured(
        &self,
        request: StructuredExtractRequest,
    ) -> Result<Value, CommandError>;
}

pub struct StructuredExtractRequest {
    pub schema: Value,
    pub content: String,
    pub purpose: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ExtractBody<'a> {
    schema: &'a Value,
    content: &'a str,
    purpose: &'a Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExtractResultBody {
    value: Value,
}

#[async_trait]
impl StructuredExtractor for HttpVisionAssist {
    async fn extract_structured(
        &self,
        request: StructuredExtractRequest,
    ) -> Result<Value, CommandError> {
        let body = ExtractBody {
            schema: &request.schema,
            content: &request.content,
            purpose: &request.purpose,
        };
        let mut call = self
            .client
            .post(&self.endpoint)
            .timeout(self.timeout)
            .json(&body);
        if let Some(bearer) = &self.bearer {
            call = call.bearer_auth(bearer);
        }
        let response = call
            .send()
            .await
            .map_err(|_| provider_error("extract endpoint request failed"))?;
        if !response.status().is_success() {
            return Err(provider_error("extract endpoint rejected the request"));
        }
        let bytes = read_bounded_body(response, "extract").await?;
        let result: ExtractResultBody = serde_json::from_slice(&bytes)
            .map_err(|_| provider_error("extract endpoint returned an invalid result"))?;
        Ok(result.value)
    }
}

#[async_trait]
impl VisionAssist for HttpVisionAssist {
    async fn propose(&self, request: VisionProposeRequest) -> Result<VisionProposal, CommandError> {
        let body = ProposeBody {
            purpose: &request.purpose,
            intent_kind: &request.intent_kind,
            stuck: stuck_name(request.stuck),
            screenshot_png: BASE64.encode(request.screenshot_png),
            context: request.context,
        };
        let mut call = self
            .client
            .post(&self.endpoint)
            .timeout(self.timeout)
            .json(&body);
        if let Some(bearer) = &self.bearer {
            call = call.bearer_auth(bearer);
        }
        let response = call
            .send()
            .await
            .map_err(|_| provider_error("vision endpoint request failed"))?;
        if !response.status().is_success() {
            return Err(provider_error(
                "vision endpoint rejected the proposal request",
            ));
        }
        let bytes = read_bounded_body(response, "vision").await?;
        let proposal: ProposalBody = serde_json::from_slice(&bytes)
            .map_err(|_| provider_error("vision endpoint returned an invalid proposal"))?;
        if !proposal.confidence.is_finite() || !(0.0..=1.0).contains(&proposal.confidence) {
            return Err(provider_error("vision proposal confidence is out of range"));
        }
        let action = match proposal.action {
            ActionBody::Click { x, y } if x.is_finite() && y.is_finite() => {
                VisionAction::Click { x, y }
            }
            ActionBody::Click { .. } => {
                return Err(provider_error("vision click coordinates are not finite"));
            }
            ActionBody::TypeText { text } if text.len() <= MAX_TEXT_BYTES => {
                VisionAction::TypeText { text }
            }
            ActionBody::TypeText { .. } => {
                return Err(provider_error("vision type text exceeded its bound"));
            }
            ActionBody::ExtractValue { value } if value.len() <= MAX_TEXT_BYTES => {
                VisionAction::ExtractValue { value }
            }
            ActionBody::ExtractValue { .. } => {
                return Err(provider_error("vision extract value exceeded its bound"));
            }
            ActionBody::ClickCandidate { index } => VisionAction::ClickCandidate { index },
        };
        Ok(VisionProposal {
            confidence: proposal.confidence,
            action,
        })
    }
}

fn stuck_name(stuck: StuckKind) -> &'static str {
    match stuck {
        StuckKind::TargetMissing => "targetMissing",
        StuckKind::TargetAmbiguous => "targetAmbiguous",
        StuckKind::ObstructionSuspected => "obstructionSuspected",
        StuckKind::VerifyNoDomSignal => "verifyNoDomSignal",
    }
}

/// Read a response body with the bound enforced DURING the read: content
/// longer than MAX_RESPONSE_BYTES must fail the call, not exhaust runtime
/// memory before a length check ever runs.
async fn read_bounded_body(
    response: reqwest::Response,
    what: &str,
) -> Result<Vec<u8>, CommandError> {
    if response
        .content_length()
        .is_some_and(|length| length > MAX_RESPONSE_BYTES as u64)
    {
        return Err(provider_error(format!(
            "{what} endpoint response exceeded its bound"
        )));
    }
    let mut bytes = Vec::new();
    let mut response = response;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| provider_error(format!("{what} endpoint response could not be read")))?
    {
        if bytes.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(provider_error(format!(
                "{what} endpoint response exceeded its bound"
            )));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(bytes)
}

fn provider_error(message: impl Into<String>) -> CommandError {
    CommandError {
        code: ErrorCode::VisionAssistFailed,
        message: message.into(),
        layer: ErrorLayer::Driver,
        retryable: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::StructuredExtractor;

    #[test]
    fn endpoint_must_be_https_or_loopback() {
        assert!(HttpVisionAssist::new(
            "http://vision.example.test/propose".into(),
            None,
            std::time::Duration::from_secs(1),
        )
        .is_err());
        assert!(HttpVisionAssist::new(
            "https://vision.example.test/propose".into(),
            None,
            std::time::Duration::from_secs(1),
        )
        .is_ok());
        assert!(HttpVisionAssist::new(
            "http://127.0.0.1:9100/propose".into(),
            None,
            std::time::Duration::from_secs(1),
        )
        .is_ok());
        assert!(
            HttpVisionAssist::new("not-a-url".into(), None, std::time::Duration::from_secs(1))
                .is_err()
        );
    }

    #[tokio::test]
    async fn extract_round_trip_returns_the_provider_value() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut request = [0_u8; 8192];
            let _ = socket.read(&mut request).await.unwrap();
            let body = br#"{"value":{"title":"Example Domain","count":2}}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                String::from_utf8_lossy(body),
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let assist = HttpVisionAssist::new(
            format!("http://{address}/extract"),
            None,
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        let value = assist
            .extract_structured(StructuredExtractRequest {
                schema: serde_json::json!({"type":"object"}),
                content: "Example Domain".into(),
                purpose: Some("page fields".into()),
            })
            .await
            .unwrap();
        assert_eq!(value["title"], serde_json::json!("Example Domain"));
        assert_eq!(value["count"], serde_json::json!(2));
    }

    #[tokio::test]
    async fn proposal_round_trip_and_confidence_validation() {
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
        let assist = HttpVisionAssist::new(
            format!("http://{address}/propose"),
            None,
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        let proposal = assist
            .propose(VisionProposeRequest {
                purpose: "Continue".into(),
                intent_kind: "locate".into(),
                screenshot_png: b"png".to_vec(),
                stuck: StuckKind::TargetMissing,
                context: None,
            })
            .await
            .unwrap();
        assert_eq!(proposal.confidence, 0.9);
        assert!(matches!(
            proposal.action,
            VisionAction::Click { x: 12.0, y: 34.0 }
        ));
    }
}
