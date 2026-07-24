use std::path::PathBuf;

use chrono::{Duration, Utc};
use config::{AppConfig, BrowserConfig, ServerConfig, StorageConfig};
use sdk_core::RuntimeService;
use types::{
    AttemptId, CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest, Evidence,
    ExecutionRecord, ExtractField, ExtractIntent, ExtractValueKind, IntentCommand, IntentHints,
    IntentResolutionPath, NavigateCommand, OpenPageRequest, PageId, PrimitiveCommand,
    RuntimeCommand, SessionId, WaitUntil, WorkflowId,
};

fn intent_envelope(session_id: &SessionId, page_id: &PageId, command: IntentCommand) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: session_id.clone(),
        page_id: Some(page_id.clone()),
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Intent(command),
    }
}

async fn completed_navigate(runtime: &RuntimeService, session_id: &SessionId, page_id: &PageId, url: String) {
    match runtime
        .submit(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session_id.clone(),
            page_id: Some(page_id.clone()),
            deadline: Utc::now() + Duration::seconds(30),
            command: RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
                url,
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            })),
        })
        .await
    {
        CommandOutcome::Completed { .. } => {}
        outcome => panic!("navigate did not complete: {outcome:?}"),
    }
}

fn field(name: &str, purpose: &str, role: Option<&str>, value: ExtractValueKind) -> ExtractField {
    ExtractField {
        name: name.into(),
        purpose: purpose.into(),
        hints: IntentHints {
            role: role.map(str::to_owned),
            ..IntentHints::default()
        },
        value,
    }
}

fn intent_record(evidence: &[Evidence]) -> ExecutionRecord {
    evidence
        .iter()
        .find_map(|item| match item {
            Evidence::IntentExecution { record } => Some(record.clone()),
            _ => None,
        })
        .expect("IntentExecution evidence")
}

fn extraction<'a>(evidence: &'a [Evidence], field_name: &str) -> &'a Evidence {
    evidence
        .iter()
        .find(|item| matches!(item, Evidence::Extraction { field, .. } if field == field_name))
        .unwrap_or_else(|| panic!("no Extraction evidence for field {field_name}"))
}

async fn build_runtime(root: &std::path::Path) -> RuntimeService {
    let config = AppConfig {
        http: config::HttpConfig {
            allow_loopback: true,
            ..config::HttpConfig::default()
        },
        server: ServerConfig {
            host: "127.0.0.1".into(),
            port: 0,
        },
        browser: BrowserConfig {
            executable: Some(PathBuf::from(
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            )),
            profiles_dir: root.join("profiles"),
            headless: true,
            max_active: 1,
            upload_roots: Vec::new(),
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
        },
        interface: config::InterfaceConfig::default(),
    };
    RuntimeService::build(&config).await.unwrap()
}

/// Live Chromium proof: ExtractIntent resolves multiple independent fields in
/// one command and reads each field's declared value kind (innerText, href
/// attribute, and a named data-* attribute) off the live DOM.
#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn extract_reads_text_href_and_attribute_fields_on_live_chromium() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let runtime = build_runtime(root.path()).await;
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "intent-extract".into(),
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

    completed_navigate(
        &runtime,
        &session.id,
        &page.id,
        format!("{}/profile", fixture.base_url()),
    )
    .await;

    let extract = IntentCommand::Extract(ExtractIntent {
        purpose: "Profile summary".into(),
        fields: vec![
            field(
                "displayName",
                "Ada Lovelace",
                Some("heading"),
                ExtractValueKind::Text,
            ),
            field(
                "profileLink",
                "View profile",
                Some("link"),
                ExtractValueKind::Href,
            ),
            field(
                "userId",
                "Ada Lovelace",
                Some("heading"),
                ExtractValueKind::Attribute {
                    attribute: "data-user-id".into(),
                },
            ),
        ],
    });

    let outcome = runtime
        .submit(intent_envelope(&session.id, &page.id, extract))
        .await;
    let CommandOutcome::Completed { evidence, .. } = outcome else {
        panic!("extract did not complete: {outcome:?}");
    };

    let record = intent_record(&evidence);
    assert_eq!(record.intent_kind, "extract");
    assert_eq!(record.verification, "extracted");

    let Evidence::Extraction {
        value,
        resolution_path,
        error_code,
        ..
    } = extraction(&evidence, "displayName")
    else {
        unreachable!()
    };
    assert_eq!(value.as_deref(), Some("Ada Lovelace"));
    assert_eq!(*resolution_path, IntentResolutionPath::Deterministic);
    assert_eq!(*error_code, None);

    let Evidence::Extraction { value, .. } = extraction(&evidence, "profileLink") else {
        unreachable!()
    };
    assert_eq!(value.as_deref(), Some("/profile/42"));

    let Evidence::Extraction { value, .. } = extraction(&evidence, "userId") else {
        unreachable!()
    };
    assert_eq!(value.as_deref(), Some("42"));
}

/// Live Chromium proof: a field that cannot be resolved is reported missing
/// in its own evidence without failing the fields that did resolve.
#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn extract_reports_missing_field_without_failing_the_whole_command_on_live_chromium() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let runtime = build_runtime(root.path()).await;
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "intent-extract-partial".into(),
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

    completed_navigate(
        &runtime,
        &session.id,
        &page.id,
        format!("{}/profile", fixture.base_url()),
    )
    .await;

    let extract = IntentCommand::Extract(ExtractIntent {
        purpose: "Profile summary".into(),
        fields: vec![
            field(
                "displayName",
                "Ada Lovelace",
                Some("heading"),
                ExtractValueKind::Text,
            ),
            field(
                "missingField",
                "Does not exist anywhere on this page",
                Some("button"),
                ExtractValueKind::Text,
            ),
        ],
    });

    let outcome = runtime
        .submit(intent_envelope(&session.id, &page.id, extract))
        .await;
    let CommandOutcome::Completed { evidence, .. } = outcome else {
        panic!("extract did not complete: {outcome:?}");
    };

    let record = intent_record(&evidence);
    assert!(record.verification.starts_with("extractedPartial:missing="));
    assert!(record.verification.contains("missingField"));

    let Evidence::Extraction { value, .. } = extraction(&evidence, "displayName") else {
        unreachable!()
    };
    assert_eq!(value.as_deref(), Some("Ada Lovelace"));

    let Evidence::Extraction {
        value, error_code, ..
    } = extraction(&evidence, "missingField")
    else {
        unreachable!()
    };
    assert_eq!(*value, None);
    assert!(error_code.is_some());
}
