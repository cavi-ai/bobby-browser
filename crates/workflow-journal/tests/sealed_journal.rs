use chrono::Utc;
use types::{
    AttemptId, CommandEnvelope, CommandId, CommandPhase, PrimitiveCommand, RuntimeCommand,
    SessionId, TypeTextCommand, WorkflowId,
};
use workflow_journal::{CommandJournal, JournalKey, JournalRecord, JsonlJournal, LineOpener};

fn typed_record(command_id: &CommandId, value: &str) -> JournalRecord {
    JournalRecord {
        sequence: 0,
        recorded_at: Utc::now(),
        command_id: command_id.clone(),
        phase: CommandPhase::Accepted,
        envelope: Some(CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: command_id.clone(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: SessionId::new(),
            page_id: None,
            deadline: Utc::now() + chrono::Duration::seconds(30),
            command: RuntimeCommand::Primitive(PrimitiveCommand::TypeText(TypeTextCommand {
                selector: "#field".into(),
                target: None,
                value: value.into(),
                clear_first: true,
                expected_url: None,
            })),
        }),
        outcome: None,
        prepared_result: None,
    }
}

fn typed_value(record: &JournalRecord) -> String {
    match &record.envelope.as_ref().unwrap().command {
        RuntimeCommand::Primitive(PrimitiveCommand::TypeText(command)) => command.value.clone(),
        other => panic!("unexpected command {other:?}"),
    }
}

fn key(byte: u8) -> JournalKey {
    JournalKey::from_bytes([byte; 32])
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

#[tokio::test]
async fn sealed_journal_hides_typed_values_and_reads_them_back() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let id = CommandId::new();
    let journal = JsonlJournal::open_sealed(&path, key(1)).await.unwrap();
    journal
        .append(typed_record(&id, "hunter2-distinct-value"))
        .await
        .unwrap();
    assert!(!contains(
        &std::fs::read(&path).unwrap(),
        "hunter2-distinct-value"
    ));
    drop(journal);

    let reopened = JsonlJournal::open_sealed(&path, key(1)).await.unwrap();
    let scan = reopened.history(id).await.unwrap();
    assert_eq!(scan.incompatible_records, 0);
    assert_eq!(scan.records.len(), 1);
    assert_eq!(typed_value(&scan.records[0]), "hunter2-distinct-value");
}

#[tokio::test]
async fn sealed_line_opens_to_the_bytes_a_keyless_journal_writes() {
    let dir = tempfile::tempdir().unwrap();
    let plain_path = dir.path().join("plain.jsonl");
    let sealed_path = dir.path().join("sealed.jsonl");
    let id = CommandId::new();
    let record = typed_record(&id, "same-bytes-value");
    let keyless = JsonlJournal::open(&plain_path).await.unwrap();
    keyless.append(record.clone()).await.unwrap();
    let sealed = JsonlJournal::open_sealed(&sealed_path, key(1))
        .await
        .unwrap();
    sealed.append(record).await.unwrap();

    let plain_bytes = std::fs::read(&plain_path).unwrap();
    let sealed_bytes = std::fs::read(&sealed_path).unwrap();
    let sealed_key = key(1);
    let opened = LineOpener::new(Some(&sealed_key))
        .open(&sealed_bytes)
        .unwrap();
    assert_eq!(opened.as_ref(), plain_bytes.as_slice());
}

#[tokio::test]
async fn sealed_journal_archives_a_line_with_altered_ciphertext() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let id = CommandId::new();
    let journal = JsonlJournal::open_sealed(&path, key(1)).await.unwrap();
    journal
        .append(typed_record(&id, "altered-value"))
        .await
        .unwrap();
    drop(journal);

    let mut bytes = std::fs::read(&path).unwrap();
    let marker = br#""ct":""#;
    let at = bytes
        .windows(marker.len())
        .position(|window| window == marker)
        .unwrap()
        + marker.len()
        + 4;
    bytes[at] = if bytes[at] == b'A' { b'B' } else { b'A' };
    std::fs::write(&path, &bytes).unwrap();

    let reopened = JsonlJournal::open_sealed(&path, key(1)).await.unwrap();
    assert_eq!(reopened.archives().await.len(), 1);
    let scan = reopened.history(id).await.unwrap();
    assert!(scan.incompatible_records > 0);
    assert!(scan.records.is_empty());
}

#[tokio::test]
async fn sealed_open_keeps_legacy_records_and_seals_new_ones() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let legacy = CommandId::new();
    let fresh = CommandId::new();
    let keyless = JsonlJournal::open(&path).await.unwrap();
    keyless
        .append(typed_record(&legacy, "legacy-plain-value"))
        .await
        .unwrap();
    drop(keyless);

    let sealed = JsonlJournal::open_sealed(&path, key(1)).await.unwrap();
    sealed
        .append(typed_record(&fresh, "fresh-sealed-value"))
        .await
        .unwrap();
    drop(sealed);
    let raw = std::fs::read(&path).unwrap();
    assert!(contains(&raw, "legacy-plain-value"));
    assert!(!contains(&raw, "fresh-sealed-value"));

    let reopened = JsonlJournal::open_sealed(&path, key(1)).await.unwrap();
    let legacy_scan = reopened.history(legacy).await.unwrap();
    let fresh_scan = reopened.history(fresh).await.unwrap();
    assert_eq!(legacy_scan.incompatible_records, 0);
    assert_eq!(typed_value(&legacy_scan.records[0]), "legacy-plain-value");
    assert_eq!(typed_value(&fresh_scan.records[0]), "fresh-sealed-value");
}

#[tokio::test]
async fn sealed_key_file_is_created_owner_only_and_reused_unchanged() {
    let dir = tempfile::tempdir().unwrap();
    let key_path = dir.path().join("nested/journal.key");
    let journal_path = dir.path().join("commands.jsonl");
    let id = CommandId::new();

    let created = JournalKey::load_or_create(&key_path).unwrap();
    let bytes = std::fs::read(&key_path).unwrap();
    assert_eq!(bytes.len(), 32);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&key_path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600);
    }
    let journal = JsonlJournal::open_sealed(&journal_path, created)
        .await
        .unwrap();
    journal
        .append(typed_record(&id, "kept-value"))
        .await
        .unwrap();
    drop(journal);

    let reloaded = JournalKey::load_or_create(&key_path).unwrap();
    assert_eq!(std::fs::read(&key_path).unwrap(), bytes);
    let reopened = JsonlJournal::open_sealed(&journal_path, reloaded)
        .await
        .unwrap();
    let scan = reopened.history(id).await.unwrap();
    assert_eq!(scan.incompatible_records, 0);
    assert_eq!(typed_value(&scan.records[0]), "kept-value");

    let bad = dir.path().join("short.key");
    std::fs::write(&bad, b"short").unwrap();
    assert_eq!(
        JournalKey::load_or_create(&bad).unwrap_err().kind(),
        std::io::ErrorKind::InvalidData
    );
}

#[tokio::test]
async fn sealed_journal_is_not_trusted_under_a_different_key() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let id = CommandId::new();
    let journal = JsonlJournal::open_sealed(&path, key(1)).await.unwrap();
    journal
        .append(typed_record(&id, "other-key-value"))
        .await
        .unwrap();
    drop(journal);

    let reopened = JsonlJournal::open_sealed(&path, key(2)).await.unwrap();
    let scan = reopened.history(id).await.unwrap();
    assert!(scan.incompatible_records > 0);
    assert!(scan.records.is_empty());
}

fn lines(bytes: &[u8]) -> Vec<Vec<u8>> {
    bytes
        .split_inclusive(|byte| *byte == b'\n')
        .map(<[u8]>::to_vec)
        .collect()
}

#[tokio::test]
async fn sealed_journal_archives_when_a_middle_line_is_removed() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let ids = [CommandId::new(), CommandId::new(), CommandId::new()];
    let journal = JsonlJournal::open_sealed(&path, key(1)).await.unwrap();
    for (index, id) in ids.iter().enumerate() {
        journal
            .append(typed_record(id, &format!("chained-value-{index}")))
            .await
            .unwrap();
    }
    drop(journal);

    let mut kept = lines(&std::fs::read(&path).unwrap());
    assert_eq!(kept.len(), 3);
    kept.remove(1);
    std::fs::write(&path, kept.concat()).unwrap();

    let reopened = JsonlJournal::open_sealed(&path, key(1)).await.unwrap();
    assert_eq!(reopened.archives().await.len(), 1);
    let scan = reopened.history(ids[2].clone()).await.unwrap();
    assert!(scan.records.is_empty());
    assert!(scan.incompatible_records > 0);
}

#[tokio::test]
async fn sealed_journal_archives_a_forged_plaintext_line() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("commands.jsonl");
    let genuine = CommandId::new();
    let forged = CommandId::new();
    let journal = JsonlJournal::open_sealed(&path, key(1)).await.unwrap();
    journal
        .append(typed_record(&genuine, "genuine-value"))
        .await
        .unwrap();
    drop(journal);

    let mut record = typed_record(&forged, "forged-value");
    record.sequence = 1;
    let mut line = serde_json::to_vec(&record).unwrap();
    line.push(b'\n');
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(&line);
    std::fs::write(&path, bytes).unwrap();

    let reopened = JsonlJournal::open_sealed(&path, key(1)).await.unwrap();
    assert_eq!(reopened.archives().await.len(), 1);
    let scan = reopened.history(forged).await.unwrap();
    assert!(scan.records.is_empty());
    assert!(scan.incompatible_records > 0);
}

#[tokio::test]
async fn sealed_journal_archives_a_line_copied_from_another_journal() {
    let dir = tempfile::tempdir().unwrap();
    let path_a = dir.path().join("a.jsonl");
    let path_b = dir.path().join("b.jsonl");
    let id_a = CommandId::new();
    let first_b = CommandId::new();
    let foreign = CommandId::new();
    let journal_a = JsonlJournal::open_sealed(&path_a, key(1)).await.unwrap();
    journal_a
        .append(typed_record(&id_a, "journal-a-value"))
        .await
        .unwrap();
    drop(journal_a);
    let journal_b = JsonlJournal::open_sealed(&path_b, key(1)).await.unwrap();
    journal_b
        .append(typed_record(&first_b, "journal-b-first"))
        .await
        .unwrap();
    journal_b
        .append(typed_record(&foreign, "journal-b-second"))
        .await
        .unwrap();
    drop(journal_b);

    let copied = lines(&std::fs::read(&path_b).unwrap()).remove(1);
    let mut bytes = std::fs::read(&path_a).unwrap();
    bytes.extend_from_slice(&copied);
    std::fs::write(&path_a, bytes).unwrap();

    let reopened = JsonlJournal::open_sealed(&path_a, key(1)).await.unwrap();
    assert_eq!(reopened.archives().await.len(), 1);
    let scan = reopened.history(foreign).await.unwrap();
    assert!(scan.records.is_empty());
    assert!(scan.incompatible_records > 0);
}
