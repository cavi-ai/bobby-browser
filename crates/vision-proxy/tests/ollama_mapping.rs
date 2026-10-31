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

/// The header of a `width` x `height` PNG, base64-encoded.
fn png_b64(width: u32, height: u32) -> String {
    use base64::Engine;
    let mut png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\x0dIHDR".to_vec();
    png.extend_from_slice(&width.to_be_bytes());
    png.extend_from_slice(&height.to_be_bytes());
    png.extend_from_slice(&[8, 6, 0, 0, 0, 0, 0, 0, 0]);
    base64::engine::general_purpose::STANDARD.encode(png)
}

fn propose_input(screenshot_png_b64: String) -> ProposeInput {
    ProposeInput {
        purpose: "find submit".into(),
        intent_kind: "locate".into(),
        stuck: "targetMissing".into(),
        screenshot_png_b64: Some(screenshot_png_b64),
        corpus_screenshot_png_b64: None,
        context: None,
    }
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
        .propose(propose_input(png_b64(1000, 1000)))
        .await
        .unwrap();

    assert_eq!(proposal.confidence, 0.91);
    assert!(matches!(proposal.action, VisionAction::Click { .. }));

    let body = state.last_body.lock().unwrap().clone().unwrap();
    assert_eq!(body["model"], json!("llava"));
    assert_eq!(body["response_format"], json!({ "type": "json_object" }));
}

#[tokio::test]
async fn ollama_scales_grid_clicks_to_screenshot_pixels() {
    let (base_url, _state) = start_mock_ollama().await;
    let upstream = OllamaUpstream::new("llava".into(), base_url);

    // The mock answers {x: 11, y: 22} on the 0-1000 grid.
    let proposal = upstream
        .propose(propose_input(png_b64(1900, 980)))
        .await
        .unwrap();

    match proposal.action {
        VisionAction::Click { x, y } => {
            assert!((x - 20.9).abs() < 1e-9, "x = {x}");
            assert!((y - 21.56).abs() < 1e-9, "y = {y}");
        }
        other => panic!("expected click action, got {other:?}"),
    }
}

#[tokio::test]
async fn ollama_refuses_a_click_it_cannot_scale() {
    let (base_url, _state) = start_mock_ollama().await;
    let upstream = OllamaUpstream::new("llava".into(), base_url);

    let refused = upstream
        .propose(propose_input("aGVsbG8=".into()))
        .await
        .unwrap_err();

    assert!(matches!(refused, UpstreamError::Invalid(_)), "{refused:?}");
}

#[tokio::test]
async fn ollama_normalizes_host_only_and_v1_base_urls_to_same_endpoint() {
    for suffix in ["", "/v1", "/v1/chat/completions"] {
        let (base_url, _state) = start_mock_ollama().await;
        let host = base_url.trim_end_matches("/v1");
        let upstream = OllamaUpstream::new("llava".into(), format!("{host}{suffix}"));
        upstream
            .propose(propose_input(png_b64(1000, 1000)))
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
            screenshot_png_b64: Some("aGVsbG8=".into()),
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
            screenshot_png_b64: Some("aGVsbG8=".into()),
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
