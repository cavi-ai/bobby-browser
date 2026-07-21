use companion_core::{CompanionRegistry, PairingInput, RegistryError};
use std::time::Duration;
use types::ProfileId;

#[tokio::test]
async fn pairing_code_is_single_use() {
    let registry = CompanionRegistry::new(Duration::from_secs(60), Duration::from_secs(300));
    let code = registry.issue_pairing_code().await;
    let input = PairingInput::firefox(code);

    registry.pair(input.clone()).await.unwrap();

    assert_eq!(
        registry.pair(input).await,
        Err(RegistryError::PairingCodeInvalid)
    );
}

#[tokio::test]
async fn attachment_resolves_before_expiry() {
    let registry = CompanionRegistry::new(Duration::from_secs(60), Duration::from_secs(300));
    let code = registry.issue_pairing_code().await;
    let paired = registry.pair(PairingInput::firefox(code)).await.unwrap();
    let lease = registry.attach(paired.profile_id.clone()).await.unwrap();

    let resolved = registry
        .resolve_attachment(&lease.attachment_id)
        .await
        .unwrap();

    assert_eq!(resolved, lease);
    assert_eq!(resolved.companion_id, paired.companion_id);
    assert_eq!(resolved.profile_id, paired.profile_id);
    assert_eq!(resolved.identity, paired.identity);
    assert_eq!(resolved.capabilities, paired.capabilities);
}

#[tokio::test]
async fn expired_pairing_code_is_rejected() {
    let registry = CompanionRegistry::new(Duration::ZERO, Duration::from_secs(300));
    let code = registry.issue_pairing_code().await;

    assert_eq!(
        registry.pair(PairingInput::firefox(code)).await,
        Err(RegistryError::PairingCodeInvalid)
    );
}

#[tokio::test]
async fn profile_mismatch_is_rejected() {
    let registry = CompanionRegistry::new(Duration::from_secs(60), Duration::from_secs(300));
    let code = registry.issue_pairing_code().await;
    let first = PairingInput::firefox(code);
    registry.pair(first.clone()).await.unwrap();

    let replacement_code = registry.issue_pairing_code().await;
    let mismatched = PairingInput {
        pairing_code: replacement_code,
        profile_id: ProfileId::new(),
        ..first
    };

    assert_eq!(
        registry.pair(mismatched).await,
        Err(RegistryError::ProfileMismatch)
    );
}

#[tokio::test]
async fn revocation_invalidates_existing_attachment() {
    let registry = CompanionRegistry::new(Duration::from_secs(60), Duration::from_secs(300));
    let code = registry.issue_pairing_code().await;
    let paired = registry.pair(PairingInput::firefox(code)).await.unwrap();
    let lease = registry.attach(paired.profile_id.clone()).await.unwrap();

    registry.revoke(&paired.companion_id).await.unwrap();

    assert!(matches!(
        registry.resolve_attachment(&lease.attachment_id).await,
        Err(RegistryError::Revoked)
    ));
}
