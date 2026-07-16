use std::path::PathBuf;

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, ServerConfig, StorageConfig};
use sdk_core::RuntimeService;
use types::{
    AttemptId, CheckpointId, CheckpointInvariant, ClickCommand, CommandClass, CommandEnvelope,
    CommandId, CommandOutcome, CommandPhase, CreateSessionRequest, Evidence, InspectCommand,
    NavigateCommand, OpenPageRequest, PageId, PrimitiveCommand, SessionId, TypeTextCommand,
    WaitUntil, WorkflowCheckpoint, WorkflowId,
};
use workflow_journal::{CommandJournal, JsonlJournal};

fn envelope(
    session_id: &SessionId,
    page_id: &PageId,
    command_id: CommandId,
    command: PrimitiveCommand,
) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id,
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: session_id.clone(),
        page_id: Some(page_id.clone()),
        deadline: Utc::now() + Duration::seconds(30),
        command,
    }
}

async fn submit(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
    command_ids: &mut Vec<CommandId>,
    command: PrimitiveCommand,
) -> Vec<Evidence> {
    let command_id = CommandId::new();
    command_ids.push(command_id.clone());
    let command_debug = format!("{command:?}");
    match runtime
        .submit(envelope(session_id, page_id, command_id, command))
        .await
    {
        CommandOutcome::Completed { evidence, .. } => evidence,
        outcome => panic!("command {command_debug} did not complete: {outcome:?}"),
    }
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn completes_dynamic_form_with_durable_evidence() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let journal_path = root.path().join("commands.jsonl");
    let profiles_dir = root.path().join("profiles");
    let config = AppConfig {
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
        },
        browser: BrowserConfig {
            executable: Some(PathBuf::from(
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            )),
            profiles_dir: profiles_dir.clone(),
            headless: true,
            max_active: 8,
        },
        storage: StorageConfig {
            journal_path: journal_path.clone(),
            checkpoints_dir: root.path().join("checkpoints"),
        },
    };
    let runtime = RuntimeService::build(&config).await.unwrap();
    let first_session = runtime
        .create_session(CreateSessionRequest {
            profile: "workflow-primary".into(),
            proxy: None,
        })
        .await
        .unwrap();
    let page = runtime
        .open_page(OpenPageRequest {
            session_id: first_session.id.clone(),
        })
        .await
        .unwrap();
    let second_session = runtime
        .create_session(CreateSessionRequest {
            profile: "workflow-secondary".into(),
            proxy: None,
        })
        .await
        .unwrap();
    let mut command_ids = Vec::new();

    let navigation = submit(
        &runtime,
        &first_session.id,
        &page.id,
        &mut command_ids,
        PrimitiveCommand::Navigate(NavigateCommand {
            url: fixture.base_url(),
            wait_until: WaitUntil::Interactive,
            timeout_ms: 10_000,
        }),
    )
    .await;
    assert!(navigation.iter().any(
        |item| matches!(item, Evidence::Navigation { title, .. } if title == "Runtime Fixture")
    ));

    submit(
        &runtime,
        &first_session.id,
        &page.id,
        &mut command_ids,
        PrimitiveCommand::TypeText(TypeTextCommand {
            selector: "#name".into(),
            value: "Ada".into(),
            clear_first: true,
        }),
    )
    .await;
    submit(
        &runtime,
        &first_session.id,
        &page.id,
        &mut command_ids,
        PrimitiveCommand::Click(ClickCommand {
            selector: "#continue".into(),
            boundary: false,
            expected_url: None,
        }),
    )
    .await;

    let mut company_ready = false;
    for _ in 0..20 {
        let command_id = CommandId::new();
        command_ids.push(command_id.clone());
        let outcome = runtime
            .submit(envelope(
                &first_session.id,
                &page.id,
                command_id,
                PrimitiveCommand::Inspect(InspectCommand {
                    selector: Some("#company".into()),
                    include_html: false,
                }),
            ))
            .await;
        if matches!(outcome, CommandOutcome::Completed { .. }) {
            company_ready = true;
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    assert!(company_ready, "dynamic second step did not appear");

    submit(
        &runtime,
        &first_session.id,
        &page.id,
        &mut command_ids,
        PrimitiveCommand::TypeText(TypeTextCommand {
            selector: "#company".into(),
            value: "Analytical Engines".into(),
            clear_first: true,
        }),
    )
    .await;
    let expected_url = format!("{}/complete", fixture.base_url());
    let workflow_id = WorkflowId::new();
    let attempt_id = AttemptId::new();
    let inspect_id = CommandId::new();
    command_ids.push(inspect_id.clone());
    let observed = match runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: inspect_id,
            workflow_id: workflow_id.clone(),
            attempt_id: attempt_id.clone(),
            session_id: first_session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: PrimitiveCommand::Inspect(InspectCommand::default()),
        })
        .await
    {
        CommandOutcome::Completed { evidence, .. } => evidence,
        outcome => panic!("pre-boundary inspection failed: {outcome:?}"),
    };
    let (current_url, title) = observed
        .iter()
        .find_map(|item| match item {
            Evidence::Inspection { url, title, .. } => Some((url.clone(), title.clone())),
            _ => None,
        })
        .expect("pre-boundary browser evidence");
    let boundary_id = CommandId::new();
    runtime
        .checkpoint(
            WorkflowCheckpoint {
                schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
                checkpoint_id: CheckpointId::new(),
                workflow_id: workflow_id.clone(),
                attempt_id: attempt_id.clone(),
                session_id: first_session.id.clone(),
                page_id: page.id.clone(),
                restart_url: fixture.base_url(),
                current_url: current_url.clone(),
                cursor: Some(command_ids.last().unwrap().clone()),
                boundary_command_id: Some(boundary_id.clone()),
                recovery_class: CommandClass::Boundary,
                invariants: vec![
                    CheckpointInvariant::Url { value: current_url },
                    CheckpointInvariant::Title { value: title },
                ],
                replayable_inputs: vec!["Ada".into(), "Analytical Engines".into()],
                evidence: Vec::new(),
                recovery_history: Vec::new(),
                created_at: Utc::now(),
            },
            observed,
        )
        .await
        .unwrap();
    command_ids.push(boundary_id.clone());
    match runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: boundary_id,
            workflow_id,
            attempt_id,
            session_id: first_session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: PrimitiveCommand::Click(ClickCommand {
                selector: "#submit".into(),
                boundary: true,
                expected_url: Some(expected_url.clone()),
            }),
        })
        .await
    {
        CommandOutcome::Completed { .. } => {}
        outcome => panic!("boundary command failed: {outcome:?}"),
    }
    let result = submit(
        &runtime,
        &first_session.id,
        &page.id,
        &mut command_ids,
        PrimitiveCommand::Inspect(InspectCommand {
            selector: Some("#result".into()),
            include_html: true,
        }),
    )
    .await;
    assert!(result.iter().any(|item| {
        matches!(item, Evidence::Inspection { url, text, .. }
            if url == &expected_url && text == "Submitted: Ada @ Analytical Engines")
    }));

    let first_profile = profiles_dir.join(first_session.id.0.to_string());
    let second_profile = profiles_dir.join(second_session.id.0.to_string());
    assert_ne!(first_profile, second_profile);
    assert!(first_profile.is_dir());
    assert!(second_profile.is_dir());

    let journal = JsonlJournal::open(&journal_path).await.unwrap();
    for command_id in &command_ids {
        let scan = journal.history(command_id.clone()).await.unwrap();
        let phases: Vec<_> = scan.records.iter().map(|record| record.phase).collect();
        let prepared = phases
            .iter()
            .position(|phase| *phase == CommandPhase::Prepared)
            .expect("prepared phase");
        let executing = phases
            .iter()
            .position(|phase| *phase == CommandPhase::Executing)
            .expect("executing phase");
        assert!(prepared < executing);
        assert_eq!(
            phases
                .iter()
                .filter(|phase| { matches!(phase, CommandPhase::Completed | CommandPhase::Failed) })
                .count(),
            1
        );
    }

    println!(
        "verified url={expected_url} result='Submitted: Ada @ Analytical Engines' commands={} profiles=({}, {})",
        command_ids.len(),
        first_profile.display(),
        second_profile.display()
    );
    runtime.sessions.delete(&first_session.id).await.unwrap();
    runtime.sessions.delete(&second_session.id).await.unwrap();
}
