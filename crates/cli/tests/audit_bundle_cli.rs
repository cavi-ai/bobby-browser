//! `bobby audit key|export|verify` against a runtime data directory written by
//! the real command journal.

use std::path::Path;
use std::process::{Command, Output};

use chrono::Utc;
use types::{
    AttemptId, CommandEnvelope, CommandId, CommandPhase, NavigateCommand, PageId, PrimitiveCommand,
    RuntimeCommand, SessionId, WaitUntil, WorkflowId,
};
use workflow_journal::{CommandJournal, JournalRecord, JsonlJournal};

fn bobby(args: &[&str], root: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_bobby"))
        .args(args)
        .env("BOBBY_BROWSER_SCOPE_DIR", root.join("scope"))
        .env_remove("BOBBY_BROWSER_CONFIG")
        .output()
        .unwrap()
}

#[tokio::test]
async fn audit_export_and_verify_through_the_cli() {
    let root = tempfile::tempdir().unwrap();
    let data = root.path().join("data");
    let config = root.path().join("config.toml");
    std::fs::write(
        &config,
        format!(
            "[storage]\njournal_path = \"{journal}\"\ncheckpoints_dir = \"{checkpoints}\"\n\
             authority_path = \"{authority}\"\nscheduler_journal_path = \"{scheduler}\"\n\n\
             [browser]\nartifacts_dir = \"{artifacts}\"\n",
            journal = data.join("commands.jsonl").display(),
            checkpoints = data.join("checkpoints").display(),
            authority = data.join("authority.json").display(),
            scheduler = data.join("scheduler.jsonl").display(),
            artifacts = data.join("artifacts").display(),
        ),
    )
    .unwrap();
    let workflow = WorkflowId::new();
    let command = CommandId::new();
    let journal = JsonlJournal::open(data.join("commands.jsonl"))
        .await
        .unwrap();
    journal
        .append(JournalRecord {
            sequence: 0,
            recorded_at: Utc::now(),
            command_id: command.clone(),
            phase: CommandPhase::Executing,
            envelope: Some(CommandEnvelope {
                schema_version: CommandEnvelope::SCHEMA_VERSION,
                command_id: command,
                workflow_id: workflow.clone(),
                attempt_id: AttemptId::new(),
                session_id: SessionId::new(),
                page_id: Some(PageId::new()),
                deadline: Utc::now() + chrono::Duration::seconds(30),
                command: RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
                    url: "https://shop.test/".into(),
                    wait_until: WaitUntil::DomContentLoaded,
                    timeout_ms: 10_000,
                })),
            }),
            outcome: None,
            prepared_result: None,
        })
        .await
        .unwrap();

    let key = bobby(&["audit", "key"], root.path());
    assert!(
        key.status.success(),
        "{}",
        String::from_utf8_lossy(&key.stderr)
    );
    let public_key = String::from_utf8(key.stdout).unwrap().trim().to_owned();
    assert_eq!(public_key.len(), 64, "{public_key}");
    assert!(root.path().join("scope/audit-signing-key.pk8").is_file());

    let bundle = root.path().join("bundle.tar");
    let workflow_id = workflow.0.to_string();
    let export = bobby(
        &[
            "audit",
            "export",
            "--workflow",
            &workflow_id,
            "--config",
            config.to_str().unwrap(),
            "--out",
            bundle.to_str().unwrap(),
        ],
        root.path(),
    );
    assert!(
        export.status.success(),
        "{}",
        String::from_utf8_lossy(&export.stderr)
    );
    assert!(String::from_utf8_lossy(&export.stderr).contains(&public_key));

    let verify = bobby(
        &[
            "audit",
            "verify",
            bundle.to_str().unwrap(),
            "--public-key",
            &public_key,
        ],
        root.path(),
    );
    assert!(
        verify.status.success(),
        "{}",
        String::from_utf8_lossy(&verify.stderr)
    );
    let stdout = String::from_utf8_lossy(&verify.stdout);
    assert!(
        stdout.contains(&format!("verified: workflow {workflow_id}, 1 files")),
        "{stdout}"
    );
    assert!(stdout.contains("(pinned)"), "{stdout}");

    let mut bytes = std::fs::read(&bundle).unwrap();
    let at = bytes
        .windows(b"shop.test".len())
        .position(|window| window == b"shop.test")
        .unwrap();
    bytes[at] = b'S';
    std::fs::write(&bundle, bytes).unwrap();
    let tampered = bobby(&["audit", "verify", bundle.to_str().unwrap()], root.path());
    assert!(!tampered.status.success());
    assert!(
        String::from_utf8_lossy(&tampered.stderr).contains("journal.jsonl"),
        "{}",
        String::from_utf8_lossy(&tampered.stderr)
    );
}
