use chrono::{Duration, Utc};
use gauntlet_server::{MFA_CODE, OPERATOR_EMAIL, OPERATOR_PASSWORD};
use sdk_core::RuntimeService;
use types::{
    AttemptId, ClickCommand, CommandEnvelope, CommandId, CommandOutcome, ElementState, PageId,
    PrimitiveCommand, RuntimeCommand, SessionId, TargetSpec, TypeTextCommand, WaitCondition,
    WaitForCommand, WorkflowId,
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

async fn probe_visible(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
    selector: &str,
    timeout_ms: u64,
) -> bool {
    completed(
        &submit(
            runtime,
            session_id,
            page_id,
            PrimitiveCommand::WaitFor(WaitForCommand {
                condition: WaitCondition::Element {
                    target: Box::new(css_target(selector)),
                    state: ElementState::Visible,
                },
                timeout_ms,
            }),
        )
        .await,
    )
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
    if probe_visible(
        runtime,
        session_id,
        page_id,
        "nav[aria-label='Primary navigation']",
        2_000,
    )
    .await
    {
        return Ok(());
    }
    if probe_visible(
        runtime,
        session_id,
        page_id,
        "button[aria-label='Accept all cookies']",
        8_000,
    )
    .await
    {
        click(
            runtime,
            session_id,
            page_id,
            "button[aria-label='Accept all cookies']",
        )
        .await?;
    }
    if probe_visible(
        runtime,
        session_id,
        page_id,
        "form[aria-label='Operator sign in']",
        8_000,
    )
    .await
    {
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
    }
    wait_visible(
        runtime,
        session_id,
        page_id,
        "nav[aria-label='Primary navigation']",
    )
    .await?;
    Ok(())
}
