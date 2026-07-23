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
            },
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
