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
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    sync::Mutex,
};
use types::{IdempotencyKey, PrincipalId};

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

async fn line(reader: &mut BufReader<File>) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    reader
        .take(MAX_LINE_BYTES + 1)
        .read_until(b'\n', &mut bytes)
        .await?;
    if bytes.len() as u64 > MAX_LINE_BYTES {
        return Err(invalid());
    }
    Ok(bytes)
}

async fn load(path: &Path) -> io::Result<Option<Loaded>> {
    let file = match File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut reader = BufReader::new(file);
    let first = line(&mut reader).await?;
    let header = serde_json::from_slice::<Header>(&first);
    let Ok(header) = header else {
        let mut bytes = first;
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
            entries: snapshot.entries,
            format: 1,
            sequence: 0,
            sha256: [0; 32],
            appended_bytes: 0,
        }));
    };
    if header.schema_version != 2 || !first.ends_with(b"\n") {
        return Err(invalid());
    }
    let expected = checksum(&DurableSnapshot {
        schema_version: 2,
        entries: &header.entries,
    })?;
    if header.sha256 != expected {
        return Err(invalid());
    }
    validate(&header.entries).map_err(|_| invalid())?;
    let mut entries = HashMap::new();
    for entry in header.entries {
        if entries
            .insert((entry.principal_id.clone(), entry.key.clone()), entry)
            .is_some()
        {
            return Err(invalid());
        }
    }
    let mut sequence = 0;
    let mut sha256 = expected;
    let mut appended_bytes = 0;
    loop {
        let bytes = line(&mut reader).await?;
        if bytes.is_empty() {
            break;
        }
        if !bytes.ends_with(b"\n") {
            return Err(invalid());
        }
        let record: Record = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
        if record.schema_version != 2
            || record.change.sequence != sequence + 1
            || record.change.previous_sha256 != sha256
            || checksum(&record.change)? != record.sha256
            || sequence >= COMPACT_RECORDS
        {
            return Err(invalid());
        }
        let mut changed = HashSet::new();
        for key in record.change.removes {
            if !changed.insert(key.clone()) || entries.remove(&key).is_none() {
                return Err(invalid());
            }
        }
        for entry in record.change.updates {
            let key = (entry.principal_id.clone(), entry.key.clone());
            if !changed.insert(key.clone()) {
                return Err(invalid());
            }
            entries.insert(key, entry);
        }
        if entries.len() > 4096 {
            return Err(invalid());
        }
        sequence += 1;
        sha256 = record.sha256;
        appended_bytes += bytes.len() as u64;
        if appended_bytes > COMPACT_BYTES + MAX_LINE_BYTES {
            return Err(invalid());
        }
    }
    Ok(Some(Loaded {
        entries: entries.into_values().collect(),
        format: 2,
        sequence,
        sha256,
        appended_bytes,
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
            exists: true,
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
