use std::path::PathBuf;
use std::sync::Arc;

use artifact_store::{ArtifactError, ArtifactStore};
use checkpoint_store::CheckpointStore;
use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, HttpConfig, ServerConfig, StorageConfig};
use sdk_core::RuntimeService;
use session_manager::SessionManager;
use types::{
    AttemptId, CheckpointId, CheckpointInvariant, CommandClass, CommandEnvelope, CommandId,
    CommandOutcome, CreateSessionRequest, DownloadUrlCommand, Evidence, ExecutionPath,
    InspectCommand, NavigateCommand, OpenPageRequest, PageId, PrimitiveCommand, RecoveryDecision,
    RuntimeCommand, TargetSpec, WaitUntil, WorkflowCheckpoint, WorkflowId,
};
use worker_pool::{ChromiumWorkerFactory, WorkerPool};
use workflow_journal::JsonlJournal;

async fn build_runtime(config: &AppConfig) -> (RuntimeService, Arc<WorkerPool>, ArtifactStore) {
    let journal = Arc::new(
        JsonlJournal::open(&config.storage.journal_path)
            .await
            .unwrap(),
    );
    let workers = Arc::new(WorkerPool::new(
        config.browser.max_active,
        Arc::new(ChromiumWorkerFactory::new(config.browser.clone())),
    ));
    let checkpoints = CheckpointStore::open(&config.storage.checkpoints_dir)
        .await
        .unwrap();
    let recovery =
        page_runtime::RecoveryCoordinator::with_workers(checkpoints.clone(), workers.clone());
    let network = network_engine::NetworkPolicy {
        allow_loopback: config.http.allow_loopback,
        allow_private_network: config.http.allow_private_network,
        max_redirects: config.http.max_redirects,
        max_header_bytes: config.http.max_header_bytes,
        max_body_bytes: config.http.max_body_bytes,
        max_download_bytes: config.http.max_download_bytes,
        request_timeout_ms: config.http.request_timeout_ms,
        max_concurrent_requests: config.http.max_concurrent_requests,
    };
    let artifacts = ArtifactStore::new(
        &config.browser.artifacts_dir,
        config
            .browser
            .max_artifact_bytes
            .max(network.max_download_bytes),
        config.browser.max_screenshot_dimension,
    );
    let adaptive = page_runtime::AdaptivePageEngine::new(
        network_engine::EligibilityPolicy::new(network.clone()),
        network_engine::DirectHttpExecutor::new(network.clone()),
        artifacts.clone(),
        network,
    );
    let pages = page_runtime::PageRuntime::new_adaptive(
        journal,
        workers.clone(),
        Some(checkpoints),
        adaptive,
    );
    let sessions = SessionManager::new(workers.clone());
    (
        RuntimeService::with_recovery(sessions, pages, recovery),
        workers,
        artifacts,
    )
}

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
        command: RuntimeCommand::Primitive(command),
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
            shutdown_timeout_ms: 10_000,
        },
        browser: BrowserConfig {
            executable: Some(PathBuf::from(
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            )),
            profiles_dir: root.path().join("profiles"),
            headless: true,
            max_active: 2,
            upload_roots: vec![root.path().join("uploads")],
            downloads_dir: root.path().join("downloads"),
            artifacts_dir: root.path().join("artifacts"),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
            max_js_result_bytes: 64 * 1024,
            max_js_timeout_ms: 30_000,
        },
        storage: StorageConfig {
            journal_path: root.path().join("commands.jsonl"),
            checkpoints_dir: root.path().join("checkpoints"),
            authority_path: root.path().join("authority.json"),
        },
        http: HttpConfig {
            allow_loopback: true,
            max_concurrent_requests: 4,
            ..HttpConfig::default()
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
    };
    let (runtime, workers, artifacts) = build_runtime(&config).await;
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "adaptive-http-live".into(),
            proxy: None,
            execution_policy: Default::default(),
        })
        .await
        .unwrap();
    let page = runtime
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();
    let stranger = runtime
        .create_session(CreateSessionRequest {
            profile: "adaptive-http-stranger".into(),
            proxy: None,
            execution_policy: Default::default(),
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
    let persisted = artifacts.get(&session.id, &artifact_id).await.unwrap();
    assert_eq!(persisted, b"workflow-download-v1");
    assert_eq!(persisted.len(), 20);
    assert_eq!(
        artifacts.get(&stranger.id, &artifact_id).await.unwrap_err(),
        ArtifactError::NotFound
    );

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
        recovery_receipts: Vec::new(),
        created_at: Utc::now(),
    };
    runtime.checkpoint(checkpoint, cookie).await.unwrap();
    let pre_recovery_worker = workers.lease(session.id.clone()).await.unwrap().worker_id();
    assert!(matches!(
        runtime.recover(&workflow).await.unwrap(),
        RecoveryDecision::Resumed { .. }
    ));
    let post_recovery_worker = workers.lease(session.id.clone()).await.unwrap().worker_id();
    assert_ne!(pre_recovery_worker, post_recovery_worker);
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
        "adaptive HTTP live proof: directHttp static+download+recovered, chromiumFallback dynamic, workers=({pre_recovery_worker:?},{post_recovery_worker:?}), artifact={artifact_id}, sha256={sha256}"
    );
}
