use std::time::Duration;

use acp_client::{AcpClientError, AcpHarnessClient, AcpVisionAssist};
use intent_engine::{
    StuckKind, VisionAssist, VisionImageRegion, VisionProposeRequest, VisionTaskPacket,
};

fn packet() -> VisionTaskPacket {
    VisionTaskPacket {
        purpose: "click the submit control".into(),
        intent_kind: "submit".into(),
        stuck: StuckKind::TargetMissing,
        screenshot_png: vec![1, 2, 3],
        region: VisionImageRegion {
            x: 100,
            y: 200,
            width: 300,
            height: 400,
            viewport_width: 800,
            viewport_height: 900,
        },
        allowed_actions: vec!["click".into()],
        evidence_digest: "a".repeat(64),
    }
}

#[tokio::test]
async fn an_interactive_permission_request_fails_closed_and_closes_the_child() {
    let temp = tempfile::tempdir().expect("tempdir");
    let log = temp.path().join("lifecycle.log");
    let client = AcpHarnessClient::new(
        env!("CARGO_BIN_EXE_fake_acp_harness"),
        [log.to_string_lossy().into_owned(), "permission".into()],
    )
    .with_timeout(Duration::from_secs(5));

    let error = client
        .delegate(packet())
        .await
        .expect_err("permission must deny");
    assert!(
        matches!(error, AcpClientError::PermissionDenied),
        "{error:?}"
    );
    let lifecycle = std::fs::read_to_string(log).expect("lifecycle log");
    assert_eq!(lifecycle.lines().collect::<Vec<_>>(), ["new", "close"]);
}

#[tokio::test]
async fn one_task_uses_one_child_and_closes_it() {
    let temp = tempfile::tempdir().expect("tempdir");
    let log = temp.path().join("lifecycle.log");
    let client = AcpHarnessClient::new(
        env!("CARGO_BIN_EXE_fake_acp_harness"),
        [log.to_string_lossy().into_owned()],
    )
    .with_timeout(Duration::from_secs(5));

    let reply = client.delegate(packet()).await.expect("vision reply");
    assert!(reply.capabilities.image);
    assert_eq!(reply.child.session_id, "vision-child");
    assert_eq!(reply.result.evidence_digest, "a".repeat(64));
    let lifecycle = std::fs::read_to_string(log).expect("lifecycle log");
    assert_eq!(lifecycle.lines().collect::<Vec<_>>(), ["new", "close"]);
}

fn client_for_mode(log: &std::path::Path, mode: &str) -> AcpHarnessClient {
    AcpHarnessClient::new(
        env!("CARGO_BIN_EXE_fake_acp_harness"),
        [log.to_string_lossy().into_owned(), mode.to_owned()],
    )
    .with_timeout(Duration::from_secs(5))
}

#[tokio::test]
async fn unsupported_image_capability_is_rejected_before_child_creation() {
    let temp = tempfile::tempdir().expect("tempdir");
    let log = temp.path().join("lifecycle.log");
    let error = client_for_mode(&log, "no-image")
        .delegate(packet())
        .await
        .expect_err("image capability must be required");
    assert!(matches!(error, AcpClientError::ImageUnsupported));
    assert!(!log.exists(), "no child session may be created");
}

#[tokio::test]
async fn malformed_output_is_rejected_after_closing_the_child() {
    let temp = tempfile::tempdir().expect("tempdir");
    let log = temp.path().join("lifecycle.log");
    let error = client_for_mode(&log, "malformed")
        .delegate(packet())
        .await
        .expect_err("malformed output must fail");
    assert!(
        matches!(error, AcpClientError::MalformedOutput(_)),
        "{error:?}"
    );
    assert_eq!(std::fs::read_to_string(log).unwrap(), "new\nclose\n");
}

#[tokio::test]
async fn oversized_streamed_output_is_rejected_after_closing_the_child() {
    let temp = tempfile::tempdir().expect("tempdir");
    let log = temp.path().join("lifecycle.log");
    let error = client_for_mode(&log, "oversized")
        .delegate(packet())
        .await
        .expect_err("oversized output must fail");
    assert!(matches!(error, AcpClientError::OutputTooLarge), "{error:?}");
    assert_eq!(std::fs::read_to_string(log).unwrap(), "new\nclose\n");
}

#[tokio::test]
async fn adapter_rejects_evidence_substitution_from_the_harness() {
    let temp = tempfile::tempdir().expect("tempdir");
    let log = temp.path().join("lifecycle.log");
    let assist = AcpVisionAssist::new(
        env!("CARGO_BIN_EXE_fake_acp_harness"),
        [
            log.to_string_lossy().into_owned(),
            "mismatched-evidence".into(),
        ],
    )
    .with_timeout(Duration::from_secs(5));
    let mut png = vec![0; 24];
    png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    png[12..16].copy_from_slice(b"IHDR");
    png[16..20].copy_from_slice(&100_u32.to_be_bytes());
    png[20..24].copy_from_slice(&80_u32.to_be_bytes());

    let error = assist
        .propose(VisionProposeRequest {
            purpose: "click submit".into(),
            intent_kind: "submit".into(),
            stuck: StuckKind::TargetMissing,
            screenshot_png: png,
        })
        .await
        .expect_err("evidence substitution must fail");
    assert_eq!(error.code, types::ErrorCode::VisionAssistFailed);
    assert!(error.message.contains("different evidence"), "{error:?}");
    assert_eq!(std::fs::read_to_string(log).unwrap(), "new\nclose\n");
}
