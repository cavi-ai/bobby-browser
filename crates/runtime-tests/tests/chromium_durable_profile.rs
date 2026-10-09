//! An opt-in durable identity for managed Chromium. A
//! `ChromiumWorkerFactory` configured with `.with_durable_profile(id)`
//! persists its user-data-dir at `<profiles_dir>/chromium/<id>` across
//! sessions instead of disposing it per session, and that same id attaches
//! context-graph promotion exactly the way an enrolled Firefox companion
//! profile does (`EnginePreferenceConfig::durable_profile_id`).
//!
//! Two sessions on the same named Chromium profile against an onboarding
//! page: the second session's `context_ask` answers persisted for
//! a field the first session verified, before the second session takes any
//! snapshot. A third session on a runtime with no context store attached
//! answers `None` for the same field.
//!
//! Managed Chromium without a profile id keeps a disposable browser profile
//! and still remembers: its runtime promotes under the shared
//! `managed-chromium` identity, and a restarted runtime answers from disk.
//!
//! Every session on a durable profile shares one Chrome that outlives the
//! runtime. Unix only: the tests stop that Chrome through `ps` and signals.
#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use chrono::{Duration, Utc};
use runtime_tests::ProfileChrome;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use test_site::{FixtureSite, Route};
use types::{
    AttemptId, Capability, CommandEnvelope, CommandId, CommandOutcome, ControlAction,
    CreateSessionRequest, FillIntent, IntentCommand, IntentHints, NavigateCommand, OpenPageRequest,
    PageId, PrimitiveCommand, RuntimeCommand, SessionId, WaitUntil, WorkflowId,
};
use worker_pool::ChromiumWorkerFactory;

const FIELD_PURPOSE: &str = "Full name";
const FIELD_VALUE: &str = "Maya Chen";
/// Sessions sharing one durable profile at once.
const SHARED_SESSIONS: usize = 8;

const ONBOARDING: &str = r#"<!doctype html><title>New relationship</title><main>
<h1>New relationship</h1>
<form>
<label for="full-name">Full name</label><input id="full-name" name="fullName">
<label for="work-email">Work email</label><input id="work-email" name="workEmail" type="email">
</form></main>"#;

async fn onboarding_site() -> FixtureSite {
    FixtureSite::spawn(vec![("/onboarding", Route::Html(ONBOARDING.into()))]).await
}

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
            max_active: SHARED_SESSIONS,
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
    let server = onboarding_site().await;
    let url = server.url("/onboarding");
    let durable_profile_id = "chromium-durable-profile";

    // Sessions 1 and 2 share one runtime: the same ChromiumWorkerFactory
    // instance, opted into the same durable profile id, and the same
    // context-promotion sink -- exactly how `compose_worker_factory_inner`
    // wires a Chromium `Exact { engine: Chromium, profileId }` selection in
    // production.
    let durable_root = tempfile::tempdir().unwrap();
    let context_dir = durable_root.path().join("context");
    let durable_config = config(durable_root.path(), Some(&context_dir));
    let _chrome = ProfileChrome(
        durable_config
            .browser
            .profiles_dir
            .join("chromium")
            .join(durable_profile_id),
    );
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

    // Session 3: a separate runtime with a plain ChromiumWorkerFactory and no
    // context store attached never sees another store's memory.
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

#[tokio::test]
#[ignore = "requires installed Chromium"]
async fn managed_chromium_remembers_across_runtimes_with_a_disposable_profile() {
    let server = onboarding_site().await;
    let url = server.url("/onboarding");
    let profile_id = config::EnginePreferenceConfig::ManagedChromium
        .durable_profile_id()
        .expect("managed Chromium carries a memory identity");
    let root = tempfile::tempdir().unwrap();
    let context_dir = root.path().join("context");
    let managed_config = config(root.path(), Some(&context_dir));

    // A plain factory, as `compose_worker_factory_inner` wires ManagedChromium:
    // every session gets its own user-data-dir.
    let first_runtime = RuntimeService::build_with_context_promotion(
        &managed_config,
        Arc::new(ChromiumWorkerFactory::new(managed_config.browser.clone())),
        profile_id,
    )
    .await
    .unwrap();
    let mut cold = Session::open(&first_runtime, &url).await;
    assert_eq!(
        cold.ask(FIELD_PURPOSE).await,
        None,
        "a cold session was answered by context it never observed"
    );
    cold.fill(FIELD_PURPOSE, FIELD_VALUE).await;
    cold.close().await;
    drop(first_runtime);

    assert!(
        !managed_config
            .browser
            .profiles_dir
            .join("chromium")
            .join(profile_id)
            .exists(),
        "managed Chromium persisted a browser profile"
    );

    // A fresh runtime over the same context dir: the memory came from disk,
    // not from the first runtime's process state.
    let second_runtime = RuntimeService::build_with_context_promotion(
        &managed_config,
        Arc::new(ChromiumWorkerFactory::new(managed_config.browser.clone())),
        profile_id,
    )
    .await
    .unwrap();
    let warm = Session::open(&second_runtime, &url).await;
    let answer = warm
        .ask(FIELD_PURPOSE)
        .await
        .unwrap_or_else(|| panic!("managed Chromium did not remember {FIELD_PURPOSE:?}"));
    assert_eq!(
        answer.observed_at,
        types::ContextObservedAt::Persisted,
        "the warm answer was not marked as remembered"
    );
    assert!(answer.confidence >= 0.75);
    warm.close().await;
}

fn evidence_text(evidence: &[types::Evidence]) -> String {
    serde_json::to_string(evidence).expect("serialize evidence")
}

async fn durable_runtime(config: &config::AppConfig, profile_id: &str) -> RuntimeService {
    let factory = Arc::new(
        ChromiumWorkerFactory::new(config.browser.clone()).with_durable_profile(profile_id.into()),
    );
    RuntimeService::build_with_worker_factory(config, factory)
        .await
        .unwrap()
}

async fn shared_site() -> FixtureSite {
    let paths: Vec<String> = (0..SHARED_SESSIONS)
        .map(|index| format!("/s/{index}"))
        .collect();
    let mut routes: Vec<(&str, Route)> = paths
        .iter()
        .enumerate()
        .map(|(index, path)| {
            (
                path.as_str(),
                Route::Html(format!(
                    "<!doctype html><title>Session {index}</title><main>Session {index}</main>"
                )),
            )
        })
        .collect();
    routes.push((
        "/set",
        Route::Html(
            r#"<!doctype html><title>Set</title><script>document.cookie="shared=yes; path=/";</script>"#
                .into(),
        ),
    ));
    FixtureSite::spawn(routes).await
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium"]
async fn sessions_on_a_durable_profile_share_one_chrome_that_outlives_the_runtime() {
    let site = shared_site().await;
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path(), None);
    let chrome = ProfileChrome(config.browser.profiles_dir.join("chromium").join("shared"));
    let runtime = durable_runtime(&config, "shared").await;

    let urls: Vec<String> = (0..SHARED_SESSIONS)
        .map(|index| site.url(&format!("/s/{index}")))
        .collect();
    let mut sessions =
        futures_util::future::join_all(urls.iter().map(|url| Session::open(&runtime, url))).await;
    let browser = chrome.pids();
    assert_eq!(
        browser.len(),
        1,
        "one Chrome serves the profile: {browser:?}"
    );

    for (index, session) in sessions.iter_mut().enumerate() {
        let listed = evidence_text(
            &session
                .submit(RuntimeCommand::Primitive(PrimitiveCommand::ListPages(
                    types::ListPagesCommand,
                )))
                .await,
        );
        for other in (0..SHARED_SESSIONS).filter(|other| *other != index) {
            assert!(
                !listed.contains(&format!("/s/{other}\"")),
                "session {index} lists session {other}'s page: {listed}"
            );
        }
        assert!(
            listed.contains(&format!("/s/{index}\"")),
            "session {index} does not list its own page: {listed}"
        );
    }

    sessions[0]
        .submit(RuntimeCommand::Primitive(PrimitiveCommand::Navigate(
            NavigateCommand {
                url: site.url("/set"),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )))
        .await;
    let cookies = evidence_text(
        &sessions[SHARED_SESSIONS - 1]
            .submit(RuntimeCommand::Primitive(PrimitiveCommand::GetCookies(
                types::GetCookiesCommand { urls: vec![] },
            )))
            .await,
    );
    assert!(
        cookies.contains("shared"),
        "a cookie one session set is missing in another: {cookies}"
    );

    let first = sessions.remove(0);
    first.close().await;
    assert_eq!(chrome.pids(), browser, "closing a session kept the browser");
    sessions[0]
        .submit(RuntimeCommand::Primitive(PrimitiveCommand::Navigate(
            NavigateCommand {
                url: site.url("/s/1"),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )))
        .await;
    for session in sessions {
        session.close().await;
    }
    drop(runtime);

    let restarted = durable_runtime(&config, "shared").await;
    let mut after = Session::open(&restarted, &site.url("/s/0")).await;
    assert_eq!(
        chrome.pids(),
        browser,
        "a new runtime attached to the same Chrome"
    );
    let cookies = evidence_text(
        &after
            .submit(RuntimeCommand::Primitive(PrimitiveCommand::GetCookies(
                types::GetCookiesCommand { urls: vec![] },
            )))
            .await,
    );
    assert!(
        cookies.contains("shared"),
        "the cookie did not survive the runtime restart: {cookies}"
    );
    after.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium"]
async fn a_durable_profile_relaunches_chrome_after_it_dies() {
    let site = shared_site().await;
    let root = tempfile::tempdir().unwrap();
    let config = config(root.path(), None);
    let chrome = ProfileChrome(
        config
            .browser
            .profiles_dir
            .join("chromium")
            .join("relaunch"),
    );
    let runtime = durable_runtime(&config, "relaunch").await;

    Session::open(&runtime, &site.url("/s/0"))
        .await
        .close()
        .await;
    let before = chrome.pids();
    assert_eq!(before.len(), 1, "one Chrome serves the profile: {before:?}");
    chrome.kill().await;

    let after = Session::open(&runtime, &site.url("/s/1")).await;
    let relaunched = chrome.pids();
    assert_eq!(
        relaunched.len(),
        1,
        "one Chrome serves the profile: {relaunched:?}"
    );
    assert_ne!(relaunched, before, "the dead Chrome was replaced");
    after.close().await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires installed Chromium"]
async fn a_profile_directory_attaches_to_the_chrome_already_running_there() {
    let site = shared_site().await;
    let root = tempfile::tempdir().unwrap();
    let profile_dir = root.path().join("signed-in");
    std::fs::create_dir_all(&profile_dir).unwrap();
    let chrome = ProfileChrome(profile_dir.clone());
    // A Chrome the user started on its own profile, with remote debugging.
    let mut command = std::process::Command::new(chrome_executable());
    command
        .arg(format!("--user-data-dir={}", profile_dir.display()))
        .args([
            "--remote-debugging-port=0",
            "--headless",
            "--no-first-run",
            "--use-mock-keychain",
            "--password-store=basic",
        ]);
    // Hosts without a usable Chrome sandbox declare it, as for managed launches.
    if std::env::var_os("BOBBY_CHROME_NO_SANDBOX").is_some() {
        command.arg("--no-sandbox");
    }
    let mut user_chrome = command
        .arg("about:blank")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .unwrap();
    for _ in 0..200 {
        if profile_dir.join("DevToolsActivePort").exists() && !chrome.pids().is_empty() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }
    let running = chrome.pids();
    assert_eq!(
        running.len(),
        1,
        "the user's Chrome is running: {running:?}, exit {:?}",
        user_chrome.try_wait()
    );

    let config = config(root.path(), None);
    let factory = Arc::new(
        ChromiumWorkerFactory::new(config.browser.clone()).with_durable_profile_dir(profile_dir),
    );
    let runtime = RuntimeService::build_with_worker_factory(&config, factory)
        .await
        .unwrap();
    let session = Session::open(&runtime, &site.url("/s/0")).await;
    assert_eq!(
        chrome.pids(),
        running,
        "the session used the running Chrome"
    );
    session.close().await;
    assert_eq!(
        chrome.pids(),
        running,
        "closing the session kept the user's Chrome"
    );
    let _ = user_chrome.kill();
    let _ = user_chrome.wait();
}
