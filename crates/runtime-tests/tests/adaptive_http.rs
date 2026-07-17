use std::path::PathBuf;

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, HttpConfig, ServerConfig, StorageConfig};
use sdk_core::RuntimeService;
use types::{
    AttemptId, CheckpointId, CheckpointInvariant, CommandClass, CommandEnvelope, CommandId,
    CommandOutcome, CreateSessionRequest, DownloadUrlCommand, Evidence, ExecutionPath,
    InspectCommand, NavigateCommand, OpenPageRequest, PageId, PrimitiveCommand, RecoveryDecision,
    TargetSpec, WaitUntil, WorkflowCheckpoint, WorkflowId,
};

fn envelope(
    session_id: &types::SessionId,
    page_id: &PageId,
    workflow_id: &WorkflowId,
    command: PrimitiveCommand,
) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: workflow_id.clone(),
        attempt_id: AttemptId::new(),
        session_id: session_id.clone(),
        page_id: Some(page_id.clone()),
        deadline: Utc::now() + Duration::seconds(30),
        command,
    }
}

async fn submit(
    runtime: &RuntimeService,
    session_id: &types::SessionId,
    page_id: &PageId,
    workflow_id: &WorkflowId,
    command: PrimitiveCommand,
) -> Vec<Evidence> {
    let command_debug = format!("{command:?}");
    match runtime
        .submit(envelope(session_id, page_id, workflow_id, command))
        .await
    {
        CommandOutcome::Completed { evidence, .. } => evidence,
        outcome => panic!("command {command_debug} did not complete: {outcome:?}"),
    }
}

fn path(evidence: &[Evidence], expected: ExecutionPath) {
    assert!(evidence
        .iter()
        .any(|item| matches!(item, Evidence::ExecutionPath { path, .. } if path == &expected)));
}

fn inspection_text(evidence: &[Evidence]) -> String {
    evidence
        .iter()
        .find_map(|item| match item {
            Evidence::Inspection { text, .. } => {
                Some(text.split_whitespace().collect::<Vec<_>>().join(" "))
            }
            _ => None,
        })
        .expect("inspection evidence")
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn adaptive_http() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let config = AppConfig {
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
            upload_roots: vec![root.path().join("uploads")],
            downloads_dir: root.path().join("downloads"),
            artifacts_dir: root.path().join("artifacts"),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
        },
        storage: StorageConfig {
            journal_path: root.path().join("commands.jsonl"),
            checkpoints_dir: root.path().join("checkpoints"),
        },
        http: HttpConfig {
            allow_loopback: true,
            max_concurrent_requests: 4,
            ..HttpConfig::default()
        },
    };
    let runtime = RuntimeService::build(&config).await.unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "adaptive-http-live".into(),
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
    let workflow = WorkflowId::new();
    let static_url = format!("{}/static", fixture.base_url());

    submit(
        &runtime,
        &session.id,
        &page.id,
        &workflow,
        PrimitiveCommand::Navigate(NavigateCommand {
            url: static_url.clone(),
            wait_until: WaitUntil::Interactive,
            timeout_ms: 10_000,
        }),
    )
    .await;
    let chromium_static = submit(
        &runtime,
        &session.id,
        &page.id,
        &workflow,
        PrimitiveCommand::Inspect(InspectCommand {
            selector: None,
            target: Some(TargetSpec {
                role: Some("status".into()),
                ..TargetSpec::default()
            }),
            include_html: false,
        }),
    )
    .await;
    path(&chromium_static, ExecutionPath::Chromium);
    let direct_static = submit(
        &runtime,
        &session.id,
        &page.id,
        &workflow,
        PrimitiveCommand::Inspect(InspectCommand::default()),
    )
    .await;
    path(&direct_static, ExecutionPath::DirectHttp);
    assert!(inspection_text(&direct_static).contains(&inspection_text(&chromium_static)));

    println!("live phase: dynamic chromium fallback");
    submit(
        &runtime,
        &session.id,
        &page.id,
        &workflow,
        PrimitiveCommand::Navigate(NavigateCommand {
            url: format!("{}/js-shell", fixture.base_url()),
            wait_until: WaitUntil::Interactive,
            timeout_ms: 10_000,
        }),
    )
    .await;
    let dynamic = submit(
        &runtime,
        &session.id,
        &page.id,
        &workflow,
        PrimitiveCommand::Inspect(InspectCommand::default()),
    )
    .await;
    path(&dynamic, ExecutionPath::ChromiumFallback);
    assert!(inspection_text(&dynamic).contains("rendered fixture"));

    println!("live phase: explicit direct download");
    let download = submit(
        &runtime,
        &session.id,
        &page.id,
        &workflow,
        PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
            url: format!("{}/download", fixture.base_url()),
            expected_content_type: Some("application/octet-stream".into()),
            max_bytes: 20,
        }),
    )
    .await;
    path(&download, ExecutionPath::DirectHttp);
    let (artifact_id, sha256) = download
        .iter()
        .find_map(|item| match item {
            Evidence::Download {
                filename,
                path,
                bytes,
                sha256,
            } => {
                assert_eq!(filename, "workflow-fixture.bin");
                assert_eq!(*bytes, 20);
                Some((path.clone(), sha256.clone()))
            }
            _ => None,
        })
        .expect("download evidence");
    assert_eq!(
        sha256,
        "c0613f7c18f7f41e5720bb3d95b6f6411e8a8b2f3b08d1ad011760069f3949ed"
    );
    assert!(root
        .path()
        .join("artifacts")
        .join(session.id.0.to_string())
        .join(&artifact_id)
        .is_dir());

    println!("live phase: browser-visible cookie and recovery");
    let cookie_url = format!("{}/cookie-echo", fixture.base_url());
    submit(
        &runtime,
        &session.id,
        &page.id,
        &workflow,
        PrimitiveCommand::Navigate(NavigateCommand {
            url: cookie_url.clone(),
            wait_until: WaitUntil::Interactive,
            timeout_ms: 10_000,
        }),
    )
    .await;
    let cookie = submit(
        &runtime,
        &session.id,
        &page.id,
        &workflow,
        PrimitiveCommand::Inspect(InspectCommand {
            selector: None,
            target: Some(TargetSpec {
                role: Some("status".into()),
                ..TargetSpec::default()
            }),
            include_html: false,
        }),
    )
    .await;
    assert!(inspection_text(&cookie).contains("downloaded=yes"));

    let checkpoint = WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: CheckpointId::new(),
        workflow_id: workflow.clone(),
        attempt_id: AttemptId::new(),
        session_id: session.id.clone(),
        page_id: page.id.clone(),
        restart_url: cookie_url.clone(),
        current_url: cookie_url.clone(),
        cursor: None,
        boundary_command_id: None,
        recovery_class: CommandClass::Reconciliable,
        invariants: vec![CheckpointInvariant::Url {
            value: cookie_url.clone(),
        }],
        replayable_inputs: Vec::new(),
        evidence: Vec::new(),
        recovery_history: Vec::new(),
        created_at: Utc::now(),
    };
    runtime.checkpoint(checkpoint, cookie).await.unwrap();
    assert!(matches!(
        runtime.recover(&workflow).await.unwrap(),
        RecoveryDecision::Resumed { .. }
    ));
    let recovered = submit(
        &runtime,
        &session.id,
        &page.id,
        &workflow,
        PrimitiveCommand::Inspect(InspectCommand::default()),
    )
    .await;
    path(&recovered, ExecutionPath::DirectHttp);
    let recovered_chromium = submit(
        &runtime,
        &session.id,
        &page.id,
        &workflow,
        PrimitiveCommand::Inspect(InspectCommand {
            selector: None,
            target: Some(TargetSpec {
                role: Some("status".into()),
                ..TargetSpec::default()
            }),
            include_html: false,
        }),
    )
    .await;
    assert!(
        inspection_text(&recovered).contains(&inspection_text(&recovered_chromium)),
        "recovered direct HTTP and Chromium must observe coherent cookie state"
    );

    println!(
        "adaptive HTTP live proof: directHttp static+download+recovered, chromiumFallback dynamic, artifact={artifact_id}, sha256={sha256}"
    );
}
