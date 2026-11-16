//! Closed journal segments: rolling, index sidecars and open-time loading.
//!
//! A journal at `P` keeps its active records in `P`. A full active file is
//! renamed to `P.segment-<N>` and gets a `P.segment-<N>.index` sidecar that lets
//! the next open skip reading the segment body. The hash chain continues across
//! files, so one opener fed every file in order opens every line.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use types::CommandId;

use crate::seal::JournalKey;
use crate::{
    archive_entry, file_identity, open_active, open_regular_read, read_indexed_records, scan_file,
    sync_parent, IndexedRecord, JournalError, JournalRecord, ScanStart, WriterState,
};

const INDEX_VERSION: u32 = 1;
const MAX_SEGMENT_NUMBER: u32 = 99_999_999;
/// Larger sidecars are rebuilt from the segment body instead of read.
const INDEX_READ_LIMIT: u64 = 512 * 1024 * 1024;

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SegmentIndex {
    version: u32,
    bytes: u64,
    records: usize,
    first_sequence: Option<u64>,
    last_sequence: Option<u64>,
    #[serde(with = "hex32")]
    previous_tail: [u8; 32],
    #[serde(with = "hex32")]
    tail: [u8; 32],
    sealed_seen: bool,
    entries: Vec<IndexEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct IndexEntry {
    command_id: CommandId,
    offset: u64,
    len: u64,
    #[serde(with = "hex32")]
    digest: [u8; 32],
    #[serde(with = "hex32")]
    previous: [u8; 32],
}

mod hex32 {
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(bytes: &[u8; 32], serializer: S) -> Result<S::Ok, S::Error> {
        let mut text = String::with_capacity(64);
        for byte in bytes {
            text.push(char::from_digit(u32::from(byte >> 4), 16).unwrap_or('0'));
            text.push(char::from_digit(u32::from(byte & 15), 16).unwrap_or('0'));
        }
        serializer.serialize_str(&text)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<[u8; 32], D::Error> {
        let text = String::deserialize(deserializer)?;
        let digits: Vec<u32> = text.chars().filter_map(|c| c.to_digit(16)).collect();
        if text.len() != 64 || digits.len() != 64 {
            return Err(serde::de::Error::custom("expected a 64-digit hex digest"));
        }
        let mut bytes = [0u8; 32];
        for (byte, pair) in bytes.iter_mut().zip(digits.chunks(2)) {
            *byte = (pair[0] << 4 | pair[1]) as u8;
        }
        Ok(bytes)
    }
}

/// Trusted state after the closed segments, which the active file continues.
pub(crate) struct Closed {
    pub entries: HashMap<CommandId, Vec<IndexedRecord>>,
    pub tail: [u8; 32],
    pub sealed_seen: bool,
    pub next_sequence: u64,
    /// Highest trusted segment number.
    pub count: u32,
}

impl Closed {
    fn empty() -> Self {
        Self {
            entries: HashMap::new(),
            tail: crate::seal::GENESIS,
            sealed_seen: false,
            next_sequence: 0,
            count: 0,
        }
    }

    pub(crate) fn start(&self) -> ScanStart {
        ScanStart {
            previous: self.tail,
            sealed_seen: self.sealed_seen,
            next_sequence: self.next_sequence,
        }
    }
}

pub(crate) enum Damage {
    Active,
    Segment(u32),
}

pub(crate) enum Read {
    Records(Vec<JournalRecord>),
    Damaged(Damage),
}

pub(crate) struct ClosedHealth {
    pub segments: usize,
    pub bytes: u64,
    pub records: usize,
    pub incompatible_records: usize,
    pub start: ScanStart,
}

fn parent_dir(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."))
}

/// The segment number in `entry`, which must be exactly `<journal>.segment-<8 digits>`.
fn segment_number(journal_name: &str, entry: &str) -> Option<u32> {
    let digits = entry
        .strip_prefix(journal_name)?
        .strip_prefix(".segment-")?;
    if digits.len() != 8 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|number| *number >= 1)
}

fn segment_path(path: &Path, number: u32) -> PathBuf {
    let name = path.file_name().unwrap_or_default().to_string_lossy();
    path.with_file_name(format!("{name}.segment-{number:08}"))
}

fn suffixed(path: &Path, suffix: &str) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}

fn number_of(segment: &Path) -> u32 {
    segment
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .and_then(|name| name.rsplit_once(".segment-")?.1.parse().ok())
        .unwrap_or(1)
}

/// Closed segments in ascending order, then the active path.
pub fn journal_files(path: &Path) -> std::io::Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    if let Some(name) = path.file_name().map(|name| name.to_string_lossy()) {
        match std::fs::read_dir(parent_dir(path)) {
            Ok(entries) => {
                for entry in entries {
                    let entry = entry?;
                    if let Some(number) =
                        segment_number(&name, &entry.file_name().to_string_lossy())
                    {
                        found.push((number, entry.path()));
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    found.sort_by_key(|(number, _)| *number);
    let mut files: Vec<PathBuf> = found.into_iter().map(|(_, file)| file).collect();
    files.push(path.to_path_buf());
    Ok(files)
}

async fn list_segments(path: &Path) -> std::io::Result<Vec<(u32, PathBuf)>> {
    let Some(name) = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return Ok(Vec::new());
    };
    let mut entries = match tokio::fs::read_dir(parent_dir(path)).await {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut found = Vec::new();
    while let Some(entry) = entries.next_entry().await? {
        if let Some(number) = segment_number(&name, &entry.file_name().to_string_lossy()) {
            found.push((number, entry.path()));
        }
    }
    found.sort_by_key(|(number, _)| *number);
    Ok(found)
}

/// Opens a closed segment read-only, or `None` when it is not a plain regular file.
pub(crate) async fn open_closed_segment(path: &Path) -> std::io::Result<Option<File>> {
    let file = match open_regular_read(path).await {
        Ok(Some(file)) => file,
        Ok(None) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if durable_file::require_sole_hard_link(&file.metadata().await?, "journal segment").is_err() {
        return Ok(None);
    }
    Ok(Some(file))
}

/// The authenticated, parsed sidecar of `segment` (a segment or an archive),
/// or `None` when absent or unreadable.
async fn read_index(
    segment: &Path,
    key: Option<&JournalKey>,
) -> std::io::Result<Option<SegmentIndex>> {
    let file = match open_regular_read(&suffixed(segment, ".index")).await {
        Ok(Some(file)) => file,
        Ok(None) => return Ok(None),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let metadata = file.metadata().await?;
    if durable_file::require_sole_hard_link(&metadata, "journal index").is_err()
        || metadata.len() > INDEX_READ_LIMIT
    {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    file.take(INDEX_READ_LIMIT).read_to_end(&mut bytes).await?;
    let json = match key {
        Some(key) => match key.open_index(&bytes) {
            Some(json) => json,
            None => return Ok(None),
        },
        None => bytes,
    };
    Ok(serde_json::from_slice(&json).ok())
}

async fn write_index(
    segment: &Path,
    index: &SegmentIndex,
    key: Option<&JournalKey>,
) -> std::io::Result<()> {
    let json = serde_json::to_vec(index).map_err(std::io::Error::other)?;
    let bytes = match key {
        Some(key) => key.seal_index(&json)?,
        None => json,
    };
    let destination = suffixed(segment, ".index");
    let temporary = suffixed(&destination, ".tmp");
    // A temporary left by an interrupted attempt is never valid content.
    match tokio::fs::remove_file(&temporary).await {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    durable_file::replace_exclusive(parent_dir(segment), &temporary, &destination, &bytes).await
}

/// The sequence after this segment when the index describes `bytes` bytes that
/// continue the chain at `previous` and the sequence order at `next_sequence`.
fn accepted(
    index: &SegmentIndex,
    bytes: u64,
    previous: &[u8; 32],
    next_sequence: u64,
) -> Option<u64> {
    if index.version != INDEX_VERSION
        || index.bytes != bytes
        || &index.previous_tail != previous
        || index.records != index.entries.len()
    {
        return None;
    }
    let next = match (index.first_sequence, index.last_sequence) {
        (None, None) if index.records == 0 => next_sequence,
        (Some(first), Some(last))
            if index.records > 0 && first >= next_sequence && last >= first =>
        {
            last.checked_add(1)?
        }
        _ => return None,
    };
    let mut end = 0u64;
    for entry in &index.entries {
        let entry_end = entry.offset.checked_add(entry.len)?;
        if entry.len == 0 || entry.offset < end || entry_end > bytes {
            return None;
        }
        end = entry_end;
    }
    Some(next)
}

fn records_of(
    index: SegmentIndex,
    segment: &Arc<PathBuf>,
) -> impl Iterator<Item = (CommandId, IndexedRecord)> + '_ {
    let segment = Arc::clone(segment);
    index.entries.into_iter().map(move |entry| {
        (
            entry.command_id,
            IndexedRecord {
                offset: entry.offset,
                len: entry.len as usize,
                digest: entry.digest,
                previous: entry.previous,
                segment: Some(Arc::clone(&segment)),
            },
        )
    })
}

/// Loads the trusted closed segments in order. The first damaged or missing
/// segment, every later one and the active file are archived intact.
pub(crate) async fn load(
    path: &Path,
    max_record_bytes: usize,
    key: Option<&JournalKey>,
) -> Result<Closed, JournalError> {
    let listed = list_segments(path).await?;
    let mut closed = Closed::empty();
    let mut damaged = None;
    for (expected, (number, segment)) in (1..).zip(&listed) {
        if *number != expected {
            damaged = Some(expected);
            break;
        }
        if !load_segment(segment, max_record_bytes, key, &mut closed).await? {
            damaged = Some(*number);
            break;
        }
        closed.count = *number;
    }
    if let Some(first) = damaged {
        archive_from(path, first, key).await?;
    }
    Ok(closed)
}

/// Returns whether the segment is trusted and merged into `closed`.
async fn load_segment(
    segment: &Path,
    max_record_bytes: usize,
    key: Option<&JournalKey>,
    closed: &mut Closed,
) -> Result<bool, JournalError> {
    let Some(file) = open_closed_segment(segment).await? else {
        return Ok(false);
    };
    let bytes = file.metadata().await?.len();
    let arc = Arc::new(segment.to_path_buf());
    if let Some(index) = read_index(segment, key).await? {
        if let Some(next) = accepted(&index, bytes, &closed.tail, closed.next_sequence) {
            closed.tail = index.tail;
            closed.sealed_seen = index.sealed_seen;
            closed.next_sequence = next;
            for (id, record) in records_of(index, &arc) {
                closed.entries.entry(id).or_default().push(record);
            }
            return Ok(true);
        }
    }
    // No usable index: read the body once, then write the index it implies.
    let mut found = HashMap::<CommandId, Vec<IndexedRecord>>::new();
    let scan = scan_file(
        file.try_clone().await?,
        Some(&mut found),
        max_record_bytes,
        key,
        closed.start(),
    )
    .await?;
    if scan.scan.torn_tail
        || scan.scan.incompatible_records > 0
        || scan.max_sequence == Some(u64::MAX)
        || (scan.records == 0 && bytes > 0)
    {
        return Ok(false);
    }
    let mut flat: Vec<(CommandId, IndexedRecord)> = found
        .into_iter()
        .flat_map(|(id, records)| records.into_iter().map(move |record| (id.clone(), record)))
        .collect();
    flat.sort_by_key(|(_, record)| record.offset);
    let index = SegmentIndex {
        version: INDEX_VERSION,
        bytes,
        records: flat.len(),
        first_sequence: scan.first_sequence,
        last_sequence: scan.max_sequence,
        previous_tail: closed.tail,
        tail: scan.last_digest,
        sealed_seen: scan.sealed_seen,
        entries: flat
            .iter()
            .map(|(id, record)| IndexEntry {
                command_id: id.clone(),
                offset: record.offset,
                len: record.len as u64,
                digest: record.digest,
                previous: record.previous,
            })
            .collect(),
    };
    write_index(segment, &index, key).await?;
    closed.tail = scan.last_digest;
    closed.sealed_seen = scan.sealed_seen;
    closed.next_sequence = scan
        .max_sequence
        .map_or(closed.next_sequence, |max| max + 1);
    for (id, mut record) in flat {
        record.segment = Some(Arc::clone(&arc));
        closed.entries.entry(id).or_default().push(record);
    }
    Ok(true)
}

/// The digest the first line of the archived file `archive` chains from, when
/// its sidecar records one.
pub(crate) async fn archive_start(
    archive: &Path,
    key: Option<&JournalKey>,
) -> std::io::Result<Option<[u8; 32]>> {
    Ok(read_index(archive, key)
        .await?
        .map(|index| index.previous_tail))
}

/// A sidecar that only records where an archived file's chain starts.
async fn write_start(
    archive: &Path,
    start: [u8; 32],
    key: Option<&JournalKey>,
) -> std::io::Result<()> {
    // Discovery already starts a sidecar-less archive at the genesis digest.
    if start == crate::seal::GENESIS {
        return Ok(());
    }
    let index = SegmentIndex {
        version: INDEX_VERSION,
        bytes: 0,
        records: 0,
        first_sequence: None,
        last_sequence: None,
        previous_tail: start,
        tail: start,
        sealed_seen: false,
        entries: Vec::new(),
    };
    write_index(archive, &index, key).await
}

/// Archives the active file intact. A known chain start is recorded beside it
/// so its records stay readable, and its commands stay reserved.
pub(crate) async fn archive_active(
    path: &Path,
    start: Option<[u8; 32]>,
    key: Option<&JournalKey>,
) -> std::io::Result<PathBuf> {
    let archive = archive_entry(path, path).await?;
    if let Some(start) = start {
        write_start(&archive, start, key).await?;
    }
    Ok(archive)
}

/// Moves segment `first`, every later segment (with sidecars) and the active
/// file into the archive namespace, intact. Each archived file that continued
/// a chain keeps a sidecar naming the digest its first line chains from.
pub(crate) async fn archive_from(
    path: &Path,
    first: u32,
    key: Option<&JournalKey>,
) -> std::io::Result<()> {
    let listed = list_segments(path).await?;
    let mut start = if first == 1 {
        Some(crate::seal::GENESIS)
    } else {
        match listed.iter().find(|(number, _)| *number == first - 1) {
            Some((_, previous)) => read_index(previous, key).await?.map(|index| index.tail),
            None => None,
        }
    };
    let mut next_number = first;
    for (number, segment) in listed.iter().filter(|(number, _)| *number >= first) {
        // A missing number breaks the running start.
        if *number != next_number {
            start = None;
        }
        next_number = number.saturating_add(1);
        let own = read_index(segment, key).await?;
        let sidecar = suffixed(segment, ".index");
        let archive = archive_entry(path, segment).await?;
        let target = suffixed(&archive, ".index");
        let present = tokio::fs::symlink_metadata(&sidecar).await.is_ok();
        match &own {
            Some(_) => tokio::fs::rename(&sidecar, &target).await?,
            None => {
                if present {
                    tokio::fs::rename(&sidecar, suffixed(&archive, ".index.damaged")).await?;
                }
                if let Some(start) = start {
                    write_start(&archive, start, key).await?;
                }
            }
        }
        if present {
            sync_parent(path).await?;
        }
        start = own.map(|index| index.tail);
    }
    if tokio::fs::symlink_metadata(path).await.is_ok() {
        archive_active(path, start, key).await?;
    }
    Ok(())
}

/// Closes the active file into the next segment and opens a fresh active file.
/// The writer's sequence and chain continue unchanged.
pub(crate) async fn roll(
    path: &Path,
    writer: &mut WriterState,
    key: Option<&JournalKey>,
) -> Result<(), JournalError> {
    let number = writer
        .segment_count
        .checked_add(1)
        .filter(|number| *number <= MAX_SEGMENT_NUMBER)
        .ok_or_else(|| std::io::Error::other("journal segment numbers are exhausted"))?;
    let segment = segment_path(path, number);
    match tokio::fs::symlink_metadata(&segment).await {
        Ok(_) => {
            return Err(std::io::Error::new(
                std::io::ErrorKind::AlreadyExists,
                "journal segment already exists",
            )
            .into())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut active: Vec<(&CommandId, &IndexedRecord)> = writer
        .offsets
        .iter()
        .flat_map(|(id, records)| {
            records
                .iter()
                .filter(|record| record.segment.is_none())
                .map(move |record| (id, record))
        })
        .collect();
    active.sort_by_key(|(_, record)| record.offset);
    let index = SegmentIndex {
        version: INDEX_VERSION,
        bytes: writer.file_len,
        records: active.len(),
        first_sequence: writer.active_first_sequence,
        last_sequence: writer
            .active_first_sequence
            .and_then(|_| writer.next_sequence.checked_sub(1)),
        previous_tail: writer.active_previous,
        tail: writer.last_digest,
        sealed_seen: writer.sealed_seen,
        entries: active
            .iter()
            .map(|(id, record)| IndexEntry {
                command_id: (*id).clone(),
                offset: record.offset,
                len: record.len as u64,
                digest: record.digest,
                previous: record.previous,
            })
            .collect(),
    };
    drop(active);

    tokio::fs::rename(path, &segment).await?;
    write_index(&segment, &index, key).await?;
    sync_parent(path).await?;
    let file = open_active(path).await?;
    file.sync_all().await?;
    sync_parent(path).await?;
    let metadata = file.metadata().await?;
    let identity = file_identity(file.try_clone().await?).await?;

    let segment = Arc::new(segment);
    for records in writer.offsets.values_mut() {
        for record in records.iter_mut().filter(|record| record.segment.is_none()) {
            record.segment = Some(Arc::clone(&segment));
        }
    }
    writer.file = file;
    writer.identity = identity;
    writer.file_len = metadata.len();
    writer.modified = metadata.modified().ok();
    writer.active_previous = writer.last_digest;
    writer.active_first_sequence = None;
    writer.active_records = 0;
    writer.segment_count = number;
    Ok(())
}

fn same_file(a: &IndexedRecord, b: &IndexedRecord) -> bool {
    match (&a.segment, &b.segment) {
        (None, None) => true,
        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
        _ => false,
    }
}

/// Reads a command's records from the active file and its closed segments.
pub(crate) async fn read(
    active: &File,
    entries: &[IndexedRecord],
    id: &CommandId,
    key: Option<&JournalKey>,
) -> Result<Read, JournalError> {
    let mut records = Vec::with_capacity(entries.len());
    for chunk in entries.chunk_by(same_file) {
        let (read, damage) = match &chunk[0].segment {
            None => (
                read_indexed_records(active, chunk, id, key).await?,
                Damage::Active,
            ),
            Some(segment) => {
                let damage = Damage::Segment(number_of(segment));
                match open_closed_segment(segment).await? {
                    Some(file) => (read_indexed_records(&file, chunk, id, key).await?, damage),
                    None => (None, damage),
                }
            }
        };
        match read {
            Some(found) => records.extend(found),
            None => return Ok(Read::Damaged(damage)),
        }
    }
    Ok(Read::Records(records))
}

/// Read-only summary of the closed segments. A segment without a usable index
/// counts as one incompatible record and its body is not read.
pub(crate) async fn inspect(
    path: &Path,
    key: Option<&JournalKey>,
) -> std::io::Result<ClosedHealth> {
    let listed = list_segments(path).await?;
    let mut health = ClosedHealth {
        segments: listed.len(),
        bytes: 0,
        records: 0,
        incompatible_records: 0,
        start: ScanStart::genesis(),
    };
    let mut expected = 1;
    for (number, segment) in &listed {
        if *number != expected {
            health.incompatible_records += 1;
        }
        expected = number.saturating_add(1);
        let Some(file) = open_closed_segment(segment).await? else {
            health.incompatible_records += 1;
            continue;
        };
        let bytes = file.metadata().await?.len();
        health.bytes += bytes;
        let accepted = match read_index(segment, key).await? {
            Some(index) => accepted(
                &index,
                bytes,
                &health.start.previous,
                health.start.next_sequence,
            )
            .map(|next| (index, next)),
            None => None,
        };
        match accepted {
            Some((index, next)) => {
                health.records += index.records;
                health.start = ScanStart {
                    previous: index.tail,
                    sealed_seen: index.sealed_seen,
                    next_sequence: next,
                };
            }
            None => health.incompatible_records += 1,
        }
    }
    Ok(health)
}
