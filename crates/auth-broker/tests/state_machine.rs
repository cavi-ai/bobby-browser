use std::sync::Arc;

use async_trait::async_trait;
use auth_broker::{
    AuthCapabilities, AuthDriver, AuthError, AuthInput, AuthProfileId, AuthProgress, AuthStatus,
    AuthStrategy, CredentialHandle,
};

struct FakeDriver;

#[async_trait]
impl AuthDriver for FakeDriver {
    async fn discover(&self, _profile: &AuthProfileId) -> Result<AuthCapabilities, AuthError> {
        Ok(AuthCapabilities::new([AuthStrategy::Advertised]))
    }

    async fn begin(
        &self,
        profile: &AuthProfileId,
        strategy: AuthStrategy,
    ) -> Result<AuthProgress, AuthError> {
        Ok(AuthProgress::Authenticated(CredentialHandle::new(
            profile.clone(),
            strategy,
            Arc::new(()),
        )))
    }

    async fn continue_auth(
        &self,
        _challenge: &auth_broker::AuthChallenge,
        _input: AuthInput,
    ) -> Result<AuthProgress, AuthError> {
        Err(AuthError::InvalidTransition)
    }

    async fn refresh(&self, handle: &CredentialHandle) -> Result<AuthProgress, AuthError> {
        Ok(AuthProgress::Authenticated(handle.clone()))
    }

    async fn revoke(&self, _handle: CredentialHandle) -> Result<(), AuthError> {
        Ok(())
    }

    async fn health(&self, _handle: &CredentialHandle) -> AuthStatus {
        AuthStatus::Healthy
    }
}

#[tokio::test]
async fn advertised_auth_returns_an_opaque_redacted_handle() {
    let driver = FakeDriver;
    let profile = AuthProfileId::new("codex").unwrap();
    let progress = driver
        .begin(&profile, AuthStrategy::Advertised)
        .await
        .unwrap();
    let AuthProgress::Authenticated(handle) = progress else {
        panic!("expected authenticated");
    };
    assert_eq!(handle.profile(), &profile);
    assert_eq!(handle.strategy(), AuthStrategy::Advertised);
    assert_eq!(format!("{handle:?}"), "CredentialHandle(REDACTED)");
}

#[test]
fn profile_ids_are_bounded_and_reject_control_characters() {
    assert!(AuthProfileId::new("").is_err());
    assert!(AuthProfileId::new("x".repeat(65)).is_err());
    assert!(AuthProfileId::new("bad\nprofile").is_err());
    assert_eq!(
        AuthProfileId::new("claude-oauth").unwrap().as_str(),
        "claude-oauth"
    );
}
