use companion_protocol::{
    ActionRequest, AttachmentGrant, BrowserEngine, BrowserIdentity, BrowserTarget,
    CompanionCapabilities, CompanionEvent, CompanionRequest, GrantedPage, InteractionPath,
    TargetDiscovery, TargetKind, PROTOCOL_VERSION,
};
use types::{AttachmentId, CommandId, PageId, ProfileId};

#[test]
fn request_round_trip_preserves_browser_neutral_fields() {
    let request = CompanionRequest::Action(ActionRequest {
        protocol_version: PROTOCOL_VERSION,
        attachment_id: AttachmentId::new(),
        command_id: CommandId::new(),
        page_id: PageId::new(),
        operation: "click".into(),
        input: serde_json::json!({"selector": "button[type=submit]"}),
        deadline_unix_ms: 1_800_000_000_000,
    });
    let encoded = serde_json::to_string(&request).unwrap();
    let decoded: CompanionRequest = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, request);
    assert!(!encoded.contains("firefox_command"));
}

#[test]
fn identity_and_capabilities_are_explicit() {
    let identity = BrowserIdentity {
        engine: BrowserEngine::Firefox,
        browser_name: "Firefox".into(),
        browser_version: "stable".into(),
        os: "macos".into(),
        profile_label: "default-release".into(),
    };
    let capabilities = CompanionCapabilities {
        observe: true,
        navigate: true,
        native_input: true,
        tabs: true,
        frames: true,
        native_dialogs: false,
    };
    assert_eq!(identity.engine, BrowserEngine::Firefox);
    assert!(capabilities.native_input);
    assert_eq!(InteractionPath::EngineNative, InteractionPath::EngineNative);
}

#[test]
fn discovery_and_grant_round_trip_with_browser_neutral_uuid_pages() {
    let profile_id = ProfileId::new();
    let target = BrowserTarget {
        target_id: "opaque-firefox-frame-handle".into(),
        kind: TargetKind::Frame,
    };
    let discovery = CompanionEvent::TargetsDiscovered(TargetDiscovery {
        protocol_version: PROTOCOL_VERSION,
        profile_id: profile_id.clone(),
        targets: vec![target.clone()],
    });
    let decoded_discovery: CompanionEvent =
        serde_json::from_str(&serde_json::to_string(&discovery).unwrap()).unwrap();
    assert_eq!(decoded_discovery, discovery);

    let page_id = PageId::new();
    let grant = CompanionRequest::Grant(AttachmentGrant {
        protocol_version: PROTOCOL_VERSION,
        attachment_id: AttachmentId::new(),
        profile_id,
        expires_at_unix_ms: 1_800_000_000_000,
        pages: vec![GrantedPage {
            target_id: target.target_id,
            page_id: page_id.clone(),
        }],
    });
    let encoded = serde_json::to_string(&grant).unwrap();
    let decoded: CompanionRequest = serde_json::from_str(&encoded).unwrap();
    assert_eq!(decoded, grant);
    assert!(encoded.contains(&page_id.0.to_string()));
    assert!(!encoded.contains("tabId"));
    assert!(!encoded.contains("frameId"));
}
