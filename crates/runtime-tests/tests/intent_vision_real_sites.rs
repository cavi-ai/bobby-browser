//! Real-sites pilot: drive public, read-only pages through the production
//! vision chain and measure behavior class on genuinely unseen site
//! families (the deferred out-of-family test).
//!
//! Public pages only, minimal request volume, read-only intents (locate
//! and observe; nothing that posts, votes, or mutates). A login wall or a
//! bot block is recorded as a result, not hidden.
//!
//! Run with the proxy up (v1 provider + adapter):
//!
//!   BOBBY_GAUNTLET_VISION_ENDPOINT=http://127.0.0.1:9200/vision \
//!   BOBBY_VISION_TOKEN=<bearer> \
//!   BOBBY_REALSITE_CORPUS_DIR=/tmp/vision-realsites \
//!   cargo test -p runtime-tests --test intent_vision_real_sites -- --ignored --nocapture --test-threads=1

use std::path::PathBuf;

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, ServerConfig, StorageConfig};
use sdk_core::RuntimeService;
use types::{
    AttemptId, CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest, Evidence,
    ExecutionPolicy, IntentCommand, IntentHints, LocateIntent, NavigateCommand, OpenPageRequest,
    PageId, PrimitiveCommand, RuntimeCommand, SessionId, WaitUntil, WorkflowId,
};

type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// (site, url, purposes) — public read-only pages; purposes are locate-only
/// observations of visible controls.
const SITES: &[(&str, &str, &[&str])] = &[
    (
        "reddit",
        "https://www.reddit.com/r/rust/",
        &[
            "Open the comments on the first post",
            "Sort the posts by new",
            "Open the second post's title",
            "Open the community wiki",
            "Message the moderators",
        ],
    ),
    (
        "x",
        "https://x.com/rustlang",
        &[
            "Open the first pinned or latest post",
            "Open the posts tab",
            "Open the media tab",
        ],
    ),
    (
        // Login wall: "Sign in" exists on the wall (should commit), the
        // posts tab does not (must abstain). The pair tests discrimination
        // on the hardest realistic page class.
        "linkedin",
        "https://www.linkedin.com/company/rust-lang/",
        &[
            "Open the posts tab",
            "Sign in",
            "Open the jobs tab",
            "Follow the company",
        ],
    ),
];

fn chrome_executable() -> PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
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
            endpoint_url: std::env::var("BOBBY_GAUNTLET_VISION_ENDPOINT").ok(),
            token_env: Some("BOBBY_VISION_TOKEN".into()),
            corpus_dir: std::env::var("BOBBY_REALSITE_CORPUS_DIR")
                .ok()
                .map(std::path::PathBuf::from),
            timeout_ms: 120_000,
            ..config::VisionConfig::default()
        },
        context: Default::default(),
        nodes: Default::default(),
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
    }
}

/// What happened to one purpose, classified from the outcome's evidence.
/// Payloads are read via the Debug print in the pilot summary.
#[derive(Debug)]
#[allow(dead_code)]
enum Behavior {
    Deterministic,
    VisionCommit,
    /// Below-floor abstain; carries the near-miss window size — 0 means the
    /// page offered nothing actionable (wall/block), >0 means the model
    /// declined a populated window.
    VisionAbstain(usize),
    Failed(String),
}

fn classify(outcome: &CommandOutcome) -> Behavior {
    let evidence = match outcome {
        CommandOutcome::Completed { evidence, .. } => evidence,
        CommandOutcome::Failed {
            evidence, error, ..
        } => {
            if format!("{:?}", error.code).contains("VisionAssist") {
                println!("    vision error: {}", error.message);
                return Behavior::VisionAbstain(window_size(evidence));
            }
            return Behavior::Failed(format!("{:?}: {}", error.code, error.message));
        }
        other => return Behavior::Failed(format!("{other:?}")),
    };
    let escalated = evidence.iter().any(|item| {
        matches!(item, Evidence::IntentExecution { record } if record.vision_proposal_sha256.is_some())
    });
    if escalated {
        Behavior::VisionCommit
    } else {
        Behavior::Deterministic
    }
}

fn window_size(evidence: &[Evidence]) -> usize {
    evidence
        .iter()
        .filter_map(|item| match item {
            Evidence::IntentExecution { record } => Some(record.candidates.len()),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

async fn open_site(runtime: &RuntimeService, url: &str) -> TestResult<(SessionId, PageId)> {
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "vision-realsites".into(),
            proxy: None,
            execution_policy: ExecutionPolicy {
                vision_assist: true,
                ..ExecutionPolicy::default()
            },
            zigzagzig: false,
        })
        .await?;
    let page = runtime
        .open_page(OpenPageRequest {
            session_id: session.id.clone(),
        })
        .await?;
    let outcome = runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(45),
            command: RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
                url: url.into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 30_000,
            })),
        })
        .await;
    match outcome {
        CommandOutcome::Completed { .. } => Ok((session.id, page.id)),
        other => Err(format!("navigate to {url} failed: {other:?}").into()),
    }
}

#[tokio::test]
#[ignore = "requires installed Chrome, a running vision-proxy, and network access"]
async fn real_sites_pilot() -> TestResult<()> {
    assert!(
        std::env::var("BOBBY_GAUNTLET_VISION_ENDPOINT")
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false),
        "BOBBY_GAUNTLET_VISION_ENDPOINT unset; the pilot collects nothing without it"
    );
    let root = tempfile::tempdir()?;
    let config = base_config(root.path());
    let endpoint = std::env::var("BOBBY_GAUNTLET_VISION_ENDPOINT").unwrap();
    let bearer = std::env::var("BOBBY_VISION_TOKEN")
        .ok()
        .filter(|token| !token.is_empty());
    let assist = std::sync::Arc::new(
        intent_engine::HttpVisionAssist::new(endpoint, bearer, std::time::Duration::from_secs(120))
            .expect("http vision assist"),
    );
    let runtime = RuntimeService::build_with_vision_assist(&config, assist).await?;

    let mut consulted = 0usize;
    for (site, url, purposes) in SITES {
        let (session, page) = match open_site(&runtime, url).await {
            Ok(ids) => ids,
            Err(error) => {
                println!("{site}: NAVIGATION FAILED ({error})");
                continue;
            }
        };
        for purpose in *purposes {
            let outcome = runtime
                .submit_with_vision_capability(
                    CommandEnvelope {
                        schema_version: CommandEnvelope::SCHEMA_VERSION,
                        command_id: CommandId::new(),
                        workflow_id: WorkflowId::new(),
                        attempt_id: AttemptId::new(),
                        session_id: session.clone(),
                        page_id: Some(page.clone()),
                        deadline: Utc::now() + Duration::seconds(180),
                        command: RuntimeCommand::Intent(IntentCommand::Locate(LocateIntent {
                            purpose: (*purpose).into(),
                            hints: IntentHints::default(),
                        })),
                    },
                    true,
                )
                .await;
            if std::env::var("BOBBY_PILOT_DEBUG").is_ok() {
                match &outcome {
                    CommandOutcome::Completed { evidence, .. } => {
                        for item in evidence {
                            if let Evidence::IntentExecution { record } = item {
                                println!(
                                    "    DEBUG path={:?} verify={} sha={:?}",
                                    record.resolution_path,
                                    record.verification,
                                    record
                                        .vision_proposal_sha256
                                        .as_deref()
                                        .map(|s| s[..8].to_string())
                                );
                            }
                        }
                    }
                    CommandOutcome::Failed { error, .. } => {
                        println!("    DEBUG error: {:?}", error)
                    }
                    other => println!("    DEBUG outcome: {}", std::any::type_name_of_val(other)),
                }
            }
            let behavior = classify(&outcome);
            if matches!(
                behavior,
                Behavior::VisionCommit | Behavior::VisionAbstain(_)
            ) {
                consulted += 1;
            }
            println!("{site} | {purpose} -> {behavior:?}");
        }
    }

    assert!(
        consulted > 0,
        "vision was never consulted on any site; the pilot produced no evidence"
    );
    Ok(())
}
