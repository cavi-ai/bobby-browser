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

mod limits;
mod sitekey;
pub use limits::ContextLimits;

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::fs::{File, OpenOptions};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Mutex;

pub use sitekey::{page_pattern, site_key};

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
    #[error("context resource limit: {0}")]
    ResourceLimit(&'static str),
    #[error("unsupported context schema {actual}; expected {expected}")]
    UnsupportedSchema { actual: u16, expected: u16 },
}

/// How a control record entered the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecordSource {
    /// Promoted from a verified runtime observation.
    Observed,
    /// Promoted from a verified vision proposal.
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
struct SiteEnvelope<Site = SiteContext> {
    schema: u16,
    /// The real site key; the filename is its canonical UTF-8 hex encoding.
    site_key: String,
    site: Site,
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
    pub skipped_total: usize,
}

/// Days since the Unix epoch for a wall-clock time — the only timestamp
/// precision this store can express.
pub fn day_since_epoch(time: chrono::DateTime<chrono::Utc>) -> u32 {
    (time.timestamp().max(0) / 86_400) as u32
}

#[derive(Default)]
struct StoreState {
    sites: BTreeMap<String, CachedSite>,
    dirty: BTreeMap<String, u64>,
    revision: u64,
    evictions: u64,
    rejected_updates: u64,
    clock: u64,
    retention_cutoff: Option<u32>,
}

struct CachedSite {
    site: SiteContext,
    bytes: usize,
    last_used: u64,
}

impl CachedSite {
    fn new(key: &str, site: SiteContext) -> Self {
        Self {
            bytes: limits::site_bytes(&site)
                .saturating_add(key.len())
                .saturating_add(limits::node_bytes::<String, Self>()),
            site,
            last_used: 0,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ContextUsage {
    pub resident_sites: usize,
    pub resident_bytes: usize,
    pub pending_changes: usize,
    pub evictions: u64,
    pub rejected_updates: u64,
}

impl StoreState {
    fn bytes(&self) -> usize {
        self.sites
            .values()
            .fold(limits::map_bytes(&self.dirty), |bytes, entry| {
                bytes.saturating_add(entry.bytes)
            })
            .saturating_add(
                self.dirty
                    .keys()
                    .fold(0usize, |bytes, key| bytes.saturating_add(key.capacity())),
            )
    }

    fn make_room(&mut self, key: &str, bytes: usize, dirty: bool, limits: ContextLimits) -> bool {
        loop {
            let old = self.sites.get(key).map_or(0, |entry| entry.bytes);
            let extra = if dirty && !self.dirty.contains_key(key) {
                limits::node_bytes::<String, u64>().saturating_add(key.len())
            } else {
                0
            };
            let wanted = self
                .bytes()
                .saturating_sub(old)
                .saturating_add(bytes)
                .saturating_add(extra);
            let count = self.sites.len()
                + self
                    .dirty
                    .keys()
                    .filter(|key| !self.sites.contains_key(*key))
                    .count()
                + usize::from(!self.sites.contains_key(key) && !self.dirty.contains_key(key));
            if wanted <= limits.max_resident_bytes && count <= limits.max_resident_sites {
                return true;
            }
            let victim = self
                .sites
                .iter()
                .filter(|(candidate, _)| {
                    candidate.as_str() != key && !self.dirty.contains_key(*candidate)
                })
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(key, _)| key.clone());
            let Some(victim) = victim else {
                return false;
            };
            self.sites.remove(&victim);
            self.evictions = self.evictions.saturating_add(1);
        }
    }

    fn insert(&mut self, key: String, mut entry: CachedSite) {
        let key = key.into_boxed_str().into_string();
        self.clock = self.clock.wrapping_add(1);
        entry.last_used = self.clock;
        self.sites.insert(key, entry);
    }

    fn get(&mut self, key: &str) -> Option<SiteContext> {
        self.clock = self.clock.wrapping_add(1);
        let entry = self.sites.get_mut(key)?;
        entry.last_used = self.clock;
        let mut site = entry.site.clone();
        if let Some(cutoff) = self.retention_cutoff {
            prune_site(&mut site, cutoff);
            if site.pages.is_empty() && site.challenges.is_empty() {
                return None;
            }
        }
        Some(site)
    }

    fn mark_dirty(&mut self, key: &str) {
        self.revision = self.revision.wrapping_add(1);
        self.dirty.insert(key.to_string(), self.revision);
    }
}

#[derive(Clone)]
pub struct ContextStore {
    root: Arc<PathBuf>,
    state: Arc<Mutex<StoreState>>,
    /// One flush at a time: two overlapping flushes of one site could
    /// otherwise rename an older snapshot over a newer one.
    flushing: Arc<Mutex<()>>,
    _lock: Arc<Lockfile>,
    limits: ContextLimits,
}

impl ContextStore {
    pub async fn usage(&self) -> ContextUsage {
        let state = self.state.lock().await;
        ContextUsage {
            resident_sites: state.sites.len(),
            resident_bytes: state.bytes(),
            pending_changes: state.dirty.len(),
            evictions: state.evictions,
            rejected_updates: state.rejected_updates,
        }
    }
    /// Opens the store for one profile, creating directories, claiming the
    /// single-writer lockfile, and building the in-memory index. Corrupt or
    /// unsupported files are skipped and reported, never fatal.
    pub async fn open(
        root: impl AsRef<Path>,
        profile_id: &str,
    ) -> Result<(Self, OpenReport), ContextStoreError> {
        Self::open_with_limits(root, profile_id, ContextLimits::default()).await
    }

    pub async fn open_with_limits(
        root: impl AsRef<Path>,
        profile_id: &str,
        limits: ContextLimits,
    ) -> Result<(Self, OpenReport), ContextStoreError> {
        limits
            .validate()
            .map_err(ContextStoreError::ResourceLimit)?;
        let root = root.as_ref().join(encode_component(profile_id));
        tokio::fs::create_dir_all(&root).await?;
        let lock = Lockfile::claim(&root)?;
        let mut state = StoreState::default();
        let mut report = OpenReport::default();
        let mut entries = tokio::fs::read_dir(&root).await?;
        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("json") {
                continue;
            }
            match load_envelope(&path, limits).await {
                Ok((key, site)) => {
                    let entry = CachedSite::new(&key, site);
                    if state.make_room(&key, entry.bytes, false, limits) {
                        state.insert(key, entry);
                    }
                    report.sites_loaded += 1;
                }
                Err(reason) => {
                    let reason: String = reason.chars().take(512).collect();
                    tracing::warn!(file = %path.display(), %reason, "context.site_skipped");
                    report.skipped_total += 1;
                    if report.skipped.len() < 128 {
                        report.skipped.push(SkippedSite { file: path, reason });
                    }
                }
            }
        }
        Ok((
            Self {
                root: Arc::new(root),
                state: Arc::new(Mutex::new(state)),
                flushing: Arc::new(Mutex::new(())),
                _lock: Arc::new(lock),
                limits,
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
        Self::open_with_limits_and_ttl(root, profile_id, ContextLimits::default(), ttl_days, today)
            .await
    }

    pub async fn open_with_limits_and_ttl(
        root: impl AsRef<Path>,
        profile_id: &str,
        limits: ContextLimits,
        ttl_days: u32,
        today: u32,
    ) -> Result<(Self, OpenReport), ContextStoreError> {
        let (store, report) = Self::open_with_limits(root, profile_id, limits).await?;
        let dropped = store.sweep(ttl_days, today).await?;
        if dropped > 0 {
            tracing::info!(dropped, "context.swept_expired_records");
        }
        Ok((store, report))
    }

    /// In-memory view of a site, if present.
    pub async fn site(&self, site_key: &str) -> Option<SiteContext> {
        {
            let mut state = self.state.lock().await;
            if let Some(site) = state.get(site_key) {
                return Some(site);
            }
            if state.dirty.contains_key(site_key) {
                return None;
            }
        }
        let _guard = self.flushing.lock().await;
        {
            let mut state = self.state.lock().await;
            if let Some(site) = state.get(site_key) {
                return Some(site);
            }
            if state.dirty.contains_key(site_key) {
                return None;
            }
        }
        let (key, mut site) = load_envelope(&self.path(site_key), self.limits)
            .await
            .ok()?;
        if key != site_key {
            return None;
        }
        if let Some(cutoff) = self.state.lock().await.retention_cutoff {
            prune_site(&mut site, cutoff);
            if site.pages.is_empty() && site.challenges.is_empty() {
                return None;
            }
        }
        let entry = CachedSite::new(&key, site.clone());
        let mut state = self.state.lock().await;
        if state.make_room(&key, entry.bytes, false, self.limits) {
            state.insert(key, entry);
        }
        Some(site)
    }

    pub async fn list_sites(&self) -> Vec<String> {
        let _guard = self.flushing.lock().await;
        let mut keys = std::collections::BTreeSet::new();
        if let Ok(mut files) = tokio::fs::read_dir(self.root.as_ref()).await {
            while let Ok(Some(file)) = files.next_entry().await {
                if file
                    .path()
                    .extension()
                    .is_some_and(|extension| extension == "json")
                {
                    if let Ok((key, mut site)) = load_envelope(&file.path(), self.limits).await {
                        if let Some(cutoff) = self.state.lock().await.retention_cutoff {
                            prune_site(&mut site, cutoff);
                            if site.pages.is_empty() && site.challenges.is_empty() {
                                continue;
                            }
                        }
                        keys.insert(key);
                    }
                }
            }
        }
        let state = self.state.lock().await;
        keys.extend(state.sites.iter().filter_map(|(key, entry)| {
            let mut site = entry.site.clone();
            if let Some(cutoff) = state.retention_cutoff {
                prune_site(&mut site, cutoff);
                if site.pages.is_empty() && site.challenges.is_empty() {
                    return None;
                }
            }
            Some(key.clone())
        }));
        for key in state
            .dirty
            .keys()
            .filter(|key| !state.sites.contains_key(*key))
        {
            keys.remove(key);
        }
        keys.into_iter().collect()
    }

    /// Replaces (or inserts) a site's context, buffering the write behind
    /// `flush`. Never fails the caller's workflow: persistence happens on
    /// flush, and flush errors degrade to session-only.
    pub async fn upsert_site(&self, site_key: &str, site: SiteContext) {
        self.update_site(site_key, |current| *current = site).await;
    }

    /// Mutate a site atomically, avoiding lost updates from cloned snapshots.
    pub async fn update_site(&self, site_key: &str, update: impl FnOnce(&mut SiteContext)) {
        let guard = self.flushing.clone().lock_owned().await;
        let current = {
            let mut state = self.state.lock().await;
            if state.dirty.contains_key(site_key) {
                Some(state.get(site_key).unwrap_or_default())
            } else {
                state.get(site_key)
            }
        };
        let mut site = match current {
            Some(site) => site,
            None => {
                let path = self.path(site_key);
                match tokio::fs::try_exists(&path).await {
                    Ok(false) => SiteContext::default(),
                    Ok(true) => match load_envelope(&path, self.limits).await {
                        Ok((key, site)) if key == site_key => site,
                        _ => {
                            self.reject_update("existing site cannot be loaded within limits")
                                .await;
                            return;
                        }
                    },
                    Err(_) => {
                        self.reject_update("existing site cannot be inspected")
                            .await;
                        return;
                    }
                }
            }
        };
        if let Some(cutoff) = self.state.lock().await.retention_cutoff {
            prune_site(&mut site, cutoff);
        }
        update(&mut site);
        let envelope = SiteEnvelope {
            schema: SCHEMA_VERSION,
            site_key: site_key.to_string(),
            site,
        };
        if let Err(reason) = self.limits.check_envelope(&envelope) {
            self.reject_update(&reason).await;
            return;
        }
        let entry = CachedSite::new(site_key, envelope.site);
        if entry
            .bytes
            .saturating_add(limits::node_bytes::<String, u64>())
            .saturating_add(site_key.len())
            > self.limits.max_resident_bytes
        {
            self.reject_update("site exceeds resident cache byte limit")
                .await;
            return;
        }
        let fits = self
            .state
            .lock()
            .await
            .make_room(site_key, entry.bytes, true, self.limits);
        let _guard = if fits {
            guard
        } else {
            // Return ownership of the writer lock after flushing. If the caller
            // cancels, the owned transaction finishes and then releases it.
            let store = self.clone();
            match tokio::spawn(async move {
                store.flush_locked().await;
                guard
            })
            .await
            {
                Ok(guard) => guard,
                Err(_) => {
                    self.reject_update("pressure flush failed").await;
                    return;
                }
            }
        };
        let mut state = self.state.lock().await;
        if !state.make_room(site_key, entry.bytes, true, self.limits) {
            drop(state);
            self.reject_update("resident cache limit; dirty updates preserved")
                .await;
            return;
        }
        state.insert(site_key.to_string(), entry);
        state.mark_dirty(site_key);
    }

    async fn reject_update(&self, reason: &str) {
        let mut state = self.state.lock().await;
        state.rejected_updates = state.rejected_updates.saturating_add(1);
        tracing::warn!(%reason, rejected_updates = state.rejected_updates, "context.update_rejected");
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
        self.update_site(site_key, |site| {
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
        })
        .await;
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
        let guard = self.flushing.clone().lock_owned().await;
        let store = self.clone();
        // The owned writer continues through cancellation of its caller. The
        // lock stays held until all filesystem work has finished.
        tokio::spawn(async move {
            let _guard = guard;
            store.flush_locked().await
        })
        .await
        .expect("context persistence task panicked")
    }

    async fn flush_locked(&self) -> Vec<String> {
        let dirty_keys: Vec<_> = self.state.lock().await.dirty.keys().cloned().collect();
        let mut failed = Vec::new();
        for key in dirty_keys {
            let (revision, site) = {
                let state = self.state.lock().await;
                let Some(revision) = state.dirty.get(&key).copied() else {
                    continue;
                };
                (
                    revision,
                    state.sites.get(&key).map(|entry| entry.site.clone()),
                )
            };
            let result = match site {
                Some(site) => self.write_site(&key, &site).await,
                None => self.remove_site_file(&key).await,
            };
            if let Err(error) = result {
                tracing::warn!(site = %key, %error, "context.flush_failed");
                failed.push(key);
            } else {
                let mut state = self.state.lock().await;
                if state.dirty.get(&key) == Some(&revision) {
                    state.dirty.remove(&key);
                }
            }
        }
        failed
    }

    async fn remove_site_file(&self, key: &str) -> Result<(), ContextStoreError> {
        match tokio::fs::remove_file(self.path(key)).await {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
        File::open(self.root.as_ref()).await?.sync_all().await?;
        Ok(())
    }

    /// Erasure is ordered after earlier writes and remains pending if I/O fails.
    pub async fn forget(&self, site_key: &str) -> Result<(), ContextStoreError> {
        let guard = self.flushing.clone().lock_owned().await;
        let store = self.clone();
        let key = site_key.to_string();
        tokio::spawn(async move {
            let _guard = guard;
            if !tokio::fs::try_exists(store.path(&key)).await? {
                let mut state = store.state.lock().await;
                state.sites.remove(&key);
                state.dirty.remove(&key);
                return Ok(());
            }
            let fits = store
                .state
                .lock()
                .await
                .make_room(&key, 0, true, store.limits);
            if !fits {
                store.flush_locked().await;
            }
            let mut state = store.state.lock().await;
            if !state.make_room(&key, 0, true, store.limits) {
                return Err(ContextStoreError::ResourceLimit(
                    "cannot buffer erasure; dirty updates preserved",
                ));
            }
            state.sites.remove(&key);
            state.mark_dirty(&key);
            let revision = state.dirty[&key];
            drop(state);
            store.remove_site_file(&key).await?;
            let mut state = store.state.lock().await;
            if state.dirty.get(&key) == Some(&revision) {
                state.dirty.remove(&key);
            }
            Ok(())
        })
        .await
        .map_err(std::io::Error::other)?
    }

    /// Drops intent stats not verified within `ttl_days` of `today` (a
    /// day-since-epoch value). Empty forms, pages, and sites are pruned and
    /// their files removed. Returns the number of stats dropped.
    pub async fn sweep(&self, ttl_days: u32, today: u32) -> Result<u64, ContextStoreError> {
        let guard = self.flushing.clone().lock_owned().await;
        let store = self.clone();
        tokio::spawn(async move {
            let _guard = guard;
            store.sweep_locked(ttl_days, today).await
        })
        .await
        .map_err(std::io::Error::other)?
    }

    async fn sweep_locked(&self, ttl_days: u32, today: u32) -> Result<u64, ContextStoreError> {
        let cutoff = today.saturating_sub(ttl_days);
        self.state.lock().await.retention_cutoff = Some(cutoff);
        let mut dropped = 0_u64;
        let keys: Vec<_> = self.state.lock().await.sites.keys().cloned().collect();
        for key in keys {
            let mut site = match self.state.lock().await.sites.get(&key) {
                Some(entry) => entry.site.clone(),
                None => continue,
            };
            let (removed, changed) = prune_site(&mut site, cutoff);
            dropped += removed;
            let empty = site.pages.is_empty() && site.challenges.is_empty();
            if !changed && !empty && !self.state.lock().await.dirty.contains_key(&key) {
                continue;
            }
            if empty {
                self.remove_site_file(&key).await?;
                let mut state = self.state.lock().await;
                state.sites.remove(&key);
                state.dirty.remove(&key);
            } else {
                self.write_site(&key, &site).await?;
                let entry = CachedSite::new(&key, site);
                let mut state = self.state.lock().await;
                state.dirty.remove(&key);
                if state.make_room(&key, entry.bytes, false, self.limits) {
                    state.insert(key, entry);
                } else {
                    state.sites.remove(&key);
                }
            }
        }
        // Evicted sites still participate in retention, one bounded file at a
        // time. Cache residency cannot exempt durable records from expiry.
        let mut files = tokio::fs::read_dir(self.root.as_ref()).await?;
        while let Some(file) = files.next_entry().await? {
            let path = file.path();
            if !path
                .extension()
                .is_some_and(|extension| extension == "json")
            {
                continue;
            }
            let Ok((key, mut site)) = load_envelope(&path, self.limits).await else {
                continue;
            };
            if self.state.lock().await.sites.contains_key(&key) {
                continue;
            }
            let (removed, changed) = prune_site(&mut site, cutoff);
            dropped += removed;
            if site.pages.is_empty() && site.challenges.is_empty() {
                self.remove_site_file(&key).await?;
            } else if changed {
                self.write_site(&key, &site).await?;
            }
        }
        if let Some(key) = self.flush_locked().await.into_iter().next() {
            return Err(
                std::io::Error::other(format!("retention persistence failed for {key}")).into(),
            );
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
            site,
        };
        self.limits
            .check_envelope(&envelope)
            .map_err(|_| ContextStoreError::ResourceLimit("site file or structure limit"))?;
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

fn prune_site(site: &mut SiteContext, cutoff: u32) -> (u64, bool) {
    let before_pages = site.pages.len();
    let mut dropped = 0;
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
    (dropped, dropped != 0 || before_pages != site.pages.len())
}

async fn load_envelope(
    path: &Path,
    limits: ContextLimits,
) -> Result<(String, SiteContext), String> {
    let file = File::open(path).await.map_err(|error| error.to_string())?;
    if file
        .metadata()
        .await
        .map_err(|error| error.to_string())?
        .len()
        > limits.max_file_bytes as u64
    {
        return Err("context site exceeds file byte limit".into());
    }
    let mut bytes = Vec::new();
    file.take((limits.max_file_bytes as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| error.to_string())?;
    if bytes.len() > limits.max_file_bytes {
        return Err("context site exceeds file byte limit".into());
    }
    decode_envelope(path, &bytes, limits)
}

fn decode_envelope(
    path: &Path,
    bytes: &[u8],
    limits: ContextLimits,
) -> Result<(String, SiteContext), String> {
    let envelope: SiteEnvelope =
        serde_json::from_slice(bytes).map_err(|error| error.to_string())?;
    if envelope.schema != SCHEMA_VERSION {
        return Err(format!(
            "unsupported schema {}; expected {SCHEMA_VERSION}",
            envelope.schema
        ));
    }
    limits.check_site(&envelope.site).map_err(str::to_string)?;
    let expected_name = format!("{}.json", encode_component(&envelope.site_key));
    if path.file_name().and_then(|name| name.to_str()) != Some(expected_name.as_str()) {
        return Err("context site identity does not match filename".into());
    }
    Ok((envelope.site_key, envelope.site))
}

/// Inspect a site without claiming the writer lock or modifying its file.
pub fn inspect_site_file(path: &Path, limits: ContextLimits) -> Result<(), String> {
    use std::io::Read;
    limits.validate().map_err(str::to_string)?;
    let file = std::fs::File::open(path).map_err(|error| error.to_string())?;
    if file.metadata().map_err(|error| error.to_string())?.len() > limits.max_file_bytes as u64 {
        return Err("context site exceeds file byte limit".into());
    }
    let mut bytes = Vec::new();
    file.take((limits.max_file_bytes as u64).saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|error| error.to_string())?;
    if bytes.len() > limits.max_file_bytes {
        return Err("context site exceeds file byte limit".into());
    }
    decode_envelope(path, &bytes, limits).map(|_| ())
}

/// Injective filesystem encoding for arbitrary UTF-8 identity strings.
fn encode_component(value: &str) -> String {
    hex::encode(value.as_bytes())
}

struct Lockfile {
    file: std::fs::File,
}

impl Lockfile {
    fn claim(root: &Path) -> Result<Self, ContextStoreError> {
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
        match file.try_lock() {
            Ok(()) => Ok(Self { file }),
            Err(std::fs::TryLockError::WouldBlock) => Err(ContextStoreError::AlreadyLocked),
            Err(std::fs::TryLockError::Error(error)) => Err(error.into()),
        }
    }
}

impl Drop for Lockfile {
    /// Releases the lock explicitly instead of leaving it to the close.
    ///
    /// A flock belongs to the open file description, not to this descriptor,
    /// and a close only releases it once every copy of the descriptor is
    /// gone. A child that any thread is spawning holds a copy from its fork
    /// until its exec, `O_CLOEXEC` notwithstanding, so a close alone let the
    /// lock outlive the store: the next open in this process, such as the
    /// one in `bobby context forget`, then reported a running bobby that did
    /// not exist. Unlocking drops the lock for every copy at once.
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

#[cfg(test)]
mod persistence_tests {
    use super::*;

    #[tokio::test]
    async fn cancelled_flush_finishes_before_erasure_and_does_not_revive_a_site() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = ContextStore::open(dir.path(), "profile").await.unwrap();
        store.upsert_site("site", SiteContext::default()).await;
        let state_guard = store.state.lock().await;
        let flushing_store = store.clone();
        let flush = tokio::spawn(async move { flushing_store.flush().await });
        while store.flushing.try_lock().is_ok() {
            tokio::task::yield_now().await;
        }
        flush.abort();
        let _ = flush.await;
        drop(state_guard);
        store.forget("site").await.unwrap();
        assert!(!store.path("site").exists());
        assert!(store.site("site").await.is_none());
        assert!(store.flush().await.is_empty());
    }
}
