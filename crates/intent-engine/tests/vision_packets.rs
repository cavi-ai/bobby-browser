use intent_engine::{
    compile_vision_packet, validate_backend_result, StuckKind, VisionAction, VisionBackendResult,
    VisionContextBudget, VisionImageRegion, VisionPacketError, VisionPacketInput,
};

fn input(purpose: String) -> VisionPacketInput {
    VisionPacketInput {
        context: None,
        purpose,
        intent_kind: "click".into(),
        stuck: StuckKind::TargetMissing,
        screenshot_png: vec![1, 2, 3],
        region: VisionImageRegion {
            x: 20,
            y: 30,
            width: 100,
            height: 80,
            viewport_width: 800,
            viewport_height: 600,
        },
        allowed_actions: vec!["click".into()],
        evidence_digest: "a".repeat(64),
    }
}

#[test]
fn context_compiler_rejects_text_over_budget() {
    let error = compile_vision_packet(
        input("x".repeat(257)),
        VisionContextBudget {
            max_text_bytes: 256,
            ..VisionContextBudget::default()
        },
    )
    .unwrap_err();
    assert_eq!(error, VisionPacketError::TextBudgetExceeded);
}

#[test]
fn context_compiler_rejects_image_over_budget() {
    let mut request = input("submit button".into());
    request.screenshot_png = vec![0; 17];
    let error = compile_vision_packet(
        request,
        VisionContextBudget {
            max_image_bytes: 16,
            ..VisionContextBudget::default()
        },
    )
    .unwrap_err();
    assert_eq!(error, VisionPacketError::ImageBudgetExceeded);
}

#[test]
fn validator_maps_crop_coordinates_to_viewport() {
    let packet = compile_vision_packet(input("submit button".into()), Default::default()).unwrap();
    let proposal = validate_backend_result(
        &packet,
        VisionBackendResult {
            confidence: 0.95,
            action: VisionAction::Click { x: 50.0, y: 40.0 },
            evidence_digest: packet.evidence_digest.clone(),
        },
    )
    .unwrap();
    assert!(matches!(
        proposal.action,
        VisionAction::Click { x: 70.0, y: 70.0 }
    ));
}

#[test]
fn validator_rejects_click_outside_crop() {
    let packet = compile_vision_packet(input("submit button".into()), Default::default()).unwrap();
    let error = validate_backend_result(
        &packet,
        VisionBackendResult {
            confidence: 0.99,
            action: VisionAction::Click { x: 101.0, y: 40.0 },
            evidence_digest: packet.evidence_digest.clone(),
        },
    )
    .unwrap_err();
    assert_eq!(error, VisionPacketError::CoordinateOutOfBounds);
}

#[test]
fn validator_rejects_mismatched_evidence() {
    let packet = compile_vision_packet(input("submit button".into()), Default::default()).unwrap();
    let error = validate_backend_result(
        &packet,
        VisionBackendResult {
            confidence: 0.99,
            action: VisionAction::Click { x: 10.0, y: 10.0 },
            evidence_digest: "b".repeat(64),
        },
    )
    .unwrap_err();
    assert_eq!(error, VisionPacketError::EvidenceMismatch);
}

#[test]
fn context_block_counts_toward_the_text_budget() {
    let mut with_context = input("purpose".to_string());
    with_context.context = Some(intent_engine::VisionPromptContext {
        url: Some("https://example.test/form".into()),
        candidates: vec![intent_engine::VisionPromptCandidate {
            role: "textbox".into(),
            name: "Email address".into(),
            ordinal: Some(1),
        }],
        recent_command_kinds: vec!["navigate".into(), "fill".into()],
    });
    // Fits the default (raised) budget...
    compile_vision_packet(with_context.clone(), VisionContextBudget::default()).unwrap();
    // ...but an oversized block is refused like any other over-budget text.
    let mut big = with_context.clone();
    big.context.as_mut().unwrap().url = Some(format!("https://example.test/{}", "x".repeat(8_192)));
    let error = compile_vision_packet(big, VisionContextBudget::default()).unwrap_err();
    assert_eq!(error, VisionPacketError::TextBudgetExceeded);
}

#[test]
fn context_block_carries_only_structural_fields() {
    // The canary for the packet builder: the serialized block has exactly
    // url/candidates/recentCommandKinds and candidates carry exactly
    // role/name/ordinal — there is nowhere for a typed value to hide.
    let context = intent_engine::VisionPromptContext {
        url: Some("https://example.test".into()),
        candidates: vec![intent_engine::VisionPromptCandidate {
            role: "textbox".into(),
            name: "Email address".into(),
            ordinal: Some(2),
        }],
        recent_command_kinds: vec!["fill".into()],
    };
    let value = serde_json::to_value(&context).unwrap();
    let mut keys: Vec<&str> = value
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(keys, ["candidates", "recentCommandKinds", "url"]);
    let mut candidate_keys: Vec<&str> = value["candidates"][0]
        .as_object()
        .unwrap()
        .keys()
        .map(String::as_str)
        .collect();
    candidate_keys.sort_unstable();
    assert_eq!(candidate_keys, ["name", "ordinal", "role"]);
}

#[test]
fn no_context_packet_is_identical_to_before() {
    let packet = compile_vision_packet(
        input("Continue".to_string()),
        VisionContextBudget::default(),
    )
    .unwrap();
    assert!(packet.context.is_none());
    let with = compile_vision_packet(
        input("Continue".to_string()),
        VisionContextBudget::default(),
    )
    .unwrap();
    assert_eq!(packet.purpose, with.purpose);
    assert_eq!(packet.evidence_digest, with.evidence_digest);
}
