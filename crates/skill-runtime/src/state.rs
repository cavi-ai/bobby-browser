use std::collections::HashMap;
#[cfg(feature = "test-support")]
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use thiserror::Error;
use types::{RecoveryReceipt, SessionId, SkillProfile, SkillSessionState, SkillTactic};

use crate::zigzagzig::settle_committed_receipt_state;

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SkillStateStoreError {
    #[error("skill session was not found")]
    SessionNotFound,
    #[error("skill session already exists")]
    DuplicateSession,
    #[error("skill session transition was cancelled")]
    Cancelled,
    #[error("skill session state is invalid: {0}")]
    InvalidState(String),
    #[error("effective skill profile is frozen for this session")]
    ProfileFrozen,
    #[error("skill session state lock was poisoned")]
    Poisoned,
}

pub struct SkillStateStore {
    sessions: RwLock<HashMap<SessionId, SkillSessionState>>,
    #[cfg(feature = "test-support")]
    injected_transition_failures: AtomicUsize,
}

impl Default for SkillStateStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillStateStore {
    pub fn new() -> Self {
        Self {
            sessions: RwLock::new(HashMap::new()),
            #[cfg(feature = "test-support")]
            injected_transition_failures: AtomicUsize::new(0),
        }
    }

    pub fn insert(&self, state: SkillSessionState) -> Result<(), SkillStateStoreError> {
        validate(&state)?;
        let mut sessions = self.write_sessions()?;
        if sessions.contains_key(&state.session_id) {
            return Err(SkillStateStoreError::DuplicateSession);
        }
        sessions.insert(state.session_id.clone(), state);
        Ok(())
    }

    pub fn get(&self, session_id: &SessionId) -> Result<SkillSessionState, SkillStateStoreError> {
        self.read_sessions()?
            .get(session_id)
            .cloned()
            .ok_or(SkillStateStoreError::SessionNotFound)
    }

    pub fn transition<F>(
        &self,
        session_id: &SessionId,
        transition: F,
    ) -> Result<(), SkillStateStoreError>
    where
        F: FnOnce(&mut SkillSessionState) -> Result<(), SkillStateStoreError>,
    {
        #[cfg(feature = "test-support")]
        {
            if self
                .injected_transition_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                    count.checked_sub(1)
                })
                .is_ok()
            {
                return Err(SkillStateStoreError::Cancelled);
            }
        }
        let mut sessions = self.write_sessions()?;
        let current = sessions
            .get(session_id)
            .cloned()
            .ok_or(SkillStateStoreError::SessionNotFound)?;
        let durable_session_id = current.session_id.clone();
        let mut next = current;
        transition(&mut next)?;
        if next.session_id != durable_session_id {
            return Err(SkillStateStoreError::InvalidState(
                "session identity cannot change during a transition".into(),
            ));
        }
        validate(&next)?;
        sessions.insert(session_id.clone(), next);
        Ok(())
    }

    #[doc(hidden)]
    #[cfg(feature = "test-support")]
    pub fn inject_transition_failures(&self, count: usize) {
        self.injected_transition_failures
            .store(count, Ordering::SeqCst);
    }

    pub fn record_tactic(
        &self,
        session_id: &SessionId,
        tactic: SkillTactic,
    ) -> Result<(), SkillStateStoreError> {
        self.transition(session_id, |state| {
            state.attempted_tactics.push(tactic);
            Ok(())
        })
    }

    pub fn settle_committed_receipt(
        &self,
        receipt: &RecoveryReceipt,
    ) -> Result<(), SkillStateStoreError> {
        #[cfg(feature = "test-support")]
        {
            if self
                .injected_transition_failures
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                    count.checked_sub(1)
                })
                .is_ok()
            {
                return Err(SkillStateStoreError::Cancelled);
            }
        }
        let mut sessions = self.write_sessions()?;
        let current = sessions
            .get(&receipt.identity.session_id)
            .cloned()
            .ok_or(SkillStateStoreError::SessionNotFound)?;
        let mut settled = current;
        settle_committed_receipt_state(&mut settled, receipt)
            .map_err(|error| SkillStateStoreError::InvalidState(format!("{error:?}")))?;
        sessions.insert(receipt.identity.session_id.clone(), settled);
        Ok(())
    }

    pub fn freeze_profile(
        &self,
        session_id: &SessionId,
        profile: SkillProfile,
    ) -> Result<(), SkillStateStoreError> {
        self.transition(session_id, |state| match &state.effective_profile {
            None => {
                state.effective_profile = Some(profile);
                Ok(())
            }
            Some(existing) if existing == &profile => Ok(()),
            Some(_) => Err(SkillStateStoreError::ProfileFrozen),
        })
    }

    fn read_sessions(
        &self,
    ) -> Result<RwLockReadGuard<'_, HashMap<SessionId, SkillSessionState>>, SkillStateStoreError>
    {
        self.sessions
            .read()
            .map_err(|_| SkillStateStoreError::Poisoned)
    }

    fn write_sessions(
        &self,
    ) -> Result<RwLockWriteGuard<'_, HashMap<SessionId, SkillSessionState>>, SkillStateStoreError>
    {
        self.sessions
            .write()
            .map_err(|_| SkillStateStoreError::Poisoned)
    }
}

fn validate(state: &SkillSessionState) -> Result<(), SkillStateStoreError> {
    serde_json::to_vec(state)
        .map(|_| ())
        .map_err(|error| SkillStateStoreError::InvalidState(error.to_string()))
}
