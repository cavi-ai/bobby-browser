use std::time::Duration;

use acp_client::AcpHarnessClient;
use intent_engine::{StuckKind, VisionImageRegion, VisionTaskPacket};

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
