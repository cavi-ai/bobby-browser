use std::{
    collections::{HashMap, HashSet},
    future::Future,
    io,
    path::Path,
    sync::{atomic::Ordering, Arc},
};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::{watch, Mutex};
use types::{
    CommandOutcome, CorrelationId, ErrorLayer, IdempotencyKey, InterfaceError, InterfaceErrorCode,
    InterfaceOperation, PrincipalId, SessionState, WorkflowCheckpoint,
};

mod durable;
pub use durable::{
    downgrade_idempotency_ledger, inspect_idempotency_ledger, IdempotencyLedgerHealth,
};

#[cfg(test)]
mod persistence_tests {
    use super::*;
    fn owner() -> PrincipalId {
        PrincipalId::from_uuid(uuid::Uuid::nil())
    }
    fn key() -> IdempotencyKey {
        IdempotencyKey::try_from("cancelled-caller").unwrap()
    }
    async fn reserve(store: &IdempotencyStore) -> Result<IdempotencyReservation, InterfaceError> {
        let now = Utc::now();
        store
            .reserve(
                owner(),
                key(),
                InterfaceOperation::SubmitCommand,
                [0; 32],
                now,
                now + Duration::seconds(2),
                CorrelationId::new(),
            )
            .await
    }
    async fn open(path: &Path) -> IdempotencyStore {
        IdempotencyStore::open_durable(path, |value| async move {
            let command_id = serde_json::from_value(value).map_err(io::Error::other)?;
            Ok(Some(CommandOutcome::Completed {
                command_id,
                evidence: vec![],
            }))
        })
        .await
        .unwrap()
    }

    #[tokio::test]
    async fn cancelled_finish_keeps_ownership_until_the_outcome_is_durable() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("ledger.json");
        let store = open(&path).await;
        let IdempotencyReservation::Acquired(permit) = reserve(&store).await.unwrap() else {
            panic!("acquire");
        };
        let command_id = types::CommandId::new();
        let writer = store.durable.as_ref().unwrap().writer.lock().await;
        assert!(tokio::time::timeout(
            std::time::Duration::from_millis(30),
            store.finish(
                permit,
                CommandOutcome::Completed {
                    command_id: command_id.clone(),
                    evidence: vec![]
                },
                Utc::now()
            )
        )
        .await
        .is_err());
        assert!(
            store.state.try_lock().is_err(),
            "owned transaction must hold state until the blocked write finishes"
        );
        drop(writer);
        assert!(
            matches!(tokio::time::timeout(std::time::Duration::from_secs(1), reserve(&store)).await.unwrap().unwrap(), IdempotencyReservation::Replay(CommandOutcome::Completed {command_id:id,..}) if id == command_id)
        );
        drop(store);
        let reopened = open(&path).await;
        assert!(
            matches!(reserve(&reopened).await.unwrap(), IdempotencyReservation::Replay(CommandOutcome::Completed {command_id:id,..}) if id == command_id)
        );
    }

    #[tokio::test]
    async fn cancelled_reserve_finishes_as_uncertain_instead_of_leaking_a_permit() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("ledger.json");
        let store = open(&path).await;
        let writer = store.durable.as_ref().unwrap().writer.lock().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), reserve(&store))
                .await
                .is_err()
        );
        assert!(store.state.try_lock().is_err());
        drop(writer);
        assert!(
            tokio::time::timeout(std::time::Duration::from_secs(1), reserve(&store))
                .await
                .unwrap()
                .unwrap_err()
                .reconciliation_required
        );
        drop(store);
        let reopened = open(&path).await;
        assert!(
            reserve(&reopened)
                .await
                .unwrap_err()
                .reconciliation_required
        );
    }

    #[tokio::test]
    async fn cancelled_abandon_finishes_the_durable_release() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("ledger.json");
        let store = open(&path).await;
        let IdempotencyReservation::Acquired(permit) = reserve(&store).await.unwrap() else {
            panic!("acquire");
        };
        let writer = store.durable.as_ref().unwrap().writer.lock().await;
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(30), store.abandon(permit))
                .await
                .is_err()
        );
        assert!(store.state.try_lock().is_err());
        drop(writer);
        let state = tokio::time::timeout(std::time::Duration::from_secs(1), store.state.lock())
            .await
            .unwrap();
        assert!(state.entries.is_empty());
        drop(state);
        drop(store);
        let reopened = open(&path).await;
        assert!(matches!(
            reserve(&reopened).await.unwrap(),
            IdempotencyReservation::Acquired(_)
        ));
    }

    #[tokio::test]
    async fn failed_checkpoint_never_admits_work_and_latches_write_uncertainty() {
        let root = tempfile::tempdir().unwrap();
        let active = root.path().join("active");
        let path = active.join("ledger.json");
        let store = open(&path).await;
        std::fs::rename(&active, root.path().join("moved")).unwrap();
        assert!(reserve(&store).await.unwrap_err().reconciliation_required);
        assert_eq!(store.integrity_issue(), Some("ledgerWriteFailed"));
        std::fs::create_dir(&active).unwrap();
        assert!(reserve(&store).await.unwrap_err().reconciliation_required);
        assert!(
            !path.exists(),
            "a failed reservation must not become fresh writable state"
        );
    }
}

/// Outcome types that an [`IdempotencyStore`] can retain and replay.
pub trait RetainedOutcome: Clone + Serialize + Send + Sync + 'static {
    /// Whether finishing with this outcome releases the reservation instead of
    /// retaining it (retryable outcomes must allow a real retry).
    fn releases_reservation(&self) -> bool;
    /// Whether this outcome must never expire or be evicted (uncertain outcomes
    /// tombstone the key until explicitly resolved).
    fn safety_relevant(&self) -> bool;
    /// Whether the effect behind this outcome is unknown, so a different
    /// request under the same key must reconcile first.
    fn uncertain(&self) -> bool {
        false
    }

    /// Durable representation. Command outcomes store only their journal
    /// identity; the command journal remains the source of page evidence.
    fn durable_value(&self) -> io::Result<serde_json::Value> {
        serde_json::to_value(self).map_err(io::Error::other)
    }
}

impl RetainedOutcome for CommandOutcome {
    fn releases_reservation(&self) -> bool {
        outcome_releases(self)
    }

    fn safety_relevant(&self) -> bool {
        !matches!(self, CommandOutcome::Completed { .. })
    }

    fn uncertain(&self) -> bool {
        matches!(self, CommandOutcome::NeedsReconciliation { .. })
    }

    fn durable_value(&self) -> io::Result<serde_json::Value> {
        let command_id = match self {
            CommandOutcome::Completed { command_id, .. }
            | CommandOutcome::RetryableFailure { command_id, .. }
            | CommandOutcome::NeedsReconciliation { command_id, .. }
            | CommandOutcome::PolicyDenied { command_id, .. }
            | CommandOutcome::ResourceExhausted { command_id, .. }
            | CommandOutcome::Restarted { command_id, .. }
            | CommandOutcome::Failed { command_id, .. } => command_id,
        };
        serde_json::to_value(command_id).map_err(io::Error::other)
    }
}

/// Retained outcomes for session/checkpoint lifecycle operations. Successes replay
/// with ordinary TTL semantics; failures are never retained (callers abandon the
/// permit on error so a retry re-executes).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionCheckpointOutcome {
    Session(SessionState),
    Checkpoint(WorkflowCheckpoint),
}

impl RetainedOutcome for SessionCheckpointOutcome {
    fn releases_reservation(&self) -> bool {
        false
    }

    fn safety_relevant(&self) -> bool {
        false
    }
}

struct Entry<O> {
    key: IdempotencyKey,
    operation: InterfaceOperation,
    canonical_sha256: [u8; 32],
    state: EntryState<O>,
    expires_at: Option<DateTime<Utc>>,
    last_used: u64,
}

enum EntryState<O> {
    Reserved {
        generation: u64,
        changed: watch::Sender<ReservationUpdate<O>>,
    },
    Retained {
        outcome: O,
        safety_relevant: bool,
    },
    Unresolved,
}

#[derive(Clone)]
enum ReservationUpdate<O> {
    Pending,
    Replay(O),
    Released,
    Unresolved,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurableEntry {
    principal_id: PrincipalId,
    key: IdempotencyKey,
    operation: InterfaceOperation,
    canonical_sha256: [u8; 32],
    expires_at: Option<DateTime<Utc>>,
    last_used: u64,
    state: DurableState,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "camelCase",
    deny_unknown_fields
)]
enum DurableState {
    Reserved,
    Unresolved,
    Retained(serde_json::Value),
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurableSnapshot {
    schema_version: u16,
    entries: Vec<DurableEntry>,
}

struct StoreState<O> {
    entries: HashMap<PrincipalId, Vec<Entry<O>>>,
    sequence: u64,
    dirty: HashSet<(PrincipalId, IdempotencyKey)>,
}

struct DurableLock {
    file: std::fs::File,
}

impl Drop for DurableLock {
    fn drop(&mut self) {
        // A forked child can still hold a copy of this open file description
        // before exec. Closing our descriptor would leave its flock held.
        let _ = self.file.unlock();
    }
}

impl<O> Default for StoreState<O> {
    fn default() -> Self {
        Self {
            entries: HashMap::new(),
            sequence: 0,
            dirty: HashSet::new(),
        }
    }
}

#[derive(Clone)]
pub struct IdempotencyStore<O = CommandOutcome> {
    per_principal_capacity: usize,
    global_capacity: usize,
    ttl: Duration,
    state: Arc<Mutex<StoreState<O>>>,
    durable: Option<Arc<durable::Persistence>>,
    integrity_issue: Option<&'static str>,
}

impl<O> std::fmt::Debug for IdempotencyStore<O> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IdempotencyStore")
            .field("per_principal_capacity", &self.per_principal_capacity)
            .field("global_capacity", &self.global_capacity)
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl<O: RetainedOutcome> Default for IdempotencyStore<O> {
    fn default() -> Self {
        Self::with_global_capacity(256, 4096, Duration::minutes(15))
    }
}

impl<O: RetainedOutcome> IdempotencyStore<O> {
    /// Open an owner-only ledger with durable appends and atomic checkpoints. A retained reference
    /// that cannot be resolved from its authoritative store fails closed.
    pub async fn open_durable<F, Fut>(path: impl AsRef<Path>, resolve: F) -> io::Result<Self>
    where
        F: Fn(serde_json::Value) -> Fut,
        Fut: Future<Output = io::Result<Option<O>>>,
    {
        let (persistence, entries) =
            durable::Persistence::open(path.as_ref().to_path_buf()).await?;
        let mut store = Self {
            durable: Some(Arc::new(persistence)),
            ..Self::default()
        };
        let entries = match entries {
            Ok(entries) => entries,
            Err(issue) => {
                store.integrity_issue = Some(issue);
                return Ok(store);
            }
        };
        let now = Utc::now();
        let mut state = store.state.lock().await;
        let mut protect_unknown = false;
        for entry in entries {
            let identity = (entry.principal_id.clone(), entry.key.clone());
            if entry.expires_at.is_some_and(|expires| expires <= now) {
                state.dirty.insert(identity);
                continue;
            }
            let restored = match entry.state {
                DurableState::Reserved | DurableState::Unresolved => EntryState::Unresolved,
                // An outcome that cannot be resolved is unknown, so the key
                // is unresolved: a retry reconciles instead of re-running.
                DurableState::Retained(value) => match resolve(value).await {
                    Ok(Some(outcome)) => EntryState::Retained {
                        safety_relevant: outcome.safety_relevant(),
                        outcome,
                    },
                    Ok(None) | Err(_) => EntryState::Unresolved,
                },
            };
            let expires_at = if matches!(
                restored,
                EntryState::Unresolved
                    | EntryState::Retained {
                        safety_relevant: true,
                        ..
                    }
            ) {
                // Missing authority cannot become permission to retry after TTL.
                if entry.expires_at.is_some() {
                    state.dirty.insert(identity);
                    protect_unknown = true;
                }
                None
            } else {
                entry.expires_at
            };
            let bucket = state.entries.entry(entry.principal_id).or_default();
            bucket.push(Entry {
                key: entry.key,
                operation: entry.operation,
                canonical_sha256: entry.canonical_sha256,
                state: restored,
                expires_at,
                last_used: entry.last_used,
            });
            state.sequence = state.sequence.max(entry.last_used);
        }
        drop(state);
        if protect_unknown {
            let transaction = store.clone();
            let result = tokio::spawn(async move {
                let mut state = transaction.state.lock().await;
                transaction.persist_locked(&mut state).await
            })
            .await
            .map_err(io::Error::other)?;
            if let Err(error) = result {
                // Keep setup available but never admit work against unprotected history.
                store.integrity_issue = Some("ledgerWriteFailed");
                tracing::warn!(%error, "idempotency uncertainty could not be persisted; mutation disabled");
            }
        }
        Ok(store)
    }

    /// Stable, payload-free reason that durable history cannot authorize writes.
    pub fn integrity_issue(&self) -> Option<&'static str> {
        self.integrity_issue.or_else(|| {
            self.durable
                .as_ref()
                .filter(|p| p.failed.load(Ordering::Acquire))
                .map(|_| "ledgerWriteFailed")
        })
    }

    pub fn ensure_writable(&self, correlation_id: &CorrelationId) -> Result<(), InterfaceError> {
        if self.integrity_issue().is_some() {
            let mut error = unresolved_error(correlation_id.clone());
            error.message = "durable idempotency history requires repair before mutation".into();
            return Err(error);
        }
        Ok(())
    }

    fn durable_entry(principal_id: &PrincipalId, entry: &Entry<O>) -> io::Result<DurableEntry> {
        let state = match &entry.state {
            EntryState::Reserved { .. } => DurableState::Reserved,
            EntryState::Unresolved => DurableState::Unresolved,
            EntryState::Retained { outcome, .. } => {
                let value = outcome.durable_value()?;
                if serde_json::to_vec(&value).map_err(io::Error::other)?.len() > 64 * 1024 {
                    return Err(io::Error::other(
                        "idempotency outcome exceeds durable bound",
                    ));
                }
                DurableState::Retained(value)
            }
        };
        Ok(DurableEntry {
            principal_id: principal_id.clone(),
            key: entry.key.clone(),
            operation: entry.operation,
            canonical_sha256: entry.canonical_sha256,
            expires_at: entry.expires_at,
            last_used: entry.last_used,
            state,
        })
    }

    async fn persist_locked(&self, state: &mut StoreState<O>) -> io::Result<()> {
        if self.integrity_issue().is_some() {
            return Err(io::Error::other(
                "durable idempotency history requires repair",
            ));
        }
        let Some(persistence) = &self.durable else {
            state.dirty.clear();
            return Ok(());
        };
        let mut writer = persistence.writer.lock().await;
        let result = if writer.needs_checkpoint() {
            let entries = state
                .entries
                .iter()
                .flat_map(|(principal, bucket)| {
                    bucket
                        .iter()
                        .map(move |entry| Self::durable_entry(principal, entry))
                })
                .collect::<io::Result<Vec<_>>>()?;
            writer.checkpoint(&persistence.path, entries).await
        } else {
            let mut updates = Vec::new();
            let mut removes = Vec::new();
            for (principal, key) in &state.dirty {
                if let Some(entry) = state
                    .entries
                    .get(principal)
                    .and_then(|bucket| bucket.iter().find(|e| &e.key == key))
                {
                    updates.push(Self::durable_entry(principal, entry)?);
                } else {
                    removes.push((principal.clone(), key.clone()));
                }
            }
            if updates.is_empty() && removes.is_empty() {
                return Ok(());
            }
            writer.append(updates, removes).await
        };
        if result.is_ok() {
            state.dirty.clear();
        } else {
            persistence.failed.store(true, Ordering::Release);
        }
        result
    }
    pub fn new(per_principal_capacity: usize, ttl: Duration) -> Self {
        Self::with_global_capacity(
            per_principal_capacity,
            per_principal_capacity.saturating_mul(64),
            ttl,
        )
    }

    pub fn with_global_capacity(
        per_principal_capacity: usize,
        global_capacity: usize,
        ttl: Duration,
    ) -> Self {
        Self {
            per_principal_capacity,
            global_capacity,
            ttl,
            state: Arc::new(Mutex::new(StoreState::default())),
            durable: None,
            integrity_issue: None,
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn reserve(
        &self,
        principal_id: PrincipalId,
        key: IdempotencyKey,
        operation: InterfaceOperation,
        canonical_sha256: [u8; 32],
        mut now: DateTime<Utc>,
        deadline: DateTime<Utc>,
        correlation_id: CorrelationId,
    ) -> Result<IdempotencyReservation<O>, InterfaceError> {
        self.ensure_writable(&correlation_id)?;
        loop {
            let wait = {
                let mut guard = self.state.lock().await;
                let state = &mut *guard;
                cleanup_expired(state, now);
                state.sequence = state.sequence.wrapping_add(1);
                let last_used = state.sequence;

                if let Some(entries) = state.entries.get_mut(&principal_id) {
                    if let Some(index) = entries.iter().position(|entry| entry.key == key) {
                        let mut entry = entries.remove(index);
                        if entry.operation != operation
                            || entry.canonical_sha256 != canonical_sha256
                        {
                            // A key whose outcome is unknown may already have
                            // taken effect. A retry after a restart runs in a
                            // new session, so its digest differs; a plain
                            // conflict there reads as "use a fresh key", which
                            // repeats the effect.
                            let uncertain = match &entry.state {
                                EntryState::Unresolved => true,
                                EntryState::Retained { outcome, .. } => outcome.uncertain(),
                                EntryState::Reserved { .. } => false,
                            };
                            entries.push(entry);
                            return Err(if uncertain {
                                unresolved_error(correlation_id)
                            } else {
                                conflict_error(correlation_id)
                            });
                        }
                        entry.last_used = last_used;
                        state.dirty.insert((principal_id.clone(), key.clone()));
                        match &entry.state {
                            EntryState::Reserved { changed, .. } => {
                                let receiver = changed.subscribe();
                                entries.push(entry);
                                Some(receiver)
                            }
                            EntryState::Retained { outcome, .. } => {
                                let outcome = outcome.clone();
                                entries.push(entry);
                                return Ok(IdempotencyReservation::Replay(outcome));
                            }
                            EntryState::Unresolved => {
                                entries.push(entry);
                                return Err(unresolved_error(correlation_id));
                            }
                        }
                    } else {
                        None
                    }
                } else {
                    None
                }
            };

            if let Some(mut receiver) = wait {
                let remaining = deadline.signed_duration_since(Utc::now());
                let Ok(remaining) = remaining.to_std() else {
                    return Err(deadline_error(correlation_id));
                };
                if tokio::time::timeout(remaining, receiver.changed())
                    .await
                    .is_err()
                {
                    return Err(deadline_error(correlation_id));
                }
                match receiver.borrow().clone() {
                    ReservationUpdate::Replay(outcome) => {
                        return Ok(IdempotencyReservation::Replay(outcome));
                    }
                    ReservationUpdate::Unresolved => return Err(unresolved_error(correlation_id)),
                    ReservationUpdate::Released | ReservationUpdate::Pending => {}
                }
                now = Utc::now();
                continue;
            }

            let store = self.clone();
            let owner = principal_id.clone();
            let reservation_key = key.clone();
            let correlation = correlation_id.clone();
            let acquired = tokio::spawn(async move {
                let principal_id = owner;
                let key = reservation_key;
                let correlation_id = correlation;
                let mut guard = store.state.lock().await;
                let state = &mut *guard;
                store.ensure_writable(&correlation_id)?;
                cleanup_expired(state, now);
                if state
                    .entries
                    .get(&principal_id)
                    .is_some_and(|entries| entries.iter().any(|entry| entry.key == key))
                {
                    return Ok(None);
                }
                make_principal_room(state, &principal_id, store.per_principal_capacity);
                make_global_room(state, store.global_capacity);
                let principal_len = state.entries.get(&principal_id).map_or(0, Vec::len);
                if store.per_principal_capacity == 0
                    || store.global_capacity == 0
                    || principal_len >= store.per_principal_capacity
                    || entry_count(state) >= store.global_capacity
                {
                    return Err(resource_exhausted_error(correlation_id));
                }
                state.sequence = state.sequence.wrapping_add(1);
                let generation = state.sequence;
                let (changed, _) = watch::channel(ReservationUpdate::Pending);
                state
                    .entries
                    .entry(principal_id.clone())
                    .or_default()
                    .push(Entry {
                        key: key.clone(),
                        operation,
                        canonical_sha256,
                        state: EntryState::Reserved {
                            generation,
                            changed,
                        },
                        expires_at: None,
                        last_used: generation,
                    });
                state.dirty.insert((principal_id.clone(), key.clone()));
                if let Err(error) = store.persist_locked(state).await {
                    if let Some(entry) = state
                        .entries
                        .get_mut(&principal_id)
                        .and_then(|bucket| bucket.iter_mut().find(|entry| entry.key == key))
                    {
                        entry.state = EntryState::Unresolved;
                    }
                    return Err(persistence_error(correlation_id, error));
                }
                Ok(Some(IdempotencyPermit {
                    principal_id,
                    key,
                    operation,
                    canonical_sha256,
                    generation,
                    store: Arc::clone(&store.state),
                    durable: store.durable.is_some(),
                    armed: true,
                    correlation_id,
                }))
            })
            .await
            .map_err(|error| {
                persistence_error(correlation_id.clone(), io::Error::other(error))
            })??;
            if let Some(permit) = acquired {
                return Ok(IdempotencyReservation::Acquired(permit));
            }
        }
    }

    pub async fn finish(
        &self,
        permit: IdempotencyPermit<O>,
        outcome: O,
        now: DateTime<Utc>,
    ) -> Result<(), InterfaceError> {
        let store = self.clone();
        let correlation = permit.correlation_id.clone();
        tokio::spawn(async move { store.finish_inner(permit, outcome, now).await })
            .await
            .map_err(|error| persistence_error(correlation, io::Error::other(error)))?
    }

    async fn finish_inner(
        &self,
        permit: IdempotencyPermit<O>,
        outcome: O,
        now: DateTime<Utc>,
    ) -> Result<(), InterfaceError> {
        self.ensure_writable(&permit.correlation_id)?;
        let mut permit = permit;
        permit.armed = false;
        let mut guard = self.state.lock().await;
        let state = &mut *guard;
        let Some(entries) = state.entries.get_mut(&permit.principal_id) else {
            return Err(conflict_error(permit.correlation_id.clone()));
        };
        let Some(index) = entries.iter().position(|entry| entry.key == permit.key) else {
            return Err(conflict_error(permit.correlation_id.clone()));
        };
        let mut entry = entries.remove(index);
        if entry.operation != permit.operation
            || entry.canonical_sha256 != permit.canonical_sha256
            || !matches!(
                &entry.state,
                EntryState::Reserved { generation, .. } if *generation == permit.generation
            )
        {
            entries.push(entry);
            return Err(conflict_error(permit.correlation_id.clone()));
        }
        let changed = match &entry.state {
            EntryState::Reserved { changed, .. } => changed.clone(),
            EntryState::Retained { .. } | EntryState::Unresolved => unreachable!(),
        };

        let releases = outcome.releases_reservation();
        let update = if releases {
            ReservationUpdate::Released
        } else {
            ReservationUpdate::Replay(outcome.clone())
        };
        if !releases {
            let safety_relevant = outcome.safety_relevant();
            state.sequence = state.sequence.wrapping_add(1);
            entry.last_used = state.sequence;
            entry.expires_at = (!safety_relevant).then_some(now + self.ttl);
            entry.state = EntryState::Retained {
                safety_relevant,
                outcome,
            };
            state
                .entries
                .entry(permit.principal_id.clone())
                .or_default()
                .push(entry);
        }
        state
            .dirty
            .insert((permit.principal_id.clone(), permit.key.clone()));
        remove_empty_buckets(state);
        if let Err(error) = self.persist_locked(state).await {
            let bucket = state
                .entries
                .entry(permit.principal_id.clone())
                .or_default();
            if let Some(item) = bucket.iter_mut().find(|item| item.key == permit.key) {
                item.state = EntryState::Unresolved;
                item.expires_at = None;
            } else {
                bucket.push(Entry {
                    key: permit.key.clone(),
                    operation: permit.operation,
                    canonical_sha256: permit.canonical_sha256,
                    state: EntryState::Unresolved,
                    expires_at: None,
                    last_used: permit.generation,
                });
            }
            changed.send_replace(ReservationUpdate::Unresolved);
            return Err(persistence_error(permit.correlation_id.clone(), error));
        }
        changed.send_replace(update);
        Ok(())
    }

    pub async fn abandon(&self, permit: IdempotencyPermit<O>) {
        let store = self.clone();
        if let Err(error) = tokio::spawn(async move { store.abandon_inner(permit).await }).await {
            tracing::error!(%error, "idempotency release task failed");
        }
    }

    async fn abandon_inner(&self, permit: IdempotencyPermit<O>) {
        let mut permit = permit;
        permit.armed = false;
        let mut guard = self.state.lock().await;
        let state = &mut *guard;
        abandon_entry(
            state,
            &permit.principal_id,
            &permit.key,
            permit.operation,
            &permit.canonical_sha256,
            permit.generation,
        );
        if let Err(error) = self.persist_locked(state).await {
            tracing::error!(%error, "idempotency release persistence failed");
            state
                .entries
                .entry(permit.principal_id.clone())
                .or_default()
                .push(Entry {
                    key: permit.key.clone(),
                    operation: permit.operation,
                    canonical_sha256: permit.canonical_sha256,
                    state: EntryState::Unresolved,
                    expires_at: None,
                    last_used: permit.generation,
                });
        }
    }
}

pub enum IdempotencyReservation<O: Send + Sync + 'static = CommandOutcome> {
    Acquired(IdempotencyPermit<O>),
    Replay(O),
}

impl<O: std::fmt::Debug + Send + Sync + 'static> std::fmt::Debug for IdempotencyReservation<O> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Acquired(_) => formatter.write_str("Acquired([REDACTED])"),
            Self::Replay(outcome) => formatter.debug_tuple("Replay").field(outcome).finish(),
        }
    }
}

pub struct IdempotencyPermit<O: Send + Sync + 'static = CommandOutcome> {
    principal_id: PrincipalId,
    key: IdempotencyKey,
    operation: InterfaceOperation,
    canonical_sha256: [u8; 32],
    generation: u64,
    correlation_id: CorrelationId,
    /// Back-reference so a dropped permit (cancelled or panicked request
    /// task) abandons its reservation instead of wedging the key forever.
    store: Arc<Mutex<StoreState<O>>>,
    durable: bool,
    armed: bool,
}

impl<O: Send + Sync + 'static> std::fmt::Debug for IdempotencyPermit<O> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("IdempotencyPermit([REDACTED])")
    }
}

/// Shared abandon body: remove a still-Reserved entry matching every permit
/// field and release its waiters. Used by `abandon` and by the permit's
/// `Drop`.
fn abandon_entry<O>(
    state: &mut StoreState<O>,
    principal_id: &PrincipalId,
    key: &IdempotencyKey,
    operation: InterfaceOperation,
    canonical_sha256: &[u8; 32],
    generation: u64,
) {
    let changed = state.entries.get_mut(principal_id).and_then(|entries| {
        let index = entries.iter().position(|entry| {
            entry.key == *key
                && entry.operation == operation
                && entry.canonical_sha256 == *canonical_sha256
                && matches!(
                    &entry.state,
                    EntryState::Reserved { generation: g, .. } if *g == generation
                )
        })?;
        let entry = entries.remove(index);
        match entry.state {
            EntryState::Reserved { changed, .. } => Some(changed),
            EntryState::Retained { .. } | EntryState::Unresolved => None,
        }
    });
    remove_empty_buckets(state);
    if let Some(changed) = changed {
        state.dirty.insert((principal_id.clone(), key.clone()));
        changed.send_replace(ReservationUpdate::Released);
    }
}

fn unresolve_entry<O>(
    state: &mut StoreState<O>,
    principal_id: &PrincipalId,
    key: &IdempotencyKey,
    operation: InterfaceOperation,
    canonical_sha256: &[u8; 32],
    generation: u64,
) {
    let changed = state.entries.get_mut(principal_id).and_then(|entries| {
        let entry = entries.iter_mut().find(|entry| {
            entry.key == *key
                && entry.operation == operation
                && entry.canonical_sha256 == *canonical_sha256
                && matches!(&entry.state, EntryState::Reserved { generation: g, .. } if *g == generation)
        })?;
        let changed = match &entry.state {
            EntryState::Reserved { changed, .. } => changed.clone(),
            _ => unreachable!(),
        };
        entry.state = EntryState::Unresolved;
        Some(changed)
    });
    if let Some(changed) = changed {
        state.dirty.insert((principal_id.clone(), key.clone()));
        changed.send_replace(ReservationUpdate::Unresolved);
    }
}

impl<O: Send + Sync + 'static> Drop for IdempotencyPermit<O> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        // Abandon inline when uncontended; spawn otherwise (dropping on the
        // executor while the store lock is held must not block the reactor).
        if let Ok(mut state) = self.store.try_lock() {
            if self.durable {
                unresolve_entry(
                    &mut state,
                    &self.principal_id,
                    &self.key,
                    self.operation,
                    &self.canonical_sha256,
                    self.generation,
                );
            } else {
                abandon_entry(
                    &mut state,
                    &self.principal_id,
                    &self.key,
                    self.operation,
                    &self.canonical_sha256,
                    self.generation,
                );
            }
            return;
        }
        let store = self.store.clone();
        let principal_id = self.principal_id.clone();
        let key = self.key.clone();
        let operation = self.operation;
        let canonical_sha256 = self.canonical_sha256;
        let generation = self.generation;
        let durable = self.durable;
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                let mut state = store.lock().await;
                if durable {
                    unresolve_entry(
                        &mut state,
                        &principal_id,
                        &key,
                        operation,
                        &canonical_sha256,
                        generation,
                    );
                } else {
                    abandon_entry(
                        &mut state,
                        &principal_id,
                        &key,
                        operation,
                        &canonical_sha256,
                        generation,
                    );
                }
            });
        }
    }
}

/// A digest that is stable across any reordering of JSON object keys.
///
/// This is the identity of an idempotent request: two submissions that mean the same thing
/// must produce the same digest, or a retry executes again instead of replaying the
/// retained result, duplicating the side effect of a boundary command.
///
/// Keys are sorted explicitly and recursively before hashing rather than relying on
/// `serde_json`'s default `BTreeMap` backing: any dependency can enable the
/// `preserve_order` feature, and Cargo feature unification then makes `Map` an
/// insertion-ordered `IndexMap` for the whole workspace.
pub fn canonical_sha256<T: Serialize>(value: &T) -> Result<[u8; 32], InterfaceError> {
    let value = serde_json::to_value(value).map_err(|_| canonicalization_error())?;
    let bytes = serde_json::to_vec(&canonicalize(value)).map_err(|_| canonicalization_error())?;
    Ok(Sha256::digest(bytes).into())
}

/// Recursively rewrites every object so its keys are in sorted order.
///
/// Rebuilding the map is what sorts iteration under `preserve_order`. Under the default
/// `BTreeMap` backing the rebuild is redundant and harmless.
fn canonicalize(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<(String, serde_json::Value)> = map
                .into_iter()
                .map(|(key, value)| (key, canonicalize(value)))
                .collect();
            entries.sort_by(|left, right| left.0.cmp(&right.0));
            serde_json::Value::Object(entries.into_iter().collect())
        }
        // Array order is part of the value; only the elements are canonicalized.
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(canonicalize).collect())
        }
        scalar => scalar,
    }
}

/// Digest of what a command *does*, ignoring which attempt is doing it.
///
/// `canonical_sha256` over a whole `CommandEnvelope` cannot be used for this:
/// `command_id`, `attempt_id`, and `deadline` are minted per attempt, so a
/// retry never matched its own first try and every retry took the conflict
/// branch instead of replaying. Over MCP that made idempotent retry
/// unreachable outright -- the gateway mints a fresh deadline on every
/// dispatch and the caller cannot pin one.
///
/// What stays in the identity is what changes the effect: which session and
/// page it runs against, the envelope schema, and the command itself.
/// `consent` is carried because a one-shot vision grant changes what the same
/// command may do -- a denial must not replay into an approved retry, nor the
/// reverse.
///
/// `workflow_id` is excluded for the same reason as the ids: the gateway mints
/// it per call (`workflow_id.unwrap_or_default()`) whenever the caller does not
/// pin one, so including it would leave retry broken for every agent that does
/// not thread a workflow by hand -- which is the common case and the one the
/// bug actually bites.
///
/// Reservations are already scoped to `(principal, key)`, so the key remains
/// the caller's own assertion of "this is the same request". Same key with a
/// different command still conflicts. That is the property worth keeping; only
/// the false conflicts go away.
pub fn command_identity_sha256(
    schema_version: u16,
    session_id: &impl Serialize,
    page_id: &impl Serialize,
    command: &impl Serialize,
    consent: bool,
) -> Result<[u8; 32], InterfaceError> {
    canonical_sha256(&serde_json::json!({
        "schemaVersion": schema_version,
        "sessionId": serde_json::to_value(session_id).map_err(|_| canonicalization_error())?,
        "pageId": serde_json::to_value(page_id).map_err(|_| canonicalization_error())?,
        "command": serde_json::to_value(command).map_err(|_| canonicalization_error())?,
        "consent": consent,
    }))
}

fn canonicalization_error() -> InterfaceError {
    InterfaceError {
        code: InterfaceErrorCode::InvalidRequest,
        layer: ErrorLayer::Interface,
        message: "request cannot be canonicalized".to_owned(),
        correlation_id: CorrelationId::new(),
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: false,
        required_capability: None,
    }
}

fn cleanup_expired<O>(state: &mut StoreState<O>, now: DateTime<Utc>) {
    for (principal, entries) in &mut state.entries {
        entries.retain(|entry| {
            let keep = entry.expires_at.is_none_or(|expires_at| expires_at > now);
            if !keep {
                state.dirty.insert((principal.clone(), entry.key.clone()));
            }
            keep
        });
    }
    remove_empty_buckets(state);
}

fn make_principal_room<O>(state: &mut StoreState<O>, principal: &PrincipalId, capacity: usize) {
    let Some(entries) = state.entries.get_mut(principal) else {
        return;
    };
    while entries.len() >= capacity {
        let Some(key) = remove_oldest_evictable(entries) else {
            break;
        };
        state.dirty.insert((principal.clone(), key));
    }
    remove_empty_buckets(state);
}

fn make_global_room<O>(state: &mut StoreState<O>, capacity: usize) {
    while entry_count(state) >= capacity {
        let candidate = state
            .entries
            .iter()
            .flat_map(|(principal, entries)| {
                entries
                    .iter()
                    .enumerate()
                    .filter(|(_, entry)| entry_is_evictable(entry))
                    .map(move |(index, entry)| (entry.last_used, principal.clone(), index))
            })
            .min_by_key(|(last_used, _, _)| *last_used);
        let Some((_, principal, index)) = candidate else {
            break;
        };
        if let Some(entries) = state.entries.get_mut(&principal) {
            let entry = entries.remove(index);
            state.dirty.insert((principal.clone(), entry.key));
        }
        remove_empty_buckets(state);
    }
}

fn remove_oldest_evictable<O>(entries: &mut Vec<Entry<O>>) -> Option<IdempotencyKey> {
    let candidate = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry_is_evictable(entry))
        .min_by_key(|(_, entry)| entry.last_used)
        .map(|(index, _)| index);
    candidate.map(|index| entries.remove(index).key)
}

fn entry_is_evictable<O>(entry: &Entry<O>) -> bool {
    matches!(
        &entry.state,
        EntryState::Retained {
            safety_relevant: false,
            ..
        }
    )
}

fn entry_count<O>(state: &StoreState<O>) -> usize {
    state.entries.values().map(Vec::len).sum()
}

fn remove_empty_buckets<O>(state: &mut StoreState<O>) {
    state.entries.retain(|_, entries| !entries.is_empty());
}

fn outcome_releases(outcome: &CommandOutcome) -> bool {
    matches!(
        outcome,
        CommandOutcome::RetryableFailure { .. }
            | CommandOutcome::ResourceExhausted { .. }
            | CommandOutcome::PolicyDenied { .. }
            | CommandOutcome::Failed {
                error: types::CommandError {
                    retryable: true,
                    ..
                },
                ..
            }
    )
}

fn conflict_error(correlation_id: CorrelationId) -> InterfaceError {
    InterfaceError {
        code: InterfaceErrorCode::IdempotencyConflict,
        layer: ErrorLayer::Interface,
        message: "idempotency key conflicts with a retained request".to_owned(),
        correlation_id,
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: false,
        required_capability: None,
    }
}

fn unresolved_error(correlation_id: CorrelationId) -> InterfaceError {
    InterfaceError {
        code: InterfaceErrorCode::IdempotencyConflict,
        layer: ErrorLayer::Interface,
        message:
            "idempotency outcome is unresolved; inspect the authoritative operation before retrying"
                .to_owned(),
        correlation_id,
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: true,
        required_capability: None,
    }
}

fn persistence_error(correlation_id: CorrelationId, error: io::Error) -> InterfaceError {
    tracing::error!(%error, "idempotency persistence failed");
    InterfaceError {
        code: InterfaceErrorCode::Internal,
        layer: ErrorLayer::Interface,
        message: "idempotency state could not be durably recorded".to_owned(),
        correlation_id,
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: true,
        required_capability: None,
    }
}

fn deadline_error(correlation_id: CorrelationId) -> InterfaceError {
    InterfaceError {
        code: InterfaceErrorCode::DeadlineExceeded,
        layer: ErrorLayer::Interface,
        message: "request deadline exceeded while awaiting idempotent result".to_owned(),
        correlation_id,
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: false,
        required_capability: None,
    }
}

fn resource_exhausted_error(correlation_id: CorrelationId) -> InterfaceError {
    InterfaceError {
        code: InterfaceErrorCode::ResourceExhausted,
        layer: ErrorLayer::Interface,
        message: "idempotency capacity exhausted".to_owned(),
        correlation_id,
        command_id: None,
        retryable: true,
        retry_after_ms: Some(1000),
        reconciliation_required: false,
        required_capability: None,
    }
}
