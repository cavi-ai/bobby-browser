use cdp_gateway::{parse_frame, CdpErrorCode, MAX_FRAME_BYTES as MAX_CDP_FRAME_BYTES};
use chrono::{Duration, Utc};
use interface_core::{Authority, AuthorityStore, Event, EventStore};
use mcp_gateway::protocol::{MAX_FRAME_BYTES as MAX_MCP_FRAME_BYTES, MCP_PROTOCOL_VERSION};
use serde_json::json;
use types::{Capability, CorrelationId, IdempotencyKey, InterfaceVersion, PrincipalId};

const SECRET: &str = "release-gate-secret-that-must-never-escape";

#[tokio::test]
async fn credentials_expire_revoke_and_never_reach_observable_payloads() {
    let authority = AuthorityStore::in_memory();
    let principal = PrincipalId::from_uuid(uuid::Uuid::new_v4());
    let token = authority
        .issue(
            principal.clone(),
            [Capability::SessionRead],
            Utc::now() + Duration::seconds(30),
        )
        .await
        .unwrap()
        .expose_once();
    let handle = authority.verify(&token).await.unwrap();
    assert!(handle.is_valid_at(Utc::now()));
    authority.revoke(&principal).await.unwrap();
    assert!(!handle.is_valid_at(Utc::now()));
    assert!(authority.verify(&token).await.is_err());

    let expired = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [Capability::SessionRead],
            Utc::now() + Duration::seconds(1),
        )
        .await
        .unwrap()
        .expose_once();
    assert!(authority
        .authenticate(&expired, Utc::now() + Duration::seconds(2))
        .await
        .is_err());

    let events = EventStore::new(2);
    events
        .append(Event::new(
            "diagnostic",
            json!({
                "authorization": format!("Bearer {SECRET}"),
                "nested": {"token": SECRET, "cookie": SECRET},
                "safe": "retained"
            }),
        ))
        .await;
    let batch = events.read_after(0.into(), 1).await.unwrap();
    let encoded = serde_json::to_string(&batch).unwrap();
    assert!(!encoded.contains(SECRET));
    assert!(encoded.contains("[REDACTED]"));
    assert!(encoded.contains("retained"));
    assert!(!format!("{authority:?} {handle:?}").contains(&token));
}

#[test]
fn protocol_inputs_fail_closed_at_version_size_method_and_identifier_boundaries() {
    assert!(serde_json::from_str::<InterfaceVersion>("\"unsupported\"").is_err());
    assert_eq!(MCP_PROTOCOL_VERSION, "2025-11-25");
    const { assert!(MAX_MCP_FRAME_BYTES <= 1024 * 1024) };

    let oversized = vec![b'x'; MAX_CDP_FRAME_BYTES + 1];
    assert_eq!(
        parse_frame(&oversized).unwrap_err().code,
        CdpErrorCode::InvalidRequest as i32
    );
    for request in [
        json!({"id": 0, "method": "Target.getTargets", "params": {}}),
        json!({"id": 1, "method": "", "params": {}}),
        json!({"id": 1, "method": "Unknown.method", "params": []}),
        json!({"id": 9_007_199_254_740_992_u64, "method": "Target.getTargets", "params": {}}),
    ] {
        assert!(parse_frame(&serde_json::to_vec(&request).unwrap()).is_err());
    }
    for hostile in ["", "\n", &"x".repeat(129)] {
        assert!(IdempotencyKey::try_from(hostile).is_err());
    }
    let correlation = CorrelationId::new();
    assert!(!serde_json::to_string(&correlation)
        .unwrap()
        .contains(['\r', '\n']));
}

#[tokio::test]
#[ignore = "requires installed Chromium and loopback fixture"]
async fn installed_chromium_security_boundary_is_operational() {
    let harness = interface_conformance::live::ChromeRuntimeHarness::start().await;
    assert!(harness.authority.verify(&harness.token).await.is_ok());
    assert!(harness
        .authority
        .verify(&harness.denied_token)
        .await
        .is_ok());
    assert!(harness.site_url().starts_with("http://127.0.0.1:"));
}
