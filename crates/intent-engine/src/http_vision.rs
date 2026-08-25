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
    Click {
        x: f64,
        y: f64,
    },
    TypeText {
        text: String,
    },
    ExtractValue {
        value: String,
    },
    ClickCandidate {
        index: u32,
    },
    TypeIntoCandidate {
        index: u32,
    },
    ExtractFromCandidate {
        index: u32,
    },
    ChallengeSolved,
    /// Stringly on the wire; mapped to the typed `ChallengeType` below with
    /// an unknown kind failing the proposal rather than coercing.
    ChallengeDetected {
        challenge_type: String,
        region: Option<RegionBody>,
        #[serde(default)]
        blocking: bool,
    },
    NoChallengeDetected,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct RegionBody {
    x: f32,
    y: f32,
    width: f32,
    height: f32,
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
    fn provider_mode(&self) -> observability::ProviderMode {
        observability::ProviderMode::Http
    }

    async fn propose(&self, request: VisionProposeRequest) -> Result<VisionProposal, CommandError> {
        let body = ProposeBody {
            purpose: &request.purpose,
            intent_kind: &request.intent_kind,
            stuck: stuck_name(request.stuck),
            screenshot_png: BASE64.encode(&request.screenshot_png),
            context: request.context.clone(),
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
            ActionBody::TypeIntoCandidate { index } => {
                validate_candidate_action(&request, index, "typeIntoCandidate")?;
                VisionAction::TypeIntoCandidate { index }
            }
            ActionBody::ExtractFromCandidate { index } => {
                validate_candidate_action(&request, index, "extractFromCandidate")?;
                VisionAction::ExtractFromCandidate { index }
            }
            ActionBody::ChallengeSolved if request.intent_kind == "solveChallenge" => {
                VisionAction::ChallengeSolved
            }
            ActionBody::ChallengeSolved => {
                return Err(provider_error(format!(
                    "vision challengeSolved action is incompatible with intent {}",
                    request.intent_kind
                )));
            }
            ActionBody::ChallengeDetected {
                challenge_type,
                region,
                blocking,
            } if request.intent_kind == "detectChallenge" => {
                let challenge_type: types::ChallengeType =
                    serde_json::from_value(serde_json::Value::String(challenge_type))
                        .map_err(|_| provider_error("vision detected an unknown challenge type"))?;
                let region = region
                    .map(|region| {
                        if region.x.is_finite()
                            && region.y.is_finite()
                            && region.width.is_finite()
                            && region.height.is_finite()
                        {
                            Ok(types::ChallengeRegion {
                                x: region.x,
                                y: region.y,
                                width: region.width,
                                height: region.height,
                            })
                        } else {
                            Err(provider_error("vision challenge region is not finite"))
                        }
                    })
                    .transpose()?;
                VisionAction::ChallengeDetected {
                    challenge_type,
                    region,
                    blocking,
                }
            }
            ActionBody::ChallengeDetected { .. } => {
                return Err(provider_error(format!(
                    "vision challengeDetected action is incompatible with intent {}",
                    request.intent_kind
                )));
            }
            ActionBody::NoChallengeDetected if request.intent_kind == "detectChallenge" => {
                VisionAction::NoChallengeDetected
            }
            ActionBody::NoChallengeDetected => {
                return Err(provider_error(format!(
                    "vision noChallengeDetected action is incompatible with intent {}",
                    request.intent_kind
                )));
            }
        };
        Ok(VisionProposal {
            confidence: proposal.confidence,
            action,
        })
    }
}

fn validate_candidate_action(
    request: &VisionProposeRequest,
    index: u32,
    action_kind: &str,
) -> Result<(), CommandError> {
    let compatible = match action_kind {
        "typeIntoCandidate" => matches!(request.intent_kind.as_str(), "fill" | "type"),
        "extractFromCandidate" => request.intent_kind == "extract",
        _ => false,
    };
    if !compatible {
        return Err(provider_error(format!(
            "vision {action_kind} action is incompatible with intent {}",
            request.intent_kind
        )));
    }
    crate::vision::validate_candidate_index(request.context.as_ref(), index)
        .map_err(|error| provider_error(error.to_string()))
}

fn stuck_name(stuck: StuckKind) -> &'static str {
    match stuck {
        StuckKind::TargetMissing => "targetMissing",
        StuckKind::TargetAmbiguous => "targetAmbiguous",
        StuckKind::ObstructionSuspected => "obstructionSuspected",
        StuckKind::VerifyNoDomSignal => "verifyNoDomSignal",
        StuckKind::ChallengePresent => "challengePresent",
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

    fn candidate_context(count: usize) -> crate::VisionPromptContext {
        crate::VisionPromptContext {
            url: Some("https://example.test/form".into()),
            candidates: (0..count)
                .map(|index| crate::VisionPromptCandidate {
                    role: "textbox".into(),
                    name: format!("Field {index}"),
                    ordinal: Some(index as u32),
                })
                .collect(),
            recent_command_kinds: vec!["fill".into()],
        }
    }

    async fn propose_candidate_action(
        body: &'static str,
        intent_kind: &str,
        context: Option<crate::VisionPromptContext>,
    ) -> Result<VisionProposal, CommandError> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut request = [0_u8; 8192];
            let _ = socket.read(&mut request).await.unwrap();
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body,
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });
        let assist = HttpVisionAssist::new(
            format!("http://{address}/propose"),
            None,
            std::time::Duration::from_secs(5),
        )
        .unwrap();
        assist
            .propose(VisionProposeRequest {
                purpose: "fill or extract a field".into(),
                intent_kind: intent_kind.into(),
                screenshot_png: b"png".to_vec(),
                stuck: StuckKind::TargetMissing,
                context,
            })
            .await
    }

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

    #[tokio::test]
    async fn candidate_actions_require_a_bounded_request_context() {
        for (body, intent_kind, context) in [
            (
                r#"{"confidence":0.9,"action":{"kind":"typeIntoCandidate","index":0}}"#,
                "fill",
                None,
            ),
            (
                r#"{"confidence":0.9,"action":{"kind":"extractFromCandidate","index":0}}"#,
                "extract",
                Some(candidate_context(0)),
            ),
            (
                r#"{"confidence":0.9,"action":{"kind":"typeIntoCandidate","index":1}}"#,
                "type",
                Some(candidate_context(1)),
            ),
        ] {
            let error = propose_candidate_action(body, intent_kind, context)
                .await
                .expect_err("candidate actions need an in-range prompt candidate");
            assert_eq!(error.code, ErrorCode::VisionAssistFailed);
        }
    }

    #[tokio::test]
    async fn candidate_actions_map_only_compatible_intents() {
        let type_for_fill = propose_candidate_action(
            r#"{"confidence":0.9,"action":{"kind":"typeIntoCandidate","index":0}}"#,
            "fill",
            Some(candidate_context(1)),
        )
        .await
        .unwrap();
        assert!(matches!(
            type_for_fill.action,
            VisionAction::TypeIntoCandidate { index: 0 }
        ));

        let type_for_type = propose_candidate_action(
            r#"{"confidence":0.9,"action":{"kind":"typeIntoCandidate","index":0}}"#,
            "type",
            Some(candidate_context(1)),
        )
        .await
        .unwrap();
        assert!(matches!(
            type_for_type.action,
            VisionAction::TypeIntoCandidate { index: 0 }
        ));

        let extract = propose_candidate_action(
            r#"{"confidence":0.9,"action":{"kind":"extractFromCandidate","index":0}}"#,
            "extract",
            Some(candidate_context(1)),
        )
        .await
        .unwrap();
        assert!(matches!(
            extract.action,
            VisionAction::ExtractFromCandidate { index: 0 }
        ));
    }

    #[tokio::test]
    async fn candidate_actions_reject_incompatible_intents() {
        for (body, intent_kind) in [
            (
                r#"{"confidence":0.9,"action":{"kind":"typeIntoCandidate","index":0}}"#,
                "extract",
            ),
            (
                r#"{"confidence":0.9,"action":{"kind":"extractFromCandidate","index":0}}"#,
                "fill",
            ),
        ] {
            let error = propose_candidate_action(body, intent_kind, Some(candidate_context(1)))
                .await
                .expect_err("candidate action must match the intent kind");
            assert_eq!(error.code, ErrorCode::VisionAssistFailed);
        }
    }
}
