#![cfg(not(unix))]

use std::sync::Arc;

use artifact_store::ArtifactStore;
use chrono::{Duration, Utc};
use interface_core::{
    ArtifactOwnershipLimits, ArtifactReader, Authority, AuthorityStore, SessionOwnershipAuthority,
};
use types::{Capability, InterfaceErrorCode, PageId, PrincipalId, SessionId};
use uuid::Uuid;

struct AllowOwnedSession;

impl SessionOwnershipAuthority for AllowOwnedSession {
    fn owns_session(&self, _principal: &PrincipalId, _session: &SessionId) -> bool {
        true
    }
}

#[tokio::test]
async fn artifact_registration_fails_closed_on_non_unix_targets() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(root.path(), 4096, 4096);
    let session = SessionId::new();
    let record = store
        .put(
            &session,
            &PageId::new(),
            "application/octet-stream",
            "bin",
            b"platform-boundary",
            4096,
        )
        .await
        .unwrap();
    let authority = AuthorityStore::in_memory();
    let token = authority
        .issue(
            PrincipalId::from_uuid(Uuid::from_u128(99)),
            [Capability::ArtifactCapture],
            Utc::now() + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let handle = authority.verify(&token).await.unwrap();
    let context = handle.context(Utc::now() + Duration::minutes(1), None);
    let reader = ArtifactReader::new(
        store,
        Arc::new(AllowOwnedSession),
        4096,
        ArtifactOwnershipLimits {
            max_records: 8,
            max_bytes: 64 * 1024,
        },
    )
    .unwrap();

    let denial = reader
        .register(&handle, &context, &session, &record)
        .await
        .unwrap_err();
    assert_eq!(denial.code, InterfaceErrorCode::ArtifactDenied);
}
