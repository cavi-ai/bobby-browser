use std::{collections::HashMap, sync::Arc};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use tokio::sync::{watch, Mutex};
use types::{
    CommandOutcome, CorrelationId, ErrorLayer, IdempotencyKey, InterfaceError, InterfaceErrorCode,
    InterfaceOperation, PrincipalId,
};

struct Entry {
    key: IdempotencyKey,
    operation: InterfaceOperation,
    canonical_sha256: [u8; 32],
    state: EntryState,
    expires_at: Option<DateTime<Utc>>,
    last_used: u64,
}

enum EntryState {
    Reserved {
        generation: u64,
        changed: watch::Sender<ReservationUpdate>,
    },
    Retained {
        outcome: CommandOutcome,
        safety_relevant: bool,
    },
}

#[derive(Clone)]
enum ReservationUpdate {
    Pending,
    Replay(CommandOutcome),
    Released,
}

#[derive(Default)]
struct StoreState {
    entries: HashMap<PrincipalId, Vec<Entry>>,
    sequence: u64,
}

#[derive(Clone)]
pub struct IdempotencyStore {
    per_principal_capacity: usize,
    global_capacity: usize,
    ttl: Duration,
    state: Arc<Mutex<StoreState>>,
}

impl std::fmt::Debug for IdempotencyStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IdempotencyStore")
            .field("per_principal_capacity", &self.per_principal_capacity)
            .field("global_capacity", &self.global_capacity)
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl Default for IdempotencyStore {
    fn default() -> Self {
        Self::with_global_capacity(256, 4096, Duration::minutes(15))
    }
}

impl IdempotencyStore {
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
    ) -> Result<IdempotencyReservation, InterfaceError> {
        loop {
            let wait = {
                let mut state = self.state.lock().await;
                cleanup_expired(&mut state, now);
                state.sequence = state.sequence.wrapping_add(1);
                let last_used = state.sequence;

                if let Some(entries) = state.entries.get_mut(&principal_id) {
                    if let Some(index) = entries.iter().position(|entry| entry.key == key) {
                        let mut entry = entries.remove(index);
                        if entry.operation != operation
                            || entry.canonical_sha256 != canonical_sha256
                        {
                            entries.push(entry);
                            return Err(conflict_error(correlation_id));
                        }
                        entry.last_used = last_used;
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
                    ReservationUpdate::Released | ReservationUpdate::Pending => {}
                }
                now = Utc::now();
                continue;
            }

            let mut state = self.state.lock().await;
            cleanup_expired(&mut state, now);
            if state
                .entries
                .get(&principal_id)
                .is_some_and(|entries| entries.iter().any(|entry| entry.key == key))
            {
                continue;
            }
            make_principal_room(&mut state, &principal_id, self.per_principal_capacity);
            make_global_room(&mut state, self.global_capacity);
            let principal_len = state.entries.get(&principal_id).map_or(0, Vec::len);
            if self.per_principal_capacity == 0
                || self.global_capacity == 0
                || principal_len >= self.per_principal_capacity
                || entry_count(&state) >= self.global_capacity
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
            return Ok(IdempotencyReservation::Acquired(IdempotencyPermit {
                principal_id,
                key,
                operation,
                canonical_sha256,
                generation,
                correlation_id,
            }));
        }
    }

    pub async fn finish(
        &self,
        permit: IdempotencyPermit,
        outcome: CommandOutcome,
        now: DateTime<Utc>,
    ) -> Result<(), InterfaceError> {
        let mut state = self.state.lock().await;
        let Some(entries) = state.entries.get_mut(&permit.principal_id) else {
            return Err(conflict_error(permit.correlation_id));
        };
        let Some(index) = entries.iter().position(|entry| entry.key == permit.key) else {
            return Err(conflict_error(permit.correlation_id));
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
            return Err(conflict_error(permit.correlation_id));
        }
        let changed = match &entry.state {
            EntryState::Reserved { changed, .. } => changed.clone(),
            EntryState::Retained { .. } => unreachable!(),
        };

        let releases = outcome_releases(&outcome);
        let update = if releases {
            ReservationUpdate::Released
        } else {
            ReservationUpdate::Replay(outcome.clone())
        };
        if !releases {
            state.sequence = state.sequence.wrapping_add(1);
            entry.last_used = state.sequence;
            entry.expires_at = Some(now + self.ttl);
            entry.state = EntryState::Retained {
                safety_relevant: !matches!(outcome, CommandOutcome::Completed { .. }),
                outcome,
            };
            state
                .entries
                .entry(permit.principal_id.clone())
                .or_default()
                .push(entry);
        }
        remove_empty_buckets(&mut state);
        changed.send_replace(update);
        Ok(())
    }
}

pub enum IdempotencyReservation {
    Acquired(IdempotencyPermit),
    Replay(CommandOutcome),
}

impl std::fmt::Debug for IdempotencyReservation {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Acquired(_) => formatter.write_str("Acquired([REDACTED])"),
            Self::Replay(outcome) => formatter.debug_tuple("Replay").field(outcome).finish(),
        }
    }
}

pub struct IdempotencyPermit {
    principal_id: PrincipalId,
    key: IdempotencyKey,
    operation: InterfaceOperation,
    canonical_sha256: [u8; 32],
    generation: u64,
    correlation_id: CorrelationId,
}

impl std::fmt::Debug for IdempotencyPermit {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("IdempotencyPermit([REDACTED])")
    }
}

pub fn canonical_sha256<T: Serialize>(value: &T) -> Result<[u8; 32], InterfaceError> {
    let bytes = serde_json::to_vec(value).map_err(|_| InterfaceError {
        code: InterfaceErrorCode::InvalidRequest,
        layer: ErrorLayer::Interface,
        message: "request cannot be canonicalized".to_owned(),
        correlation_id: CorrelationId::new(),
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: false,
        required_capability: None,
    })?;
    Ok(Sha256::digest(bytes).into())
}

fn cleanup_expired(state: &mut StoreState, now: DateTime<Utc>) {
    for entries in state.entries.values_mut() {
        entries.retain(|entry| entry.expires_at.is_none_or(|expires_at| expires_at > now));
    }
    remove_empty_buckets(state);
}

fn make_principal_room(state: &mut StoreState, principal: &PrincipalId, capacity: usize) {
    let Some(entries) = state.entries.get_mut(principal) else {
        return;
    };
    while entries.len() >= capacity && remove_oldest_evictable(entries) {}
    remove_empty_buckets(state);
}

fn make_global_room(state: &mut StoreState, capacity: usize) {
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
            entries.remove(index);
        }
        remove_empty_buckets(state);
    }
}

fn remove_oldest_evictable(entries: &mut Vec<Entry>) -> bool {
    let candidate = entries
        .iter()
        .enumerate()
        .filter(|(_, entry)| entry_is_evictable(entry))
        .min_by_key(|(_, entry)| entry.last_used)
        .map(|(index, _)| index);
    if let Some(index) = candidate {
        entries.remove(index);
        true
    } else {
        false
    }
}

fn entry_is_evictable(entry: &Entry) -> bool {
    matches!(
        &entry.state,
        EntryState::Retained {
            safety_relevant: false,
            ..
        }
    )
}

fn entry_count(state: &StoreState) -> usize {
    state.entries.values().map(Vec::len).sum()
}

fn remove_empty_buckets(state: &mut StoreState) {
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
