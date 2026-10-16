use std::{fs, process::Command};

#[test]
fn incident_bundle_preserves_damaged_sources_and_excludes_payloads() {
    let root = tempfile::tempdir().unwrap();
    let journal = root.path().join("commands.jsonl");
    let bytes = b"{\"private\":\"PAYLOAD_CANARY\"}\n{broken";
    fs::write(&journal, bytes).unwrap();
    let config = root.path().join("config.toml");
    fs::write(&config, "[storage]\njournal_path='commands.jsonl'\nscheduler_journal_path='jobs.jsonl'\nauthority_path='authority.json'\ncheckpoints_dir='checkpoints'\n").unwrap();
    let output = root.path().join("incident.json");
    let result = Command::new(env!("CARGO_BIN_EXE_bobby"))
        .args(["doctor", "--config"])
        .arg(&config)
        .arg("--incident-bundle")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let bundle: serde_json::Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    assert_eq!(bundle["schemaVersion"], 1);
    assert_eq!(bundle["replayAuthority"], false);
    let stores = bundle["stores"].as_array().unwrap();
    assert_eq!(stores.len(), 5);
    let commands = stores
        .iter()
        .find(|s| s["name"] == "command-journal")
        .unwrap();
    assert_eq!(commands["status"], "needsRepair");
    use sha2::{Digest, Sha256};
    assert_eq!(commands["sha256"], hex::encode(Sha256::digest(bytes)));
    assert_eq!(fs::read(&journal).unwrap(), bytes);
    assert!(!fs::read_to_string(&output)
        .unwrap()
        .contains("PAYLOAD_CANARY"));
    assert!(!root.path().join("commands.lock").exists());
    let before = fs::read(&output).unwrap();
    let repeated = Command::new(env!("CARGO_BIN_EXE_bobby"))
        .args(["doctor", "--config"])
        .arg(&config)
        .arg("--incident-bundle")
        .arg(&output)
        .output()
        .unwrap();
    assert!(!repeated.status.success());
    assert_eq!(fs::read(&output).unwrap(), before);
}

#[test]
fn incident_bundle_conflicts_with_mutating_doctor_modes() {
    let result = Command::new(env!("CARGO_BIN_EXE_bobby"))
        .args(["doctor", "--fix", "--incident-bundle", "incident.json"])
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(String::from_utf8_lossy(&result.stderr).contains("cannot be used with"));
}

#[test]
fn bundle_refuses_a_missing_source_as_its_output() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    fs::write(
        &config,
        "[storage]\njournal_path='commands.jsonl'\nscheduler_journal_path='jobs.jsonl'\nauthority_path='authority.json'\ncheckpoints_dir='checkpoints'\n",
    )
    .unwrap();
    let source = root.path().join("jobs.jsonl");
    let result = Command::new(env!("CARGO_BIN_EXE_bobby"))
        .args(["doctor", "--config"])
        .arg(&config)
        .arg("--incident-bundle")
        .arg(&source)
        .output()
        .unwrap();
    assert!(!result.status.success());
    assert!(!source.exists());
}

#[test]
fn busy_idempotency_history_is_reported_without_claiming_a_hash() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    fs::write(
        &config,
        "[storage]\njournal_path='commands.jsonl'\nscheduler_journal_path='jobs.jsonl'\nauthority_path='authority.json'\ncheckpoints_dir='checkpoints'\n",
    )
    .unwrap();
    let ledger = root.path().join("commands.idempotency.json");
    let bytes = b"{\"schemaVersion\":99,\"entries\":[]}";
    fs::write(&ledger, bytes).unwrap();
    let owner = fs::File::create(root.path().join("commands.idempotency.lock")).unwrap();
    owner.lock().unwrap();
    let output = root.path().join("incident.json");
    let result = Command::new(env!("CARGO_BIN_EXE_bobby"))
        .args(["doctor", "--config"])
        .arg(&config)
        .arg("--incident-bundle")
        .arg(&output)
        .output()
        .unwrap();
    assert!(
        result.status.success(),
        "{}",
        String::from_utf8_lossy(&result.stderr)
    );
    let bundle: serde_json::Value = serde_json::from_slice(&fs::read(&output).unwrap()).unwrap();
    let store = bundle["stores"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "idempotency-commands")
        .unwrap();
    assert_eq!(store["status"], "busy");
    assert!(store["sha256"].is_null());
    assert_eq!(fs::read(&ledger).unwrap(), bytes);
    drop(owner);
    let offline = root.path().join("offline.json");
    let result = Command::new(env!("CARGO_BIN_EXE_bobby"))
        .args(["doctor", "--config"])
        .arg(&config)
        .arg("--incident-bundle")
        .arg(&offline)
        .output()
        .unwrap();
    assert!(result.status.success());
    let bundle: serde_json::Value = serde_json::from_slice(&fs::read(offline).unwrap()).unwrap();
    let store = bundle["stores"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "idempotency-commands")
        .unwrap();
    assert_eq!(store["status"], "needsRepair");
    assert_eq!(store["health"]["integrityIssue"], "unreadableLedger");
    assert_eq!(fs::read(ledger).unwrap(), bytes);
}

#[test]
fn output_cannot_create_storage_locks_authority_or_checkpoint_records() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    fs::write(&config, "[storage]\njournal_path='commands.jsonl'\nscheduler_journal_path='jobs.jsonl'\nauthority_path='authority.json'\ncheckpoints_dir='checkpoints'\n").unwrap();
    fs::create_dir(root.path().join("checkpoints")).unwrap();
    for relative in [
        "commands.lock",
        "commands.idempotency.lock",
        "authority.json",
        "checkpoints/incident.json",
    ] {
        let output = root.path().join(relative);
        let result = Command::new(env!("CARGO_BIN_EXE_bobby"))
            .args(["doctor", "--config"])
            .arg(&config)
            .arg("--incident-bundle")
            .arg(&output)
            .output()
            .unwrap();
        assert!(
            !result.status.success(),
            "accepted protected destination {relative}"
        );
        assert!(!output.exists(), "created protected destination {relative}");
    }
}

#[cfg(unix)]
#[test]
fn symlink_sources_are_not_followed_or_hashed() {
    let root = tempfile::tempdir().unwrap();
    let config = root.path().join("config.toml");
    fs::write(&config, "[storage]\njournal_path='commands.jsonl'\nscheduler_journal_path='jobs.jsonl'\nauthority_path='authority.json'\ncheckpoints_dir='checkpoints'\n").unwrap();
    let secret = root.path().join("private.txt");
    fs::write(&secret, b"PRIVATE_SYMLINK_TARGET").unwrap();
    std::os::unix::fs::symlink(&secret, root.path().join("commands.jsonl")).unwrap();
    let output = root.path().join("incident.json");
    let result = Command::new(env!("CARGO_BIN_EXE_bobby"))
        .args(["doctor", "--config"])
        .arg(&config)
        .arg("--incident-bundle")
        .arg(&output)
        .output()
        .unwrap();
    assert!(result.status.success());
    let bytes = fs::read(&output).unwrap();
    let bundle: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let store = bundle["stores"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["name"] == "command-journal")
        .unwrap();
    assert_eq!(store["status"], "unsupportedFileType");
    assert!(store["sha256"].is_null());
    assert!(!String::from_utf8(bytes)
        .unwrap()
        .contains("PRIVATE_SYMLINK_TARGET"));
    assert_eq!(fs::read(secret).unwrap(), b"PRIVATE_SYMLINK_TARGET");
    use std::os::unix::fs::PermissionsExt;
    assert_eq!(
        fs::metadata(output).unwrap().permissions().mode() & 0o777,
        0o600
    );
}
