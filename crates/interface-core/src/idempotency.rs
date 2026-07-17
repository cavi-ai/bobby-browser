use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Mutex},
};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use sha2::{Digest, Sha256};
use types::{
    CommandOutcome, CorrelationId, ErrorLayer, IdempotencyKey, InterfaceError, InterfaceErrorCode,
    InterfaceOperation, PrincipalId,
};

#[derive(Clone)]
struct Entry {
    key: IdempotencyKey,
    operation: InterfaceOperation,
    canonical_sha256: [u8; 32],
    committed_response: CommandOutcome,
    expires_at: DateTime<Utc>,
}

#[derive(Clone)]
pub struct IdempotencyStore {
    per_principal_capacity: usize,
    ttl: Duration,
    entries: Arc<Mutex<HashMap<PrincipalId, VecDeque<Entry>>>>,
}

impl std::fmt::Debug for IdempotencyStore {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("IdempotencyStore")
            .field("per_principal_capacity", &self.per_principal_capacity)
            .field("ttl", &self.ttl)
            .finish_non_exhaustive()
    }
}

impl Default for IdempotencyStore {
    fn default() -> Self {
        Self::new(256, Duration::minutes(15))
    }
}

impl IdempotencyStore {
    pub fn new(per_principal_capacity: usize, ttl: Duration) -> Self {
        Self {
            per_principal_capacity,
            ttl,
            entries: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn record_committed_outcome(
        &self,
        principal_id: PrincipalId,
        key: IdempotencyKey,
        operation: InterfaceOperation,
        canonical_sha256: [u8; 32],
        outcome: CommandOutcome,
        now: DateTime<Utc>,
    ) -> Result<(), InterfaceError> {
        if self.per_principal_capacity == 0 || !matches!(outcome, CommandOutcome::Completed { .. })
        {
            return Ok(());
        }
        let mut all_entries = self.entries.lock().expect("idempotency mutex poisoned");
        let entries = all_entries.entry(principal_id).or_default();
        entries.retain(|entry| entry.expires_at > now);
        if let Some(index) = entries.iter().position(|entry| entry.key == key) {
            if entries[index].operation != operation
                || entries[index].canonical_sha256 != canonical_sha256
            {
                return Err(conflict_error(CorrelationId::new()));
            }
            return Ok(());
        }
        while entries.len() >= self.per_principal_capacity {
            entries.pop_front();
        }
        entries.push_back(Entry {
            key,
            operation,
            canonical_sha256,
            committed_response: outcome,
            expires_at: now + self.ttl,
        });
        Ok(())
    }

    pub fn lookup_outcome(
        &self,
        principal_id: &PrincipalId,
        key: &IdempotencyKey,
        operation: InterfaceOperation,
        canonical_sha256: [u8; 32],
        now: DateTime<Utc>,
        correlation_id: CorrelationId,
    ) -> Result<Option<CommandOutcome>, InterfaceError> {
        let mut all_entries = self.entries.lock().expect("idempotency mutex poisoned");
        let Some(entries) = all_entries.get_mut(principal_id) else {
            return Ok(None);
        };
        entries.retain(|entry| entry.expires_at > now);
        let Some(index) = entries.iter().position(|entry| &entry.key == key) else {
            return Ok(None);
        };
        if entries[index].operation != operation
            || entries[index].canonical_sha256 != canonical_sha256
        {
            return Err(conflict_error(correlation_id));
        }
        let entry = entries.remove(index).expect("entry position was present");
        let response = entry.committed_response.clone();
        entries.push_back(entry);
        Ok(Some(response))
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

fn conflict_error(correlation_id: CorrelationId) -> InterfaceError {
    InterfaceError {
        code: InterfaceErrorCode::IdempotencyConflict,
        layer: ErrorLayer::Interface,
        message: "idempotency key conflicts with a committed request".to_owned(),
        correlation_id,
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: false,
        required_capability: None,
    }
}
