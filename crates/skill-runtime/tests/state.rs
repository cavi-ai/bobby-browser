use std::collections::BTreeMap;

use chrono::{Duration, Utc};
use skill_runtime::{
    SkillBrowserEngine, SkillCapability, SkillEvidenceRef, SkillProfile, SkillSessionState,
    SkillStateStore, SkillStateStoreError, SkillTactic,
};
use types::SessionId;

fn state() -> SkillSessionState {
    SkillSessionState::new(
        SessionId::new(),
        BTreeMap::from([("SkillGhost".into(), "1.0.0".into())]),
        None,
        None,
        None,
        None,
        None,
        Vec::new(),
        Vec::new(),
        Utc::now() + Duration::minutes(5),
    )
    .unwrap()
}

#[test]
fn cancelled_transition_never_partially_mutates_a_session() {
    let store = SkillStateStore::new();
    let initial = state();
    let session_id = initial.session_id.clone();
    store.insert(initial).unwrap();

    let outcome = store.transition(&session_id, |next| {
        next.attempted_tactics.push(SkillTactic::ObserveAgain);
        Err(SkillStateStoreError::Cancelled)
    });

    assert!(matches!(outcome, Err(SkillStateStoreError::Cancelled)));
    assert!(store.get(&session_id).unwrap().attempted_tactics.is_empty());
}

#[test]
fn second_insert_is_rejected_without_replacing_frozen_session_state() {
    let store = SkillStateStore::new();
    let initial = state();
    let session_id = initial.session_id.clone();
    store.insert(initial).unwrap();
    let profile = SkillProfile::new(
        "1.0.0",
        SkillBrowserEngine::Firefox,
        [SkillCapability::Locale],
        "profile-digest-a",
    )
    .unwrap();
    store.freeze_profile(&session_id, profile).unwrap();
    store
        .record_tactic(&session_id, SkillTactic::ObserveAgain)
        .unwrap();
    let before = store.get(&session_id).unwrap();

    let mut replacement = state();
    replacement.session_id = session_id.clone();
    assert!(matches!(
        store.insert(replacement),
        Err(SkillStateStoreError::DuplicateSession)
    ));
    assert_eq!(store.get(&session_id).unwrap(), before);
}

#[test]
fn transition_cannot_replace_the_durable_session_identity() {
    let store = SkillStateStore::new();
    let state = state();
    let session_id = state.session_id.clone();
    store.insert(state).unwrap();

    assert!(matches!(
        store.transition(&session_id, |next| {
            next.session_id = SessionId::new();
            Ok(())
        }),
        Err(SkillStateStoreError::InvalidState(_))
    ));
    assert_eq!(store.get(&session_id).unwrap().session_id, session_id);
}

#[test]
fn tactic_history_is_bounded_without_corrupting_existing_state() {
    let store = SkillStateStore::new();
    let state = state();
    let session_id = state.session_id.clone();
    store.insert(state).unwrap();

    for _ in 0..32 {
        store
            .record_tactic(&session_id, SkillTactic::ObserveAgain)
            .unwrap();
    }
    assert!(matches!(
        store.record_tactic(&session_id, SkillTactic::ObserveAgain),
        Err(SkillStateStoreError::InvalidState(_))
    ));
    assert_eq!(store.get(&session_id).unwrap().attempted_tactics.len(), 32);
}

#[test]
fn profile_is_frozen_after_first_successful_assignment() {
    let store = SkillStateStore::new();
    let state = state();
    let session_id = state.session_id.clone();
    store.insert(state).unwrap();

    let first = SkillProfile::new(
        "1.0.0",
        SkillBrowserEngine::Firefox,
        [SkillCapability::Locale],
        "profile-digest-a",
    )
    .unwrap();
    let conflicting = SkillProfile::new(
        "1.0.1",
        SkillBrowserEngine::Chromium,
        [SkillCapability::Locale],
        "profile-digest-b",
    )
    .unwrap();

    store.freeze_profile(&session_id, first.clone()).unwrap();
    assert!(matches!(
        store.freeze_profile(&session_id, conflicting),
        Err(SkillStateStoreError::ProfileFrozen)
    ));
    assert_eq!(
        store.get(&session_id).unwrap().effective_profile,
        Some(first)
    );
}

#[test]
fn secret_bearing_evidence_is_rejected_before_persistence() {
    let store = SkillStateStore::new();
    let mut state = state();
    state.evidence.push(SkillEvidenceRef {
        artifact_id: "authorization-token".into(),
        sha256: "0".repeat(64),
    });

    assert!(matches!(
        store.insert(state),
        Err(SkillStateStoreError::InvalidState(_))
    ));
}
