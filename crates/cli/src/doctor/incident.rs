//! Read-only, payload-free storage observations. This is not a recovery backup.
use std::{
    fs::{self, Metadata, OpenOptions},
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::{block_on_inspect, idempotency_ledgers, AppConfig};

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct StoreObservation {
    name: &'static str,
    path: PathBuf,
    status: &'static str,
    health: Option<Value>,
    bytes: Option<u64>,
    sha256: Option<String>,
}

#[derive(Clone, Copy)]
enum Kind {
    Commands,
    Jobs,
    Idempotency,
}

fn locations(config: &AppConfig) -> Vec<(&'static str, PathBuf, Kind)> {
    let mut paths = vec![
        (
            "command-journal",
            config.storage.journal_path.clone(),
            Kind::Commands,
        ),
        (
            "scheduler-journal",
            config.storage.scheduler_journal_path.clone(),
            Kind::Jobs,
        ),
    ];
    paths.extend(
        idempotency_ledgers(config)
            .into_iter()
            .map(|(name, path)| (name, path, Kind::Idempotency)),
    );
    paths
}

fn normalized(path: &Path) -> Result<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()?.join(path)
    };
    let mut result = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::ParentDir => {
                result.pop();
            }
            Component::CurDir => {}
            other => result.push(other.as_os_str()),
        }
    }
    for ancestor in result.ancestors() {
        if let Ok(base) = ancestor.canonicalize() {
            return Ok(base.join(result.strip_prefix(ancestor)?));
        }
    }
    Ok(result)
}

fn same_file(before: &Metadata, after: &Metadata) -> bool {
    let matches = before.len() == after.len()
        && before.modified().ok().is_some()
        && before.modified().ok() == after.modified().ok()
        && after.is_file();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        matches && before.dev() == after.dev() && before.ino() == after.ino()
    }
    #[cfg(not(unix))]
    {
        matches
    }
}

fn source_hash(path: &Path, before: &Metadata) -> io::Result<Option<String>> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(0x0020_0000);
    }
    let file = options.open(path)?;
    if !same_file(before, &file.metadata()?) {
        return Ok(None);
    }
    // Bound the read to the observed size even if a live writer keeps appending.
    let mut reader = (&file).take(before.len().saturating_add(1));
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    let mut bytes = 0_u64;
    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
        bytes += read as u64;
    }
    let after = fs::symlink_metadata(path)?;
    if bytes != before.len()
        || !same_file(before, &file.metadata()?)
        || !same_file(before, &after)
        || after.file_type().is_symlink()
    {
        return Ok(None);
    }
    Ok(Some(hex::encode(digest.finalize())))
}

fn busy(error: &(dyn std::error::Error + 'static)) -> bool {
    let mut current = Some(error);
    while let Some(error) = current {
        if error
            .downcast_ref::<io::Error>()
            .is_some_and(|e| e.kind() == io::ErrorKind::WouldBlock)
        {
            return true;
        }
        current = error.source();
    }
    false
}

fn inspect(path: PathBuf, kind: Kind) -> Result<(bool, Value)> {
    block_on_inspect(async move {
        match kind {
            Kind::Idempotency => {
                let health = interface_core::inspect_idempotency_ledger(path).await?;
                Ok((
                    health.integrity_issue.is_none(),
                    json!({"exists": health.exists, "format": health.format, "entries": health.entries, "integrityIssue": health.integrity_issue}),
                ))
            }
            Kind::Commands => {
                let h = workflow_journal::JsonlJournal::inspect(path).await?;
                Ok((
                    !h.torn_tail && h.incompatible_records == 0,
                    json!({"exists":h.exists,"records":h.records,"bytes":h.bytes,"tornTail":h.torn_tail,"incompatibleRecords":h.incompatible_records}),
                ))
            }
            Kind::Jobs => {
                let h = task_scheduler::JournalJobStore::inspect(path).await?;
                Ok((
                    !h.torn_tail && h.incompatible_records == 0,
                    json!({"exists":h.exists,"records":h.records,"bytes":h.bytes,"tornTail":h.torn_tail,"incompatibleRecords":h.incompatible_records}),
                ))
            }
        }
    })
}

fn observe(name: &'static str, path: PathBuf, kind: Kind) -> StoreObservation {
    let mut observed = StoreObservation {
        name,
        path,
        status: "unavailable",
        health: None,
        bytes: None,
        sha256: None,
    };
    let metadata = match fs::symlink_metadata(&observed.path) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => metadata,
        Ok(_) => {
            observed.status = "unsupportedFileType";
            return observed;
        }
        Err(error) => {
            if error.kind() == io::ErrorKind::NotFound {
                observed.status = "missing";
            }
            return observed;
        }
    };
    observed.bytes = Some(metadata.len());
    match inspect(observed.path.clone(), kind) {
        Ok((healthy, health)) => {
            observed.status = if healthy { "healthy" } else { "needsRepair" };
            observed.health = Some(health);
        }
        Err(error) if busy(error.as_ref()) => {
            observed.status = "busy";
            return observed;
        }
        // Parser errors may contain record data. Export only a stable classification.
        Err(_) => {
            observed.status = "inspectionFailed";
        }
    }
    match source_hash(&observed.path, &metadata) {
        Ok(Some(hash)) => observed.sha256 = Some(hash),
        Ok(None) => observed.status = "changedDuringCollection",
        Err(_) => observed.status = "unavailable",
    }
    observed
}

pub(super) fn export(config: &AppConfig, output: &Path) -> Result<()> {
    let parent = output
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let parent = parent
        .canonicalize()
        .context("incident output parent must exist")?;
    let target = parent.join(
        output
            .file_name()
            .context("incident output must name a file")?,
    );
    let locations = locations(config);
    for (_, source, _) in &locations {
        let identity = source.canonicalize().unwrap_or(normalized(source)?);
        if identity == target || normalized(source)? == normalized(&target)? {
            bail!("incident output cannot be a storage source");
        }
    }
    let protected_files = locations
        .iter()
        .map(|(_, path, _)| path)
        .chain(std::iter::once(&config.storage.authority_path));
    for path in protected_files {
        if normalized(&path.with_extension("lock"))? == normalized(&target)?
            || normalized(path)? == normalized(&target)?
        {
            bail!("incident output cannot be a storage source or lock");
        }
    }
    for directory in [
        &config.storage.checkpoints_dir,
        &config.browser.artifacts_dir,
    ] {
        let directory = directory.canonicalize().unwrap_or(normalized(directory)?);
        if target.starts_with(directory) {
            bail!("incident output must be outside checkpoint and artifact directories");
        }
    }
    // Publishing must never replace any file, including an existing report or symlink.
    if fs::symlink_metadata(&target).is_ok() {
        bail!("incident output already exists");
    }
    let stores: Vec<_> = locations
        .into_iter()
        .map(|(name, path, kind)| observe(name, path, kind))
        .collect();
    let directories: Vec<_> = [
        ("checkpoints", &config.storage.checkpoints_dir),
        ("artifacts", &config.browser.artifacts_dir),
    ]
    .into_iter()
    .map(|(name, path)| json!({"name":name,"path":path,"exists":path.is_dir()}))
    .collect();
    let bundle = json!({
        "schemaVersion": 1,
        "runtimeVersion": env!("CARGO_PKG_VERSION"),
        "collectedAt": chrono::Utc::now(),
        "consistency": "nonAtomicObservation",
        "replayAuthority": false,
        "stores": stores,
        "directories": directories,
        "recoveryPrerequisites": [
            "Pause callers and stop Bobby before offline repair.",
            "Preserve original history and backups; this report contains no recovery data.",
            "Reconcile uncertain effects against authoritative external state before retrying.",
            "Validate a designated recovery candidate offline; a source hash is not proof that restoration is safe.",
            "Doctor --fix does not repair durable execution history."
        ]
    });
    let bytes = serde_json::to_vec_pretty(&bundle)?;
    let mut temporary = tempfile::NamedTempFile::new_in(&parent)?;
    temporary.write_all(&bytes)?;
    temporary.write_all(b"\n")?;
    temporary.as_file().sync_all()?;
    temporary.persist_noclobber(&target).map_err(|e| e.error)?;
    #[cfg(unix)]
    fs::File::open(parent)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn changed_source_never_receives_a_stale_hash() {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("commands.jsonl");
        fs::write(&source, "before").unwrap();
        let before = fs::metadata(&source).unwrap();
        fs::write(&source, "different observed length").unwrap();
        assert!(source_hash(&source, &before).unwrap().is_none());
    }
}
