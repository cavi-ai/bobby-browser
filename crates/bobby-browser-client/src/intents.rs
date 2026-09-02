//! Builders for intent [`RuntimeCommand`] and [`CommandEnvelope`] values —
//! the Rust twin of the TypeScript SDK's `intents` module.
//!
//! Pass envelopes to [`BrowserRuntimeClient::submit`](crate::http::BrowserRuntimeClient::submit).
//! Prefer these helpers over hand-rolling nested `{ kind, input }` shapes.
//!
//! Nested wire shape:
//! `{ kind: "intent", input: { kind: "locate", input: { … } } }`.

use crate::challenges::{DetectChallengeIntent, SolveChallengeIntent};
use crate::commands::{
    CommandEnvelope, CompleteFormIntent, ControlAction, DismissObstructionIntent, ExtractField,
    ExtractIntent, FillIntent, FollowIntent, IntentCommand, IntentHints, LocateIntent,
    RuntimeCommand, SubmitAndVerifyIntent, WaitCondition, WaitForCommand, WaitForStateIntent,
    MAX_INTENT_PURPOSE_BYTES,
};
use crate::ids::{AttemptId, CommandId, PageId, SessionId, WorkflowId};
use chrono::{DateTime, Utc};

/// Meta that every intent envelope carries. Mirrors the TypeScript SDK's
/// `IntentEnvelopeMeta`.
#[derive(Debug, Clone)]
pub struct IntentEnvelopeMeta {
    pub command_id: CommandId,
    pub workflow_id: WorkflowId,
    pub attempt_id: AttemptId,
    pub session_id: SessionId,
    pub page_id: Option<PageId>,
    /// RFC-3339 deadline stamped on the envelope.
    pub deadline: DateTime<Utc>,
}

/// Throws the Rust equivalent of the TS SDK's `assertIntentPurpose`: an
/// empty or over-budget purpose fails the builder instead of failing at the
/// runtime after a round trip.
fn assert_intent_purpose(purpose: &str) {
    if purpose.len() > MAX_INTENT_PURPOSE_BYTES {
        panic!(
            "intent purpose exceeds {MAX_INTENT_PURPOSE_BYTES} bytes (got {})",
            purpose.len()
        );
    }
    if purpose.trim().is_empty() {
        panic!("intent purpose must be a non-empty string");
    }
}

/// `setText`'s `clear_first` is a plain `bool` with a serde default on the
/// wire, so Rust callers get the TS SDK's normalization for free: an absent
/// `clearFirst` decodes to `true` (replace). The builder is an identity
/// function kept for parity with the TypeScript module.
fn normalize_fill_value(value: ControlAction) -> ControlAction {
    value
}

/// Wrap an intent [`RuntimeCommand`] in a [`CommandEnvelope`].
pub fn intent_envelope(meta: IntentEnvelopeMeta, command: RuntimeCommand) -> CommandEnvelope {
    debug_assert!(
        matches!(command, RuntimeCommand::Intent(_)),
        "intentEnvelope requires a RuntimeCommand with kind \"intent\""
    );
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: meta.command_id,
        workflow_id: meta.workflow_id,
        attempt_id: meta.attempt_id,
        session_id: meta.session_id,
        page_id: meta.page_id,
        deadline: meta.deadline,
        command,
    }
}

/// Build a `locate` intent command.
pub fn locate_runtime_command(input: LocateIntent) -> RuntimeCommand {
    assert_intent_purpose(&input.purpose);
    RuntimeCommand::Intent(IntentCommand::Locate(LocateIntent {
        purpose: input.purpose,
        hints: input.hints,
    }))
}

/// Build a `fill` intent command.
pub fn fill_runtime_command(input: FillIntent) -> RuntimeCommand {
    assert_intent_purpose(&input.purpose);
    RuntimeCommand::Intent(IntentCommand::Fill(FillIntent {
        purpose: input.purpose,
        hints: input.hints,
        value: normalize_fill_value(input.value),
    }))
}

/// Build a `completeForm` intent command (1–128 uniquely named fields).
pub fn complete_form_runtime_command(input: CompleteFormIntent) -> RuntimeCommand {
    assert_intent_purpose(&input.purpose);
    assert!(
        !input.fields.is_empty(),
        "completeForm fields must not be empty"
    );
    assert!(
        input.fields.len() <= 128,
        "completeForm fields must not exceed 128 items"
    );
    let mut names = std::collections::HashSet::new();
    let fields = input
        .fields
        .into_iter()
        .map(|mut field| {
            assert!(
                !field.name.trim().is_empty(),
                "completeForm field name must not be empty"
            );
            assert!(
                names.insert(field.name.clone()),
                "duplicate completeForm field name: {}",
                field.name
            );
            assert_intent_purpose(&field.purpose);
            field.value = normalize_fill_value(field.value);
            field
        })
        .collect();
    RuntimeCommand::Intent(IntentCommand::CompleteForm(CompleteFormIntent {
        purpose: input.purpose,
        fields,
    }))
}

/// Build a `submitAndVerify` intent command.
pub fn submit_and_verify_runtime_command(input: SubmitAndVerifyIntent) -> RuntimeCommand {
    assert_intent_purpose(&input.purpose);
    RuntimeCommand::Intent(IntentCommand::SubmitAndVerify(SubmitAndVerifyIntent {
        purpose: input.purpose,
        hints: input.hints,
        expected_state: input.expected_state,
    }))
}

/// Build a `waitForState` intent command.
pub fn wait_for_state_runtime_command(input: WaitForStateIntent) -> RuntimeCommand {
    RuntimeCommand::Intent(IntentCommand::WaitForState(WaitForStateIntent {
        condition: input.condition,
        timeout_ms: input.timeout_ms,
    }))
}

/// Build a `follow` intent command.
pub fn follow_runtime_command(input: FollowIntent) -> RuntimeCommand {
    assert_intent_purpose(&input.purpose);
    RuntimeCommand::Intent(IntentCommand::Follow(FollowIntent {
        purpose: input.purpose,
        hints: input.hints,
        expected_destination: input.expected_destination,
        boundary: input.boundary,
    }))
}

/// Build a `dismissObstruction` intent command.
pub fn dismiss_obstruction_runtime_command(input: DismissObstructionIntent) -> RuntimeCommand {
    assert_intent_purpose(&input.purpose);
    RuntimeCommand::Intent(IntentCommand::DismissObstruction(
        DismissObstructionIntent {
            purpose: input.purpose,
            hints: input.hints,
            timeout_ms: input.timeout_ms,
        },
    ))
}

/// Build an `extract` intent command (at least one uniquely named field).
pub fn extract_runtime_command(input: ExtractIntent) -> RuntimeCommand {
    assert_intent_purpose(&input.purpose);
    assert!(
        !input.fields.is_empty(),
        "extract intent must include at least one field"
    );
    let mut seen = std::collections::HashSet::new();
    for field in &input.fields {
        assert_intent_purpose(&field.purpose);
        assert!(
            seen.insert(field.name.clone()),
            "duplicate extract field name: {}",
            field.name
        );
    }
    RuntimeCommand::Intent(IntentCommand::Extract(ExtractIntent {
        purpose: input.purpose,
        fields: input.fields,
    }))
}

/// Build a `detectChallenge` intent command (Replayable, read-only).
pub fn detect_challenge_runtime_command(input: DetectChallengeIntent) -> RuntimeCommand {
    assert_intent_purpose(&input.purpose);
    RuntimeCommand::Intent(IntentCommand::DetectChallenge(DetectChallengeIntent {
        purpose: input.purpose,
        hints: input.hints,
    }))
}

/// Build a `solveChallenge` intent command (Reconciliable vision solve loop).
pub fn solve_challenge_runtime_command(input: SolveChallengeIntent) -> RuntimeCommand {
    assert_intent_purpose(&input.purpose);
    RuntimeCommand::Intent(IntentCommand::SolveChallenge(SolveChallengeIntent {
        purpose: input.purpose,
        hints: input.hints,
    }))
}

/// Convenience: [`locate`] runtime command + [`intent_envelope`].
pub fn locate_envelope(
    meta: IntentEnvelopeMeta,
    purpose: &str,
    hints: Option<IntentHints>,
) -> CommandEnvelope {
    intent_envelope(
        meta,
        locate_runtime_command(LocateIntent {
            purpose: purpose.into(),
            hints: hints.unwrap_or_default(),
        }),
    )
}

/// Convenience: [`fill`] runtime command + [`intent_envelope`].
pub fn fill_envelope(
    meta: IntentEnvelopeMeta,
    purpose: &str,
    value: ControlAction,
    hints: Option<IntentHints>,
) -> CommandEnvelope {
    intent_envelope(
        meta,
        fill_runtime_command(FillIntent {
            purpose: purpose.into(),
            value,
            hints: hints.unwrap_or_default(),
        }),
    )
}

/// Convenience: [`submit_and_verify`] runtime command + [`intent_envelope`].
pub fn submit_and_verify_envelope(
    meta: IntentEnvelopeMeta,
    purpose: &str,
    expected_state: WaitForCommand,
    hints: Option<IntentHints>,
) -> CommandEnvelope {
    intent_envelope(
        meta,
        submit_and_verify_runtime_command(SubmitAndVerifyIntent {
            purpose: purpose.into(),
            expected_state,
            hints: hints.unwrap_or_default(),
        }),
    )
}

/// Convenience: [`wait_for_state`] runtime command + [`intent_envelope`].
pub fn wait_for_state_envelope(
    meta: IntentEnvelopeMeta,
    condition: WaitCondition,
    timeout_ms: u64,
) -> CommandEnvelope {
    intent_envelope(
        meta,
        wait_for_state_runtime_command(WaitForStateIntent {
            condition,
            timeout_ms,
        }),
    )
}

/// Convenience: [`follow`] runtime command + [`intent_envelope`].
pub fn follow_envelope(
    meta: IntentEnvelopeMeta,
    purpose: &str,
    expected_destination: WaitForCommand,
    boundary: bool,
    hints: Option<IntentHints>,
) -> CommandEnvelope {
    intent_envelope(
        meta,
        follow_runtime_command(FollowIntent {
            purpose: purpose.into(),
            expected_destination,
            hints: hints.unwrap_or_default(),
            boundary,
        }),
    )
}

/// Convenience: [`dismiss_obstruction`] runtime command + [`intent_envelope`].
pub fn dismiss_obstruction_envelope(
    meta: IntentEnvelopeMeta,
    purpose: &str,
    timeout_ms: Option<u64>,
) -> CommandEnvelope {
    intent_envelope(
        meta,
        dismiss_obstruction_runtime_command(DismissObstructionIntent {
            purpose: purpose.into(),
            hints: IntentHints::default(),
            timeout_ms: timeout_ms
                .unwrap_or(crate::commands::DEFAULT_DISMISS_OBSTRUCTION_TIMEOUT_MS),
        }),
    )
}

/// Convenience: [`extract`] runtime command + [`intent_envelope`].
pub fn extract_envelope(
    meta: IntentEnvelopeMeta,
    purpose: &str,
    fields: Vec<ExtractField>,
) -> CommandEnvelope {
    intent_envelope(
        meta,
        extract_runtime_command(ExtractIntent {
            purpose: purpose.into(),
            fields,
        }),
    )
}

/// Convenience: [`detect_challenge`] runtime command + [`intent_envelope`].
pub fn detect_challenge_envelope(
    meta: IntentEnvelopeMeta,
    purpose: &str,
    hints: Option<crate::challenges::DetectChallengeHints>,
) -> CommandEnvelope {
    intent_envelope(
        meta,
        detect_challenge_runtime_command(DetectChallengeIntent {
            purpose: purpose.into(),
            hints: hints.unwrap_or_default(),
        }),
    )
}

/// Convenience: [`solve_challenge`] runtime command + [`intent_envelope`].
pub fn solve_challenge_envelope(
    meta: IntentEnvelopeMeta,
    purpose: &str,
    hints: Option<crate::challenges::SolveChallengeHints>,
) -> CommandEnvelope {
    intent_envelope(
        meta,
        solve_challenge_runtime_command(SolveChallengeIntent {
            purpose: purpose.into(),
            hints: hints.unwrap_or_default(),
        }),
    )
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::{CompleteFormField, ExtractValueKind, WaitUntil};
    use crate::ids::PageId;

    fn meta() -> IntentEnvelopeMeta {
        IntentEnvelopeMeta {
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: SessionId::new(),
            page_id: Some(PageId::new()),
            deadline: Utc::now(),
        }
    }

    /// Golden wire shape: the nested `{kind, input}` envelope matches the
    /// TypeScript SDK's byte-for-byte (serde tag + camelCase rename).
    #[test]
    fn submit_envelope_wraps_the_nested_wire_shape() {
        let envelope = submit_and_verify_envelope(
            meta(),
            "Submit the customer priority",
            WaitForCommand {
                condition: WaitCondition::Document {
                    ready: WaitUntil::Interactive,
                },
                timeout_ms: 1_000,
            },
            None,
        );
        assert_eq!(envelope.schema_version, CommandEnvelope::SCHEMA_VERSION);
        match &envelope.command {
            RuntimeCommand::Intent(IntentCommand::SubmitAndVerify(submit)) => {
                assert_eq!(submit.purpose, "Submit the customer priority");
                assert_eq!(
                    submit.expected_state.condition,
                    WaitCondition::Document {
                        ready: WaitUntil::Interactive
                    }
                );
            }
            other => panic!("expected intent submitAndVerify, got {other:?}"),
        }
    }

    #[test]
    fn complete_form_builds_unique_named_fields() {
        let command = complete_form_runtime_command(CompleteFormIntent {
            purpose: "fill the onboarding form".into(),
            fields: vec![CompleteFormField {
                name: "email".into(),
                purpose: "work address".into(),
                hints: IntentHints::default(),
                value: ControlAction::SetText {
                    value: "ada@example.test".into(),
                    clear_first: true,
                },
            }],
        });
        match command {
            RuntimeCommand::Intent(IntentCommand::CompleteForm(form)) => {
                assert_eq!(form.fields.len(), 1);
            }
            other => panic!("expected completeForm, got {other:?}"),
        }
    }

    #[test]
    fn duplicate_extract_field_names_are_refused() {
        let field = ExtractField {
            name: "email".into(),
            purpose: "address".into(),
            hints: IntentHints::default(),
            value: crate::commands::ExtractValueKind::Text,
        };
        let result = std::panic::catch_unwind(|| {
            extract_runtime_command(ExtractIntent {
                purpose: "read the header".into(),
                fields: vec![field.clone(), field],
            })
        });
        assert!(result.is_err(), "duplicate field names must panic");
    }

    #[test]
    fn challenge_envelopes_carry_their_hints() {
        let envelope = solve_challenge_envelope(
            meta(),
            "clear the recaptcha",
            Some(crate::challenges::SolveChallengeHints {
                region: Some(crate::challenges::ChallengeRegion {
                    x: 1.0,
                    y: 2.0,
                    width: 3.0,
                    height: 4.0,
                }),
                timeout_ms: 45_000,
            }),
        );
        match &envelope.command {
            RuntimeCommand::Intent(IntentCommand::SolveChallenge(solve)) => {
                assert_eq!(solve.hints.timeout_ms, 45_000);
                assert!(solve.hints.region.is_some());
            }
            other => panic!("expected solveChallenge, got {other:?}"),
        }
    }
}
