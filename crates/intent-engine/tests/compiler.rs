use intent_engine::{compile_intent, IntentPlan};
use types::{
    FillIntent, FillValue, FollowIntent, IntentCommand, IntentHints, LocateIntent, TextMatch,
    WaitCondition, WaitForCommand, WaitForStateIntent, WaitUntil,
};

#[test]
fn compile_locate_uses_purpose_as_accessible_name_hint() {
    let plan = compile_intent(&IntentCommand::Locate(LocateIntent {
        purpose: "Continue".into(),
        hints: IntentHints {
            role: Some("button".into()),
            ..IntentHints::default()
        },
    }))
    .expect("compile");
    assert!(matches!(plan, IntentPlan::Locate { .. }));
    let IntentPlan::Locate { target, .. } = plan else {
        panic!()
    };
    assert_eq!(target.role.as_deref(), Some("button"));
    assert!(target.accessible_name.is_some() || target.text.is_some());
}

#[test]
fn compile_rejects_empty_purpose() {
    let err = compile_intent(&IntentCommand::Locate(LocateIntent {
        purpose: "   ".into(),
        hints: IntentHints::default(),
    }))
    .expect_err("empty purpose");
    assert!(matches!(err, intent_engine::CompileError::EmptyPurpose));
}

#[test]
fn compile_rejects_oversized_purpose() {
    let err = compile_intent(&IntentCommand::Locate(LocateIntent {
        purpose: "x".repeat(types::MAX_INTENT_PURPOSE_BYTES + 1),
        hints: IntentHints::default(),
    }))
    .expect_err("too long");
    assert!(matches!(err, intent_engine::CompileError::PurposeTooLong));
}

#[test]
fn compile_fill_maps_purpose_to_text_when_role_absent() {
    let plan = compile_intent(&IntentCommand::Fill(FillIntent {
        purpose: "Email".into(),
        hints: IntentHints::default(),
        value: FillValue::Text {
            text: "a@b.co".into(),
            clear_first: true,
        },
    }))
    .expect("compile");
    let IntentPlan::Fill { target, value } = plan else {
        panic!("expected Fill plan");
    };
    assert_eq!(target.role, None);
    assert_eq!(target.text, Some(TextMatch::Contains("Email".into())));
    assert!(matches!(
        value,
        FillValue::Text {
            text,
            clear_first: true,
        } if text == "a@b.co"
    ));
}

#[test]
fn compile_follow_carries_target_expected_destination_and_boundary_flag() {
    let plan = compile_intent(&IntentCommand::Follow(FollowIntent {
        purpose: "Details".into(),
        hints: IntentHints {
            role: Some("link".into()),
            ..IntentHints::default()
        },
        expected_destination: WaitForCommand {
            condition: WaitCondition::Url {
                matcher: TextMatch::Contains("/details".into()),
            },
            timeout_ms: 5_000,
        },
        boundary: true,
    }))
    .expect("compile");
    let IntentPlan::Follow {
        target,
        expected_destination,
        boundary,
    } = plan
    else {
        panic!("expected Follow plan");
    };
    assert_eq!(target.role.as_deref(), Some("link"));
    assert_eq!(target.accessible_name.as_deref(), Some("Details"));
    assert_eq!(expected_destination.timeout_ms, 5_000);
    assert!(boundary);
}

#[test]
fn compile_wait_for_state_is_wait_only() {
    let plan = compile_intent(&IntentCommand::WaitForState(WaitForStateIntent {
        condition: WaitCondition::Document {
            ready: WaitUntil::Interactive,
        },
        timeout_ms: 5_000,
    }))
    .expect("compile");
    let IntentPlan::WaitForState {
        condition,
        timeout_ms,
    } = plan
    else {
        panic!("expected WaitForState plan");
    };
    assert_eq!(timeout_ms, 5_000);
    assert!(matches!(
        condition,
        WaitCondition::Document {
            ready: WaitUntil::Interactive
        }
    ));
}
