use std::sync::{Arc, Mutex};

use artifact_store::{ArtifactRecord, ArtifactStore};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use types::{
    Capability, ErrorLayer, InterfaceError, InterfaceErrorCode, PageId, PrincipalId,
    RequestContext, SessionId,
};
use uuid::Uuid;

use crate::{CapabilityHandle, SessionOwnershipAuthority};

const OWNERSHIP_DIRECTORY: &str = ".interface-artifact-ownership";
const OWNERSHIP_LOCK_FILE: &str = ".quota.lock";
const MAX_OWNERSHIP_BYTES: u64 = 64 * 1024;
const MAX_MANIFEST_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArtifactReference {
    reference_id: Uuid,
    artifact_id: String,
    sha256: String,
    bytes: u64,
    media_type: String,
}

impl ArtifactReference {
    pub fn artifact_id(&self) -> &str {
        &self.artifact_id
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ArtifactContent {
    pub media_type: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactOwnershipLimits {
    pub max_records: usize,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ArtifactReaderInitError;

impl std::fmt::Display for ArtifactReaderInitError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("artifact ownership boundary initialization failed")
    }
}

impl std::error::Error for ArtifactReaderInitError {}

#[derive(Debug, Clone, Copy, Default)]
struct OwnershipUsage {
    records: usize,
    bytes: u64,
}

#[derive(Clone)]
pub struct ArtifactReader {
    store: ArtifactStore,
    session_ownership: Arc<dyn SessionOwnershipAuthority>,
    max_read_bytes: u64,
    ownership_limits: ArtifactOwnershipLimits,
    ownership_usage: Arc<Mutex<OwnershipUsage>>,
}

impl ArtifactReader {
    pub fn new(
        store: ArtifactStore,
        session_ownership: Arc<dyn SessionOwnershipAuthority>,
        max_read_bytes: usize,
        ownership_limits: ArtifactOwnershipLimits,
    ) -> Result<Self, ArtifactReaderInitError> {
        assert!(max_read_bytes > 0, "artifact read bound must be positive");
        assert!(
            ownership_limits.max_records > 0 && ownership_limits.max_bytes > 0,
            "artifact ownership limits must be positive"
        );
        let usage =
            scan_ownership_usage(store.configured_root()).map_err(|_| ArtifactReaderInitError)?;
        if usage.records > ownership_limits.max_records || usage.bytes > ownership_limits.max_bytes
        {
            return Err(ArtifactReaderInitError);
        }
        Ok(Self {
            store,
            session_ownership,
            max_read_bytes: max_read_bytes as u64,
            ownership_limits,
            ownership_usage: Arc::new(Mutex::new(usage)),
        })
    }

    pub async fn register(
        &self,
        handle: &CapabilityHandle,
        ctx: &RequestContext,
        session_id: &SessionId,
        record: &ArtifactRecord,
    ) -> Result<ArtifactReference, InterfaceError> {
        if !authenticated_for(handle, ctx, Capability::ArtifactCapture)
            || !self
                .session_ownership
                .owns_session(&ctx.principal_id, session_id)
        {
            return Err(artifact_denied(ctx));
        }

        let root = self.store.configured_root().to_path_buf();
        let session_id = session_id.clone();
        let record = record.clone();
        let principal_id = ctx.principal_id.clone();
        let max_read_bytes = self.max_read_bytes;
        let ownership_limits = self.ownership_limits;
        let ownership_usage = self.ownership_usage.clone();
        let result = tokio::task::spawn_blocking(move || {
            let verified = read_committed_artifact(
                &root,
                &session_id,
                &record.artifact_id,
                &record.sha256,
                record.bytes,
                None,
                max_read_bytes,
            )?;
            let reference = ArtifactReference {
                reference_id: deterministic_reference_id(
                    &principal_id,
                    &session_id,
                    &record.artifact_id,
                )?,
                artifact_id: record.artifact_id,
                sha256: record.sha256,
                bytes: record.bytes,
                media_type: verified.media_type.clone(),
            };
            let ownership = OwnershipMetadata {
                principal_id,
                session_id,
                reference: reference.clone(),
                committed_path: verified.committed_path,
            };
            let usage = persist_ownership(&root, &ownership, ownership_limits)?;
            *ownership_usage.lock().map_err(|_| BoundaryError::Denied)? = usage;
            Ok::<_, BoundaryError>(reference)
        })
        .await;

        match result {
            Ok(Ok(reference)) => Ok(reference),
            Ok(Err(_)) | Err(_) => Err(artifact_denied(ctx)),
        }
    }

    pub async fn read(
        &self,
        handle: &CapabilityHandle,
        ctx: &RequestContext,
        session_id: &SessionId,
        reference: &ArtifactReference,
    ) -> Result<ArtifactContent, InterfaceError> {
        if !authenticated_for(handle, ctx, Capability::ArtifactRead) {
            return Err(artifact_denied(ctx));
        }

        let root = self.store.configured_root().to_path_buf();
        let principal_id = ctx.principal_id.clone();
        let session_id = session_id.clone();
        let reference = reference.clone();
        let max_read_bytes = self.max_read_bytes;
        let result = tokio::task::spawn_blocking(move || {
            let ownership = read_ownership(&root, reference.reference_id)?;
            if ownership.principal_id != principal_id
                || ownership.session_id != session_id
                || ownership.reference != reference
            {
                return Err(BoundaryError::Denied);
            }
            let verified = read_committed_artifact(
                &root,
                &session_id,
                &reference.artifact_id,
                &reference.sha256,
                reference.bytes,
                Some(&reference.media_type),
                max_read_bytes,
            )?;
            if ownership.committed_path != verified.committed_path {
                return Err(BoundaryError::Denied);
            }
            Ok::<_, BoundaryError>(ArtifactContent {
                media_type: verified.media_type,
                bytes: verified.bytes,
            })
        })
        .await;

        match result {
            Ok(Ok(content)) => Ok(content),
            Ok(Err(_)) | Err(_) => Err(artifact_denied(ctx)),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OwnershipMetadata {
    principal_id: PrincipalId,
    session_id: SessionId,
    reference: ArtifactReference,
    committed_path: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommittedManifest {
    filename: String,
    media_type: String,
    page_id: PageId,
    bytes: u64,
    sha256: String,
}

struct VerifiedArtifact {
    committed_path: String,
    media_type: String,
    bytes: Vec<u8>,
}

#[derive(Debug)]
enum BoundaryError {
    Denied,
    #[cfg(not(unix))]
    Unsupported,
}

fn authenticated_for(
    handle: &CapabilityHandle,
    ctx: &RequestContext,
    capability: Capability,
) -> bool {
    let now = Utc::now();
    ctx.validate_at(now).is_ok()
        && !handle.is_invalid_at(now)
        && ctx.principal_id == *handle.principal_id()
        && handle.allows(capability)
        && ctx.capabilities.contains(capability)
}

fn artifact_denied(ctx: &RequestContext) -> InterfaceError {
    InterfaceError {
        code: InterfaceErrorCode::ArtifactDenied,
        layer: ErrorLayer::Interface,
        message: "artifact access denied".to_owned(),
        correlation_id: ctx.correlation_id.clone(),
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: false,
        required_capability: None,
    }
}

fn valid_artifact_id(value: &str) -> bool {
    Uuid::parse_str(value).is_ok()
        || (value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit()))
}

fn valid_sha256(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn valid_extension(value: &str) -> bool {
    !value.is_empty() && value.len() <= 10 && value.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

fn deterministic_reference_id(
    principal_id: &PrincipalId,
    session_id: &SessionId,
    artifact_id: &str,
) -> Result<Uuid, BoundaryError> {
    use sha2::{Digest, Sha256};

    let key = serde_json::to_vec(&(principal_id, session_id, artifact_id))
        .map_err(|_| BoundaryError::Denied)?;
    let digest = Sha256::digest(key);
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    Ok(Uuid::from_bytes(bytes))
}

#[cfg(unix)]
fn read_committed_artifact(
    root: &std::path::Path,
    session_id: &SessionId,
    artifact_id: &str,
    expected_sha256: &str,
    expected_bytes: u64,
    expected_media_type: Option<&str>,
    max_read_bytes: u64,
) -> Result<VerifiedArtifact, BoundaryError> {
    use rustix::fs::{open, openat, Mode, OFlags};
    use sha2::{Digest, Sha256};

    if !valid_artifact_id(artifact_id)
        || !valid_sha256(expected_sha256)
        || expected_bytes > max_read_bytes
        || expected_media_type
            .is_some_and(|media_type| media_type.is_empty() || media_type.len() > 255)
    {
        return Err(BoundaryError::Denied);
    }

    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let file_flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK;
    let root_fd = open(root, directory_flags, Mode::empty()).map_err(|_| BoundaryError::Denied)?;
    let session_component = session_id.0.to_string();
    let session_fd = openat(&root_fd, &session_component, directory_flags, Mode::empty())
        .map_err(|_| BoundaryError::Denied)?;
    let artifact_fd = openat(&session_fd, artifact_id, directory_flags, Mode::empty())
        .map_err(|_| BoundaryError::Denied)?;

    let manifest_name = format!("{artifact_id}.json");
    let manifest_fd = openat(&artifact_fd, &manifest_name, file_flags, Mode::empty())
        .map_err(|_| BoundaryError::Denied)?;
    let manifest_bytes = read_bounded_file(manifest_fd.into(), MAX_MANIFEST_BYTES)?;
    let manifest: CommittedManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|_| BoundaryError::Denied)?;
    let prefix = format!("{artifact_id}.");
    let extension = manifest
        .filename
        .strip_prefix(&prefix)
        .filter(|extension| valid_extension(extension))
        .ok_or(BoundaryError::Denied)?;
    if manifest.media_type.is_empty()
        || manifest.media_type.len() > 255
        || manifest.sha256 != expected_sha256
        || manifest.bytes != expected_bytes
        || expected_media_type.is_some_and(|media_type| manifest.media_type != media_type)
    {
        return Err(BoundaryError::Denied);
    }
    let _ = manifest.page_id;

    let payload_name = format!("{artifact_id}.{extension}");
    let payload_fd = openat(&artifact_fd, &payload_name, file_flags, Mode::empty())
        .map_err(|_| BoundaryError::Denied)?;
    let bytes = read_bounded_file(payload_fd.into(), expected_bytes)?;
    if bytes.len() as u64 != expected_bytes
        || format!("{:x}", Sha256::digest(&bytes)) != expected_sha256
    {
        return Err(BoundaryError::Denied);
    }

    Ok(VerifiedArtifact {
        committed_path: format!("{session_component}/{artifact_id}/{payload_name}"),
        media_type: manifest.media_type,
        bytes,
    })
}

#[cfg(unix)]
fn read_bounded_file(fd: std::fs::File, expected_max: u64) -> Result<Vec<u8>, BoundaryError> {
    use std::io::Read;

    let metadata = fd.metadata().map_err(|_| BoundaryError::Denied)?;
    if !metadata.file_type().is_file() || metadata.len() > expected_max {
        return Err(BoundaryError::Denied);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    fd.take(expected_max.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| BoundaryError::Denied)?;
    if bytes.len() as u64 > expected_max {
        return Err(BoundaryError::Denied);
    }
    Ok(bytes)
}

#[cfg(unix)]
fn open_ownership_directory(
    root: &std::path::Path,
    create: bool,
) -> Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd), BoundaryError> {
    use rustix::fs::{mkdirat, open, openat, Mode, OFlags};

    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let root_fd = open(root, flags, Mode::empty()).map_err(|_| BoundaryError::Denied)?;
    if create {
        match mkdirat(
            &root_fd,
            OWNERSHIP_DIRECTORY,
            Mode::from_bits_truncate(0o700),
        ) {
            Ok(()) | Err(rustix::io::Errno::EXIST) => {}
            Err(_) => return Err(BoundaryError::Denied),
        }
    }
    let ownership_fd = openat(&root_fd, OWNERSHIP_DIRECTORY, flags, Mode::empty())
        .map_err(|_| BoundaryError::Denied)?;
    Ok((root_fd, ownership_fd))
}

#[cfg(unix)]
fn open_locked_ownership_directory(
    root: &std::path::Path,
) -> Result<(std::os::fd::OwnedFd, std::os::fd::OwnedFd, std::fs::File), BoundaryError> {
    use rustix::fs::{flock, openat, FlockOperation, Mode, OFlags};

    let (root_fd, ownership_fd) = open_ownership_directory(root, true)?;
    let lock_fd = openat(
        &ownership_fd,
        OWNERSHIP_LOCK_FILE,
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|_| BoundaryError::Denied)?;
    let lock_file = std::fs::File::from(lock_fd);
    if !lock_file
        .metadata()
        .map_err(|_| BoundaryError::Denied)?
        .file_type()
        .is_file()
    {
        return Err(BoundaryError::Denied);
    }
    flock(&lock_file, FlockOperation::LockExclusive).map_err(|_| BoundaryError::Denied)?;
    Ok((root_fd, ownership_fd, lock_file))
}

#[cfg(unix)]
fn read_ownership_from_directory(
    ownership_fd: &std::os::fd::OwnedFd,
    reference_id: Uuid,
) -> Result<Option<(OwnershipMetadata, u64)>, BoundaryError> {
    use rustix::fs::{openat, Mode, OFlags};

    let filename = format!("{reference_id}.json");
    let fd = match openat(
        ownership_fd,
        &filename,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK,
        Mode::empty(),
    ) {
        Ok(fd) => fd,
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(_) => return Err(BoundaryError::Denied),
    };
    let file = std::fs::File::from(fd);
    let length = file.metadata().map_err(|_| BoundaryError::Denied)?.len();
    let bytes = read_bounded_file(file, MAX_OWNERSHIP_BYTES)?;
    let ownership: OwnershipMetadata =
        serde_json::from_slice(&bytes).map_err(|_| BoundaryError::Denied)?;
    if ownership.reference.reference_id != reference_id
        || deterministic_reference_id(
            &ownership.principal_id,
            &ownership.session_id,
            &ownership.reference.artifact_id,
        )? != reference_id
        || !valid_artifact_id(&ownership.reference.artifact_id)
        || !valid_sha256(&ownership.reference.sha256)
    {
        return Err(BoundaryError::Denied);
    }
    Ok(Some((ownership, length)))
}

#[cfg(unix)]
fn scan_ownership_usage_locked(
    ownership_fd: &std::os::fd::OwnedFd,
) -> Result<OwnershipUsage, BoundaryError> {
    use rustix::fs::{unlinkat, AtFlags, Dir};

    let mut directory = Dir::read_from(ownership_fd).map_err(|_| BoundaryError::Denied)?;
    let mut usage = OwnershipUsage::default();
    while let Some(entry) = directory.read() {
        let entry = entry.map_err(|_| BoundaryError::Denied)?;
        let name = entry
            .file_name()
            .to_str()
            .map_err(|_| BoundaryError::Denied)?;
        if name == "." || name == ".." || name == OWNERSHIP_LOCK_FILE {
            continue;
        }
        if name.starts_with('.') && name.ends_with(".tmp") {
            unlinkat(ownership_fd, name, AtFlags::empty()).map_err(|_| BoundaryError::Denied)?;
            continue;
        }
        let Some(reference) = name.strip_suffix(".json") else {
            continue;
        };
        let reference_id = Uuid::parse_str(reference).map_err(|_| BoundaryError::Denied)?;
        let (_, bytes) = read_ownership_from_directory(ownership_fd, reference_id)?
            .ok_or(BoundaryError::Denied)?;
        usage.records = usage.records.checked_add(1).ok_or(BoundaryError::Denied)?;
        usage.bytes = usage
            .bytes
            .checked_add(bytes)
            .ok_or(BoundaryError::Denied)?;
    }
    Ok(usage)
}

#[cfg(unix)]
fn scan_ownership_usage(root: &std::path::Path) -> Result<OwnershipUsage, BoundaryError> {
    std::fs::create_dir_all(root).map_err(|_| BoundaryError::Denied)?;
    let (_root_fd, ownership_fd, _lock_file) = open_locked_ownership_directory(root)?;
    scan_ownership_usage_locked(&ownership_fd)
}

#[cfg(unix)]
fn persist_ownership(
    root: &std::path::Path,
    ownership: &OwnershipMetadata,
    limits: ArtifactOwnershipLimits,
) -> Result<OwnershipUsage, BoundaryError> {
    use std::io::Write;

    use rustix::fs::{fsync, openat, renameat, unlinkat, AtFlags, Mode, OFlags};

    let bytes = serde_json::to_vec(ownership).map_err(|_| BoundaryError::Denied)?;
    if bytes.len() as u64 > MAX_OWNERSHIP_BYTES {
        return Err(BoundaryError::Denied);
    }
    let (_root_fd, ownership_fd, _lock_file) = open_locked_ownership_directory(root)?;
    let usage = scan_ownership_usage_locked(&ownership_fd)?;
    let final_name = format!("{}.json", ownership.reference.reference_id);
    if let Some((existing, _)) =
        read_ownership_from_directory(&ownership_fd, ownership.reference.reference_id)?
    {
        return if existing == *ownership {
            Ok(usage)
        } else {
            Err(BoundaryError::Denied)
        };
    }
    let new_records = usage.records.checked_add(1).ok_or(BoundaryError::Denied)?;
    let new_bytes = usage
        .bytes
        .checked_add(bytes.len() as u64)
        .ok_or(BoundaryError::Denied)?;
    if new_records > limits.max_records || new_bytes > limits.max_bytes {
        return Err(BoundaryError::Denied);
    }

    let temporary_name = format!(".{}.tmp", ownership.reference.reference_id);
    match unlinkat(&ownership_fd, &temporary_name, AtFlags::empty()) {
        Ok(()) | Err(rustix::io::Errno::NOENT) => {}
        Err(_) => return Err(BoundaryError::Denied),
    }
    let temporary_fd = openat(
        &ownership_fd,
        &temporary_name,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_bits_truncate(0o600),
    )
    .map_err(|_| BoundaryError::Denied)?;
    let write_result = (|| {
        let mut file = std::fs::File::from(temporary_fd);
        file.write_all(&bytes).map_err(|_| BoundaryError::Denied)?;
        file.sync_all().map_err(|_| BoundaryError::Denied)?;
        renameat(&ownership_fd, &temporary_name, &ownership_fd, &final_name)
            .map_err(|_| BoundaryError::Denied)?;
        fsync(&ownership_fd).map_err(|_| BoundaryError::Denied)?;
        Ok(OwnershipUsage {
            records: new_records,
            bytes: new_bytes,
        })
    })();
    if write_result.is_err() {
        let _ = unlinkat(&ownership_fd, &temporary_name, AtFlags::empty());
    }
    write_result
}

#[cfg(unix)]
fn read_ownership(
    root: &std::path::Path,
    reference_id: Uuid,
) -> Result<OwnershipMetadata, BoundaryError> {
    let (_root_fd, ownership_fd) = open_ownership_directory(root, false)?;
    read_ownership_from_directory(&ownership_fd, reference_id)?
        .map(|(ownership, _)| ownership)
        .ok_or(BoundaryError::Denied)
}

// There is no canonicalize-then-open fallback. Non-Unix builds fail closed until
// an equivalent no-reparse, handle-relative implementation is provided.
#[cfg(not(unix))]
fn read_committed_artifact(
    _root: &std::path::Path,
    _session_id: &SessionId,
    _artifact_id: &str,
    _expected_sha256: &str,
    _expected_bytes: u64,
    _expected_media_type: Option<&str>,
    _max_read_bytes: u64,
) -> Result<VerifiedArtifact, BoundaryError> {
    Err(BoundaryError::Unsupported)
}

#[cfg(not(unix))]
fn persist_ownership(
    _root: &std::path::Path,
    _ownership: &OwnershipMetadata,
    _limits: ArtifactOwnershipLimits,
) -> Result<OwnershipUsage, BoundaryError> {
    Err(BoundaryError::Unsupported)
}

#[cfg(not(unix))]
fn scan_ownership_usage(_root: &std::path::Path) -> Result<OwnershipUsage, BoundaryError> {
    Ok(OwnershipUsage::default())
}

#[cfg(not(unix))]
fn read_ownership(
    _root: &std::path::Path,
    _reference_id: Uuid,
) -> Result<OwnershipMetadata, BoundaryError> {
    Err(BoundaryError::Unsupported)
}
