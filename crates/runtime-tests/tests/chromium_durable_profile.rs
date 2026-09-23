//! Phase 3 D2: an opt-in durable identity for managed Chromium. A
//! `ChromiumWorkerFactory` configured with `.with_durable_profile(id)`
//! persists its user-data-dir at `<profiles_dir>/chromium/<id>` across
//! sessions instead of disposing it per session, and that same id attaches
//! context-graph promotion exactly the way an enrolled Firefox companion
//! profile does (`EnginePreferenceConfig::durable_profile_id`).
//!
//! Two sessions on the same named Chromium profile against the gauntlet
//! onboarding page: the second session's `context_ask` answers persisted for
//! a field the first session verified, before the second session takes any
//! snapshot. A third, disposable session (no durable profile, no context
//! promotion attached) answers `None` for the same field.

#[allow(dead_code)]
#[path = "modern_gauntlet/mod.rs"]
mod modern_gauntlet;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{Duration, Utc};
use modern_gauntlet::scenario::{ScenarioConfig, ScenarioServer};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use types::{
    AttemptId, Capability, CommandEnvelope, CommandId, CommandOutcome, ControlAction,
    CreateSessionRequest, FillIntent, IntentCommand, IntentHints, NavigateCommand, OpenPageRequest,
    PageId, PrimitiveCommand, RuntimeCommand, SessionId, WaitUntil, WorkflowId,
};
use worker_pool::ChromiumWorkerFactory;

const FIELD_PURPOSE: &str = "Full name";
const FIELD_VALUE: &str = "Maya Chen";

fn chrome_executable() -> PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

fn config(root: &Path, context_dir: Option<&Path>) -> config::AppConfig {
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
            dir: context_dir.map(Path::to_path_buf),
            ..config::ContextConfig::default()
        },
        nodes: Default::default(),
    }
}

struct Session {
    runtime: RuntimeService,
    authed: AuthenticatedRuntime,
    ctx: types::RequestContext,
    session: SessionId,
    page: PageId,
}

impl Session {
    async fn open(runtime: &RuntimeService, url: &str) -> Self {
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
        let session = runtime
            .create_session(CreateSessionRequest {
                profile: "chromium-durable-profile".into(),
                proxy: None,
                execution_policy: Default::default(),
                zigzagzig: false,
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
        let mut this = Self {
            runtime: runtime.clone(),
            authed,
            ctx,
            session: session.id,
            page: page.id,
        };
        this.submit(RuntimeCommand::Primitive(PrimitiveCommand::Navigate(
            NavigateCommand {
                url: url.into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )))
        .await;
        modern_gauntlet::unlock::unlock_northstar_session(&this.runtime, &this.session, &this.page)
            .await
            .unwrap();
        this
    }

    async fn submit(&mut self, command: RuntimeCommand) -> Vec<types::Evidence> {
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
            outcome => panic!("session command failed: {outcome:?}"),
        }
    }

    async fn fill(&mut self, purpose: &str, value: &str) {
        self.submit(RuntimeCommand::Intent(IntentCommand::Fill(FillIntent {
            purpose: purpose.into(),
            hints: IntentHints {
                role: Some("textbox".into()),
                ..IntentHints::default()
            },
            value: ControlAction::SetText {
                value: value.into(),
                clear_first: true,
            },
        })))
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
async fn durable_chromium_profile_persists_context_across_sessions() {
    let server = ScenarioServer::start(ScenarioConfig::seeded("chromium-durable-profile"))
        .await
        .unwrap();
    let url = server.application_url("/onboarding");
    let durable_profile_id = "chromium-durable-profile";

    // Sessions 1 and 2 share one runtime: the same ChromiumWorkerFactory
    // instance, opted into the same durable profile id, and the same
    // context-promotion sink -- exactly how `compose_worker_factory_inner`
    // wires a Chromium `Exact { engine: Chromium, profileId }` selection in
    // production.
    let durable_root = tempfile::tempdir().unwrap();
    let context_dir = durable_root.path().join("context");
    let durable_config = config(durable_root.path(), Some(&context_dir));
    let durable_factory = Arc::new(
        ChromiumWorkerFactory::new(durable_config.browser.clone())
            .with_durable_profile(durable_profile_id.to_string()),
    );
    let durable_runtime = RuntimeService::build_with_context_promotion(
        &durable_config,
        durable_factory,
        durable_profile_id,
    )
    .await
    .unwrap();

    // Session 1, cold: never observed this field before.
    let mut cold = Session::open(&durable_runtime, &url).await;
    assert_eq!(
        cold.ask(FIELD_PURPOSE).await,
        None,
        "a cold session was answered by context it never observed"
    );
    cold.fill(FIELD_PURPOSE, FIELD_VALUE).await;
    cold.close().await;

    // The durable user-data-dir persisted on disk across the session close,
    // at the exact path `ChromiumWorkerFactory::launch` computes for a
    // durable profile id.
    let expected_profile_dir = durable_config
        .browser
        .profiles_dir
        .join("chromium")
        .join(durable_profile_id);
    assert!(
        expected_profile_dir.is_dir(),
        "durable Chromium profile directory was never created at {}",
        expected_profile_dir.display()
    );

    // Session 2, same runtime, same durable profile: context_ask answers
    // persisted for the field session 1 verified, before this session takes
    // any snapshot (this test never submits one).
    let warm = Session::open(&durable_runtime, &url).await;
    let answer = warm
        .ask(FIELD_PURPOSE)
        .await
        .unwrap_or_else(|| panic!("persisted context did not answer {FIELD_PURPOSE:?}"));
    assert_eq!(
        answer.observed_at,
        types::ContextObservedAt::Persisted,
        "the warm answer was not marked as remembered"
    );
    assert!(answer.confidence >= 0.75);
    warm.close().await;

    // Session 3, disposable: a separate runtime with a plain
    // ChromiumWorkerFactory (no durable profile) and no context promotion
    // attached -- the unchanged, pre-existing behavior. Answers None.
    let disposable_root = tempfile::tempdir().unwrap();
    let disposable_config = config(disposable_root.path(), None);
    let disposable_runtime = RuntimeService::build(&disposable_config).await.unwrap();
    let disposable = Session::open(&disposable_runtime, &url).await;
    assert_eq!(
        disposable.ask(FIELD_PURPOSE).await,
        None,
        "a disposable session was answered by another profile's persisted context"
    );
    disposable.close().await;
}
