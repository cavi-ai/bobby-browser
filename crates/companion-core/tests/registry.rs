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
async fn paired_profile_ids_expose_only_completed_pairings() {
    let registry = CompanionRegistry::new(Duration::from_secs(60), Duration::from_secs(300));
    assert!(registry.paired_profile_ids().await.is_empty());

    let code = registry.issue_pairing_code().await;
    let paired = registry.pair(PairingInput::firefox(code)).await.unwrap();

    assert_eq!(registry.paired_profile_ids().await, vec![paired.profile_id]);
}

#[tokio::test]
async fn successful_pairing_issues_a_debug_redacted_reconnect_credential() {
    let registry = CompanionRegistry::new(Duration::from_secs(60), Duration::from_secs(300));
    let code = registry.issue_pairing_code().await;
    let session = registry
        .pair_with_credential(PairingInput::firefox(code))
        .await
        .unwrap();
    let credential = session.credential.expose_secret().to_owned();

    assert!(!credential.is_empty());
    assert!(!format!("{:?}", session.credential).contains(&credential));
    assert_eq!(
        registry.authenticate_credential(&credential).await.unwrap(),
        session.companion
    );

    registry
        .revoke(&session.companion.companion_id)
        .await
        .unwrap();
    assert_eq!(
        registry.authenticate_credential(&credential).await,
        Err(RegistryError::Revoked)
    );
}

#[tokio::test]
async fn credential_authentication_cannot_consume_an_initial_pairing_code() {
    let registry = CompanionRegistry::new(Duration::from_secs(60), Duration::from_secs(300));
    let code = registry.issue_pairing_code().await;

    assert_eq!(
        registry.authenticate_credential(&code).await,
        Err(RegistryError::CredentialInvalid)
    );
    registry.pair(PairingInput::firefox(code)).await.unwrap();
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
async fn attachment_renewal_preserves_identity_and_extends_the_bound() {
    let registry = CompanionRegistry::new(Duration::from_secs(60), Duration::from_secs(300));
    let code = registry.issue_pairing_code().await;
    let paired = registry.pair(PairingInput::firefox(code)).await.unwrap();
    let lease = registry.attach(paired.profile_id).await.unwrap();

    let renewed = registry
        .renew_attachment(&lease.attachment_id)
        .await
        .unwrap();

    assert_eq!(renewed.attachment_id, lease.attachment_id);
    assert_eq!(renewed.profile_id, lease.profile_id);
    assert_eq!(renewed.companion_id, lease.companion_id);
    assert!(renewed.expires_at > lease.expires_at);
    assert_eq!(
        registry
            .resolve_attachment(&renewed.attachment_id)
            .await
            .unwrap(),
        renewed
    );
}

#[tokio::test]
async fn expired_attachment_cannot_be_renewed() {
    let registry = CompanionRegistry::new(Duration::from_secs(60), Duration::ZERO);
    let code = registry.issue_pairing_code().await;
    let paired = registry.pair(PairingInput::firefox(code)).await.unwrap();
    let lease = registry.attach(paired.profile_id).await.unwrap();

    assert_eq!(
        registry.renew_attachment(&lease.attachment_id).await,
        Err(RegistryError::AttachmentExpired)
    );
}

#[tokio::test]
async fn expired_attachment_is_rejected() {
    let registry = CompanionRegistry::new(Duration::from_secs(60), Duration::ZERO);
    let code = registry.issue_pairing_code().await;
    let paired = registry.pair(PairingInput::firefox(code)).await.unwrap();
    let lease = registry.attach(paired.profile_id).await.unwrap();

    assert_eq!(
        registry.resolve_attachment(&lease.attachment_id).await,
        Err(RegistryError::AttachmentExpired)
    );
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
