use companion_protocol::{
    ActionRequest, BrowserEngine, BrowserIdentity, CompanionCapabilities, CompanionRequest,
    InteractionPath, PROTOCOL_VERSION,
};
use types::{AttachmentId, CommandId, PageId};

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
