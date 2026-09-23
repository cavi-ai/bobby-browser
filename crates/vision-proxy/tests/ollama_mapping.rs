use std::sync::{Arc, Mutex};

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use serde_json::{json, Value};
use vision_proxy::{OllamaUpstream, ProposeInput, Upstream, UpstreamError, VisionAction};

#[derive(Clone, Default)]
struct MockOllamaState {
    last_body: Arc<Mutex<Option<Value>>>,
}

async fn mock_chat_completions(
    State(state): State<MockOllamaState>,
    Json(body): Json<Value>,
) -> Json<Value> {
    *state.last_body.lock().unwrap() = Some(body.clone());

    let user_text = body["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .find(|message| message["role"] == "user")
                .and_then(extract_user_text)
        })
        .unwrap_or_default();

    let content = if user_text.contains("MALFORMED") {
        "MODEL_MARKER: I cannot safely do that."
    } else {
        r#"{"confidence":0.91,"action":{"kind":"click","x":11.0,"y":22.0}}"#
    };

    Json(json!({
        "choices": [{
            "message": { "content": content }
        }]
    }))
}

fn extract_user_text(message: &Value) -> Option<String> {
    message["content"].as_array().and_then(|parts| {
        parts
            .iter()
            .find(|part| part["type"] == "text")
            .and_then(|part| part["text"].as_str())
            .map(str::to_string)
    })
}

async fn start_mock_ollama() -> (String, MockOllamaState) {
    let state = MockOllamaState::default();
    let app = Router::new()
        .route("/v1/chat/completions", post(mock_chat_completions))
        .with_state(state.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (format!("http://{addr}/v1"), state)
}

async fn start_rejecting_ollama(status: StatusCode, body: &'static str) -> String {
    async fn reject(State((status, body)): State<(StatusCode, &'static str)>) -> impl IntoResponse {
        (status, body)
    }

    let app = Router::new()
        .route("/v1/chat/completions", post(reject))
        .with_state((status, body));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    format!("http://{addr}/v1")
}

#[tokio::test]
async fn ollama_composes_url_once_and_requests_structured_json() {
    let (base_url, state) = start_mock_ollama().await;
    let upstream = OllamaUpstream::new("llava".into(), base_url);

    let proposal = upstream
        .propose(ProposeInput {
            purpose: "find submit".into(),
            intent_kind: "locate".into(),
            stuck: "targetMissing".into(),
            screenshot_png_b64: "aGVsbG8=".into(),
            corpus_screenshot_png_b64: None,
            context: None,
        })
        .await
        .unwrap();

    assert_eq!(proposal.confidence, 0.91);
    match proposal.action {
        VisionAction::Click { x, y } => {
            assert!((x - 11.0).abs() < f64::EPSILON);
            assert!((y - 22.0).abs() < f64::EPSILON);
        }
        other => panic!("expected click action, got {other:?}"),
    }

    let body = state.last_body.lock().unwrap().clone().unwrap();
    assert_eq!(body["model"], json!("llava"));
    assert_eq!(body["response_format"], json!({ "type": "json_object" }));
}

#[tokio::test]
async fn ollama_normalizes_host_only_and_v1_base_urls_to_same_endpoint() {
    for suffix in ["", "/v1", "/v1/chat/completions"] {
        let (base_url, _state) = start_mock_ollama().await;
        let host = base_url.trim_end_matches("/v1");
        let upstream = OllamaUpstream::new("llava".into(), format!("{host}{suffix}"));
        upstream
            .propose(ProposeInput {
                purpose: "find submit".into(),
                intent_kind: "locate".into(),
                stuck: "targetMissing".into(),
                screenshot_png_b64: "aGVsbG8=".into(),
                corpus_screenshot_png_b64: None,
                context: None,
            })
            .await
            .unwrap();
    }
}

#[tokio::test]
async fn malformed_ollama_refusal_is_invalid_model_reply() {
    let (base_url, _state) = start_mock_ollama().await;
    let upstream = OllamaUpstream::new("llava".into(), base_url);

    let err = upstream
        .propose(ProposeInput {
            purpose: "MALFORMED".into(),
            intent_kind: "locate".into(),
            stuck: "targetMissing".into(),
            screenshot_png_b64: "aGVsbG8=".into(),
            corpus_screenshot_png_b64: None,
            context: None,
        })
        .await
        .unwrap_err();

    match err {
        UpstreamError::Invalid(message) => {
            assert!(message.contains("Ollama model reply"), "{message}");
            assert!(!message.contains("MODEL_MARKER"), "{message}");
            assert!(!message.contains("reply_prefix"), "{message}");
        }
        other => panic!("expected invalid model reply, got {other:?}"),
    }
}

#[tokio::test]
async fn upstream_rejection_does_not_include_response_body() {
    let base_url = start_rejecting_ollama(StatusCode::FORBIDDEN, "UPSTREAM_SECRET_BODY").await;
    let upstream = OllamaUpstream::new("llava".into(), base_url);

    let err = upstream
        .propose(ProposeInput {
            purpose: "find submit".into(),
            intent_kind: "locate".into(),
            stuck: "targetMissing".into(),
            screenshot_png_b64: "aGVsbG8=".into(),
            corpus_screenshot_png_b64: None,
            context: None,
        })
        .await
        .unwrap_err();

    match err {
        UpstreamError::Rejected(message) => {
            assert!(message.contains("status=403"), "{message}");
            assert!(message.contains("authentication failed"), "{message}");
            assert!(!message.contains("UPSTREAM_SECRET_BODY"), "{message}");
        }
        other => panic!("expected rejection, got {other:?}"),
    }
}
