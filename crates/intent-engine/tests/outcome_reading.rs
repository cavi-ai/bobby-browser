//! After a landed submit, the vision node reads what the page shows and the
//! submit's result carries that reading.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use dom_engine::{Candidate, CandidateState};
use intent_engine::{HttpVisionAssist, IntentBrowser, IntentEngine, IntentOutcome, VisionContext};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, ErrorCode, ErrorLayer, Evidence,
    IntentCommand, IntentHints, PageId, SubmitAndVerifyIntent, TargetSpec, TextMatch,
    TypeTextCommand, UploadFilesCommand, WaitCondition, WaitForCommand,
};

struct FakeBrowser {
    verified: bool,
}

#[async_trait]
impl IntentBrowser for FakeBrowser {
    async fn collect_candidates(
        &self,
        _page_id: &PageId,
        _target: &TargetSpec,
    ) -> Result<Vec<Candidate>, CommandError> {
        Ok(vec![Candidate {
            id: "submit".into(),
            css: Some("#submit".into()),
            tag: None,
            test_id: None,
            role: Some("button".into()),
            name: Some("Create account".into()),
            label: None,
            text: "Create account".into(),
            attributes: BTreeMap::new(),
            state: CandidateState {
                attached: true,
                visible: true,
                enabled: true,
            },
            frame_path: Vec::new(),
        }])
    }

    async fn click(
        &self,
        _page_id: &PageId,
        _command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(Vec::new())
    }

    async fn click_xy(&self, _: &PageId, _: f64, _: f64) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported("click_xy"))
    }

    async fn type_text(
        &self,
        _: &PageId,
        _: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported("type_text"))
    }

    async fn upload_files(
        &self,
        _: &PageId,
        _: &UploadFilesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported("upload_files"))
    }

    async fn wait_for(
        &self,
        _page_id: &PageId,
        _command: &WaitForCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        if self.verified {
            Ok(Vec::new())
        } else {
            Err(CommandError {
                code: ErrorCode::DeadlineExceeded,
                message: "url did not match within 5000ms".into(),
                layer: ErrorLayer::Page,
                retryable: true,
            })
        }
    }

    async fn capture_screenshot(
        &self,
        _page_id: &PageId,
        _command: &CaptureScreenshotCommand,
    ) -> Result<(Vec<u8>, Vec<Evidence>), CommandError> {
        Ok((
            b"\x89PNG frame".to_vec(),
            vec![Evidence::Screenshot {
                artifact_id: "after-submit".into(),
                media_type: "image/png".into(),
                width: 1280,
                height: 800,
                bytes: 11,
                sha256: "0".repeat(64),
            }],
        ))
    }

    async fn capture_sanitized_screenshot(
        &self,
        _page_id: &PageId,
    ) -> Result<Vec<u8>, CommandError> {
        Ok(b"\x89PNG masked frame".to_vec())
    }
}

fn unsupported(op: &str) -> CommandError {
    CommandError {
        code: ErrorCode::Internal,
        message: format!("{op} not supported by fake browser"),
        layer: ErrorLayer::Page,
        retryable: false,
    }
}

/// A vision endpoint that answers every propose with `answer` and keeps each
/// request body.
async fn vision_endpoint(
    answer: serde_json::Value,
) -> (String, Arc<Mutex<Vec<serde_json::Value>>>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/vision", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&requests);
    tokio::spawn(async move {
        loop {
            let Ok((mut stream, _)) = listener.accept().await else {
                return;
            };
            let mut raw = Vec::new();
            let mut buf = [0_u8; 8192];
            let body = loop {
                let n = stream.read(&mut buf).await.unwrap_or(0);
                if n == 0 {
                    break None;
                }
                raw.extend_from_slice(&buf[..n]);
                let text = String::from_utf8_lossy(&raw).to_string();
                if let Some((head, body)) = text.split_once("\r\n\r\n") {
                    let length = head
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                        })
                        .unwrap_or(0);
                    if body.len() >= length {
                        break Some(body[..length].to_string());
                    }
                }
            };
            if let Some(body) = body {
                seen.lock()
                    .unwrap()
                    .push(serde_json::from_str(&body).unwrap_or_default());
            }
            let payload = answer.to_string();
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
        }
    });
    (url, requests)
}

fn submit() -> IntentCommand {
    IntentCommand::SubmitAndVerify(SubmitAndVerifyIntent {
        purpose: "create the account".into(),
        hints: IntentHints {
            role: Some("button".into()),
            accessible_name: Some("Create account".into()),
            ..IntentHints::default()
        },
        expected_state: WaitForCommand {
            condition: WaitCondition::Url {
                matcher: TextMatch::Contains("/welcome".into()),
            },
            timeout_ms: 5_000,
        },
    })
}

fn vision(url: &str, gates_open: bool) -> VisionContext {
    VisionContext {
        session_ok: gates_open,
        capability_ok: true,
        assist: Some(Arc::new(
            HttpVisionAssist::new(url.into(), None, std::time::Duration::from_secs(5)).unwrap(),
        )),
        ..VisionContext::default()
    }
}

fn readings(evidence: &[Evidence]) -> Vec<serde_json::Value> {
    evidence
        .iter()
        .map(|item| serde_json::to_value(item).unwrap())
        .filter(|item| item["kind"] == "outcomeReading")
        .collect()
}

#[tokio::test]
async fn a_rejected_submit_reports_the_message_the_page_shows() {
    let (url, requests) = vision_endpoint(serde_json::json!({
        "confidence": 0.875,
        "action": {"kind": "outcomeRead", "outcome": "rejected", "message": "This email is already registered"}
    }))
    .await;
    let outcome = IntentEngine::execute(
        &submit(),
        &PageId::new(),
        &FakeBrowser { verified: false },
        &vision(&url, true),
    )
    .await;
    let IntentOutcome::Failed { error, evidence } = outcome else {
        panic!("a submit whose expected state never held must fail");
    };
    assert_eq!(error.code, ErrorCode::VerificationFailed);
    assert!(
        error
            .message
            .contains("the page shows a rejection: \"This email is already registered\""),
        "{}",
        error.message
    );
    assert_eq!(
        readings(&evidence),
        vec![serde_json::json!({
            "kind": "outcomeReading",
            "outcome": "rejected",
            "message": "This email is already registered",
            "confidence": 0.875
        })]
    );
    assert!(
        evidence.iter().any(|item| matches!(
            item,
            Evidence::Screenshot { artifact_id, .. } if artifact_id == "after-submit"
        )),
        "a failed submit keeps a screenshot of the page it left: {evidence:?}"
    );
    let requests = requests.lock().unwrap();
    assert_eq!(requests.len(), 1, "{requests:?}");
    assert_eq!(requests[0]["intentKind"], "readOutcome");
}

#[tokio::test]
async fn a_verified_submit_carries_the_confirmation_the_page_shows() {
    let (url, _requests) = vision_endpoint(serde_json::json!({
        "confidence": 0.9375,
        "action": {"kind": "outcomeRead", "outcome": "confirmed", "message": "Welcome aboard"}
    }))
    .await;
    let outcome = IntentEngine::execute(
        &submit(),
        &PageId::new(),
        &FakeBrowser { verified: true },
        &vision(&url, true),
    )
    .await;
    let IntentOutcome::Completed { evidence } = outcome else {
        panic!("a submit whose expected state held completes");
    };
    assert_eq!(
        readings(&evidence),
        vec![serde_json::json!({
            "kind": "outcomeReading",
            "outcome": "confirmed",
            "message": "Welcome aboard",
            "confidence": 0.9375
        })]
    );
}

#[tokio::test]
async fn a_session_without_vision_assist_sends_nothing_and_keeps_the_failure() {
    let (url, requests) = vision_endpoint(serde_json::json!({
        "confidence": 0.9,
        "action": {"kind": "outcomeRead", "outcome": "rejected", "message": "unused"}
    }))
    .await;
    let outcome = IntentEngine::execute(
        &submit(),
        &PageId::new(),
        &FakeBrowser { verified: false },
        &vision(&url, false),
    )
    .await;
    let IntentOutcome::Failed { error, evidence } = outcome else {
        panic!("a submit whose expected state never held must fail");
    };
    assert_eq!(error.code, ErrorCode::VerificationFailed);
    assert!(readings(&evidence).is_empty(), "{evidence:?}");
    assert!(requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn a_low_confidence_reading_is_left_out() {
    let (url, _requests) = vision_endpoint(serde_json::json!({
        "confidence": 0.3,
        "action": {"kind": "outcomeRead", "outcome": "rejected", "message": "maybe an error"}
    }))
    .await;
    let outcome = IntentEngine::execute(
        &submit(),
        &PageId::new(),
        &FakeBrowser { verified: false },
        &vision(&url, true),
    )
    .await;
    let IntentOutcome::Failed { error, evidence } = outcome else {
        panic!("a submit whose expected state never held must fail");
    };
    assert!(readings(&evidence).is_empty(), "{evidence:?}");
    assert!(
        !error.message.contains("maybe an error"),
        "{}",
        error.message
    );
}
