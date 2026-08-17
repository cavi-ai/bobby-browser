//! Spec C exit proof (T8): a cold session completes the Northstar onboarding
//! station with discovery snapshots; a second session on the same durable
//! profile is answered by the persisted context graph before any snapshot
//! and completes with strictly fewer runtime commands. Also measures
//! fuzzy-match and store-open latency across 100 synthetic sites.

// The exit test reuses only the scenario server; the driver's unused items
// are live in modern_gauntlet_e2e.
#[allow(dead_code)]
#[path = "modern_gauntlet/mod.rs"]
mod modern_gauntlet;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use chrono::{Duration, Utc};
use modern_gauntlet::scenario::{ScenarioConfig, ScenarioServer};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use types::{
    AttemptId, Capability, CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest,
    FillIntent, FillValue, IntentCommand, IntentHints, NavigateCommand, OpenPageRequest, PageId,
    PrimitiveCommand, RuntimeCommand, SessionId, WaitUntil, WorkflowId,
};
use worker_pool::ChromiumWorkerFactory;

const FIELDS: [(&str, &str); 4] = [
    ("Full name", "Maya Chen"),
    ("Work email", "maya@atlas.example"),
    ("Company name", "Atlas Labs"),
    ("Postal code", "10001"),
];

fn chrome_executable() -> PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

fn config(root: &std::path::Path, context_dir: &std::path::Path) -> config::AppConfig {
    config::AppConfig {
        http: config::HttpConfig {
            allow_loopback: true,
            ..config::HttpConfig::default()
        },
        server: config::ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            shutdown_timeout_ms: 10_000,
        },
        browser: config::BrowserConfig {
            executable: Some(chrome_executable()),
            profiles_dir: root.join("profiles"),
            headless: true,
            max_active: 1,
            upload_roots: vec![],
            downloads_dir: root.join("downloads"),
            artifacts_dir: root.join("artifacts"),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
            max_js_result_bytes: 64 * 1024,
            max_js_timeout_ms: 30_000,
        },
        storage: config::StorageConfig {
            journal_path: root.join("commands.jsonl"),
            checkpoints_dir: root.join("checkpoints"),
            authority_path: root.join("authority.json"),
            scheduler_journal_path: root.join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
        context: config::ContextConfig {
            dir: Some(context_dir.to_path_buf()),
            ..config::ContextConfig::default()
        },
        nodes: Default::default(),
    }
}

struct Station {
    runtime: RuntimeService,
    authed: AuthenticatedRuntime,
    ctx: types::RequestContext,
    session: SessionId,
    page: PageId,
    commands: usize,
}

impl Station {
    async fn open(runtime: &RuntimeService, authed: &AuthenticatedRuntime, url: &str) -> Self {
        let session = runtime
            .create_session(CreateSessionRequest {
                profile: "northstar-profile".into(),
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
        let ctx = types::RequestContext::new_for_test(
            authed.capability_handle().principal_id().clone(),
            [
                Capability::SessionRead,
                Capability::SessionWrite,
                Capability::PageRead,
                Capability::PageWrite,
                Capability::BrowserMutate,
                Capability::IntentExecute,
                Capability::ContextRead,
            ],
            Utc::now() + Duration::minutes(5),
        );
        let mut station = Self {
            runtime: runtime.clone(),
            authed: authed.clone(),
            ctx,
            session: session.id,
            page: page.id,
            commands: 0,
        };
        station
            .submit(RuntimeCommand::Primitive(PrimitiveCommand::Navigate(
                NavigateCommand {
                    url: url.into(),
                    wait_until: WaitUntil::Interactive,
                    timeout_ms: 10_000,
                },
            )))
            .await;
        station
    }

    async fn submit(&mut self, command: RuntimeCommand) -> Vec<types::Evidence> {
        self.commands += 1;
        match self
            .runtime
            .submit(CommandEnvelope {
                schema_version: CommandEnvelope::SCHEMA_VERSION,
                command_id: CommandId::new(),
                workflow_id: WorkflowId::new(),
                attempt_id: AttemptId::new(),
                session_id: self.session.clone(),
                page_id: Some(self.page.clone()),
                deadline: Utc::now() + Duration::seconds(30),
                command,
            })
            .await
        {
            CommandOutcome::Completed { evidence, .. } => evidence,
            outcome => panic!("station command failed: {outcome:?}"),
        }
    }

    async fn fill(&mut self, purpose: &str, value: &str) {
        self.submit(RuntimeCommand::Intent(IntentCommand::Fill(FillIntent {
            purpose: purpose.into(),
            hints: IntentHints {
                role: Some("textbox".into()),
                ..IntentHints::default()
            },
            value: FillValue::Text {
                text: value.into(),
                clear_first: true,
            },
        })))
        .await;
    }

    async fn complete_onboarding(&mut self, submit_selector: &str) {
        for (purpose, value) in FIELDS {
            self.fill(purpose, value).await;
        }
        self.submit(RuntimeCommand::Primitive(PrimitiveCommand::Click(
            types::ClickCommand {
                selector: submit_selector.into(),
                target: None,
                boundary: false,
                expected_url: None,
                modifiers: Vec::new(),
            },
        )))
        .await;
    }

    async fn ask(&self, description: &str) -> Option<types::ContextAnswer> {
        interface_core::RuntimeInterface::context_ask(
            &self.authed,
            self.ctx.clone(),
            self.session.clone(),
            self.page.clone(),
            description.into(),
        )
        .await
        .unwrap()
    }

    async fn close(self) {
        interface_core::RuntimeInterface::delete_session(&self.authed, self.ctx, self.session)
            .await
            .unwrap();
    }
}

#[tokio::test]
#[ignore = "requires installed Chromium"]
async fn remembered_site_completes_onboarding_with_fewer_commands() {
    let server = ScenarioServer::start(ScenarioConfig::seeded("context-exit"))
        .await
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    let context_dir = root.path().join("context");
    let config = config(root.path(), &context_dir);
    let factory = Arc::new(ChromiumWorkerFactory::new(config.browser.clone()));
    let runtime =
        RuntimeService::build_with_context_promotion(&config, factory, "northstar-profile")
            .await
            .unwrap();
    let authority = interface_core::AuthorityStore::in_memory();
    let handle = authority
        .issue(
            types::PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [
                Capability::SessionRead,
                Capability::SessionWrite,
                Capability::PageRead,
                Capability::PageWrite,
                Capability::BrowserMutate,
                Capability::IntentExecute,
                Capability::ContextRead,
            ],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let handle = authority.verify(&handle).await.unwrap();
    let authed = AuthenticatedRuntime::new(runtime.clone(), handle);
    let url = server.application_url("/onboarding");

    // Session 1, cold: discovery snapshot, then the station.
    let mut cold = Station::open(&runtime, &authed, &url).await;
    for (purpose, _) in FIELDS {
        assert_eq!(
            cold.ask(purpose).await,
            None,
            "a cold session was answered by context it never observed"
        );
    }
    cold.submit(RuntimeCommand::Primitive(
        PrimitiveCommand::AccessibilitySnapshot(types::AccessibilitySnapshotCommand {
            max_nodes: Some(256),
            target: None,
        }),
    ))
    .await;
    cold.complete_onboarding("form[aria-label='Customer onboarding'] button[type='submit']")
        .await;
    let cold_commands = cold.commands;
    cold.close().await;

    // Session 2, remembered: answers before any snapshot.
    let mut warm = Station::open(&runtime, &authed, &url).await;
    for (purpose, _) in FIELDS {
        let answer = warm
            .ask(purpose)
            .await
            .unwrap_or_else(|| panic!("persisted context did not answer {purpose:?}"));
        assert_eq!(
            answer.observed_at,
            types::ContextObservedAt::Persisted,
            "the warm answer was not marked as remembered"
        );
        assert!(answer.confidence >= 0.75);
    }
    let warm_commands_before_station = warm.commands;
    warm.complete_onboarding("form[aria-label='Customer onboarding'] button[type='submit']")
        .await;
    let warm_commands = warm.commands;
    warm.close().await;

    assert_eq!(warm_commands_before_station, 1, "only the navigate ran");
    assert!(
        warm_commands < cold_commands,
        "remembered session must run strictly fewer commands: warm={warm_commands} cold={cold_commands}"
    );
    let snapshot = server.snapshot().await;
    assert_eq!(snapshot.onboarding_records, 2);
}

#[tokio::test]
async fn fuzzy_match_latency_across_100_sites() {
    let root = tempfile::tempdir().unwrap();
    let (store, _) = context_store::ContextStore::open(root.path(), "bench")
        .await
        .unwrap();
    for index in 0..100 {
        let controls: Vec<context_store::ControlContext> = (0..20)
            .map(|field| {
                let mut intents = std::collections::BTreeMap::new();
                intents.insert(
                    "fill".to_string(),
                    context_store::IntentStats {
                        success_count: 3,
                        failure_count: 0,
                        last_verified_day: Some(20_000),
                        source: Some(context_store::RecordSource::Observed),
                    },
                );
                context_store::ControlContext {
                    role: "textbox".into(),
                    accessible_name: format!("Field {field} of site {index}"),
                    ordinal: None,
                    form_membership: "page".into(),
                    intents,
                }
            })
            .collect();
        let mut forms = std::collections::BTreeMap::new();
        forms.insert("page".to_string(), context_store::FormContext { controls });
        let mut pages = std::collections::BTreeMap::new();
        pages.insert("/form".to_string(), context_store::PageContext { forms });
        store
            .upsert_site(
                &format!("https://site-{index}.example"),
                context_store::SiteContext { pages },
            )
            .await;
    }
    assert!(store.flush().await.is_empty());
    drop(store);

    let reopen_started = Instant::now();
    let (store, report) = context_store::ContextStore::open(root.path(), "bench")
        .await
        .unwrap();
    let reopen_ms = reopen_started.elapsed().as_secs_f64() * 1_000.0;
    assert_eq!(report.sites_loaded, 100);

    let promotion = page_runtime::ContextPromotion::new(store);
    let url = Some("https://site-42.example/form");
    // Exact, then fuzzy (reordered tokens), timed over 1_000 iterations.
    let exact_started = Instant::now();
    for _ in 0..1_000 {
        assert!(promotion.ask(url, "Field 7 of site 42").await.is_some());
    }
    let exact_us = exact_started.elapsed().as_secs_f64() * 1e6 / 1_000.0;
    let fuzzy_started = Instant::now();
    for _ in 0..1_000 {
        assert!(promotion.ask(url, "site 42 of Field 7").await.is_some());
    }
    let fuzzy_us = fuzzy_started.elapsed().as_secs_f64() * 1e6 / 1_000.0;

    let measurements = serde_json::json!({
        "sites": 100,
        "controlsPerSite": 20,
        "reopenMs": reopen_ms,
        "exactAskMicros": exact_us,
        "fuzzyAskMicros": fuzzy_us,
    });
    println!("context-graph measurements: {measurements}");
    let benchmarks = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|path| path.parent())
        .unwrap()
        .join("benchmarks");
    std::fs::create_dir_all(&benchmarks).unwrap();
    std::fs::write(
        benchmarks.join("context-graph.json"),
        serde_json::to_string_pretty(&measurements).unwrap(),
    )
    .unwrap();
    assert!(
        fuzzy_us < 1_000.0,
        "fuzzy ask too slow for the file-store decision: {fuzzy_us}us"
    );
}
