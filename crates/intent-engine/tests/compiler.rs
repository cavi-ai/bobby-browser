use intent_engine::{compile_intent, CompileError, IntentPlan};
use types::{
    CompleteFormField, CompleteFormIntent, DismissObstructionIntent, ExtractField, ExtractIntent,
    ExtractValueKind, FillIntent, FillValue, FollowIntent, IntentCommand, IntentHints,
    LocateIntent, TextMatch, WaitCondition, WaitForCommand, WaitForStateIntent, WaitUntil,
};

#[test]
fn compile_complete_form_preserves_order_and_rejects_duplicate_names() {
    let field = |name: &str, label: &str| CompleteFormField {
        name: name.into(),
        purpose: format!("fill {name}"),
        hints: IntentHints {
            role: Some("textbox".into()),
            near_text: Some(TextMatch::Exact(label.into())),
            ..Default::default()
        },
        value: FillValue::Text {
            text: name.into(),
            clear_first: true,
        },
    };
    let plan = compile_intent(&IntentCommand::CompleteForm(CompleteFormIntent {
        purpose: "complete application".into(),
        fields: vec![field("name", "Full name"), field("email", "Email address")],
    }))
    .unwrap();
    let IntentPlan::CompleteForm { fields } = plan else {
        panic!("expected complete form")
    };
    assert_eq!(
        fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>(),
        ["name", "email"]
    );
    assert_eq!(
        fields[0].target.accessible_name.as_deref(),
        Some("Full name")
    );

    let error = compile_intent(&IntentCommand::CompleteForm(CompleteFormIntent {
        purpose: "complete application".into(),
        fields: vec![field("name", "Full name"), field("name", "Email address")],
    }))
    .unwrap_err();
    assert!(matches!(error, CompileError::DuplicateFieldName(name) if name == "name"));
}

#[test]
fn compile_locate_keeps_role_only_when_purpose_is_descriptive_prose() {
    let plan = compile_intent(&IntentCommand::Locate(LocateIntent {
        purpose: "Continue button in the document preview iframe".into(),
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
    assert_eq!(target.accessible_name, None);
    assert_eq!(target.text, None);
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
fn compile_fill_uses_purpose_as_accessible_name_when_role_is_present() {
    let plan = compile_intent(&IntentCommand::Fill(FillIntent {
        purpose: "Full name".into(),
        hints: IntentHints {
            role: Some("textbox".into()),
            ..IntentHints::default()
        },
        value: FillValue::Text {
            text: "Ada Lovelace".into(),
            clear_first: true,
        },
    }))
    .expect("compile");
    let IntentPlan::Fill { target, .. } = plan else {
        panic!("expected Fill plan");
    };
    assert_eq!(target.role.as_deref(), Some("textbox"));
    assert_eq!(target.accessible_name.as_deref(), Some("Full name"));
    assert_eq!(target.text, None);
}

#[test]
fn compile_fill_uses_near_text_as_the_control_name_without_conflating_task_purpose() {
    let plan = compile_intent(&IntentCommand::Fill(FillIntent {
        purpose: "enter the applicant email".into(),
        hints: IntentHints {
            role: Some("textbox".into()),
            near_text: Some(TextMatch::Exact("Email address".into())),
            ..IntentHints::default()
        },
        value: FillValue::Text {
            text: "ada@example.test".into(),
            clear_first: true,
        },
    }))
    .expect("compile");
    let IntentPlan::Fill { target, .. } = plan else {
        panic!("expected Fill plan");
    };
    assert_eq!(target.role.as_deref(), Some("textbox"));
    assert_eq!(target.accessible_name.as_deref(), Some("Email address"));
    assert_eq!(target.text, None);
}

#[test]
fn compile_fill_preserves_snapshot_ordinal_for_duplicate_controls() {
    let plan = compile_intent(&IntentCommand::Fill(FillIntent {
        purpose: "enter the work phone".into(),
        hints: IntentHints {
            role: Some("textbox".into()),
            near_text: Some(TextMatch::Exact("Phone".into())),
            ordinal: Some(1),
            ..IntentHints::default()
        },
        value: FillValue::Text {
            text: "555-0102".into(),
            clear_first: true,
        },
    }))
    .expect("compile");
    let IntentPlan::Fill { target, .. } = plan else {
        panic!("expected Fill plan");
    };
    assert_eq!(target.role.as_deref(), Some("textbox"));
    assert_eq!(target.accessible_name.as_deref(), Some("Phone"));
    assert_eq!(target.ordinal, Some(1));
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
    assert_eq!(target.text, None);
    assert_eq!(expected_destination.timeout_ms, 5_000);
    assert!(boundary);
}

#[test]
fn compile_dismiss_obstruction_carries_target_and_timeout() {
    let plan = compile_intent(&IntentCommand::DismissObstruction(
        DismissObstructionIntent {
            purpose: "Cookie notice close button".into(),
            hints: IntentHints {
                role: Some("button".into()),
                ..IntentHints::default()
            },
            timeout_ms: 3_000,
        },
    ))
    .expect("compile");
    let IntentPlan::DismissObstruction { target, timeout_ms } = plan else {
        panic!("expected DismissObstruction plan");
    };
    assert_eq!(target.role.as_deref(), Some("button"));
    assert_eq!(
        target.accessible_name.as_deref(),
        Some("Cookie notice close button")
    );
    assert_eq!(target.text, None);
    assert_eq!(timeout_ms, 3_000);
}

#[test]
fn compile_extract_resolves_each_field_to_its_own_target_and_value_kind() {
    let plan = compile_intent(&IntentCommand::Extract(ExtractIntent {
        purpose: "Profile summary".into(),
        fields: vec![
            ExtractField {
                name: "displayName".into(),
                purpose: "Display name".into(),
                hints: IntentHints::default(),
                value: ExtractValueKind::Text,
            },
            ExtractField {
                name: "profileLink".into(),
                purpose: "Profile link".into(),
                hints: IntentHints {
                    role: Some("link".into()),
                    ..IntentHints::default()
                },
                value: ExtractValueKind::Href,
            },
        ],
    }))
    .expect("compile");
    let IntentPlan::Extract { fields } = plan else {
        panic!("expected Extract plan");
    };
    assert_eq!(fields.len(), 2);
    assert_eq!(fields[0].name, "displayName");
    assert_eq!(
        fields[0].target.text,
        Some(TextMatch::Contains("Display name".into()))
    );
    assert!(matches!(fields[0].value, ExtractValueKind::Text));
    assert_eq!(fields[1].name, "profileLink");
    assert_eq!(fields[1].target.role.as_deref(), Some("link"));
    assert_eq!(
        fields[1].target.accessible_name.as_deref(),
        Some("Profile link")
    );
    assert_eq!(fields[1].target.text, None);
    assert!(matches!(fields[1].value, ExtractValueKind::Href));
}

#[test]
fn compile_extract_rejects_empty_field_list() {
    let err = compile_intent(&IntentCommand::Extract(ExtractIntent {
        purpose: "Profile summary".into(),
        fields: vec![],
    }))
    .expect_err("no fields");
    assert_eq!(err, CompileError::NoExtractFields);
}

#[test]
fn compile_extract_rejects_empty_field_name() {
    let err = compile_intent(&IntentCommand::Extract(ExtractIntent {
        purpose: "Profile summary".into(),
        fields: vec![ExtractField {
            name: "   ".into(),
            purpose: "Display name".into(),
            hints: IntentHints::default(),
            value: ExtractValueKind::Text,
        }],
    }))
    .expect_err("empty field name");
    assert_eq!(err, CompileError::EmptyFieldName);
}

#[test]
fn compile_extract_rejects_duplicate_field_names() {
    let err = compile_intent(&IntentCommand::Extract(ExtractIntent {
        purpose: "Profile summary".into(),
        fields: vec![
            ExtractField {
                name: "displayName".into(),
                purpose: "Display name".into(),
                hints: IntentHints::default(),
                value: ExtractValueKind::Text,
            },
            ExtractField {
                name: "displayName".into(),
                purpose: "Secondary name".into(),
                hints: IntentHints::default(),
                value: ExtractValueKind::Text,
            },
        ],
    }))
    .expect_err("duplicate field name");
    assert_eq!(err, CompileError::DuplicateFieldName("displayName".into()));
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

#[test]
fn compile_fill_accepts_a_snapshot_target_accessible_name_verbatim() {
    // An `a11y_snapshot` node's `target` is `{role, accessibleName, ordinal}`.
    // Pasting it into `hints` used to drop the name and resolve on role plus
    // ordinal alone, which silently targets the wrong control.
    let plan = compile_intent(&IntentCommand::Fill(FillIntent {
        purpose: "enter the applicant email".into(),
        hints: IntentHints {
            role: Some("textbox".into()),
            accessible_name: Some("Email address".into()),
            ..IntentHints::default()
        },
        value: FillValue::Text {
            text: "ada@example.test".into(),
            clear_first: true,
        },
    }))
    .expect("compile");
    let IntentPlan::Fill { target, .. } = plan else {
        panic!("expected Fill plan");
    };
    assert_eq!(target.role.as_deref(), Some("textbox"));
    assert_eq!(target.accessible_name.as_deref(), Some("Email address"));
    assert_eq!(target.text, None);
}

#[test]
fn compile_target_treats_accessible_name_and_exact_near_text_as_one_hint() {
    let compile = |hints: IntentHints| {
        let plan = compile_intent(&IntentCommand::Locate(LocateIntent {
            purpose: "find the email field".into(),
            hints,
        }))
        .expect("compile");
        let IntentPlan::Locate { target } = plan else {
            panic!("expected Locate plan");
        };
        target
    };

    let from_accessible_name = compile(IntentHints {
        role: Some("textbox".into()),
        accessible_name: Some("Email address".into()),
        ..IntentHints::default()
    });
    let from_near_text = compile(IntentHints {
        role: Some("textbox".into()),
        near_text: Some(TextMatch::Exact("Email address".into())),
        ..IntentHints::default()
    });
    let from_both = compile(IntentHints {
        role: Some("textbox".into()),
        accessible_name: Some("Email address".into()),
        near_text: Some(TextMatch::Exact("Email address".into())),
        ..IntentHints::default()
    });

    assert_eq!(
        from_accessible_name.accessible_name,
        from_near_text.accessible_name
    );
    assert_eq!(from_both.accessible_name, from_near_text.accessible_name);
}

#[test]
fn compile_target_refuses_two_different_names_rather_than_guessing() {
    let error = compile_intent(&IntentCommand::Locate(LocateIntent {
        purpose: "find the email field".into(),
        hints: IntentHints {
            role: Some("textbox".into()),
            accessible_name: Some("Email address".into()),
            near_text: Some(TextMatch::Exact("Home phone".into())),
            ..IntentHints::default()
        },
    }))
    .expect_err("conflicting names must not compile");
    assert_eq!(error, CompileError::ConflictingNameHints);
}

#[test]
fn compile_target_keeps_a_non_exact_near_text_alongside_no_accessible_name() {
    let plan = compile_intent(&IntentCommand::Locate(LocateIntent {
        purpose: "find the banner".into(),
        hints: IntentHints {
            near_text: Some(TextMatch::Contains("Accept".into())),
            ..IntentHints::default()
        },
    }))
    .expect("compile");
    let IntentPlan::Locate { target } = plan else {
        panic!("expected Locate plan");
    };
    assert_eq!(target.text, Some(TextMatch::Contains("Accept".into())));
    assert_eq!(target.accessible_name, None);
}
