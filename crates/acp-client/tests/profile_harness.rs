use auth_broker::{AuthDriver, AuthProfileId};
use intent_engine::{StuckKind, VisionProposeRequest};

fn request() -> VisionProposeRequest {
    let mut png = vec![0; 24];
    png[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
    png[12..16].copy_from_slice(b"IHDR");
    png[16..20].copy_from_slice(&100_u32.to_be_bytes());
    png[20..24].copy_from_slice(&80_u32.to_be_bytes());
    VisionProposeRequest {
        purpose: "click submit".into(),
        intent_kind: "submit".into(),
        stuck: StuckKind::TargetMissing,
        screenshot_png: Some(png),
        corpus_screenshot_png: None,
        context: None,
    }
}

fn registry_with_profile(log: &std::path::Path) -> node_registry::NodeRegistry {
    let config = config::AppConfig::from_toml_str(&format!(
        r#"
[vision.acp_profiles.fake]
command = {command:?}
args = [{log:?}]
auth = "none"
env = {{ FAKE_ACP_PROBE = "from-profile" }}
session_meta = {{ probe = {{ lean = true }} }}
"#,
        command = env!("CARGO_BIN_EXE_fake_acp_harness"),
        log = log.to_string_lossy(),
    ))
    .expect("profile config");
    node_registry::NodeRegistry::from_config(&config)
}

fn lifecycle(log: &std::path::Path) -> Vec<String> {
    std::fs::read_to_string(log)
        .unwrap_or_default()
        .lines()
        .map(str::to_owned)
        .collect()
}

#[tokio::test]
async fn a_configured_profile_serves_every_proposal_from_one_harness_with_its_env_and_session_meta()
{
    let temp = tempfile::tempdir().expect("tempdir");
    let log = temp.path().join("lifecycle.log");
    let registry = registry_with_profile(&log);

    for _ in 0..2 {
        registry
            .vision("fake")
            .expect("profile resolves")
            .propose(request())
            .await
            .expect("vision proposal");
    }

    let meta = r#"new:{"probe":{"lean":true}}"#;
    assert_eq!(
        lifecycle(&log),
        ["initialize:from-profile", meta, "close", meta, "close"]
    );
}

#[tokio::test]
async fn a_profile_login_starts_the_harness_with_the_profile_env() {
    let temp = tempfile::tempdir().expect("tempdir");
    let log = temp.path().join("lifecycle.log");
    let registry = registry_with_profile(&log);

    registry
        .auth_driver("fake")
        .expect("profile resolves")
        .discover(&AuthProfileId::new("fake".to_owned()).expect("profile id"))
        .await
        .expect("harness capabilities");

    assert_eq!(lifecycle(&log), ["initialize:from-profile"]);
}
