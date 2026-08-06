use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use vision_proxy::{
    ExtractInput, OpenAiUpstream, ProposeInput, Upstream, UpstreamError, VisionAction,
};

#[derive(Clone, Default)]
struct MockOpenAiState {
    last_body: Arc<Mutex<Option<Value>>>,
    had_authorization: Arc<Mutex<bool>>,
}

async fn mock_chat_completions(
    State(state): State<MockOpenAiState>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    *state.last_body.lock().unwrap() = Some(body.clone());
    *state.had_authorization.lock().unwrap() = headers.contains_key("authorization");

    let user_text = body["messages"]
        .as_array()
        .and_then(|msgs| {
            msgs.iter()
                .find(|m| m["role"] == "user")
                .and_then(extract_user_text)
        })
        .unwrap_or_default();

    let content = if user_text.contains("MALFORMED") {
        "not valid json at all"
    } else if user_text.contains("extract structured value") {
        r#"{"value":{"title":"x"}}"#
    } else {
        r#"{"confidence":0.88,"action":{"kind":"click","x":42.0,"y":84.0}}"#
    };

    Json(json!({
        "choices": [{
            "message": { "content": content }
        }]
    }))
}

fn extract_user_text(message: &Value) -> Option<String> {
    if let Some(text) = message["content"].as_str() {
        return Some(text.to_string());
    }
    message["content"].as_array().and_then(|parts| {
        parts
            .iter()
            .find(|p| p["type"] == "text")
            .and_then(|p| p["text"].as_str())
            .map(str::to_string)
    })
}

async fn start_mock_openai() -> (String, MockOpenAiState) {
    let state = MockOpenAiState::default();
    let app = Router::new()
        .route("/chat/completions", post(mock_chat_completions))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}"), state)
}

#[tokio::test]
async fn propose_posts_expected_openai_body_and_maps_response() {
    let (base_url, state) = start_mock_openai().await;
    let upstream = OpenAiUpstream::new("test-key".into(), "gpt-4o".into(), base_url);

    let proposal = upstream
        .propose(ProposeInput {
            purpose: "find submit".into(),
            intent_kind: "click".into(),
            stuck: "targetMissing".into(),
            screenshot_png_b64: "aGVsbG8=".into(),
            context: None,
        })
        .await
        .unwrap();

    assert_eq!(proposal.confidence, 0.88);
    match proposal.action {
        VisionAction::Click { x, y } => {
            assert!((x - 42.0).abs() < f64::EPSILON);
            assert!((y - 84.0).abs() < f64::EPSILON);
        }
        _ => panic!("expected click action"),
    }

    let body = state
        .last_body
        .lock()
        .unwrap()
        .clone()
        .expect("request captured");
    assert_eq!(body["model"], json!("gpt-4o"));
    assert_eq!(body["response_format"], json!({ "type": "json_object" }));

    let messages = body["messages"].as_array().expect("messages array");
    let user = messages
        .iter()
        .find(|m| m["role"] == "user")
        .expect("user message");
    let content = user["content"].as_array().expect("multimodal content");
    let image_part = content
        .iter()
        .find(|p| p["type"] == "image_url")
        .expect("image_url part");
    let url = image_part["image_url"]["url"]
        .as_str()
        .expect("image data url");
    assert!(
        url.starts_with("data:image/png;base64,"),
        "expected png data url, got {url}"
    );
    assert!(url.contains("aGVsbG8="));
}

#[tokio::test]
async fn malformed_model_json_returns_invalid() {
    let (base_url, _state) = start_mock_openai().await;
    let upstream = OpenAiUpstream::new("test-key".into(), "gpt-4o".into(), base_url);

    let err = upstream
        .propose(ProposeInput {
            purpose: "MALFORMED".into(),
            intent_kind: "click".into(),
            stuck: "targetMissing".into(),
            screenshot_png_b64: "abc".into(),
            context: None,
        })
        .await
        .unwrap_err();

    assert!(matches!(err, UpstreamError::Invalid(_)));
}

#[tokio::test]
async fn empty_api_key_omits_authorization_header() {
    let (base_url, state) = start_mock_openai().await;
    let upstream = OpenAiUpstream::new(String::new(), "gpt-4o".into(), base_url);

    upstream
        .propose(ProposeInput {
            purpose: "find submit".into(),
            intent_kind: "click".into(),
            stuck: "targetMissing".into(),
            screenshot_png_b64: "aGVsbG8=".into(),
            context: None,
        })
        .await
        .unwrap();

    assert!(
        !*state.had_authorization.lock().unwrap(),
        "empty api key must omit Authorization header"
    );
}

#[tokio::test]
async fn extract_maps_value_from_model_json() {
    let (base_url, state) = start_mock_openai().await;
    let upstream = OpenAiUpstream::new("test-key".into(), "gpt-4o".into(), base_url);

    let response = upstream
        .extract(ExtractInput {
            schema: json!({"type": "object", "properties": {"title": {"type": "string"}}}),
            content: "page text".into(),
            purpose: Some("extract title".into()),
        })
        .await
        .unwrap();

    assert_eq!(response.value, json!({"title": "x"}));

    let body = state
        .last_body
        .lock()
        .unwrap()
        .clone()
        .expect("request captured");
    assert_eq!(body["model"], json!("gpt-4o"));
    assert_eq!(body["response_format"], json!({ "type": "json_object" }));
}

#[tokio::test]
async fn propose_renders_the_context_block_into_the_prompt() {
    let (base_url, state) = start_mock_openai().await;
    let upstream = OpenAiUpstream::new("test-key".into(), "gpt-4o".into(), base_url);

    upstream
        .propose(ProposeInput {
            purpose: "find email field".into(),
            intent_kind: "fill".into(),
            stuck: "targetMissing".into(),
            screenshot_png_b64: "aGVsbG8=".into(),
            context: Some(vision_proxy::wire::ProposeContext {
                url: Some("https://example.test/signup".into()),
                candidates: vec![
                    vision_proxy::wire::ProposeContextCandidate {
                        role: "textbox".into(),
                        name: "Email address".into(),
                        ordinal: Some(1),
                    },
                    vision_proxy::wire::ProposeContextCandidate {
                        role: "button".into(),
                        name: "Continue".into(),
                        ordinal: None,
                    },
                ],
                recent_command_kinds: vec!["navigate".into(), "fill".into()],
            }),
        })
        .await
        .unwrap();

    let body = state
        .last_body
        .lock()
        .unwrap()
        .clone()
        .expect("request captured");
    let messages = body["messages"].as_array().expect("messages array");
    let user = messages
        .iter()
        .find(|m| m["role"] == "user")
        .expect("user message");
    let text = user["content"]
        .as_array()
        .expect("multimodal content")
        .iter()
        .find(|p| p["type"] == "text")
        .expect("text part")["text"]
        .as_str()
        .unwrap()
        .to_string();
    assert!(text.contains("purpose: find email field"), "{text}");
    assert!(text.contains("url: https://example.test/signup"), "{text}");
    assert!(text.contains("candidates:"), "{text}");
    assert!(text.contains("- textbox \"Email address\" (#1)"), "{text}");
    assert!(text.contains("- button \"Continue\""), "{text}");
    assert!(text.contains("recentCommands: navigate, fill"), "{text}");
}
