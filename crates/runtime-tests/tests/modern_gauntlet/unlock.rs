use chrono::{Duration, Utc};
use gauntlet_server::{MFA_CODE, OPERATOR_EMAIL, OPERATOR_PASSWORD};
use sdk_core::RuntimeService;
use types::{
    AccessibilityNode, AccessibilitySnapshotCommand, AttemptId, CheckpointId, CheckpointInvariant,
    CommandClass, CommandEnvelope, CommandId, CommandOutcome, CompleteFormField,
    CompleteFormIntent, ControlAction, ElementState, Evidence, FollowIntent, InspectCommand,
    IntentCommand, IntentHints, PageId, PrimitiveCommand, RuntimeCommand, SessionId,
    SubmitAndVerifyIntent, TargetSpec, WaitCondition, WaitForCommand, WorkflowCheckpoint,
    WorkflowId,
};

pub type UnlockResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn css_target(selector: &str) -> TargetSpec {
    TargetSpec {
        css: Some(selector.into()),
        ..TargetSpec::default()
    }
}

async fn submit(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
    command: PrimitiveCommand,
) -> CommandOutcome {
    runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session_id.clone(),
            page_id: Some(page_id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(command),
        })
        .await
}

fn completed(outcome: &CommandOutcome) -> bool {
    matches!(outcome, CommandOutcome::Completed { .. })
}

fn contains_accessible_node(nodes: &[AccessibilityNode], role: &str, name: &str) -> bool {
    nodes.iter().any(|node| {
        (node.role.as_deref() == Some(role) && node.name.as_deref() == Some(name))
            || contains_accessible_node(&node.children, role, name)
    })
}

async fn accessibility_snapshot(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
) -> UnlockResult<Vec<AccessibilityNode>> {
    match submit(
        runtime,
        session_id,
        page_id,
        PrimitiveCommand::AccessibilitySnapshot(AccessibilitySnapshotCommand {
            max_nodes: Some(256),
            target: None,
        }),
    )
    .await
    {
        CommandOutcome::Completed { evidence, .. } => evidence
            .into_iter()
            .find_map(|item| match item {
                Evidence::AccessibilitySnapshot { nodes, .. } => Some(nodes),
                _ => None,
            })
            .ok_or_else(|| "accessibility snapshot completed without evidence".into()),
        outcome => Err(format!("accessibility snapshot failed: {outcome:?}").into()),
    }
}

async fn wait_visible(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
    selector: &str,
) -> UnlockResult<()> {
    let outcome = submit(
        runtime,
        session_id,
        page_id,
        PrimitiveCommand::WaitFor(WaitForCommand {
            condition: WaitCondition::Element {
                target: Box::new(css_target(selector)),
                state: ElementState::Visible,
            },
            timeout_ms: 10_000,
        }),
    )
    .await;
    if completed(&outcome) {
        Ok(())
    } else {
        Err(format!("wait {selector} failed: {outcome:?}").into())
    }
}

fn named_target(role: &str, accessible_name: &str) -> TargetSpec {
    TargetSpec {
        role: Some(role.into()),
        accessible_name: Some(accessible_name.into()),
        ..TargetSpec::default()
    }
}

fn name_hints(role: &str, accessible_name: &str) -> IntentHints {
    IntentHints {
        role: Some(role.into()),
        accessible_name: Some(accessible_name.into()),
        ..IntentHints::default()
    }
}

fn visible_cmd(role: &str, accessible_name: &str) -> WaitForCommand {
    WaitForCommand {
        condition: WaitCondition::Element {
            target: Box::new(named_target(role, accessible_name)),
            state: ElementState::Visible,
        },
        timeout_ms: 10_000,
    }
}

async fn submit_intent(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
    command: IntentCommand,
) -> CommandOutcome {
    runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session_id.clone(),
            page_id: Some(page_id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Intent(command),
        })
        .await
}

/// Follow a control (`intent_follow`) and wait for `expected_destination`.
/// Not a boundary action: dismissing the cookie banner has no side effect
/// worth a checkpoint, matching the plain `Click` this replaced.
async fn follow(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
    purpose: &str,
    role: &str,
    accessible_name: &str,
    expected_destination: WaitForCommand,
) -> UnlockResult<()> {
    let outcome = submit_intent(
        runtime,
        session_id,
        page_id,
        IntentCommand::Follow(FollowIntent {
            purpose: purpose.into(),
            hints: name_hints(role, accessible_name),
            expected_destination,
            boundary: false,
        }),
    )
    .await;
    if completed(&outcome) {
        Ok(())
    } else {
        Err(format!("follow {accessible_name} failed: {outcome:?}").into())
    }
}

/// Fill every field of a form in one `intent_complete_form` call. A field
/// whose `revealed_by` hints are set is not in the DOM until that control is
/// clicked -- the engine clicks it and waits for the field to appear before
/// filling it (see `intent-engine`'s `reveal_field`).
async fn complete_form(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
    purpose: &str,
    fields: Vec<CompleteFormField>,
) -> UnlockResult<()> {
    let outcome = submit_intent(
        runtime,
        session_id,
        page_id,
        IntentCommand::CompleteForm(CompleteFormIntent {
            purpose: purpose.into(),
            fields,
        }),
    )
    .await;
    if completed(&outcome) {
        Ok(())
    } else {
        Err(format!("completeForm {purpose} failed: {outcome:?}").into())
    }
}

/// Click a submit control (`intent_submit_and_verify`) and wait for
/// `expected_state`, which only holds once the submission actually lands.
/// `SubmitAndVerify` is always boundary-class, so it needs a verified
/// pre-action checkpoint first: inspect the page for its url/title, save a
/// checkpoint naming the command about to run, then submit that exact
/// command (mirrors `ModernRuntime::submit_intent_boundary`).
async fn submit_and_verify(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
    purpose: &str,
    accessible_name: &str,
    expected_state: WaitForCommand,
) -> UnlockResult<()> {
    let workflow_id = WorkflowId::new();
    let attempt_id = AttemptId::new();
    let inspect_id = CommandId::new();
    let preflight = runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: inspect_id.clone(),
            workflow_id: workflow_id.clone(),
            attempt_id: attempt_id.clone(),
            session_id: session_id.clone(),
            page_id: Some(page_id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(
                PrimitiveCommand::Inspect(InspectCommand::default()),
            ),
        })
        .await;
    let observed = match preflight {
        CommandOutcome::Completed { evidence, .. } => evidence,
        other => return Err(format!("submitAndVerify preflight failed: {other:?}").into()),
    };
    let (url, title) = observed
        .iter()
        .find_map(|item| match item.journal_safe() {
            Evidence::Inspection { url, title, .. } => Some((url, title)),
            _ => None,
        })
        .ok_or("submitAndVerify preflight completed without inspection evidence")?;
    let command_id = CommandId::new();
    runtime
        .checkpoint(
            WorkflowCheckpoint {
                schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
                checkpoint_id: CheckpointId::new(),
                workflow_id: workflow_id.clone(),
                attempt_id: attempt_id.clone(),
                session_id: session_id.clone(),
                page_id: page_id.clone(),
                restart_url: url.clone(),
                current_url: url.clone(),
                cursor: Some(inspect_id.clone()),
                boundary_command_id: Some(command_id.clone()),
                recovery_class: CommandClass::Boundary,
                invariants: vec![
                    CheckpointInvariant::Url { value: url },
                    CheckpointInvariant::Title { value: title },
                ],
                replayable_inputs: Vec::new(),
                evidence: Vec::new(),
                recovery_history: Vec::new(),
                recovery_receipts: Vec::new(),
                created_at: Utc::now(),
            },
            vec![inspect_id],
        )
        .await?;
    let outcome = runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id,
            workflow_id,
            attempt_id,
            session_id: session_id.clone(),
            page_id: Some(page_id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Intent(IntentCommand::SubmitAndVerify(
                SubmitAndVerifyIntent {
                    purpose: purpose.into(),
                    hints: name_hints("button", accessible_name),
                    expected_state,
                },
            )),
        })
        .await;
    if completed(&outcome) {
        Ok(())
    } else {
        Err(format!("submitAndVerify {accessible_name} failed: {outcome:?}").into())
    }
}

/// Cookie banner → operator sign-in → MFA, in three intent calls: `follow`
/// the cookie accept control, `completeForm` the sign-in fields plus the MFA
/// code (revealed by submitting the sign-in form), then `submitAndVerify`
/// the code. Call after Navigate to a Northstar URL on a fresh
/// `RuntimeService` session. No-op if the shell is already up.
pub async fn unlock_northstar_session(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
) -> UnlockResult<()> {
    wait_visible(runtime, session_id, page_id, "#app > *").await?;
    let nodes = accessibility_snapshot(runtime, session_id, page_id).await?;
    if contains_accessible_node(&nodes, "navigation", "Primary navigation") {
        return Ok(());
    }

    if contains_accessible_node(&nodes, "button", "Accept all cookies") {
        follow(
            runtime,
            session_id,
            page_id,
            "dismiss the cookie banner",
            "button",
            "Accept all cookies",
            WaitForCommand {
                condition: WaitCondition::Element {
                    target: Box::new(named_target("dialog", "Cookie preferences")),
                    state: ElementState::Detached,
                },
                timeout_ms: 10_000,
            },
        )
        .await?;
    }

    complete_form(
        runtime,
        session_id,
        page_id,
        "operator sign in",
        vec![
            CompleteFormField {
                name: "email".into(),
                purpose: "work email".into(),
                hints: name_hints("textbox", "Work email"),
                value: ControlAction::SetText {
                    value: OPERATOR_EMAIL.into(),
                    clear_first: true,
                },
                revealed_by: None,
            },
            CompleteFormField {
                name: "password".into(),
                purpose: "password".into(),
                hints: name_hints("textbox", "Password"),
                value: ControlAction::SetText {
                    value: OPERATOR_PASSWORD.into(),
                    clear_first: true,
                },
                revealed_by: None,
            },
            CompleteFormField {
                name: "code".into(),
                purpose: "authentication code".into(),
                hints: name_hints("textbox", "Authentication code"),
                value: ControlAction::SetText {
                    value: MFA_CODE.into(),
                    clear_first: true,
                },
                revealed_by: Some(name_hints("button", "Continue")),
            },
        ],
    )
    .await?;

    submit_and_verify(
        runtime,
        session_id,
        page_id,
        "verify the authentication code",
        "Verify code",
        visible_cmd("navigation", "Primary navigation"),
    )
    .await?;

    Ok(())
}
