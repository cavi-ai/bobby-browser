//! Versioned, checksummed idempotency log and explicit legacy conversion.

use super::{canonical_sha256, DurableEntry, DurableLock, DurableSnapshot, DurableState};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::{
    collections::{HashMap, HashSet},
    io,
    path::{Path, PathBuf},
    sync::atomic::AtomicBool,
};
use tokio::{
    fs::{File, OpenOptions},
    io::{AsyncReadExt, AsyncWriteExt, BufReader},
    sync::Mutex,
};
use types::{IdempotencyKey, PrincipalId};

use workflow_journal::{
    JsonlLine, JsonlRead, LedgerDecoder, LedgerHealth, LedgerIssueKind, PreparedRecord,
    RecordObservation, ScanControl, ScanOptions, SequenceRule, SequenceVerdict,
};

type Key = (PrincipalId, IdempotencyKey);
const MAX_LINE_BYTES: u64 = (64 * 1024 + 2048) * 4096;
const COMPACT_RECORDS: u64 = 4096;
const COMPACT_BYTES: u64 = 16 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Header {
    schema_version: u16,
    entries: Vec<DurableEntry>,
    sha256: [u8; 32],
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Change {
    sequence: u64,
    previous_sha256: [u8; 32],
    updates: Vec<DurableEntry>,
    removes: Vec<Key>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Record {
    schema_version: u16,
    change: Change,
    sha256: [u8; 32],
}

pub(super) struct Loaded {
    health: LedgerHealth,
    pub entries: Vec<DurableEntry>,
    pub format: u16,
    sequence: u64,
    sha256: [u8; 32],
    appended_bytes: u64,
}

pub(super) struct Persistence {
    pub path: PathBuf,
    pub writer: Mutex<Journal>,
    pub failed: AtomicBool,
    _lock: DurableLock,
}

pub(super) struct Journal {
    file: Option<File>,
    format: u16,
    sequence: u64,
    sha256: [u8; 32],
    appended_bytes: u64,
}

fn invalid() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "idempotency history requires repair",
    )
}

fn checksum(value: &impl Serialize) -> io::Result<[u8; 32]> {
    canonical_sha256(value).map_err(|_| invalid())
}

fn lock(path: &Path) -> io::Result<DurableLock> {
    let mut options = std::fs::OpenOptions::new();
    options.create(true).read(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path.with_extension("lock"))?;
    file.try_lock().map_err(|error| {
        let error: io::Error = error.into();
        if error.kind() == io::ErrorKind::WouldBlock {
            io::Error::new(
                error.kind(),
                "idempotency ledger is in use; stop Bobby before converting it",
            )
        } else {
            error
        }
    })?;
    Ok(DurableLock { file })
}

pub(super) fn validate(entries: &[DurableEntry]) -> Result<(), &'static str> {
    let mut keys = HashSet::new();
    let mut counts = HashMap::new();
    for entry in entries {
        match &entry.state {
            DurableState::Reserved | DurableState::Unresolved if entry.expires_at.is_some() => {
                return Err("invalidLedgerState")
            }
            DurableState::Retained(value)
                if serde_json::to_vec(value)
                    .map_err(|_| "unreadableLedger")?
                    .len()
                    > 64 * 1024 =>
            {
                return Err("ledgerCapacityExceeded")
            }
            _ => {}
        }
        // Expired successes cannot authorize replay, but unresolved keys never expire.
        if entry.expires_at.is_some_and(|at| at <= Utc::now()) {
            continue;
        }
        if !keys.insert((entry.principal_id.clone(), entry.key.clone())) {
            return Err("duplicateLedgerKey");
        }
        let count = counts.entry(entry.principal_id.clone()).or_insert(0);
        *count += 1;
        if *count > 256 || keys.len() > 4096 {
            return Err("ledgerCapacityExceeded");
        }
    }
    Ok(())
}

enum V2Prepared {
    Header(HashMap<Key, DurableEntry>, [u8; 32]),
    Change(Record),
}

struct V2Decoder {
    header: Option<Header>,
    entries: HashMap<Key, DurableEntry>,
    sequence: u64,
    sha256: [u8; 32],
    appended_bytes: u64,
}

impl V2Decoder {
    fn rejected(kind: LedgerIssueKind, sequence: Option<u64>) -> PreparedRecord<V2Prepared> {
        PreparedRecord {
            observation: RecordObservation::Rejected {
                sequence_hint: sequence,
                kind,
            },
            record: None,
        }
    }
}

impl LedgerDecoder for V2Decoder {
    type Record = V2Prepared;
    fn decode(&mut self, line: &JsonlLine<'_>) -> io::Result<PreparedRecord<V2Prepared>> {
        if let Some(header) = self.header.take() {
            if header.schema_version != 2 {
                return Ok(Self::rejected(LedgerIssueKind::Schema, None));
            }
            let expected = checksum(&DurableSnapshot {
                schema_version: 2,
                entries: &header.entries,
            })?;
            if header.sha256 != expected {
                return Ok(Self::rejected(LedgerIssueKind::Checksum, None));
            }
            if validate(&header.entries).is_err() {
                return Ok(Self::rejected(LedgerIssueKind::InvalidState, None));
            }
            let mut entries = HashMap::new();
            for entry in header.entries {
                if entries
                    .insert((entry.principal_id.clone(), entry.key.clone()), entry)
                    .is_some()
                {
                    return Ok(Self::rejected(LedgerIssueKind::InvalidState, None));
                }
            }
            return Ok(PreparedRecord {
                observation: RecordObservation::Accepted { sequence: None },
                record: Some(V2Prepared::Header(entries, header.sha256)),
            });
        }
        let record: Record = match serde_json::from_slice(line.bytes) {
            Ok(record) => record,
            Err(_) => return Ok(Self::rejected(LedgerIssueKind::Decode, None)),
        };
        let sequence = Some(record.change.sequence);
        if record.schema_version != 2 {
            return Ok(Self::rejected(LedgerIssueKind::Schema, sequence));
        }
        if record.change.previous_sha256 != self.sha256
            || checksum(&record.change)? != record.sha256
        {
            return Ok(Self::rejected(LedgerIssueKind::Checksum, sequence));
        }
        let mut changed = HashSet::new();
        for key in &record.change.removes {
            if !changed.insert(key.clone()) || !self.entries.contains_key(key) {
                return Ok(Self::rejected(LedgerIssueKind::InvalidState, sequence));
            }
        }
        let mut size = self.entries.len() - record.change.removes.len();
        for entry in &record.change.updates {
            let key = (entry.principal_id.clone(), entry.key.clone());
            if !changed.insert(key.clone()) {
                return Ok(Self::rejected(LedgerIssueKind::InvalidState, sequence));
            }
            if !self.entries.contains_key(&key) {
                size += 1;
            }
        }
        if size > 4096
            || self.sequence >= COMPACT_RECORDS
            || self.appended_bytes.saturating_add(line.bytes.len() as u64)
                > COMPACT_BYTES + MAX_LINE_BYTES
        {
            return Ok(Self::rejected(LedgerIssueKind::InvalidState, sequence));
        }
        Ok(PreparedRecord {
            observation: RecordObservation::Accepted { sequence },
            record: Some(V2Prepared::Change(record)),
        })
    }

    fn apply(
        &mut self,
        line: &JsonlLine<'_>,
        prepared: PreparedRecord<V2Prepared>,
        verdict: SequenceVerdict,
    ) -> io::Result<ScanControl> {
        if verdict == SequenceVerdict::Invalid {
            return Ok(ScanControl::Stop);
        }
        match prepared.record {
            Some(V2Prepared::Header(entries, sha256)) => {
                self.sha256 = sha256;
                self.entries = entries;
            }
            Some(V2Prepared::Change(record)) => {
                for key in record.change.removes {
                    self.entries.remove(&key);
                }
                for entry in record.change.updates {
                    self.entries
                        .insert((entry.principal_id.clone(), entry.key.clone()), entry);
                }
                self.sequence = record.change.sequence;
                self.sha256 = record.sha256;
                self.appended_bytes += line.bytes.len() as u64;
            }
            None => return Err(invalid()),
        }
        Ok(ScanControl::Continue)
    }
}

async fn load(path: &Path) -> io::Result<Option<Loaded>> {
    let file = match File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut reader = BufReader::new(file);
    let first = workflow_journal::read_jsonl_line(&mut reader, MAX_LINE_BYTES).await?;
    let first_bytes = match &first {
        JsonlRead::Complete(bytes) | JsonlRead::Torn(bytes) => bytes.as_slice(),
        JsonlRead::Eof => &[],
        JsonlRead::TooLong => return Err(invalid()),
    };
    let Ok(header) = serde_json::from_slice::<Header>(first_bytes) else {
        // V1 is a whole snapshot, including multiline/no-newline input.
        let mut bytes = match first {
            JsonlRead::Complete(bytes) | JsonlRead::Torn(bytes) => bytes,
            _ => Vec::new(),
        };
        let remaining = MAX_LINE_BYTES.saturating_sub(bytes.len() as u64);
        reader.take(remaining + 1).read_to_end(&mut bytes).await?;
        if bytes.len() as u64 > MAX_LINE_BYTES {
            return Err(invalid());
        }
        let snapshot: DurableSnapshot = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if snapshot.schema_version != 1 {
            return Err(invalid());
        }
        return Ok(Some(Loaded {
            health: LedgerHealth {
                exists: true,
                bytes_observed: bytes.len() as u64,
                decoded_records: 1,
                ..LedgerHealth::default()
            },
            entries: snapshot.entries,
            format: 1,
            sequence: 0,
            sha256: [0; 32],
            appended_bytes: 0,
        }));
    };
    let mut decoder = V2Decoder {
        header: Some(header),
        entries: HashMap::new(),
        sequence: 0,
        sha256: [0; 32],
        appended_bytes: 0,
    };
    let health = workflow_journal::scan_jsonl_prefetched(
        &mut reader,
        first,
        ScanOptions {
            max_line_bytes: MAX_LINE_BYTES,
            sequence_rule: SequenceRule::Consecutive { first: 1 },
        },
        &mut decoder,
    )
    .await?;
    if health.first_issue.is_some() {
        return Err(invalid());
    }
    Ok(Some(Loaded {
        health,
        entries: decoder.entries.into_values().collect(),
        format: 2,
        sequence: decoder.sequence,
        sha256: decoder.sha256,
        appended_bytes: decoder.appended_bytes,
    }))
}

impl Persistence {
    pub async fn open(
        path: PathBuf,
    ) -> io::Result<(Self, Result<Vec<DurableEntry>, &'static str>)> {
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(parent).await?;
        }
        let owner = lock(&path)?;
        let loaded = load(&path).await;
        let (journal, entries) = match loaded {
            Ok(loaded) => {
                let (format, sequence, sha256, appended_bytes, entries) = loaded
                    .map_or((0, 0, [0; 32], 0, Vec::new()), |s| {
                        (s.format, s.sequence, s.sha256, s.appended_bytes, s.entries)
                    });
                let validation = validate(&entries).map(|_| entries);
                let file = if format == 2 {
                    Some(OpenOptions::new().append(true).open(&path).await?)
                } else {
                    None
                };
                (
                    Journal {
                        file,
                        format,
                        sequence,
                        sha256,
                        appended_bytes,
                    },
                    validation,
                )
            }
            Err(error) if error.kind() == io::ErrorKind::InvalidData => (
                Journal {
                    file: None,
                    format: 0,
                    sequence: 0,
                    sha256: [0; 32],
                    appended_bytes: 0,
                },
                Err("unreadableLedger"),
            ),
            Err(error) => return Err(error),
        };
        Ok((
            Self {
                path,
                writer: Mutex::new(journal),
                failed: AtomicBool::new(false),
                _lock: owner,
            },
            entries,
        ))
    }
}

impl Journal {
    pub fn needs_checkpoint(&self) -> bool {
        self.format != 2 || self.sequence >= COMPACT_RECORDS || self.appended_bytes >= COMPACT_BYTES
    }

    pub async fn checkpoint(&mut self, path: &Path, entries: Vec<DurableEntry>) -> io::Result<()> {
        let sha256 = checksum(&DurableSnapshot {
            schema_version: 2,
            entries: &entries,
        })?;
        let mut bytes = serde_json::to_vec(&Header {
            schema_version: 2,
            entries,
            sha256,
        })
        .map_err(io::Error::other)?;
        bytes.push(b'\n');
        replace(path, &bytes).await?;
        self.file = Some(OpenOptions::new().append(true).open(path).await?);
        self.format = 2;
        self.sequence = 0;
        self.sha256 = sha256;
        self.appended_bytes = 0;
        Ok(())
    }

    pub async fn append(
        &mut self,
        updates: Vec<DurableEntry>,
        removes: Vec<Key>,
    ) -> io::Result<()> {
        let change = Change {
            sequence: self.sequence.checked_add(1).ok_or_else(invalid)?,
            previous_sha256: self.sha256,
            updates,
            removes,
        };
        let sha256 = checksum(&change)?;
        let mut bytes = serde_json::to_vec(&Record {
            schema_version: 2,
            change,
            sha256,
        })
        .map_err(io::Error::other)?;
        bytes.push(b'\n');
        if bytes.len() as u64 > MAX_LINE_BYTES {
            return Err(invalid());
        }
        let file = self.file.as_mut().ok_or_else(invalid)?;
        file.write_all(&bytes).await?;
        file.flush().await?;
        file.sync_data().await?;
        self.sequence += 1;
        self.sha256 = sha256;
        self.appended_bytes += bytes.len() as u64;
        Ok(())
    }
}

async fn replace(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let temporary = path.with_extension(format!("{}.tmp", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    let result = async {
        let mut file = options.open(&temporary).await?;
        file.write_all(bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(&temporary, path).await?;
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            File::open(parent).await?.sync_all().await?;
        }
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(&temporary).await;
    }
    result
}

/// Read-only ledger health. Never creates a ledger or lock file.
#[derive(Debug, Default)]
pub struct IdempotencyLedgerHealth {
    pub exists: bool,
    pub format: Option<u16>,
    pub entries: usize,
    pub integrity_issue: Option<&'static str>,
}

pub async fn inspect_idempotency_ledger(
    path: impl AsRef<Path>,
) -> io::Result<IdempotencyLedgerHealth> {
    let _owner = match std::fs::File::open(path.as_ref().with_extension("lock")) {
        Ok(file) => {
            file.try_lock_shared()?;
            Some(DurableLock { file })
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    match load(path.as_ref()).await {
        Ok(None) => Ok(IdempotencyLedgerHealth::default()),
        Ok(Some(loaded)) => Ok(IdempotencyLedgerHealth {
            exists: loaded.health.exists,
            format: Some(loaded.format),
            entries: loaded.entries.len(),
            integrity_issue: validate(&loaded.entries).err(),
        }),
        Err(error) if error.kind() == io::ErrorKind::InvalidData => Ok(IdempotencyLedgerHealth {
            exists: true,
            integrity_issue: Some("unreadableLedger"),
            ..Default::default()
        }),
        Err(error) => Err(error),
    }
}

/// Convert a healthy, offline ledger to v1 without resolving or dropping keys.
/// Returns the preserved source backup, or None when missing/already v1.
pub async fn downgrade_idempotency_ledger(path: impl AsRef<Path>) -> io::Result<Option<PathBuf>> {
    let path = path.as_ref().to_path_buf();
    tokio::spawn(async move {
        if !tokio::fs::try_exists(&path).await? {
            return Ok(None);
        }
        let _owner = lock(&path)?;
        let Some(mut loaded) = load(&path).await? else {
            return Ok(None);
        };
        validate(&loaded.entries).map_err(|_| invalid())?;
        if loaded.format == 1 {
            return Ok(None);
        }
        // Reserved execution may have taken effect; rollback never grants replay.
        for entry in &mut loaded.entries {
            if matches!(
                entry.state,
                DurableState::Reserved | DurableState::Unresolved
            ) {
                entry.state = DurableState::Unresolved;
                entry.expires_at = None;
            }
        }
        let backup = path.with_extension(format!("{}.v2.backup", uuid::Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.create_new(true).write(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut destination = options.open(&backup).await?;
        let mut source = File::open(&path).await?;
        tokio::io::copy(&mut source, &mut destination).await?;
        destination.sync_all().await?;
        drop(destination);
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            File::open(parent).await?.sync_all().await?;
        }
        let bytes = serde_json::to_vec(&DurableSnapshot {
            schema_version: 1,
            entries: loaded.entries,
        })
        .map_err(io::Error::other)?;
        replace(&path, &bytes).await?;
        Ok(Some(backup))
    })
    .await
    .map_err(io::Error::other)?
}
