//! A real Chromium workflow, exported and replayed through the `bobby` binary.
//! With `BOBBY_WRITE_REPLAY_SAMPLE=<path>` the replay is also written there;
//! that is how the docs sample is regenerated.

use std::path::Path;
use std::process::Command;

use chrono::{Duration, Utc};
use sdk_core::RuntimeService;
use types::{
    AttemptId, CaptureScreenshotCommand, ClickCommand, CommandEnvelope, CommandId, CommandOutcome,
    ControlAction, CreateSessionRequest, FillIntent, IntentCommand, IntentHints, NavigateCommand,
    OpenPageRequest, PrimitiveCommand, RuntimeCommand, ScreenshotMode, WaitUntil, WorkflowId,
};

fn chrome() -> std::path::PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(Into::into)
        .unwrap_or_else(|_| "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome".into())
}

fn bobby(args: &[&str], scope: &Path) {
    let output = Command::new(env!("CARGO_BIN_EXE_bobby"))
        .args(args)
        .env("BOBBY_BROWSER_SCOPE_DIR", scope)
        .env_remove("BOBBY_BROWSER_CONFIG")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "bobby {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn a_chromium_workflow_replays_with_its_screenshots() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let site = test_site::spawn().await;
    let mut config = config::AppConfig::default();
    config.http.allow_loopback = true;
    config.browser.executable = Some(chrome());
    config.browser.headless = true;
    config.browser.max_active = 1;
    config.browser.profiles_dir = data.join("profiles");
    config.browser.upload_roots = Vec::new();
    config.browser.downloads_dir = data.join("downloads");
    config.browser.artifacts_dir = data.join("artifacts");
    config.storage.journal_path = data.join("commands.jsonl");
    config.storage.checkpoints_dir = data.join("checkpoints");
    config.storage.authority_path = data.join("authority.json");
    config.storage.scheduler_journal_path = data.join("scheduler.jsonl");
    for directory in [
        &config.browser.artifacts_dir,
        &config.storage.checkpoints_dir,
    ] {
        std::fs::create_dir_all(directory).unwrap();
    }
    let config_path = root.path().join("config.toml");
    std::fs::write(
        &config_path,
        format!(
            "[storage]\njournal_path = \"{}\"\ncheckpoints_dir = \"{}\"\nauthority_path = \"{}\"\n\
             scheduler_journal_path = \"{}\"\n\n[browser]\nartifacts_dir = \"{}\"\n",
            config.storage.journal_path.display(),
            config.storage.checkpoints_dir.display(),
            config.storage.authority_path.display(),
            config.storage.scheduler_journal_path.display(),
            config.browser.artifacts_dir.display(),
        ),
    )
    .unwrap();

    let runtime = RuntimeService::build(&config).await.unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "replay".into(),
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
    let workflow = WorkflowId::new();
    let screenshot = || {
        RuntimeCommand::Primitive(PrimitiveCommand::CaptureScreenshot(
            CaptureScreenshotCommand {
                mode: ScreenshotMode::Viewport,
            },
        ))
    };
    let steps = [
        RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
            url: site.base_url(),
            wait_until: WaitUntil::DomContentLoaded,
            timeout_ms: 15_000,
        })),
        RuntimeCommand::Intent(IntentCommand::Fill(FillIntent {
            purpose: "Name".into(),
            hints: IntentHints {
                role: Some("textbox".into()),
                accessible_name: Some("Name".into()),
                ..IntentHints::default()
            },
            value: ControlAction::SetText {
                value: "Maya Chen".into(),
                clear_first: true,
            },
        })),
        screenshot(),
        RuntimeCommand::Primitive(PrimitiveCommand::Click(ClickCommand {
            selector: "#continue".into(),
            target: None,
            boundary: false,
            expected_url: None,
            modifiers: Vec::new(),
        })),
        screenshot(),
    ];
    for command in steps {
        let outcome = runtime
            .submit(CommandEnvelope {
                schema_version: CommandEnvelope::SCHEMA_VERSION,
                command_id: CommandId::new(),
                workflow_id: workflow.clone(),
                attempt_id: AttemptId::new(),
                session_id: session.id.clone(),
                page_id: Some(page.id.clone()),
                deadline: Utc::now() + Duration::seconds(30),
                command,
            })
            .await;
        assert!(
            matches!(outcome, CommandOutcome::Completed { .. }),
            "{outcome:?}"
        );
    }

    let scope = root.path().join("scope");
    let bundle = root.path().join("workflow.tar");
    let replay = root.path().join("workflow.html");
    let workflow_id = workflow.0.to_string();
    bobby(
        &[
            "audit",
            "export",
            "--workflow",
            &workflow_id,
            "--config",
            config_path.to_str().unwrap(),
            "--out",
            bundle.to_str().unwrap(),
        ],
        &scope,
    );
    bobby(
        &[
            "audit",
            "replay",
            bundle.to_str().unwrap(),
            "--out",
            replay.to_str().unwrap(),
        ],
        &scope,
    );
    let html = std::fs::read_to_string(&replay).unwrap();
    assert_eq!(
        html.matches("<section>").count(),
        5,
        "one section per command"
    );
    assert_eq!(html.matches("data:image/png;base64,").count(), 2);
    assert!(html.contains("intent fill · Name"));
    assert!(html.contains("click · #continue"));
    assert_eq!(html.matches("badge ok").count(), 5);
    if let Ok(sample) = std::env::var("BOBBY_WRITE_REPLAY_SAMPLE") {
        std::fs::write(sample, &html).unwrap();
    }
    runtime.sessions.delete(&session.id).await.unwrap();
}
