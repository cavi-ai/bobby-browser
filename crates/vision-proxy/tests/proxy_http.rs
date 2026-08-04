use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::Router;
use tower::ServiceExt;
use vision_proxy::{
    router, AppState, ExtractInput, ExtractResponse, ProposeInput, ProposeResponse, Upstream,
    UpstreamError, VisionAction,
};

struct MockUpstream {
    propose_calls: AtomicUsize,
    extract_calls: AtomicUsize,
    proposal: Mutex<Option<ProposeResponse>>,
    extract_response: Mutex<Option<ExtractResponse>>,
}

impl MockUpstream {
    fn new(proposal: ProposeResponse, extract: ExtractResponse) -> Self {
        Self {
            propose_calls: AtomicUsize::new(0),
            extract_calls: AtomicUsize::new(0),
            proposal: Mutex::new(Some(proposal)),
            extract_response: Mutex::new(Some(extract)),
        }
    }
}

#[async_trait]
impl Upstream for MockUpstream {
    async fn propose(&self, _input: ProposeInput) -> Result<ProposeResponse, UpstreamError> {
        self.propose_calls.fetch_add(1, Ordering::SeqCst);
        self.proposal
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| UpstreamError::Invalid("no proposal configured".into()))
    }

    async fn extract(&self, _input: ExtractInput) -> Result<ExtractResponse, UpstreamError> {
        self.extract_calls.fetch_add(1, Ordering::SeqCst);
        self.extract_response
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| UpstreamError::Invalid("no extract configured".into()))
    }
}

fn test_app(upstream: Arc<MockUpstream>) -> Router {
    router(AppState {
        path: "/vision".to_string(),
        bearer_token: "test-token".to_string(),
        upstream,
    })
}

async fn post(app: &mut Router, auth: Option<&str>, body: &str) -> (StatusCode, String) {
    let mut req = Request::builder()
        .method("POST")
        .uri("/vision")
        .header("content-type", "application/json");
    if let Some(token) = auth {
        req = req.header("authorization", format!("Bearer {token}"));
    }
    let response = app
        .oneshot(req.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    (status, String::from_utf8_lossy(&bytes).into_owned())
}

#[tokio::test]
async fn missing_bearer_returns_401_without_calling_upstream() {
    let upstream = Arc::new(MockUpstream::new(
        ProposeResponse {
            confidence: 0.9,
            action: VisionAction::Click { x: 1.0, y: 2.0 },
        },
        ExtractResponse {
            value: serde_json::json!("ok"),
        },
    ));
    let mut app = test_app(upstream.clone());

    let (status, _) = post(
        &mut app,
        None,
        r#"{"purpose":"p","intentKind":"k","stuck":"s","screenshotPng":"abc"}"#,
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(upstream.propose_calls.load(Ordering::SeqCst), 0);
    assert_eq!(upstream.extract_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn wrong_bearer_returns_401_without_calling_upstream() {
    let upstream = Arc::new(MockUpstream::new(
        ProposeResponse {
            confidence: 0.9,
            action: VisionAction::Click { x: 1.0, y: 2.0 },
        },
        ExtractResponse {
            value: serde_json::json!("ok"),
        },
    ));
    let mut app = test_app(upstream.clone());

    let (status, _) = post(
        &mut app,
        Some("wrong"),
        r#"{"purpose":"p","intentKind":"k","stuck":"s","screenshotPng":"abc"}"#,
    )
    .await;

    assert_eq!(status, StatusCode::UNAUTHORIZED);
    assert_eq!(upstream.propose_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn propose_shaped_body_returns_validated_proposal() {
    let upstream = Arc::new(MockUpstream::new(
        ProposeResponse {
            confidence: 0.85,
            action: VisionAction::Click { x: 10.0, y: 20.0 },
        },
        ExtractResponse {
            value: serde_json::json!(null),
        },
    ));
    let mut app = test_app(upstream.clone());

    let (status, body) = post(
        &mut app,
        Some("test-token"),
        r#"{"purpose":"find button","intentKind":"click","stuck":"none","screenshotPng":"aGVsbG8="}"#,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(upstream.propose_calls.load(Ordering::SeqCst), 1);
    assert_eq!(upstream.extract_calls.load(Ordering::SeqCst), 0);
    let parsed: ProposeResponse = serde_json::from_str(&body).unwrap();
    assert!((parsed.confidence - 0.85).abs() < f32::EPSILON);
    match parsed.action {
        VisionAction::Click { x, y } => {
            assert!((x - 10.0).abs() < f64::EPSILON);
            assert!((y - 20.0).abs() < f64::EPSILON);
        }
        _ => panic!("expected click action"),
    }
}

#[tokio::test]
async fn extract_shaped_body_returns_value() {
    let upstream = Arc::new(MockUpstream::new(
        ProposeResponse {
            confidence: 0.9,
            action: VisionAction::Click { x: 0.0, y: 0.0 },
        },
        ExtractResponse {
            value: serde_json::json!({"title": "Example"}),
        },
    ));
    let mut app = test_app(upstream.clone());

    let (status, body) = post(
        &mut app,
        Some("test-token"),
        r#"{"schema":{"type":"object"},"content":"page text","purpose":"extract title"}"#,
    )
    .await;

    assert_eq!(status, StatusCode::OK);
    assert_eq!(upstream.extract_calls.load(Ordering::SeqCst), 1);
    assert_eq!(upstream.propose_calls.load(Ordering::SeqCst), 0);
    let parsed: ExtractResponse = serde_json::from_str(&body).unwrap();
    assert_eq!(parsed.value, serde_json::json!({"title": "Example"}));
}

#[tokio::test]
async fn body_with_both_shapes_returns_400() {
    let upstream = Arc::new(MockUpstream::new(
        ProposeResponse {
            confidence: 0.9,
            action: VisionAction::Click { x: 0.0, y: 0.0 },
        },
        ExtractResponse {
            value: serde_json::json!(null),
        },
    ));
    let mut app = test_app(upstream.clone());

    let (status, _) = post(
        &mut app,
        Some("test-token"),
        r#"{"screenshotPng":"abc","schema":{"type":"object"},"content":"text"}"#,
    )
    .await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(upstream.propose_calls.load(Ordering::SeqCst), 0);
    assert_eq!(upstream.extract_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn empty_body_returns_400() {
    let upstream = Arc::new(MockUpstream::new(
        ProposeResponse {
            confidence: 0.9,
            action: VisionAction::Click { x: 0.0, y: 0.0 },
        },
        ExtractResponse {
            value: serde_json::json!(null),
        },
    ));
    let mut app = test_app(upstream.clone());

    let (status, _) = post(&mut app, Some("test-token"), "{}").await;

    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert_eq!(upstream.propose_calls.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn bad_confidence_from_upstream_returns_502() {
    let upstream = Arc::new(MockUpstream::new(
        ProposeResponse {
            confidence: 1.5,
            action: VisionAction::Click { x: 1.0, y: 2.0 },
        },
        ExtractResponse {
            value: serde_json::json!(null),
        },
    ));
    let mut app = test_app(upstream.clone());

    let (status, body) = post(
        &mut app,
        Some("test-token"),
        r#"{"purpose":"p","intentKind":"k","stuck":"s","screenshotPng":"abc"}"#,
    )
    .await;

    assert_eq!(status, StatusCode::BAD_GATEWAY);
    assert!(body.contains("error"));
    assert_eq!(upstream.propose_calls.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn http_vision_assist_contract_over_bound_proxy() {
    use intent_engine::{HttpVisionAssist, StuckKind, VisionAssist, VisionProposeRequest};
    use vision_proxy::{router, AppState, ProxyConfig};

    let upstream = Arc::new(MockUpstream::new(
        ProposeResponse {
            confidence: 0.77,
            action: VisionAction::Click { x: 5.0, y: 6.0 },
        },
        ExtractResponse {
            value: serde_json::json!(null),
        },
    ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = ProxyConfig {
        bind: addr,
        path: "/vision".to_string(),
        bearer_token: "contract-token".to_string(),
    };
    let state = AppState {
        path: config.path.clone(),
        bearer_token: config.bearer_token.clone(),
        upstream: upstream.clone(),
    };
    let app = router(state);
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });

    let assist = HttpVisionAssist::new(
        format!("http://{addr}/vision"),
        Some("contract-token".into()),
        std::time::Duration::from_secs(5),
    )
    .unwrap();

    let proposal = assist
        .propose(VisionProposeRequest {
            purpose: "Continue".into(),
            intent_kind: "locate".into(),
            screenshot_png: b"png-bytes".to_vec(),
            stuck: StuckKind::TargetMissing,
        })
        .await
        .unwrap();

    assert_eq!(proposal.confidence, 0.77);
    assert!(matches!(
        proposal.action,
        intent_engine::VisionAction::Click { x: 5.0, y: 6.0 }
    ));
    assert_eq!(upstream.propose_calls.load(Ordering::SeqCst), 1);
}
