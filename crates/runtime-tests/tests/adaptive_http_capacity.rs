use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration as StdDuration, Instant};

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, HttpConfig, ServerConfig, StorageConfig};
use network_engine::state::HttpStateSnapshot;
use network_engine::{DirectHttpExecutor, HttpCandidate, NetworkPolicy};
use sdk_core::RuntimeService;
use std::collections::BTreeMap;
use types::{
    AttemptId, CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest, Evidence,
    ExecutionPath, InspectCommand, NavigateCommand, OpenPageRequest, PageId, PrimitiveCommand,
    RuntimeCommand, SessionId, WaitUntil, WorkflowId,
};
use worker_pool::{ChromiumWorkerFactory, WorkerFactory};

fn config(root: &tempfile::TempDir) -> AppConfig {
    AppConfig {
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
            max_active: 8,
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
            request_timeout_ms: 5_000,
            ..HttpConfig::default()
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
    }
}

fn envelope(session: &SessionId, page: &PageId, command: PrimitiveCommand) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: session.clone(),
        page_id: Some(page.clone()),
        deadline: Utc::now() + Duration::seconds(10),
        command: RuntimeCommand::Primitive(command),
    }
}

fn median(mut samples: Vec<StdDuration>) -> StdDuration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

fn dispersion(samples: &[StdDuration]) -> (u128, u128) {
    let mut millis = samples
        .iter()
        .map(StdDuration::as_millis)
        .collect::<Vec<_>>();
    millis.sort_unstable();
    (millis[millis.len() / 4], millis[(millis.len() * 3) / 4])
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn eight_inspections_complete_with_a_peak_of_four_and_no_browser_dispatch() {
    let site = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let runtime = Arc::new(RuntimeService::build(&config(&root)).await.unwrap());
    let slow_url = format!("{}/slow", site.base_url());
    let mut pages = Vec::new();
    for index in 0..8 {
        let session = runtime
            .create_session(CreateSessionRequest {
                profile: format!("capacity-{index}"),
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
        let outcome = runtime
            .submit(envelope(
                &session.id,
                &page.id,
                PrimitiveCommand::Navigate(NavigateCommand {
                    url: slow_url.clone(),
                    wait_until: WaitUntil::Interactive,
                    timeout_ms: 5_000,
                }),
            ))
            .await;
        assert!(matches!(outcome, CommandOutcome::Completed { .. }));
        pages.push((session.id, page.id));
    }

    site.reset_peak_requests();
    let batch_started = Instant::now();
    let mut tasks = Vec::new();
    for (session, page) in &pages {
        let runtime = runtime.clone();
        let session = session.clone();
        let page = page.clone();
        tasks.push(tokio::spawn(async move {
            let started = Instant::now();
            let outcome = runtime
                .submit(envelope(
                    &session,
                    &page,
                    PrimitiveCommand::Inspect(InspectCommand::default()),
                ))
                .await;
            (started.elapsed(), outcome)
        }));
    }

    let mut queued_direct_wall_clock = Vec::new();
    for task in tasks {
        let (elapsed, outcome) = task.await.unwrap();
        let evidence = match outcome {
            CommandOutcome::Completed { evidence, .. } => evidence,
            outcome => panic!("direct runtime inspection failed: {outcome:?}"),
        };
        assert!(evidence.iter().any(|item| matches!(
            item,
            Evidence::ExecutionPath {
                path: ExecutionPath::DirectHttp,
                ..
            }
        )));
        assert!(!evidence.iter().any(|item| matches!(
            item,
            Evidence::ExecutionPath {
                path: ExecutionPath::Chromium | ExecutionPath::ChromiumFallback,
                ..
            }
        )));
        assert!(elapsed < StdDuration::from_secs(5));
        queued_direct_wall_clock.push(elapsed);
    }
    let batch_elapsed = batch_started.elapsed();
    assert_eq!(site.peak_requests(), 4);

    let direct = DirectHttpExecutor::new(NetworkPolicy {
        allow_loopback: true,
        ..NetworkPolicy::default()
    });
    let snapshot = HttpStateSnapshot {
        version: 1,
        current_url: slow_url.clone(),
        cookies: Vec::new(),
        cache_validators: BTreeMap::new(),
        user_agent: "capacity-proof".into(),
        language: "en-US".into(),
    };
    assert!(matches!(
        direct
            .inspect(&snapshot, &InspectCommand::default())
            .await
            .unwrap(),
        HttpCandidate::Inspection { .. }
    ));
    let mut direct_wall_clock = Vec::new();
    for _ in 0..7 {
        let started = Instant::now();
        assert!(matches!(
            direct
                .inspect(&snapshot, &InspectCommand::default())
                .await
                .unwrap(),
            HttpCandidate::Inspection { .. }
        ));
        direct_wall_clock.push(started.elapsed());
    }

    let factory = ChromiumWorkerFactory::new(config(&root).browser);
    let chromium = factory.launch(&SessionId::new()).await.unwrap();
    let chromium_page = PageId::new();
    chromium.open_page(chromium_page.clone()).await.unwrap();
    chromium
        .navigate(
            &chromium_page,
            &NavigateCommand {
                url: slow_url.clone(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 5_000,
            },
        )
        .await
        .unwrap();
    chromium
        .inspect(&chromium_page, &InspectCommand::default())
        .await
        .unwrap();
    let mut chromium_wall_clock = Vec::new();
    for _ in 0..7 {
        let started = Instant::now();
        chromium
            .navigate(
                &chromium_page,
                &NavigateCommand {
                    url: slow_url.clone(),
                    wait_until: WaitUntil::Interactive,
                    timeout_ms: 5_000,
                },
            )
            .await
            .unwrap();
        chromium
            .inspect(&chromium_page, &InspectCommand::default())
            .await
            .unwrap();
        chromium_wall_clock.push(started.elapsed());
    }

    let direct_spread = dispersion(&direct_wall_clock);
    let chromium_spread = dispersion(&chromium_wall_clock);
    let direct_median = median(direct_wall_clock.clone());
    let chromium_median = median(chromium_wall_clock.clone());
    println!(
        "capacity correctness: peak={} completed=8 eligible_browser_dispatches=0 batch_wall_clock_ms={} queued_request_median_ms={}; performance report_only=true same_url=/slow wall_clock_boundary=transport_plus_parse after_warmup=1 samples=7 workload_direct=direct_executor_get_plus_parse workload_chromium=raw_worker_navigate_plus_inspect direct_http_median_ms={} direct_http_iqr_ms={}-{} chromium_median_ms={} chromium_iqr_ms={}-{}",
        site.peak_requests(),
        batch_elapsed.as_millis(),
        median(queued_direct_wall_clock).as_millis(),
        direct_median.as_millis(),
        direct_spread.0, direct_spread.1,
        chromium_median.as_millis(), chromium_spread.0, chromium_spread.1
    );
}
