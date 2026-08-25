//! F4: `SessionManager::create` must store the caller-supplied `ExecutionPolicy` on the
//! `SessionState` rather than always defaulting to deny. Deny-by-default is still the
//! invariant for requests that omit the field (covered at the type layer in
//! `types/tests/contracts.rs`); this file proves the session-manager plumbing itself
//! carries an explicit grant through, and does not silently discard it.

use session_manager::SessionManager;
use types::{CreateSessionRequest, ExecutionPolicy};

#[tokio::test]
async fn create_without_workers_stores_default_deny_policy() {
    let manager = SessionManager::default();
    let session = manager
        .create(CreateSessionRequest {
            profile: "default".into(),
            proxy: None,
            execution_policy: ExecutionPolicy::default(),
            zigzagzig: false,
        })
        .await
        .unwrap();

    assert!(!session.execution_policy.javascript_evaluation);
    assert_eq!(
        manager.get(&session.id).await.unwrap().execution_policy,
        ExecutionPolicy::default()
    );
}

#[tokio::test]
async fn create_with_explicit_javascript_grant_stores_it_on_the_session() {
    let manager = SessionManager::default();
    let session = manager
        .create(CreateSessionRequest {
            profile: "default".into(),
            proxy: None,
            execution_policy: ExecutionPolicy {
                javascript_evaluation: true,
                ..ExecutionPolicy::default()
            },
            zigzagzig: false,
        })
        .await
        .unwrap();

    assert!(session.execution_policy.javascript_evaluation);
    assert!(
        manager
            .get(&session.id)
            .await
            .unwrap()
            .execution_policy
            .javascript_evaluation
    );
}

#[tokio::test]
async fn zigzagzig_session_forces_every_capability_on_and_records_the_flag() {
    let manager = SessionManager::default();
    let session = manager
        .create(CreateSessionRequest {
            profile: "godmode".into(),
            proxy: None,
            // The caller passes an all-deny policy; godmode overrides it —
            // the ladder escalates into vision solving, so the session must
            // be allowed to use what the ladder reaches for.
            execution_policy: ExecutionPolicy::default(),
            zigzagzig: true,
        })
        .await
        .unwrap();

    assert!(session.zigzagzig);
    assert!(session.execution_policy.javascript_evaluation);
    assert!(session.execution_policy.vision_assist);
    assert!(session.execution_policy.fingerprint);
    assert!(session.execution_policy.humanize);

    let stored = manager.get(&session.id).await.unwrap();
    assert!(stored.zigzagzig);
    assert!(stored.execution_policy.humanize);
}

#[tokio::test]
async fn non_zigzagzig_session_records_the_flag_as_off() {
    let manager = SessionManager::default();
    let session = manager
        .create(CreateSessionRequest {
            profile: "default".into(),
            proxy: None,
            execution_policy: ExecutionPolicy::default(),
            zigzagzig: false,
        })
        .await
        .unwrap();

    assert!(!session.zigzagzig);
    assert_eq!(session.execution_policy, ExecutionPolicy::default());
}
