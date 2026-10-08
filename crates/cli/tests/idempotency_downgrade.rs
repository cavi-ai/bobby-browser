//! Real CLI rollback using isolated configuration and durable ledger files.
use chrono::{Duration, Utc};
use interface_core::{IdempotencyReservation, IdempotencyStore};
use std::{
    path::{Path, PathBuf},
    process::Command,
};
use types::{CommandOutcome, CorrelationId, IdempotencyKey, InterfaceOperation, PrincipalId};

fn fixture(root: &Path) -> (PathBuf, Vec<PathBuf>) {
    let mut config = config::AppConfig::default();
    config.storage.journal_path = root.join("commands.jsonl");
    config.storage.scheduler_journal_path = root.join("jobs.jsonl");
    config.storage.checkpoints_dir = root.join("checkpoints");
    config.storage.authority_path = root.join("authority.json");
    config.browser.profiles_dir = root.join("profiles");
    config.browser.downloads_dir = root.join("downloads");
    config.browser.artifacts_dir = root.join("artifacts");
    config.context.dir = Some(root.join("context"));
    let path = root.join("config.toml");
    std::fs::write(&path, toml::to_string(&config).unwrap()).unwrap();
    let ledgers = vec![
        config
            .storage
            .journal_path
            .with_extension("idempotency.json"),
        config
            .storage
            .journal_path
            .with_extension("lifecycle-idempotency.json"),
        config
            .storage
            .scheduler_journal_path
            .with_extension("idempotency.json"),
    ];
    (path, ledgers)
}

async fn seed(path: &Path) -> IdempotencyStore {
    let store = IdempotencyStore::open_durable(path, |_| async {
        Ok::<Option<CommandOutcome>, std::io::Error>(None)
    })
    .await
    .unwrap();
    let now = Utc::now();
    let IdempotencyReservation::Acquired(permit) = store
        .reserve(
            PrincipalId::from_uuid(uuid::Uuid::nil()),
            IdempotencyKey::try_from("unknown").unwrap(),
            InterfaceOperation::SubmitCommand,
            [0; 32],
            now,
            now + Duration::seconds(30),
            CorrelationId::new(),
        )
        .await
        .unwrap()
    else {
        panic!("acquire");
    };
    drop(permit);
    store
}

fn doctor(config: &Path, root: &Path) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_bobby"))
        .current_dir(root)
        .args([
            "doctor",
            "--fix",
            "--downgrade-idempotency",
            "--skip-health",
            "--json",
            "--config",
        ])
        .arg(config)
        .arg("--bootstrap-env")
        .arg(root.join("bootstrap.env"))
        .env(
            "AUTOMATION_RUNTIME_BROWSER_SELECTION",
            r#"{"preference":{"mode":"managedChromium"}}"#,
        )
        .env("BOBBY_BROWSER_INSTALL_DIR", root.join("not-installed"))
        .output()
        .unwrap()
}

#[tokio::test]
async fn doctor_converts_all_configured_ledgers_and_reports_backups() {
    let root = tempfile::tempdir().unwrap();
    let (config, paths) = fixture(root.path());
    let mut originals = Vec::new();
    for path in &paths {
        drop(seed(path).await);
        originals.push(std::fs::read(path).unwrap());
    }
    let output = doctor(&config, root.path());
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let actions = String::from_utf8(output.stderr).unwrap();
    assert_eq!(actions.matches("converted ").count(), 3, "{actions}");
    assert_eq!(
        actions.matches("source preserved at").count(),
        3,
        "{actions}"
    );
    for path in &paths {
        let old: serde_json::Value = serde_json::from_slice(&std::fs::read(path).unwrap()).unwrap();
        assert_eq!(old["schemaVersion"], 1);
        assert_eq!(old["entries"][0]["state"]["kind"], "unresolved");
        assert!(old["entries"][0]["expiresAt"].is_null());
    }
    let backups: Vec<_> = std::fs::read_dir(root.path())
        .unwrap()
        .filter_map(|entry| {
            let entry = entry.unwrap();
            entry
                .file_name()
                .to_string_lossy()
                .ends_with(".v2.backup")
                .then(|| std::fs::read(entry.path()).unwrap())
        })
        .collect();
    assert_eq!(backups.len(), 3);
    assert!(originals.iter().all(|bytes| backups.contains(bytes)));
    assert!(
        serde_json::from_slice::<serde_json::Value>(&output.stdout).is_ok(),
        "JSON report must stay on stdout"
    );
}

#[tokio::test]
async fn doctor_refuses_live_or_damaged_history_without_erasing_it() {
    let root = tempfile::tempdir().unwrap();
    let (config, paths) = fixture(root.path());
    let store = seed(&paths[0]).await;
    let original = std::fs::read(&paths[0]).unwrap();
    let output = doctor(&config, root.path());
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("stop Bobby"));
    assert_eq!(std::fs::read(&paths[0]).unwrap(), original);
    drop(store);
    let damaged = b"damaged ledger";
    std::fs::write(&paths[0], damaged).unwrap();
    let output = doctor(&config, root.path());
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("preserve the ledger and any .v2.backup"));
    assert_eq!(std::fs::read(&paths[0]).unwrap(), damaged);
}
