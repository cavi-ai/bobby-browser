use std::path::PathBuf;

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, ServerConfig, StorageConfig};
use sdk_core::RuntimeService;
use types::{
    AttemptId, CheckpointId, CheckpointInvariant, ClickAndWaitForDownloadCommand,
    ClickAndWaitForPopupCommand, ClickCommand, ClosePageCommand, CommandClass, CommandEnvelope,
    CommandId, CommandOutcome, CommandPhase, CreateSessionRequest, Evidence, InspectCommand,
    ListPagesCommand, NavigateCommand, OpenPageCommand, OpenPageRequest, PageId, PrimitiveCommand,
    SessionId, TypeTextCommand, UploadFilesCommand, WaitUntil, WorkflowCheckpoint, WorkflowId,
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

async fn submit_boundary(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
    command: PrimitiveCommand,
) -> Vec<Evidence> {
    let workflow_id = WorkflowId::new();
    let attempt_id = AttemptId::new();
    let inspect_id = CommandId::new();
    let observed = match runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: inspect_id.clone(),
            workflow_id: workflow_id.clone(),
            attempt_id: attempt_id.clone(),
            session_id: session_id.clone(),
            page_id: Some(page_id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: PrimitiveCommand::Inspect(InspectCommand::default()),
        })
        .await
    {
        CommandOutcome::Completed { evidence, .. } => evidence,
        outcome => panic!("boundary preflight failed: {outcome:?}"),
    };
    let (url, title) = observed
        .iter()
        .find_map(|item| match item {
            Evidence::Inspection { url, title, .. } => Some((url.clone(), title.clone())),
            _ => None,
        })
        .unwrap();
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
                cursor: Some(inspect_id),
                boundary_command_id: Some(command_id.clone()),
                recovery_class: CommandClass::Boundary,
                invariants: vec![
                    CheckpointInvariant::Url { value: url },
                    CheckpointInvariant::Title { value: title },
                ],
                replayable_inputs: Vec::new(),
                evidence: Vec::new(),
                recovery_history: Vec::new(),
                created_at: Utc::now(),
            },
            observed,
        )
        .await
        .unwrap();
    match runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id,
            workflow_id,
            attempt_id,
            session_id: session_id.clone(),
            page_id: Some(page_id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command,
        })
        .await
    {
        CommandOutcome::Completed { evidence, .. } => evidence,
        outcome => panic!("boundary command failed: {outcome:?}"),
    }
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn completes_dynamic_form_with_durable_evidence() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let journal_path = root.path().join("commands.jsonl");
    let profiles_dir = root.path().join("profiles");
    let uploads_dir = root.path().join("uploads");
    std::fs::create_dir(&uploads_dir).unwrap();
    let resume = uploads_dir.join("resume.txt");
    std::fs::write(&resume, b"Ada Lovelace").unwrap();
    let config = AppConfig {
        http: config::HttpConfig {
            allow_loopback: true,
            ..config::HttpConfig::default()
        },
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
            upload_roots: vec![uploads_dir],
            downloads_dir: root.path().join("downloads"),
            artifacts_dir: root.path().join("artifacts"),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
        },
        storage: StorageConfig {
            journal_path: journal_path.clone(),
            checkpoints_dir: root.path().join("checkpoints"),
            authority_path: root.path().join("authority.json"),
        },
        interface: config::InterfaceConfig::default(),
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

    let uploaded = submit(
        &runtime,
        &first_session.id,
        &page.id,
        &mut command_ids,
        PrimitiveCommand::UploadFiles(UploadFilesCommand {
            selector: "#resume".into(),
            target: None,
            paths: vec![resume.to_string_lossy().into_owned()],
        }),
    )
    .await;
    assert!(matches!(&uploaded[0], Evidence::Upload { paths, .. } if paths.len() == 1));
    println!("workflow-proof upload={uploaded:?}");

    submit(
        &runtime,
        &first_session.id,
        &page.id,
        &mut command_ids,
        PrimitiveCommand::TypeText(TypeTextCommand {
            selector: "#name".into(),
            target: None,
            value: "Ada".into(),
            clear_first: true,
        }),
    )
    .await;
    println!("workflow-proof name typed");
    submit(
        &runtime,
        &first_session.id,
        &page.id,
        &mut command_ids,
        PrimitiveCommand::Click(ClickCommand {
            selector: "#continue".into(),
            target: None,
            boundary: false,
            expected_url: None,
        }),
    )
    .await;
    println!("workflow-proof continued");

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
                    selector: None,
                    target: Some(types::TargetSpec {
                        css: Some("#company".into()),
                        ..types::TargetSpec::default()
                    }),
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
    println!("workflow-proof company ready");

    submit(
        &runtime,
        &first_session.id,
        &page.id,
        &mut command_ids,
        PrimitiveCommand::TypeText(TypeTextCommand {
            selector: "#company".into(),
            target: None,
            value: "Analytical Engines".into(),
            clear_first: true,
        }),
    )
    .await;
    println!("workflow-proof company typed");
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
                target: None,
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
            target: None,
            include_html: true,
        }),
    )
    .await;
    assert!(result.iter().any(|item| {
        matches!(item, Evidence::Inspection { url, text, .. }
            if url == &expected_url && text == "Submitted: Ada @ Analytical Engines")
    }));

    let popup = submit_boundary(
        &runtime,
        &first_session.id,
        &page.id,
        PrimitiveCommand::ClickAndWaitForPopup(ClickAndWaitForPopupCommand {
            selector: "#root-popup".into(),
            target: None,
            timeout_ms: 5_000,
        }),
    )
    .await;
    let popup_id = match &popup[0] {
        Evidence::Popup { page_id, title, .. } => {
            assert_eq!(title, "Popup");
            page_id.clone()
        }
        other => panic!("unexpected popup evidence: {other:?}"),
    };
    println!("workflow-proof popup={popup:?}");
    let popup_details = submit(
        &runtime,
        &first_session.id,
        &popup_id,
        &mut command_ids,
        PrimitiveCommand::Inspect(InspectCommand {
            selector: Some("#details".into()),
            target: None,
            include_html: false,
        }),
    )
    .await;
    assert!(matches!(&popup_details[0], Evidence::Inspection { text, .. } if text == "Details"));
    let denied = runtime
        .submit(envelope(
            &second_session.id,
            &popup_id,
            CommandId::new(),
            PrimitiveCommand::Inspect(InspectCommand::default()),
        ))
        .await;
    assert!(
        matches!(denied, CommandOutcome::Failed { error, .. } if error.code == types::ErrorCode::InvalidRequest)
    );

    let opened = submit(
        &runtime,
        &first_session.id,
        &page.id,
        &mut command_ids,
        PrimitiveCommand::OpenPage(OpenPageCommand {
            url: Some(format!("{}/popup", fixture.base_url())),
        }),
    )
    .await;
    let tab_id = match &opened[0] {
        Evidence::Page { page_id, title, .. } => {
            assert_eq!(title, "Popup");
            page_id.clone()
        }
        other => panic!("unexpected page evidence: {other:?}"),
    };
    let listed = submit(
        &runtime,
        &first_session.id,
        &page.id,
        &mut command_ids,
        PrimitiveCommand::ListPages(ListPagesCommand),
    )
    .await;
    assert!(
        matches!(&listed[0], Evidence::Pages { pages } if pages.iter().any(|item| item.page_id == tab_id))
    );

    submit(
        &runtime,
        &first_session.id,
        &page.id,
        &mut command_ids,
        PrimitiveCommand::ClosePage(ClosePageCommand {
            page_id: tab_id.clone(),
        }),
    )
    .await;
    assert!(runtime.pages.get(&tab_id).await.is_err());
    println!("workflow-proof tab={tab_id:?} closed");

    let downloaded = submit_boundary(
        &runtime,
        &first_session.id,
        &page.id,
        PrimitiveCommand::ClickAndWaitForDownload(ClickAndWaitForDownloadCommand {
            selector: "#download".into(),
            target: None,
            timeout_ms: 5_000,
        }),
    )
    .await;
    assert!(
        matches!(&downloaded[0], Evidence::Download { filename, bytes, sha256, .. } if filename == "workflow-fixture.bin" && *bytes == 20 && sha256 == "c0613f7c18f7f41e5720bb3d95b6f6411e8a8b2f3b08d1ad011760069f3949ed")
    );
    println!("workflow-proof download={downloaded:?}");

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
