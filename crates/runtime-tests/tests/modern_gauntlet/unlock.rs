use chrono::{Duration, Utc};
use gauntlet_server::{MFA_CODE, OPERATOR_EMAIL, OPERATOR_PASSWORD};
use sdk_core::RuntimeService;
use types::{
    AccessibilityNode, AccessibilitySnapshotCommand, AttemptId, ClickCommand, CommandEnvelope,
    CommandId, CommandOutcome, ElementState, Evidence, PageId, PrimitiveCommand, RuntimeCommand,
    SessionId, TargetSpec, TypeTextCommand, WaitCondition, WaitForCommand, WorkflowId,
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

async fn click(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
    selector: &str,
) -> UnlockResult<()> {
    let outcome = submit(
        runtime,
        session_id,
        page_id,
        PrimitiveCommand::Click(ClickCommand {
            selector: selector.into(),
            target: None,
            boundary: false,
            expected_url: None,
            modifiers: Vec::new(),
        }),
    )
    .await;
    if completed(&outcome) {
        Ok(())
    } else {
        Err(format!("click {selector} failed: {outcome:?}").into())
    }
}

async fn type_text(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
    selector: &str,
    value: &str,
) -> UnlockResult<()> {
    let outcome = submit(
        runtime,
        session_id,
        page_id,
        PrimitiveCommand::TypeText(TypeTextCommand {
            selector: selector.into(),
            target: None,
            value: value.into(),
            clear_first: true,
            expected_url: None,
        }),
    )
    .await;
    if completed(&outcome) {
        Ok(())
    } else {
        Err(format!("type {selector} failed: {outcome:?}").into())
    }
}

/// Cookie banner → operator sign-in → MFA. Call after Navigate to a Northstar
/// URL on a fresh `RuntimeService` session. No-op if the shell is already up.
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
        click(
            runtime,
            session_id,
            page_id,
            "button[aria-label='Accept all cookies']",
        )
        .await?;
    }
    wait_visible(
        runtime,
        session_id,
        page_id,
        "form[aria-label='Operator sign in']",
    )
    .await?;
    type_text(
        runtime,
        session_id,
        page_id,
        "input[aria-label='Work email']",
        OPERATOR_EMAIL,
    )
    .await?;
    type_text(
        runtime,
        session_id,
        page_id,
        "input[aria-label='Password']",
        OPERATOR_PASSWORD,
    )
    .await?;
    click(
        runtime,
        session_id,
        page_id,
        "form[aria-label='Operator sign in'] button[type='submit']",
    )
    .await?;
    wait_visible(
        runtime,
        session_id,
        page_id,
        "form[aria-label='Multi-factor authentication']",
    )
    .await?;
    type_text(
        runtime,
        session_id,
        page_id,
        "input[aria-label='Authentication code']",
        MFA_CODE,
    )
    .await?;
    click(
        runtime,
        session_id,
        page_id,
        "form[aria-label='Multi-factor authentication'] button[type='submit']",
    )
    .await?;
    wait_visible(
        runtime,
        session_id,
        page_id,
        "nav[aria-label='Primary navigation']",
    )
    .await?;
    Ok(())
}
