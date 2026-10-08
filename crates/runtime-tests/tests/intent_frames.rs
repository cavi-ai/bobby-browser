//! Live installed-Chromium proof: intent resolution auto-descends one level
//! into iframes, so an in-frame control resolves without the caller naming
//! a framePath for content it cannot see.

use std::path::PathBuf;

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, ServerConfig, StorageConfig};
use sdk_core::RuntimeService;
use test_site::{FixtureSite, Route};
use types::{
    AttemptId, ClickCommand, CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest,
    ElementState, FormControlKind, FormControlTarget, IntentCommand, IntentHints, NavigateCommand,
    OpenPageRequest, PrimitiveCommand, RuntimeCommand, TargetSpec, UploadFilesCommand,
    WaitCondition, WaitForCommand, WaitUntil, WorkflowId,
};

const DOCUMENTS: &str = r##"<!doctype html><title>Documents</title><main>
<header><h1>CUSTOMER RECORDS</h1><h2>Documents</h2></header>
<form aria-label="Upload customer document">
<div role="group" aria-label="Document dropzone">Drop a customer document here</div>
<label>Customer document <input type="file" aria-label="Customer document"></label>
<button type="submit">Upload document</button>
</form>
<div id="result"></div>
<script>
class StaticPreview extends HTMLElement {
  connectedCallback() {
    const root = this.attachShadow({ mode: "open" });
    const frame = document.createElement("iframe");
    frame.id = "document-preview";
    frame.title = "Document preview";
    frame.src = "/preview";
    const confirm = document.createElement("button");
    confirm.id = "confirm-preview";
    confirm.type = "button";
    confirm.setAttribute("aria-label", "Confirm document preview");
    confirm.textContent = "Confirm document";
    confirm.addEventListener("click", () => {
      fetch("/api/documents/confirm", { method: "POST" }).then(() => {
        confirm.disabled = true;
      });
    });
    root.append(frame, confirm);
  }
}
customElements.define("static-preview", StaticPreview);
const form = document.querySelector("form");
const input = form.querySelector("input");
form.addEventListener("submit", (event) => {
  event.preventDefault();
  if (!input.files.length) return;
  fetch("/api/documents", { method: "POST" }).then(() => {
    const preview = document.createElement("static-preview");
    preview.id = "document-preview-widget";
    preview.setAttribute("role", "group");
    preview.setAttribute("aria-label", "Document preview widget");
    document.getElementById("result").append(preview);
  });
});
</script></main>"##;

const PREVIEW: &str =
    "<!doctype html><title>Preview</title><p>Preview of the uploaded document</p>";

const CUSTOMERS: &str = r##"<!doctype html><title>Customers</title><main>
<h1>Customers</h1>
<div aria-label="Search customers">
<input aria-label="Search customers" type="search">
<button type="button">Search</button>
<ul role="listbox" aria-label="Search customers suggestions" hidden></ul>
</div>
<div id="found"></div>
<script>
const list = document.querySelector("ul");
document.querySelector("div button").addEventListener("click", () => {
  const option = document.createElement("li");
  option.setAttribute("role", "option");
  option.tabIndex = 0;
  option.textContent = "Atlas Labs";
  option.addEventListener("click", () => {
    list.hidden = true;
    const link = document.createElement("a");
    link.href = "/customers/cus_atlas";
    link.textContent = "Atlas Labs";
    document.getElementById("found").append(link);
  });
  list.replaceChildren(option);
  list.hidden = false;
});
</script></main>"##;

const CUSTOMER_DETAIL: &str = r##"<!doctype html><title>Atlas Labs</title><main>
<h1>Atlas Labs</h1>
<button type="button" role="combobox" aria-label="Customer priority" aria-expanded="false" id="priority">Normal</button>
<ul role="listbox" aria-label="Customer priority" hidden id="choices">
<li role="option" tabindex="0" data-value="low">Low</li>
<li role="option" tabindex="0" data-value="normal">Normal</li>
<li role="option" tabindex="0" data-value="high">High</li>
</ul>
<button type="button" id="save">Save priority</button>
<div id="saved"></div>
<script>
let value = "normal";
const button = document.getElementById("priority");
const choices = document.getElementById("choices");
button.addEventListener("click", () => { choices.hidden = !choices.hidden; });
for (const option of choices.querySelectorAll("li")) {
  option.addEventListener("click", () => {
    value = option.dataset.value;
    button.textContent = option.textContent;
    choices.hidden = true;
  });
}
document.getElementById("save").addEventListener("click", () => {
  fetch("/api/priority/" + value, { method: "POST" }).then(() => {
    document.getElementById("saved").textContent = "Priority saved";
  });
});
</script></main>"##;

const ONBOARDING: &str = r##"<!doctype html><title>New relationship</title><main>
<h1>New relationship</h1>
<section id="step1">
<label for="full-name">Full name</label><input id="full-name">
<button type="button" id="next1">Next</button>
</section>
<section id="step2" hidden>
<label for="company">Company name</label><input id="company">
<button type="button" id="next2">Next</button>
</section>
<section id="step3" hidden>
<label for="plan">Plan</label>
<select id="plan"><option value="starter">Starter</option><option value="growth">Growth</option></select>
<div id="cycle-row" hidden>
<label for="cycle">Billing cycle</label>
<select id="cycle"><option value="monthly">Monthly</option><option value="annual">Annual</option></select>
</div>
</section>
<script>
const show = (hide, reveal) => {
  document.getElementById(hide).hidden = true;
  document.getElementById(reveal).hidden = false;
};
document.getElementById("next1").addEventListener("click", () => show("step1", "step2"));
document.getElementById("next2").addEventListener("click", () => show("step2", "step3"));
document.getElementById("plan").addEventListener("change", (event) => {
  document.getElementById("cycle-row").hidden = event.target.value !== "growth";
});
</script></main>"##;

const REPORTS: &str = r##"<!doctype html><title>Reports</title><main>
<h1>Reports</h1>
<form aria-label="Generate report"><button type="submit">Generate report</button></form>
<div id="out"></div>
<script>
document.querySelector("form").addEventListener("submit", (event) => {
  event.preventDefault();
  fetch("/api/report", { method: "POST" }).then(() => {
    const out = document.getElementById("out");
    out.textContent = "Report ready ";
    const link = document.createElement("a");
    link.href = "/api/reports/rep_1/download";
    link.textContent = "atlas-operations.csv";
    out.append(link);
  });
});
</script></main>"##;

fn html(body: &str) -> Route {
    Route::Html(body.into())
}

fn ok_json() -> Route {
    Route::Raw {
        content_type: "application/json",
        body: "{}".into(),
    }
}

async fn site() -> FixtureSite {
    FixtureSite::spawn(vec![
        ("/customers", html(CUSTOMERS)),
        ("/customers/cus_atlas", html(CUSTOMER_DETAIL)),
        ("/customers/cus_atlas/documents", html(DOCUMENTS)),
        ("/preview", html(PREVIEW)),
        ("/onboarding", html(ONBOARDING)),
        ("/reports", html(REPORTS)),
        ("/api/documents", ok_json()),
        ("/api/documents/confirm", ok_json()),
        ("/api/priority/high", ok_json()),
        ("/api/report", ok_json()),
        (
            "/api/reports/rep_1/download",
            Route::Raw {
                content_type: "text/csv",
                body: "a,b\n".into(),
            },
        ),
    ])
    .await
}

/// Waits for `path` to have been requested `count` times, then holds briefly
/// so an extra request would be counted.
async fn expect_hits(server: &FixtureSite, path: &str, count: usize) {
    for _ in 0..100 {
        if server.hits(path) >= count {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert_eq!(server.hits(path), count, "requests to {path}");
}

fn chrome_executable() -> PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

fn preview_widget_target() -> TargetSpec {
    TargetSpec {
        css: Some("#document-preview-widget".into()),
        ..TargetSpec::default()
    }
}

fn in_preview_shadow(css: &str) -> TargetSpec {
    TargetSpec {
        css: Some(css.into()),
        shadow_path: vec![Box::new(preview_widget_target())],
        ..TargetSpec::default()
    }
}

fn target_spec(target: &FormControlTarget) -> TargetSpec {
    let segment = |segment: &types::SemanticTargetSegment| {
        Box::new(TargetSpec {
            role: Some(segment.role.clone()),
            accessible_name: Some(segment.accessible_name.clone()),
            ordinal: segment.ordinal,
            ..TargetSpec::default()
        })
    };
    TargetSpec {
        role: Some(target.role.clone()),
        accessible_name: Some(target.accessible_name.clone()),
        ordinal: target.ordinal,
        frame_path: target.frame_path.iter().map(segment).collect(),
        shadow_path: target.shadow_path.iter().map(segment).collect(),
        ..TargetSpec::default()
    }
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn intent_locate_resolves_inside_an_iframe_without_a_frame_path() {
    let server = site().await;
    let root = tempfile::tempdir().unwrap();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/approved-upload.txt");
    let config = AppConfig {
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
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
            profiles_dir: root.path().join("profiles"),
            headless: true,
            max_active: 8,
            upload_roots: vec![fixture.parent().unwrap().to_path_buf()],
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
            scheduler_journal_path: root.path().join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        context: Default::default(),
        nodes: Default::default(),
    };
    let runtime = RuntimeService::build(&config).await.unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "intent-frames".into(),
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

    let submit_primitive = |command: PrimitiveCommand| {
        runtime.submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(command),
        })
    };

    let outcome = submit_primitive(PrimitiveCommand::Navigate(NavigateCommand {
        url: server.url("/customers/cus_atlas/documents"),
        wait_until: WaitUntil::Interactive,
        timeout_ms: 30_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    let form_snapshot = runtime
        .form_snapshot(&session.id, &page.id, None)
        .await
        .unwrap();
    let file_target = form_snapshot
        .forms
        .iter()
        .flat_map(|form| form.controls.iter())
        .chain(form_snapshot.unowned_controls.iter())
        .find(|control| control.control_kind == FormControlKind::File)
        .and_then(|control| control.target.as_ref())
        .map(target_spec)
        .expect("file input target from form snapshot");
    let outcome = submit_primitive(PrimitiveCommand::UploadFiles(UploadFilesCommand {
        selector: String::new(),
        target: Some(file_target),
        paths: vec![fixture.to_string_lossy().into_owned()],
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    let outcome = submit_primitive(PrimitiveCommand::Click(types::ClickCommand {
        selector: "form[aria-label='Upload customer document'] button".into(),
        target: None,
        boundary: false,
        expected_url: None,
        modifiers: Vec::new(),
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    let outcome = submit_primitive(PrimitiveCommand::WaitFor(WaitForCommand {
        condition: WaitCondition::Element {
            target: Box::new(preview_widget_target()),
            state: ElementState::Attached,
        },
        timeout_ms: 15_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = submit_primitive(PrimitiveCommand::WaitFor(WaitForCommand {
        condition: WaitCondition::Element {
            target: Box::new(in_preview_shadow("#confirm-preview")),
            state: ElementState::Visible,
        },
        timeout_ms: 15_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "preview confirm never became visible: {outcome:?}"
    );

    // A plain (non-boundary) click into the shadow must work and must not
    // kill the page target.
    let shadow_button = || in_preview_shadow("#confirm-preview");
    let outcome = submit_primitive(PrimitiveCommand::WaitFor(WaitForCommand {
        condition: WaitCondition::Element {
            target: Box::new(shadow_button()),
            state: ElementState::Visible,
        },
        timeout_ms: 15_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "in-widget confirm button never became visible: {outcome:?}"
    );
    let outcome = submit_primitive(PrimitiveCommand::Click(types::ClickCommand {
        selector: String::new(),
        target: Some(shadow_button()),
        boundary: false,
        expected_url: None,
        modifiers: Vec::new(),
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "in-frame click failed: {outcome:?}"
    );

    // The page must still be alive and answer afterwards.
    let outcome =
        submit_primitive(PrimitiveCommand::Inspect(types::InspectCommand::default())).await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "page died after the in-frame click: {outcome:?}"
    );
    expect_hits(&server, "/api/documents/confirm", 1).await;

    runtime.sessions.delete(&session.id).await.unwrap();
}

/// Page-scoped text waits must match plain page text (regression: agents
/// reported {css:body} and {role:main} text waits never matching).
#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn page_scoped_text_wait_matches_body_text() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let config = AppConfig {
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
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
            scheduler_journal_path: root.path().join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        context: Default::default(),
        nodes: Default::default(),
    };
    let runtime = RuntimeService::build(&config).await.unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "text-wait".into(),
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
    let submit = |command: PrimitiveCommand| {
        runtime.submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(command),
        })
    };
    let outcome = submit(PrimitiveCommand::Navigate(NavigateCommand {
        url: format!("{}/", fixture.base_url()),
        wait_until: WaitUntil::Interactive,
        timeout_ms: 30_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    for target in [
        TargetSpec {
            css: Some("body".into()),
            ..TargetSpec::default()
        },
        TargetSpec {
            role: Some("main".into()),
            ..TargetSpec::default()
        },
    ] {
        let outcome = submit(PrimitiveCommand::WaitFor(WaitForCommand {
            condition: WaitCondition::Text {
                target: Box::new(target.clone()),
                matcher: types::TextMatch::Contains("Continue".into()),
            },
            timeout_ms: 5_000,
        }))
        .await;
        assert!(
            matches!(outcome, CommandOutcome::Completed { .. }),
            "page-scoped text wait failed for {target:?}: {outcome:?}"
        );
    }
    runtime.sessions.delete(&session.id).await.unwrap();
}

/// Full agent-path repro: intent submit with a page-scoped text
/// expectedState must observe the post-submit confirmation.
#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn intent_submit_with_text_expected_state_observes_the_confirmation() {
    let server = site().await;
    let root = tempfile::tempdir().unwrap();
    let config = AppConfig {
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
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
            scheduler_journal_path: root.path().join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        context: Default::default(),
        nodes: Default::default(),
    };
    let runtime = RuntimeService::build(&config).await.unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "text-expect".into(),
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
    let submit = |command: RuntimeCommand| {
        runtime.submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(60),
            command,
        })
    };

    let outcome = submit(RuntimeCommand::Primitive(PrimitiveCommand::Navigate(
        NavigateCommand {
            url: server.url("/customers"),
            wait_until: WaitUntil::Interactive,
            timeout_ms: 30_000,
        },
    )))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    // Search and open the customer, mirroring the journey.
    let outcome = submit(RuntimeCommand::Primitive(PrimitiveCommand::TypeText(
        types::TypeTextCommand {
            selector: "input[aria-label='Search customers']".into(),
            target: None,
            value: "Atlas".into(),
            clear_first: true,
            expected_url: None,
        },
    )))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = submit(RuntimeCommand::Primitive(PrimitiveCommand::Click(
        types::ClickCommand {
            selector: "[aria-label='Search customers'] button".into(),
            target: None,
            boundary: false,
            expected_url: None,
            modifiers: Vec::new(),
        },
    )))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    // Mirror the e2e gold path: pick the overlay option first (the bare
    // href click can land on the open overlay and never change the URL),
    // then follow the ledger link and wait for the customer URL before
    // waiting on the combobox.
    let outcome = submit(RuntimeCommand::Primitive(PrimitiveCommand::WaitFor(
        WaitForCommand {
            condition: WaitCondition::Element {
                target: Box::new(TargetSpec {
                    role: Some("option".into()),
                    accessible_name: Some("Atlas Labs".into()),
                    ..TargetSpec::default()
                }),
                state: types::ElementState::Visible,
            },
            timeout_ms: 5_000,
        },
    )))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = submit(RuntimeCommand::Primitive(PrimitiveCommand::Click(
        types::ClickCommand {
            selector: String::new(),
            target: Some(TargetSpec {
                role: Some("option".into()),
                accessible_name: Some("Atlas Labs".into()),
                ..TargetSpec::default()
            }),
            boundary: false,
            expected_url: None,
            modifiers: Vec::new(),
        },
    )))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = submit(RuntimeCommand::Primitive(PrimitiveCommand::WaitFor(
        WaitForCommand {
            condition: WaitCondition::Element {
                target: Box::new(TargetSpec {
                    role: Some("link".into()),
                    accessible_name: Some("Atlas Labs".into()),
                    ..TargetSpec::default()
                }),
                state: types::ElementState::Visible,
            },
            timeout_ms: 5_000,
        },
    )))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = submit(RuntimeCommand::Primitive(PrimitiveCommand::Click(
        types::ClickCommand {
            selector: String::new(),
            target: Some(TargetSpec {
                role: Some("link".into()),
                accessible_name: Some("Atlas Labs".into()),
                ..TargetSpec::default()
            }),
            boundary: false,
            expected_url: None,
            modifiers: Vec::new(),
        },
    )))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = submit(RuntimeCommand::Primitive(PrimitiveCommand::WaitFor(
        WaitForCommand {
            condition: WaitCondition::Url {
                matcher: types::TextMatch::Contains("/customers/cus_atlas".into()),
            },
            timeout_ms: 10_000,
        },
    )))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "customer detail URL was not reached: {outcome:?}"
    );

    let outcome = submit(RuntimeCommand::Primitive(PrimitiveCommand::WaitFor(
        WaitForCommand {
            condition: WaitCondition::Element {
                target: Box::new(TargetSpec {
                    role: Some("combobox".into()),
                    accessible_name: Some("Customer priority".into()),
                    ..TargetSpec::default()
                }),
                state: types::ElementState::Visible,
            },
            timeout_ms: 5_000,
        },
    )))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "customer detail did not become interactive: {outcome:?}"
    );

    // Set the priority via the v2 listbox (role=combobox button, not a
    // native select), then submit with the text expectedState the agents used.
    let outcome = submit(RuntimeCommand::Primitive(PrimitiveCommand::Click(
        types::ClickCommand {
            selector: String::new(),
            target: Some(TargetSpec {
                role: Some("combobox".into()),
                accessible_name: Some("Customer priority".into()),
                ..TargetSpec::default()
            }),
            boundary: false,
            expected_url: None,
            modifiers: Vec::new(),
        },
    )))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = submit(RuntimeCommand::Primitive(PrimitiveCommand::WaitFor(
        WaitForCommand {
            condition: WaitCondition::Element {
                target: Box::new(TargetSpec {
                    role: Some("option".into()),
                    accessible_name: Some("High".into()),
                    ..TargetSpec::default()
                }),
                state: types::ElementState::Visible,
            },
            timeout_ms: 5_000,
        },
    )))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = submit(RuntimeCommand::Primitive(PrimitiveCommand::Click(
        types::ClickCommand {
            selector: String::new(),
            target: Some(TargetSpec {
                role: Some("option".into()),
                accessible_name: Some("High".into()),
                ..TargetSpec::default()
            }),
            boundary: false,
            expected_url: None,
            modifiers: Vec::new(),
        },
    )))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    // Boundary commands need a matching verified checkpoint: pin the ids.
    let workflow_id = WorkflowId::new();
    let attempt_id = AttemptId::new();
    let command_id = CommandId::new();
    let preflight = runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: workflow_id.clone(),
            attempt_id: attempt_id.clone(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(PrimitiveCommand::Inspect(
                types::InspectCommand::default(),
            )),
        })
        .await;
    let CommandOutcome::Completed {
        evidence: observed,
        command_id: inspect_id,
    } = preflight
    else {
        panic!("preflight inspect failed: {preflight:?}")
    };
    let (url, title) = observed
        .iter()
        .find_map(|item| match item {
            types::Evidence::Inspection { url, title, .. } => Some((url.clone(), title.clone())),
            _ => None,
        })
        .unwrap();
    runtime
        .checkpoint(
            types::WorkflowCheckpoint {
                schema_version: types::WorkflowCheckpoint::SCHEMA_VERSION,
                checkpoint_id: types::CheckpointId::new(),
                workflow_id: workflow_id.clone(),
                attempt_id: attempt_id.clone(),
                session_id: session.id.clone(),
                page_id: page.id.clone(),
                restart_url: url.clone(),
                current_url: url.clone(),
                cursor: Some(inspect_id.clone()),
                boundary_command_id: Some(command_id.clone()),
                recovery_class: types::CommandClass::Boundary,
                invariants: vec![
                    types::CheckpointInvariant::Url { value: url },
                    types::CheckpointInvariant::Title { value: title },
                ],
                replayable_inputs: Vec::new(),
                evidence: Vec::new(),
                recovery_history: Vec::new(),
                recovery_receipts: Vec::new(),
                created_at: Utc::now(),
            },
            vec![inspect_id],
        )
        .await
        .unwrap();

    let outcome = runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id,
            workflow_id,
            attempt_id,
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(60),
            command: RuntimeCommand::Intent(IntentCommand::SubmitAndVerify(
                types::SubmitAndVerifyIntent {
                    purpose: "Save the customer priority".into(),
                    hints: IntentHints {
                        role: Some("button".into()),
                        accessible_name: Some("Save priority".into()),
                        ..IntentHints::default()
                    },
                    expected_state: WaitForCommand {
                        condition: WaitCondition::Text {
                            target: Box::new(TargetSpec {
                                css: Some("body".into()),
                                ..TargetSpec::default()
                            }),
                            matcher: types::TextMatch::Contains("Priority saved".into()),
                        },
                        timeout_ms: 20_000,
                    },
                },
            )),
        })
        .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "intent submit with text expectedState failed: {outcome:?}"
    );
    expect_hits(&server, "/api/priority/high", 1).await;
    runtime.sessions.delete(&session.id).await.unwrap();
}

/// Shared setup: local fixture site, headless installed-Chromium runtime,
/// documents page with the upload flow completed so the preview iframe is live.
#[allow(dead_code)]
struct DocumentsPageProbe {
    server: FixtureSite,
    runtime: RuntimeService,
    session_id: types::SessionId,
    page_id: types::PageId,
    _root: tempfile::TempDir,
}

#[allow(dead_code)]
async fn documents_page_with_preview(seed: &str) -> DocumentsPageProbe {
    let server = site().await;
    let root = tempfile::tempdir().unwrap();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/approved-upload.txt");
    let config = AppConfig {
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
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
            profiles_dir: root.path().join("profiles"),
            headless: true,
            max_active: 8,
            upload_roots: vec![fixture.parent().unwrap().to_path_buf()],
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
            scheduler_journal_path: root.path().join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        context: Default::default(),
        nodes: Default::default(),
    };
    let runtime = RuntimeService::build(&config).await.unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: seed.into(),
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
    let submit = |command: PrimitiveCommand| {
        runtime.submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(command),
        })
    };
    let outcome = submit(PrimitiveCommand::Navigate(NavigateCommand {
        url: server.url("/customers/cus_atlas/documents"),
        wait_until: WaitUntil::Interactive,
        timeout_ms: 30_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    // The documents route renders its form from the SPA bundle, so `Interactive`
    // (DOMContentLoaded) can land before the file input exists. Snapshotting
    // straight after the navigate raced the render and failed this test in CI
    // with "file input target from form snapshot".
    let outcome = submit(PrimitiveCommand::WaitFor(WaitForCommand {
        condition: WaitCondition::Element {
            target: Box::new(TargetSpec {
                css: Some("input[type=\"file\"]".into()),
                ..TargetSpec::default()
            }),
            state: ElementState::Attached,
        },
        timeout_ms: 15_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let form_snapshot = runtime
        .form_snapshot(&session.id, &page.id, None)
        .await
        .unwrap();
    let file_target = form_snapshot
        .forms
        .iter()
        .flat_map(|form| form.controls.iter())
        .chain(form_snapshot.unowned_controls.iter())
        .find(|control| control.control_kind == FormControlKind::File)
        .and_then(|control| control.target.as_ref())
        .map(target_spec)
        .expect("file input target from form snapshot");
    let outcome = submit(PrimitiveCommand::UploadFiles(UploadFilesCommand {
        selector: String::new(),
        target: Some(file_target),
        paths: vec![fixture.to_string_lossy().into_owned()],
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = submit(PrimitiveCommand::Click(types::ClickCommand {
        selector: "form[aria-label='Upload customer document'] button".into(),
        target: None,
        boundary: false,
        expected_url: None,
        modifiers: Vec::new(),
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = submit(PrimitiveCommand::WaitFor(WaitForCommand {
        condition: WaitCondition::Element {
            target: Box::new(preview_widget_target()),
            state: ElementState::Attached,
        },
        timeout_ms: 15_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    DocumentsPageProbe {
        server,
        runtime,
        session_id: session.id,
        page_id: page.id,
        _root: root,
    }
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn control_action_reports_revealed_conditional_controls() {
    let server = site().await;
    let root = tempfile::tempdir().unwrap();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/approved-upload.txt");
    let config = AppConfig {
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
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
            profiles_dir: root.path().join("profiles"),
            headless: true,
            max_active: 8,
            upload_roots: vec![fixture.parent().unwrap().to_path_buf()],
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
            scheduler_journal_path: root.path().join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        context: Default::default(),
        nodes: Default::default(),
    };
    let runtime = RuntimeService::build(&config).await.unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "revealed-controls".into(),
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
    let submit = |command: PrimitiveCommand| {
        runtime.submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(command),
        })
    };
    let outcome = submit(PrimitiveCommand::Navigate(NavigateCommand {
        url: server.url("/onboarding"),
        wait_until: WaitUntil::Interactive,
        timeout_ms: 30_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    let click_named = |name: &str| {
        submit(PrimitiveCommand::Click(ClickCommand {
            selector: String::new(),
            target: Some(TargetSpec {
                role: Some("button".into()),
                accessible_name: Some(name.into()),
                ..TargetSpec::default()
            }),
            boundary: false,
            expected_url: None,
            modifiers: Vec::new(),
        }))
    };
    let wait_named = |role: &str, name: &str| {
        submit(PrimitiveCommand::WaitFor(WaitForCommand {
            condition: WaitCondition::Element {
                target: Box::new(TargetSpec {
                    role: Some(role.into()),
                    accessible_name: Some(name.into()),
                    ..TargetSpec::default()
                }),
                state: ElementState::Visible,
            },
            timeout_ms: 10_000,
        }))
    };
    let outcome = click_named("Next").await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = wait_named("textbox", "Company name").await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = click_named("Next").await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = wait_named("combobox", "Plan").await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    // Selecting the growth plan reveals the Billing cycle select, which does
    // not exist in the snapshot taken beforehand.
    let outcome = submit(PrimitiveCommand::ControlAction(
        types::ControlActionCommand {
            target: types::FormControlTarget {
                role: "combobox".into(),
                accessible_name: "Plan".into(),
                ordinal: None,
                frame_path: Vec::new(),
                shadow_path: Vec::new(),
            },
            action: types::ControlAction::SelectOne {
                value: "growth".into(),
            },
        },
    ))
    .await;
    let CommandOutcome::Completed { evidence, .. } = outcome else {
        panic!("plan select failed: {outcome:?}");
    };
    let revealed = evidence.iter().find_map(|item| match item {
        types::Evidence::ControlAction { action } => Some(action.revealed_controls.clone()),
        _ => None,
    });
    let revealed = revealed.expect("control action evidence");
    let billing = revealed
        .iter()
        .find(|control| control.accessible_name.as_deref() == Some("Billing cycle"));
    let billing = billing.unwrap_or_else(|| {
        panic!("Billing cycle select missing from revealed controls: {revealed:?}")
    });
    let target = billing.target.as_ref().expect("revealed control target");
    assert_eq!(target.role, "combobox");
    runtime.sessions.delete(&session.id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn a11y_snapshot_exposes_link_urls() {
    let server = site().await;
    let root = tempfile::tempdir().unwrap();
    let fixture =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/approved-upload.txt");
    let config = AppConfig {
        cdp: config::CdpConfig::default(),
        mcp: config::McpConfig::default(),
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
            profiles_dir: root.path().join("profiles"),
            headless: true,
            max_active: 8,
            upload_roots: vec![fixture.parent().unwrap().to_path_buf()],
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
            scheduler_journal_path: root.path().join("scheduler-jobs.jsonl"),
        },
        interface: config::InterfaceConfig::default(),
        observability: config::ObservabilityConfig::default(),
        vision: config::VisionConfig::default(),
        context: Default::default(),
        nodes: Default::default(),
    };
    let runtime = RuntimeService::build(&config).await.unwrap();
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "a11y-link-urls".into(),
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
    let submit = |command: PrimitiveCommand| {
        runtime.submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: Utc::now() + Duration::seconds(45),
            command: RuntimeCommand::Primitive(command),
        })
    };
    let outcome = submit(PrimitiveCommand::Navigate(NavigateCommand {
        url: server.url("/reports"),
        wait_until: WaitUntil::Interactive,
        timeout_ms: 30_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = submit(PrimitiveCommand::Click(types::ClickCommand {
        selector: "form[aria-label='Generate report'] button".into(),
        target: None,
        boundary: false,
        expected_url: None,
        modifiers: Vec::new(),
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );
    let outcome = submit(PrimitiveCommand::WaitFor(WaitForCommand {
        condition: WaitCondition::Text {
            target: Box::new(TargetSpec {
                css: Some("body".into()),
                ..TargetSpec::default()
            }),
            matcher: types::TextMatch::Contains("Report ready".into()),
        },
        timeout_ms: 30_000,
    }))
    .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "{outcome:?}"
    );

    let outcome = submit(PrimitiveCommand::AccessibilitySnapshot(
        types::AccessibilitySnapshotCommand {
            max_nodes: None,
            target: None,
        },
    ))
    .await;
    let CommandOutcome::Completed { evidence, .. } = outcome else {
        panic!("a11y snapshot failed: {outcome:?}");
    };
    let nodes = evidence
        .iter()
        .find_map(|item| match item {
            types::Evidence::AccessibilitySnapshot { nodes, .. } => Some(nodes),
            _ => None,
        })
        .expect("snapshot evidence");
    fn flatten<'a>(
        nodes: &'a [types::AccessibilityNode],
        out: &mut Vec<&'a types::AccessibilityNode>,
    ) {
        for node in nodes {
            out.push(node);
            flatten(&node.children, out);
        }
    }
    let mut flat = Vec::new();
    flatten(nodes, &mut flat);
    let link = flat
        .iter()
        .find(|node| {
            node.role.as_deref() == Some("link")
                && node
                    .name
                    .as_deref()
                    .is_some_and(|name| name.contains("atlas-operations.csv"))
        })
        .expect("download link missing from a11y snapshot");
    let url = link.url.as_deref().expect("download link carries no url");
    assert!(
        url.contains("/api/reports/") && url.ends_with("/download"),
        "unexpected link url: {url}"
    );
    runtime.sessions.delete(&session.id).await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn a11y_snapshot_scopes_to_a_target_subtree() {
    let probe = documents_page_with_preview("a11y-scoped").await;
    let snapshot = |target: TargetSpec| {
        probe.runtime.submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: probe.session_id.clone(),
            page_id: Some(probe.page_id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(PrimitiveCommand::AccessibilitySnapshot(
                types::AccessibilitySnapshotCommand {
                    max_nodes: None,
                    target: Some(target),
                },
            )),
        })
    };
    fn flatten_owned(nodes: &[types::AccessibilityNode], out: &mut Vec<types::AccessibilityNode>) {
        for node in nodes {
            out.push(node.clone());
            flatten_owned(&node.children, out);
        }
    }
    let nodes_of = |outcome: CommandOutcome| {
        let CommandOutcome::Completed { evidence, .. } = outcome else {
            panic!("scoped snapshot failed: {outcome:?}");
        };
        let nodes = evidence
            .iter()
            .find_map(|item| match item {
                types::Evidence::AccessibilitySnapshot { nodes, .. } => Some(nodes.clone()),
                _ => None,
            })
            .expect("snapshot evidence");
        let mut flat = Vec::new();
        flatten_owned(&nodes, &mut flat);
        flat
    };

    // Main-frame scope: the upload form subtree carries its controls and none
    // of the page chrome around it.
    let flat = nodes_of(
        snapshot(TargetSpec {
            role: Some("form".into()),
            accessible_name: Some("Upload customer document".into()),
            ..TargetSpec::default()
        })
        .await,
    );
    assert!(
        flat.iter()
            .any(|node| node.name.as_deref() == Some("Upload document")),
        "scoped form snapshot lost its submit button: {flat:?}"
    );
    assert!(
        !flat
            .iter()
            .any(|node| node.name.as_deref() == Some("CUSTOMER RECORDS")),
        "scoped snapshot leaked page chrome: {flat:?}"
    );

    // Widget scope: the preview widget carries the confirm control and none
    // of the page chrome around it.
    let flat = nodes_of(
        snapshot(TargetSpec {
            role: Some("group".into()),
            accessible_name: Some("Document preview widget".into()),
            ..TargetSpec::default()
        })
        .await,
    );
    assert!(
        flat.iter()
            .any(|node| node.name.as_deref() == Some("Confirm document preview")),
        "widget-scoped snapshot lost the confirm button: {flat:?}"
    );
    assert!(
        !flat
            .iter()
            .any(|node| node.name.as_deref() == Some("Upload document")),
        "widget-scoped snapshot leaked the outer page: {flat:?}"
    );
    probe
        .runtime
        .sessions
        .delete(&probe.session_id)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn a11y_snapshot_descends_into_iframes() {
    let probe = documents_page_with_preview("a11y-frames").await;
    let evidence = probe
        .runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: probe.session_id.clone(),
            page_id: Some(probe.page_id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(PrimitiveCommand::AccessibilitySnapshot(
                types::AccessibilitySnapshotCommand {
                    max_nodes: None,
                    target: None,
                },
            )),
        })
        .await;
    let CommandOutcome::Completed { evidence, .. } = evidence else {
        panic!("a11y snapshot failed: {evidence:?}");
    };
    let nodes = evidence
        .iter()
        .find_map(|item| match item {
            types::Evidence::AccessibilitySnapshot { nodes, .. } => Some(nodes),
            _ => None,
        })
        .expect("snapshot evidence");
    fn flatten<'a>(
        nodes: &'a [types::AccessibilityNode],
        out: &mut Vec<&'a types::AccessibilityNode>,
    ) {
        for node in nodes {
            out.push(node);
            flatten(&node.children, out);
        }
    }
    let mut flat = Vec::new();
    flatten(nodes, &mut flat);
    for node in &flat {
        if node.role.is_some() || node.name.is_some() {
            println!(
                "role={:?} name={:?} target={:?}",
                node.role, node.name, node.target
            );
        }
    }
    let confirm = flat
        .iter()
        .find(|node| {
            node.name.as_deref() == Some("Confirm document preview")
                && node.role.as_deref() == Some("button")
                && node
                    .target
                    .as_ref()
                    .is_some_and(|target| !target.frame_path.is_empty())
        })
        .or_else(|| {
            flat.iter().find(|node| {
                node.name.as_deref() == Some("Confirm document preview")
                    && node.role.as_deref() == Some("button")
            })
        });
    let confirm = confirm.expect("confirm button missing from a11y snapshot");
    let target = confirm
        .target
        .as_ref()
        .expect("confirm button has no target");
    let control_target = types::FormControlTarget {
        role: target.role.clone(),
        accessible_name: target.accessible_name.clone(),
        ordinal: target.ordinal,
        frame_path: Vec::new(),
        shadow_path: vec![types::SemanticTargetSegment {
            role: "group".into(),
            accessible_name: "Document preview widget".into(),
            ordinal: None,
        }],
    };

    // Activate the widget-shadow confirm the same way the documents e2e does.
    let outcome = probe
        .runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: probe.session_id.clone(),
            page_id: Some(probe.page_id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(PrimitiveCommand::ControlAction(
                types::ControlActionCommand {
                    target: control_target,
                    action: types::ControlAction::Activate,
                },
            )),
        })
        .await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "control_action with the snapshot target failed: {outcome:?}"
    );
    expect_hits(&probe.server, "/api/documents/confirm", 1).await;
    probe
        .runtime
        .sessions
        .delete(&probe.session_id)
        .await
        .unwrap();
}
