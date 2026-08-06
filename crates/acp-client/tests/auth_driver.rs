use std::time::Duration;

use acp_client::AcpAuthDriver;
use auth_broker::{AuthDriver, AuthError, AuthProfileId, AuthProgress, AuthStatus, AuthStrategy};

fn driver(mode: &str) -> AcpAuthDriver {
    let log = tempfile::tempdir()
        .expect("tempdir")
        .keep()
        .join(format!("{mode}.log"));
    AcpAuthDriver::new(
        env!("CARGO_BIN_EXE_fake_acp_harness"),
        [log.to_string_lossy().into_owned(), mode.to_owned()],
    )
    .with_timeout(Duration::from_secs(5))
}

fn profile() -> AuthProfileId {
    AuthProfileId::new("vision-test").expect("profile id is valid")
}

#[tokio::test]
async fn discover_maps_the_harness_methods_to_supported_strategies() {
    let capabilities = driver("oauth-device-code")
        .discover(&profile())
        .await
        .expect("discovery succeeds");

    assert!(capabilities.supports(AuthStrategy::None));
    assert!(capabilities.supports(AuthStrategy::Advertised));
    assert!(capabilities.supports(AuthStrategy::OAuthDeviceCode));
    assert!(!capabilities.supports(AuthStrategy::Environment));
}

#[tokio::test]
async fn begin_distinguishes_authenticated_pending_rejected_and_unsupported() {
    let authenticated = driver("oauth-device-code")
        .begin(&profile(), AuthStrategy::OAuthDeviceCode)
        .await
        .expect("matching method authenticates");
    assert!(matches!(authenticated, AuthProgress::Authenticated(_)));

    let pending = driver("auth-fail")
        .begin(&profile(), AuthStrategy::Advertised)
        .await
        .expect("auth-required becomes a pending challenge");
    assert!(matches!(pending, AuthProgress::Pending(_)));

    let rejected = driver("auth-rejected")
        .begin(&profile(), AuthStrategy::Advertised)
        .await;
    assert_eq!(
        rejected.expect_err("rejection stays rejected"),
        AuthError::Rejected
    );

    let unsupported = driver("password")
        .begin(&profile(), AuthStrategy::OAuthDeviceCode)
        .await;
    assert_eq!(
        unsupported.expect_err("unadvertised strategy is unsupported"),
        AuthError::UnsupportedStrategy
    );
}

#[tokio::test]
async fn health_and_refresh_revalidate_the_advertised_method() {
    let handle = match driver("oauth-device-code")
        .begin(&profile(), AuthStrategy::OAuthDeviceCode)
        .await
        .expect("initial authentication succeeds")
    {
        AuthProgress::Authenticated(handle) => handle,
        AuthProgress::Pending(_) => panic!("expected authenticated handle"),
    };

    let changed = driver("password");
    assert_eq!(
        changed.health(&handle).await,
        AuthStatus::ReauthenticationRequired
    );
    assert_eq!(
        changed
            .refresh(&handle)
            .await
            .expect_err("refresh cannot reuse a removed method"),
        AuthError::UnsupportedStrategy
    );

    assert_eq!(
        driver("initialize-disconnect").health(&handle).await,
        AuthStatus::Unavailable
    );
}
