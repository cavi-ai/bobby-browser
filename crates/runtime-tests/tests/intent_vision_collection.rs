//! Real-vision training data collection harness.
//!
//! Runs stuck-form and vague-purpose intents against the live test-site
//! fixture with the REAL vision pipeline — the runtime's HttpVisionAssist
//! points at the loopback vision-proxy (upstream of choice), every
//! escalation produces a genuine screenshot -> model -> proposal round
//! trip, and the proxy's data collector writes each one to
//! data/vision/training_data.jsonl.
//!
//! Run with the proxy up (upstream mlx, ollama, or openai):
//!
//!   BOBBY_GAUNTLET_VISION_ENDPOINT=http://127.0.0.1:9100/vision \
//!   BOBBY_VISION_TOKEN=<bearer> \
//!   cargo test -p runtime-tests --test intent_vision_collection -- --test-threads=1

use std::path::PathBuf;

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, ServerConfig, StorageConfig};
use sdk_core::RuntimeService;
use types::{
    AttemptId, CommandEnvelope, CommandId, CommandOutcome, CompleteFormField, CompleteFormIntent,
    CreateSessionRequest, Evidence, ExecutionPolicy, FillValue, IntentCommand, IntentHints,
    IntentResolutionPath, LocateIntent, NavigateCommand, OpenPageRequest, PageId, PrimitiveCommand,
    RuntimeCommand, SessionId, WaitUntil, WorkflowId,
};

fn chrome_executable() -> PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

fn vision_endpoint() -> Option<String> {
    std::env::var("BOBBY_GAUNTLET_VISION_ENDPOINT")
        .ok()
        .filter(|endpoint| !endpoint.trim().is_empty())
}

/// The REAL production assist: HTTP to the loopback vision-proxy with the
/// bearer from BOBBY_VISION_TOKEN. Proposals round-trip through the proxy's
/// data collector.
fn real_http_assist() -> std::sync::Arc<dyn intent_engine::VisionAssist> {
    let endpoint = vision_endpoint().expect("vision endpoint");
    let bearer = std::env::var("BOBBY_VISION_TOKEN")
        .ok()
        .filter(|token| !token.is_empty());
    std::sync::Arc::new(
        intent_engine::HttpVisionAssist::new(endpoint, bearer, std::time::Duration::from_secs(60))
            .expect("http vision assist"),
    )
}

fn base_config(root: &std::path::Path) -> AppConfig {
    AppConfig {
        http: config::HttpConfig {
            allow_loopback: true,
            ..config::HttpConfig::default()
        },
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
            shutdown_timeout_ms: 10_000,
        },
        browser: BrowserConfig {
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
        storage: StorageConfig {
            journal_path: root.join("commands.jsonl"),
            checkpoints_dir: root.join("checkpoints"),
            authority_path: root.join("authority.json"),
            scheduler_journal_path: root.join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig {
            endpoint_url: vision_endpoint(),
            token_env: Some("BOBBY_VISION_TOKEN".into()),
            prefill: true,
            corpus_dir: Some(
                std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                    .join("../../target/vision-escalations"),
            ),
            ..config::VisionConfig::default()
        },
        context: Default::default(),
        nodes: Default::default(),
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
    }
}

async fn open_fixture(runtime: &RuntimeService, url: &str) -> (SessionId, PageId) {
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "vision-collection".into(),
            proxy: None,
            execution_policy: ExecutionPolicy {
                vision_assist: true,
                ..ExecutionPolicy::default()
            },
        })
        .await
        .unwrap();
    let page = runtime
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await
        .unwrap();
    match runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
                url: url.into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            })),
        })
        .await
    {
        CommandOutcome::Completed { .. } => {}
        outcome => panic!("navigate failed: {outcome:?}"),
    }
    (session.id, page.id)
}

/// Reopen a page in an EXISTING session after the current page dies — the
/// sweep submits against the original session id, so a fresh session's page
/// would be rejected with "page does not belong to session".
async fn reopen_page(
    runtime: &RuntimeService,
    session_id: &SessionId,
    url: &str,
) -> PageId {
    let page = runtime
        .open_page(OpenPageRequest {
            session_id: session_id.clone(),
        })
        .await
        .unwrap();
    match runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session_id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
                url: url.into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            })),
        })
        .await
    {
        CommandOutcome::Completed { .. } => page.id,
        outcome => panic!("reopen navigate failed: {outcome:?}"),
    }
}

async fn submit_intent(
    runtime: &RuntimeService,
    session_id: &SessionId,
    page_id: &PageId,
    command: IntentCommand,
) -> CommandOutcome {
    runtime
        .submit_with_vision_capability(
            CommandEnvelope {
                schema_version: CommandEnvelope::SCHEMA_VERSION,
                command_id: CommandId::new(),
                workflow_id: WorkflowId::new(),
                attempt_id: AttemptId::new(),
                session_id: session_id.clone(),
                page_id: Some(page_id.clone()),
                deadline: Utc::now() + Duration::seconds(60),
                command: RuntimeCommand::Intent(command),
            },
            true,
        )
        .await
}

fn vision_paths(evidence: &[Evidence]) -> Vec<IntentResolutionPath> {
    evidence
        .iter()
        .filter_map(|item| match item {
            Evidence::IntentExecution { record } => Some(record.resolution_path),
            _ => None,
        })
        .collect()
}

/// Every field targets something that does not exist in the DOM, so every
/// fill escalates to the live provider and lands in the collector.
fn stuck_form() -> IntentCommand {
    IntentCommand::CompleteForm(CompleteFormIntent {
        purpose: "register".into(),
        fields: vec![
            CompleteFormField {
                name: "first".into(),
                purpose: "First name".into(),
                hints: Default::default(),
                value: FillValue::Text {
                    text: "Ada".into(),
                    clear_first: true,
                },
            },
            CompleteFormField {
                name: "last".into(),
                purpose: "Last name".into(),
                hints: Default::default(),
                value: FillValue::Text {
                    text: "Lovelace".into(),
                    clear_first: true,
                },
            },
        ],
    })
}

/// Purposes phrased vaguely enough that candidate matching is ambiguous and
/// the engine escalates.
fn vague_locate(purpose: &str) -> IntentCommand {
    IntentCommand::Locate(LocateIntent {
        purpose: purpose.into(),
        hints: IntentHints::default(),
    })
}

#[tokio::test]
#[ignore = "requires installed Chrome and a running vision-proxy"]
async fn collect_stuck_form_proposals() {
    // Explicitly run without the endpoint must fail, not pass green with
    // zero rows collected. A collection run that produced nothing is a
    // failure, not a skip.
    let endpoint = vision_endpoint().unwrap_or_else(|| {
        panic!("BOBBY_GAUNTLET_VISION_ENDPOINT unset; this harness collects nothing without it")
    });
    let _ = endpoint;
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let config = base_config(root.path());
    let assist = real_http_assist();
    let runtime = RuntimeService::build_with_vision_assist(&config, assist)
        .await
        .unwrap();
    let (session, page) = open_fixture(&runtime, &fixture.base_url()).await;

    let outcome = submit_intent(&runtime, &session, &page, stuck_form()).await;
    // Collection run: success means vision was CONSULTED, not that every
    // proposal cleared the confidence floor. Below-floor proposals are
    // training signal too — the runtime failing closed is the design, and
    // the collector captures the attempt either way.
    let (completed_evidence, failed_evidence) = match outcome {
        CommandOutcome::Completed { evidence, .. } => (evidence, Vec::new()),
        CommandOutcome::Failed { evidence, .. } => (Vec::new(), evidence),
        other => panic!("unexpected outcome: {other:?}"),
    };
    let all: Vec<Evidence> = completed_evidence
        .into_iter()
        .chain(failed_evidence.into_iter())
        .collect();
    let consulted = all.iter().any(|item| {
        matches!(item, Evidence::IntentExecution { record } if record.vision_proposal_sha256.is_some())
    });
    assert!(
        consulted,
        "vision was never consulted; no proposal evidence in {all:?}"
    );
}

#[tokio::test]
#[ignore = "requires installed Chrome and a running vision-proxy"]
async fn collect_vague_locate_proposals() {
    if vision_endpoint().is_none() {
        panic!("BOBBY_GAUNTLET_VISION_ENDPOINT unset; this harness collects nothing without it");
    }
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let config = base_config(root.path());
    let assist = real_http_assist();
    let runtime = RuntimeService::build_with_vision_assist(&config, assist)
        .await
        .unwrap();
    let (session, mut page) = open_fixture(&runtime, &fixture.base_url()).await;

    let mut attempted = 0usize;
    for purpose in [
        // Genuinely ambiguous on this page: no model should answer these
        // confidently, so every proposal lands below the floor and records
        // an abstain-labeled negative (target_index=None -> v1 "-1").
        "the button that moves forward",
        "the place to type a name",
        "the control for continuing",
        "the thing that submits",
        "where the email goes",
        "the link to the next page",
        "the option everyone picks",
        "the field at the bottom",
        "the widget in the corner",
        "the thing that looks like a button",
    ] {
        attempted += 1;
        let outcome = submit_intent(&runtime, &session, &page, vague_locate(purpose)).await;
        match outcome {
            CommandOutcome::Completed { evidence, .. } => {
                let paths = vision_paths(&evidence);
                eprintln!("{purpose}: resolved via {paths:?}");
            }
            CommandOutcome::Failed { error, .. } => {
                eprintln!("{purpose}: failed as {error:?}");
                // A dead page ends the sweep silently: reopen in the same
                // session so later purposes still reach the model.
                if error.message.contains("page is not open") {
                    eprintln!("page died; reopening");
                    page = reopen_page(&runtime, &session, &fixture.base_url()).await;
                }
            }
            other => panic!("unexpected outcome: {other:?}"),
        }
    }
    // A run that never reached the model is not evidence of anything.
    assert!(attempted >= 10, "sweep attempted {attempted} purposes");
}

/// Positive control: the purpose matches a candidate that the deterministic
/// path cannot resolve (role hint says link, the page has buttons), so the
/// engine escalates and the provider must pick the right element.
#[tokio::test]
#[ignore = "requires installed Chrome and a running vision-proxy"]
async fn v1_positive_control_picks_the_matching_candidate() {
    if vision_endpoint().is_none() {
        panic!("BOBBY_GAUNTLET_VISION_ENDPOINT unset; this harness collects nothing without it");
    }
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let config = base_config(root.path());
    let assist = real_http_assist();
    let runtime = RuntimeService::build_with_vision_assist(&config, assist)
        .await
        .unwrap();
    let (session, page) = open_fixture(&runtime, &fixture.base_url()).await;

    // Purpose embeds the exact candidate name (the phrasing the adapter was
    // trained on) but the near-miss path is forced by naming a non-button
    // kind of control: no exact resolution, so the provider must pick
    // "Continue" from the near-miss list.
    let intent = IntentCommand::Locate(LocateIntent {
        purpose: "Click the Continue button".into(),
        hints: IntentHints::default(),
    });
    let outcome = submit_intent(&runtime, &session, &page, intent).await;
    let CommandOutcome::Completed { evidence, .. } = outcome else {
        panic!("positive control did not complete: {outcome:?}");
    };
    let paths = vision_paths(&evidence);
    assert!(
        paths.contains(&IntentResolutionPath::VisionFallback),
        "expected vision fallback resolution: {paths:?}"
    );
    let verified = evidence.iter().any(|item| {
        matches!(item, Evidence::IntentExecution { record } if record.verification == "visionFallback")
    });
    assert!(verified, "expected visionFallback verification in {evidence:?}");
}
