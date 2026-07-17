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
    owners: RwLock<HashMap<SessionId, PrincipalId>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionOwnershipRecordError {
    CapacityExhausted,
    OwnershipConflict,
    Poisoned,
}

impl SessionOwnershipRegistry {
    pub fn bounded(capacity: usize) -> (Arc<Self>, SessionOwnershipRecorder) {
        assert!(capacity > 0, "session ownership capacity must be positive");
        let inner = Arc::new(RegistryInner {
            capacity,
            owners: RwLock::new(HashMap::with_capacity(capacity)),
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
            .owners
            .read()
            .ok()
            .and_then(|owners| owners.get(session).cloned())
            .is_some_and(|owner| owner == *principal)
    }
}

impl SessionOwnershipRecorder {
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
            .owners
            .write()
            .map_err(|_| SessionOwnershipRecordError::Poisoned)?;
        if let Some(owner) = owners.get(&session) {
            return if *owner == principal {
                Ok(())
            } else {
                Err(SessionOwnershipRecordError::OwnershipConflict)
            };
        }
        if owners.len() >= self.inner.capacity {
            return Err(SessionOwnershipRecordError::CapacityExhausted);
        }
        owners.insert(session, principal);
        Ok(())
    }
}
