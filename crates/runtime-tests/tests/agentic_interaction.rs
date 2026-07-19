use std::path::PathBuf;

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, ServerConfig, StorageConfig};
use sdk_core::RuntimeService;
use types::{
    AttemptId, CaptureScreenshotCommand, ClickCommand, CommandEnvelope, CommandId, CommandOutcome,
    CreateSessionRequest, ElementState, ErrorCode, Evidence, NavigateCommand, OpenPageRequest,
    PageId, PrimitiveCommand, ScreenshotMode, SessionId, TargetSpec, TextMatch, TypeTextCommand,
    WaitCondition, WaitForCommand, WaitUntil, WorkflowId,
};

fn envelope(
    session_id: &SessionId,
    page_id: &PageId,
    command: PrimitiveCommand,
) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: session_id.clone(),
        page_id: Some(page_id.clone()),
        deadline: Utc::now() + Duration::seconds(30),
        command,
    }
}

async fn completed(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
    command: PrimitiveCommand,
) -> Vec<Evidence> {
    match runtime.submit(envelope(session_id, page_id, command)).await {
        CommandOutcome::Completed { evidence, .. } => evidence,
        outcome => panic!("command did not complete: {outcome:?}"),
    }
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn completes_semantic_drift_frame_shadow_wait_and_artifact_workflow() {
    let child = test_site::spawn().await;
    let host = test_site::spawn_frame_host(&child.base_url()).await;
    let root = tempfile::tempdir().unwrap();
    let artifacts_dir = root.path().join("artifacts");
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
            profiles_dir: root.path().join("profiles"),
            headless: true,
            max_active: 1,
            upload_roots: vec![root.path().to_path_buf()],
            downloads_dir: root.path().join("downloads"),
            artifacts_dir: artifacts_dir.clone(),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
        },
        storage: StorageConfig {
            journal_path: root.path().join("commands.jsonl"),
            checkpoints_dir: root.path().join("checkpoints"),
        },
        interface: config::InterfaceConfig::default(),
    };
    let runtime = RuntimeService::build(&config).await.unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "agentic-proof".into(),
            proxy: None,
        })
        .await
        .unwrap();
    let page = runtime
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();
    completed(
        &runtime,
        &session.id,
        &page.id,
        PrimitiveCommand::Navigate(NavigateCommand {
            url: host.base_url(),
            wait_until: WaitUntil::Interactive,
            timeout_ms: 10_000,
        }),
    )
    .await;

    let frame_path = vec![
        Box::new(TargetSpec {
            role: Some("iframe".into()),
            accessible_name: Some("Outer".into()),
            ..TargetSpec::default()
        }),
        Box::new(TargetSpec {
            role: Some("iframe".into()),
            accessible_name: Some("Cross".into()),
            ..TargetSpec::default()
        }),
    ];
    completed(
        &runtime,
        &session.id,
        &page.id,
        PrimitiveCommand::TypeText(TypeTextCommand {
            selector: String::new(),
            target: Some(TargetSpec {
                label: Some("Name".into()),
                frame_path,
                ..TargetSpec::default()
            }),
            value: "Ada".into(),
            clear_first: true,
        }),
    )
    .await;

    let drift = TargetSpec {
        role: Some("button".into()),
        accessible_name: Some("Drift action".into()),
        ..TargetSpec::default()
    };
    for _ in 0..2 {
        completed(
            &runtime,
            &session.id,
            &page.id,
            PrimitiveCommand::Click(ClickCommand {
                selector: String::new(),
                target: Some(drift.clone()),
                boundary: false,
                expected_url: None,
            }),
        )
        .await;
    }
    assert!(matches!(
        runtime
            .submit(envelope(
                &session.id,
                &page.id,
                PrimitiveCommand::Click(ClickCommand {
                    selector: String::new(),
                    target: Some(TargetSpec {
                        role: Some("button".into()),
                        accessible_name: Some("Ambiguous".into()),
                        ..TargetSpec::default()
                    }),
                    boundary: false,
                    expected_url: None,
                })
            ))
            .await,
        CommandOutcome::Failed { error, .. } if error.code == ErrorCode::TargetAmbiguous
    ));

    completed(
        &runtime,
        &session.id,
        &page.id,
        PrimitiveCommand::WaitFor(WaitForCommand {
            condition: WaitCondition::Text {
                target: Box::new(TargetSpec {
                    css: Some("#status".into()),
                    ..TargetSpec::default()
                }),
                matcher: TextMatch::Exact("ready".into()),
            },
            timeout_ms: 2_000,
        }),
    )
    .await;
    completed(
        &runtime,
        &session.id,
        &page.id,
        PrimitiveCommand::WaitFor(WaitForCommand {
            condition: WaitCondition::Element {
                target: Box::new(TargetSpec {
                    css: Some("#old-action".into()),
                    ..TargetSpec::default()
                }),
                state: ElementState::Detached,
            },
            timeout_ms: 2_000,
        }),
    )
    .await;

    let screenshot = completed(
        &runtime,
        &session.id,
        &page.id,
        PrimitiveCommand::CaptureScreenshot(CaptureScreenshotCommand {
            mode: ScreenshotMode::Element {
                target: Box::new(TargetSpec {
                    role: Some("button".into()),
                    accessible_name: Some("Inside".into()),
                    shadow_path: vec![Box::new(TargetSpec {
                        css: Some("#host".into()),
                        ..TargetSpec::default()
                    })],
                    ..TargetSpec::default()
                }),
            },
        }),
    )
    .await;
    let artifact_id = screenshot
        .iter()
        .find_map(|evidence| match evidence {
            Evidence::Screenshot {
                artifact_id,
                bytes,
                sha256,
                ..
            } if *bytes > 0 && sha256.len() == 64 => Some(artifact_id),
            _ => None,
        })
        .expect("hashed screenshot evidence");
    assert!(artifacts_dir
        .join(session.id.0.to_string())
        .join(artifact_id)
        .join(format!("{artifact_id}.png"))
        .is_file());
}
