//! Durable shared context graph (Spec C): per-profile, per-site structural
//! memory of forms and controls, promoted from the session-hot
//! `page-runtime` context layer on verified success.
//!
//! What persists (schema v1), and only this: site key, page patterns, form
//! keys, control `{role, accessible_name, ordinal, form_membership}`, and
//! per-intent counters with a coarse day-precision `last_verified_day`.
//! Never: typed values, credentials, page text, screenshots, journal ids,
//! or exact timestamps.
//!
//! Storage is one JSON document per site under
//! `<root>/<profile-id>/<site-key>.json` with atomic temp-write-then-rename
//! and an in-memory index built at open — the checkpoint-store pattern, no
//! database. A lockfile enforces the single-writer rule: only the runtime
//! process opens the store, and a second opener is refused.

mod sitekey;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

pub use sitekey::site_key;

pub const SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Error)]
pub enum ContextStoreError {
    #[error("context store I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("context store serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
    #[error("context store is already open by another writer")]
    AlreadyLocked,
    /// The lockfile could not be claimed for a reason that is not another
    /// writer holding it: the path is not a regular file, or the path and the
    /// opened descriptor disagree about which inode they are.
    ///
    /// Kept distinct from [`Self::AlreadyLocked`] because the CLI tells the
    /// operator to stop a running bobby, which is wrong advice for every one
    /// of these.
    #[error("context store lockfile is unusable: {0}")]
    LockUnusable(&'static str),
    #[error("unsupported context schema {actual}; expected {expected}")]
    UnsupportedSchema { actual: u16, expected: u16 },
}

/// How a control record entered the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecordSource {
    /// Promoted from a verified runtime observation.
    Observed,
    /// Promoted from a verified vision proposal (Spec B prefill).
    VisionPromoted,
}

/// Per-intent-kind counters for one control. `last_verified_day` is days
/// since the Unix epoch — coarse day precision by construction, so no exact
/// timestamp can ever persist.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct IntentStats {
    pub success_count: u64,
    pub failure_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified_day: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<RecordSource>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ControlContext {
    pub role: String,
    pub accessible_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<u32>,
    /// Key of the enclosing form within the page, or a stable page-level
    /// marker for controls outside any form.
    pub form_membership: String,
    #[serde(default)]
    pub intents: BTreeMap<String, IntentStats>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormContext {
    #[serde(default)]
    pub controls: Vec<ControlContext>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PageContext {
    #[serde(default)]
    pub forms: BTreeMap<String, FormContext>,
}

/// Counters for one challenge kind (e.g. `recaptchaV2Checkbox`) seen on a
/// site. Day-precision stamp only, same privacy discipline as intent stats.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChallengeStats {
    pub success_count: u64,
    pub failure_count: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified_day: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiteContext {
    #[serde(default)]
    pub pages: BTreeMap<String, PageContext>,
    /// Per-site challenge outcomes keyed by challenge kind, promoted from
    /// `solveChallenge` intent outcomes. The prior a future detector reads.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub challenges: BTreeMap<String, ChallengeStats>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SiteEnvelope {
    schema: u16,
    /// The real site key; the filename is a sanitized encoding of it and
    /// does not round-trip.
    site_key: String,
    site: SiteContext,
}

/// A site file that failed to load. Corruption is reported and skipped —
/// never panics, never fails the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkippedSite {
    pub file: PathBuf,
    pub reason: String,
}

#[derive(Debug, Default)]
pub struct OpenReport {
    pub sites_loaded: usize,
    pub skipped: Vec<SkippedSite>,
}

/// Days since the Unix epoch for a wall-clock time — the only timestamp
/// precision this store can express.
pub fn day_since_epoch(time: chrono::DateTime<chrono::Utc>) -> u32 {
    (time.timestamp().max(0) / 86_400) as u32
}

pub struct ContextStore {
    root: Arc<PathBuf>,
    sites: Mutex<BTreeMap<String, SiteContext>>,
    dirty: Mutex<BTreeMap<String, bool>>,
    _lock: Lockfile,
}

impl ContextStore {
    /// Opens the store for one profile, creating directories, claiming the
    /// single-writer lockfile, and building the in-memory index. Corrupt or
    /// unsupported files are skipped and reported, never fatal.
    pub async fn open(
        root: impl AsRef<Path>,
        profile_id: &str,
    ) -> Result<(Self, OpenReport), ContextStoreError> {
        let root = root.as_ref().join(encode_component(profile_id));
        tokio::fs::create_dir_all(&root).await?;
        let lock = Lockfile::claim(&root).await?;
        let mut index = BTreeMap::new();
        let mut report = OpenReport::default();
        let mut entries = tokio::fs::read_dir(&root).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            match load_envelope(&path).await {
                Ok((key, site)) => {
                    index.insert(key, site);
                    report.sites_loaded += 1;
                }
                Err(reason) => {
                    tracing::warn!(file = %path.display(), %reason, "context.site_skipped");
                    report.skipped.push(SkippedSite { file: path, reason });
                }
            }
        }
        Ok((
            Self {
                root: Arc::new(root),
                sites: Mutex::new(index),
                dirty: Mutex::new(BTreeMap::new()),
                _lock: lock,
            },
            report,
        ))
    }

    /// Opens a store and applies its retention policy before returning it to
    /// the runtime. `today` is explicit so boundary behavior stays testable.
    pub async fn open_with_ttl(
        root: impl AsRef<Path>,
        profile_id: &str,
        ttl_days: u32,
        today: u32,
    ) -> Result<(Self, OpenReport), ContextStoreError> {
        let (store, report) = Self::open(root, profile_id).await?;
        store.sweep(ttl_days, today).await?;
        Ok((store, report))
    }

    /// In-memory view of a site, if present.
    pub async fn site(&self, site_key: &str) -> Option<SiteContext> {
        self.sites.lock().await.get(site_key).cloned()
    }

    pub async fn list_sites(&self) -> Vec<String> {
        self.sites.lock().await.keys().cloned().collect()
    }

    /// Replaces (or inserts) a site's context, buffering the write behind
    /// `flush`. Never fails the caller's workflow: persistence happens on
    /// flush, and flush errors degrade to session-only.
    pub async fn upsert_site(&self, site_key: &str, site: SiteContext) {
        self.sites.lock().await.insert(site_key.to_string(), site);
        self.dirty.lock().await.insert(site_key.to_string(), true);
    }

    /// Records one challenge outcome against a site. Success stamps the
    /// coarse day; failure only increments. Buffered behind `flush`.
    pub async fn record_challenge(
        &self,
        site_key: &str,
        challenge_kind: &str,
        success: bool,
        today: u32,
    ) {
        let mut site = self.site(site_key).await.unwrap_or_default();
        let stats = site
            .challenges
            .entry(challenge_kind.to_string())
            .or_default();
        if success {
            stats.success_count += 1;
            stats.last_verified_day = Some(today);
        } else {
            stats.failure_count += 1;
        }
        self.upsert_site(site_key, site).await;
    }

    /// The most-attempted challenge kind for a site and its stats — the
    /// probabilistic prior a challenge detector boosts from. `None` when the
    /// site has no recorded challenge history.
    pub async fn challenge_prior(&self, site_key: &str) -> Option<(String, ChallengeStats)> {
        self.site(site_key)
            .await?
            .challenges
            .into_iter()
            .max_by_key(|(_, stats)| stats.success_count + stats.failure_count)
    }

    /// Persists every dirty site. Returns the keys that failed to write;
    /// they stay dirty and remain available in memory for this session.
    pub async fn flush(&self) -> Vec<String> {
        let dirty_keys: Vec<String> = {
            let mut dirty = self.dirty.lock().await;
            let keys = dirty.keys().cloned().collect();
            dirty.clear();
            keys
        };
        let mut failed = Vec::new();
        for key in dirty_keys {
            let site = self.sites.lock().await.get(&key).cloned();
            let Some(site) = site else { continue };
            if let Err(error) = self.write_site(&key, &site).await {
                tracing::warn!(site = %key, %error, "context.flush_failed");
                self.dirty.lock().await.insert(key.clone(), true);
                failed.push(key);
            }
        }
        failed
    }

    /// Removes a site's context entirely — memory and file. Total and
    /// immediate.
    pub async fn forget(&self, site_key: &str) -> Result<(), ContextStoreError> {
        self.sites.lock().await.remove(site_key);
        self.dirty.lock().await.remove(site_key);
        match tokio::fs::remove_file(self.path(site_key)).await {
            Ok(()) => {
                File::open(self.root.as_ref()).await?.sync_all().await?;
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error.into()),
        }
    }

    /// Drops intent stats not verified within `ttl_days` of `today` (a
    /// day-since-epoch value). Empty forms, pages, and sites are pruned and
    /// their files removed. Returns the number of stats dropped.
    pub async fn sweep(&self, ttl_days: u32, today: u32) -> Result<u64, ContextStoreError> {
        let cutoff = today.saturating_sub(ttl_days);
        let mut dropped = 0_u64;
        let mut emptied = Vec::new();
        let mut changed = Vec::new();
        {
            let mut sites = self.sites.lock().await;
            for (key, site) in sites.iter_mut() {
                let before_pages = site.pages.len();
                let before = dropped;
                for page in site.pages.values_mut() {
                    for form in page.forms.values_mut() {
                        for control in &mut form.controls {
                            control.intents.retain(|_, stats| {
                                let keep = stats.last_verified_day.is_some_and(|day| day >= cutoff);
                                if !keep {
                                    dropped += 1;
                                }
                                keep
                            });
                        }
                        form.controls.retain(|control| !control.intents.is_empty());
                    }
                    page.forms.retain(|_, form| !form.controls.is_empty());
                }
                site.pages.retain(|_, page| !page.forms.is_empty());
                if site.pages.is_empty() {
                    emptied.push(key.clone());
                } else if dropped != before || site.pages.len() != before_pages {
                    changed.push(key.clone());
                }
            }
            for key in &emptied {
                sites.remove(key);
            }
        }
        {
            let mut dirty = self.dirty.lock().await;
            for key in changed {
                dirty.insert(key, true);
            }
        }
        for key in emptied {
            tokio::fs::remove_file(self.path(&key)).await?;
        }
        let failed = self.flush().await;
        if !failed.is_empty() {
            return Err(std::io::Error::other(format!(
                "retention sweep failed to persist {} site(s): {}",
                failed.len(),
                failed.join(", ")
            ))
            .into());
        }
        Ok(dropped)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, site_key: &str) -> PathBuf {
        self.root
            .join(format!("{}.json", encode_component(site_key)))
    }

    async fn write_site(&self, key: &str, site: &SiteContext) -> Result<(), ContextStoreError> {
        let envelope = SiteEnvelope {
            schema: SCHEMA_VERSION,
            site_key: key.to_string(),
            site: site.clone(),
        };
        let destination = self.path(key);
        let temporary = self.root.join(format!(
            ".{}.{}.tmp",
            encode_component(key),
            uuid::Uuid::new_v4()
        ));
        let result = async {
            let mut options = OpenOptions::new();
            options.create_new(true).write(true);
            // Context is remembered form structure; owner-only, matching the
            // checkpoint and authority stores.
            #[cfg(unix)]
            options.mode(0o600);
            let mut file = options.open(&temporary).await?;
            file.write_all(&serde_json::to_vec(&envelope)?).await?;
            file.sync_all().await?;
            drop(file);
            tokio::fs::rename(&temporary, destination).await?;
            File::open(self.root.as_ref()).await?.sync_all().await?;
            Ok::<_, ContextStoreError>(())
        }
        .await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&temporary).await;
        }
        result
    }
}

async fn load_envelope(path: &Path) -> Result<(String, SiteContext), String> {
    let bytes = tokio::fs::read(path)
        .await
        .map_err(|error| error.to_string())?;
    let envelope: SiteEnvelope =
        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
    if envelope.schema != SCHEMA_VERSION {
        return Err(format!(
            "unsupported schema {}; expected {SCHEMA_VERSION}",
            envelope.schema
        ));
    }
    Ok((envelope.site_key, envelope.site))
}

/// Injective filesystem encoding for arbitrary UTF-8 identity strings.
fn encode_component(value: &str) -> String {
    hex::encode(value.as_bytes())
}

/// Attempts to claim the lockfile before reporting contention.
const LOCK_CLAIM_ATTEMPTS: u32 = 5;
/// Pause between claim attempts.
const LOCK_CLAIM_BACKOFF: std::time::Duration = std::time::Duration::from_millis(20);

struct Lockfile {
    _file: std::fs::File,
}

impl Lockfile {
    async fn claim(root: &Path) -> Result<Self, ContextStoreError> {
        let path = root.join(".context-store.lock");
        if let Ok(metadata) = std::fs::symlink_metadata(&path) {
            if !metadata.file_type().is_file() {
                return Err(ContextStoreError::LockUnusable(
                    "existing lock path is not a regular file",
                ));
            }
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(&path)?;
        let path_metadata = std::fs::symlink_metadata(&path)?;
        let file_metadata = file.metadata()?;
        if !path_metadata.file_type().is_file() || !file_metadata.file_type().is_file() {
            return Err(ContextStoreError::LockUnusable(
                "lock path or descriptor is not a regular file",
            ));
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::{MetadataExt, PermissionsExt};
            if path_metadata.dev() != file_metadata.dev()
                || path_metadata.ino() != file_metadata.ino()
            {
                return Err(ContextStoreError::LockUnusable(
                    "lock path and descriptor disagree about the inode",
                ));
            }
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        }
        // A lock released moments ago is not always claimable on the very next
        // attempt: `bobby context forget` opens, drops, and reopens the store
        // in one process, and on Linux that hand-off loses the race often
        // enough to fail a test run every time. Retry briefly.
        //
        // This does not soften real contention. A running bobby holds the lock
        // for its whole lifetime, so it still fails after the last attempt --
        // the window only covers a close that has just happened.
        let mut attempt = 0;
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Self { _file: file }),
                Err(std::fs::TryLockError::WouldBlock) if attempt < LOCK_CLAIM_ATTEMPTS - 1 => {
                    attempt += 1;
                    tokio::time::sleep(LOCK_CLAIM_BACKOFF).await;
                }
                Err(std::fs::TryLockError::WouldBlock) => {
                    return Err(ContextStoreError::AlreadyLocked)
                }
                Err(std::fs::TryLockError::Error(error)) => return Err(error.into()),
            }
        }
    }
}
