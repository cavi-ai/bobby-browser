use std::{
    collections::HashMap,
    sync::{Arc, RwLock},
};

use types::{PrincipalId, SessionId};

pub trait SessionOwnershipAuthority: Send + Sync {
    fn owns_session(&self, principal: &PrincipalId, session: &SessionId) -> bool;
}

#[derive(Clone)]
pub struct SessionOwnershipRegistry {
    inner: Arc<RegistryInner>,
}

#[derive(Clone)]
pub struct SessionOwnershipRecorder {
    inner: Arc<RegistryInner>,
}

struct RegistryInner {
    capacity: usize,
    state: RwLock<RegistryState>,
}

struct RegistryState {
    owners: HashMap<SessionId, PrincipalId>,
    reservations: HashMap<u64, PrincipalId>,
    next_reservation_id: u64,
    fail_next_finalize: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOwnershipRecordError {
    CapacityExhausted,
    OwnershipConflict,
    FinalizeFailed,
    Poisoned,
}

pub struct SessionOwnershipReservation {
    inner: Arc<RegistryInner>,
    reservation_id: u64,
    active: bool,
}

impl SessionOwnershipRegistry {
    pub fn bounded(capacity: usize) -> (Arc<Self>, SessionOwnershipRecorder) {
        assert!(capacity > 0, "session ownership capacity must be positive");
        let inner = Arc::new(RegistryInner {
            capacity,
            state: RwLock::new(RegistryState {
                owners: HashMap::with_capacity(capacity),
                reservations: HashMap::with_capacity(capacity),
                next_reservation_id: 1,
                fail_next_finalize: false,
            }),
        });
        (
            Arc::new(Self {
                inner: inner.clone(),
            }),
            SessionOwnershipRecorder { inner },
        )
    }
}

impl SessionOwnershipAuthority for SessionOwnershipRegistry {
    fn owns_session(&self, principal: &PrincipalId, session: &SessionId) -> bool {
        self.inner
            .state
            .read()
            .ok()
            .and_then(|state| state.owners.get(session).cloned())
            .is_some_and(|owner| owner == *principal)
    }
}

impl SessionOwnershipRecorder {
    pub fn reserve(
        &self,
        principal: PrincipalId,
    ) -> Result<SessionOwnershipReservation, SessionOwnershipRecordError> {
        let mut state = self
            .inner
            .state
            .write()
            .map_err(|_| SessionOwnershipRecordError::Poisoned)?;
        if state.owners.len() + state.reservations.len() >= self.inner.capacity {
            return Err(SessionOwnershipRecordError::CapacityExhausted);
        }
        let reservation_id = state.next_reservation_id;
        state.next_reservation_id = state
            .next_reservation_id
            .checked_add(1)
            .ok_or(SessionOwnershipRecordError::CapacityExhausted)?;
        state.reservations.insert(reservation_id, principal);
        Ok(SessionOwnershipReservation {
            inner: self.inner.clone(),
            reservation_id,
            active: true,
        })
    }

    /// Records only the result of a session creation that already passed the
    /// authenticated runtime boundary. Holding this recorder is trusted authority;
    /// artifact readers receive only the separate read-only trait object.
    pub fn record_authenticated_session(
        &self,
        principal: PrincipalId,
        session: SessionId,
    ) -> Result<(), SessionOwnershipRecordError> {
        let mut owners = self
            .inner
            .state
            .write()
            .map_err(|_| SessionOwnershipRecordError::Poisoned)?;
        if let Some(owner) = owners.owners.get(&session) {
            return if *owner == principal {
                Ok(())
            } else {
                Err(SessionOwnershipRecordError::OwnershipConflict)
            };
        }
        if owners.owners.len() + owners.reservations.len() >= self.inner.capacity {
            return Err(SessionOwnershipRecordError::CapacityExhausted);
        }
        owners.owners.insert(session, principal);
        Ok(())
    }

    #[doc(hidden)]
    pub fn inject_finalize_failure_once_for_test(&self) {
        if let Ok(mut state) = self.inner.state.write() {
            state.fail_next_finalize = true;
        }
    }
}

impl SessionOwnershipReservation {
    pub fn finalize(mut self, session: SessionId) -> Result<(), SessionOwnershipRecordError> {
        let mut state = self
            .inner
            .state
            .write()
            .map_err(|_| SessionOwnershipRecordError::Poisoned)?;
        let principal = state
            .reservations
            .remove(&self.reservation_id)
            .ok_or(SessionOwnershipRecordError::FinalizeFailed)?;
        self.active = false;
        if state.fail_next_finalize {
            state.fail_next_finalize = false;
            return Err(SessionOwnershipRecordError::FinalizeFailed);
        }
        if let Some(owner) = state.owners.get(&session) {
            return if *owner == principal {
                Ok(())
            } else {
                Err(SessionOwnershipRecordError::OwnershipConflict)
            };
        }
        state.owners.insert(session, principal);
        Ok(())
    }
}

impl Drop for SessionOwnershipReservation {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Ok(mut state) = self.inner.state.write() {
            state.reservations.remove(&self.reservation_id);
        }
        self.active = false;
    }
}
