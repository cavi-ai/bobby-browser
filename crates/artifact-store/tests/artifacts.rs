use artifact_store::{ArtifactError, ArtifactStore};
use types::{PageId, SessionId};

const ONE_PIXEL_PNG: &[u8] = &[
    137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6, 0,
    0, 0, 31, 21, 196, 137,
];

#[tokio::test]
async fn stores_hashed_session_private_artifacts_atomically() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(root.path(), 1024, 4096);
    let owner = SessionId::new();
    let other = SessionId::new();
    let record = store
        .put_png(&owner, &PageId::new(), ONE_PIXEL_PNG)
        .await
        .unwrap();

    assert_eq!((record.width, record.height), (1, 1));
    assert_eq!(record.bytes, ONE_PIXEL_PNG.len() as u64);
    assert_eq!(record.sha256.len(), 64);
    assert_eq!(
        store.get(&owner, &record.artifact_id).await.unwrap(),
        ONE_PIXEL_PNG
    );
    assert_eq!(
        store.get(&other, &record.artifact_id).await.unwrap_err(),
        ArtifactError::NotFound
    );
    assert!(root
        .path()
        .join(owner.0.to_string())
        .join(format!("{}.png", record.artifact_id))
        .is_file());
}

#[tokio::test]
async fn rejects_oversized_or_invalid_png_payloads() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(root.path(), 8, 4096);
    assert_eq!(
        store
            .put_png(&SessionId::new(), &PageId::new(), ONE_PIXEL_PNG)
            .await
            .unwrap_err(),
        ArtifactError::TooLarge
    );
    let store = ArtifactStore::new(root.path(), 1024, 4096);
    assert_eq!(
        store
            .put_png(&SessionId::new(), &PageId::new(), b"not png")
            .await
            .unwrap_err(),
        ArtifactError::InvalidPng
    );
}
