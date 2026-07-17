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
        .join(&record.artifact_id)
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

#[tokio::test]
async fn generic_artifacts_are_private_validated_and_atomic() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(root.path(), 1024, 4096);
    let session = SessionId::new();
    let other_session = SessionId::new();
    let page = PageId::new();

    let record = store
        .put(
            &session,
            &page,
            "application/octet-stream",
            "bin",
            b"download-v1",
            1024,
        )
        .await
        .unwrap();

    assert_eq!(record.media_type, "application/octet-stream");
    assert_eq!(
        store.get(&session, &record.artifact_id).await.unwrap(),
        b"download-v1"
    );
    assert!(store
        .get(&other_session, &record.artifact_id)
        .await
        .is_err());
    assert!(store
        .put(&session, &page, "text/plain", "../txt", b"x", 10)
        .await
        .is_err());

    let oversized_session = SessionId::new();
    assert_eq!(
        store
            .put(
                &oversized_session,
                &page,
                "application/octet-stream",
                "bin",
                b"too-large",
                4,
            )
            .await
            .unwrap_err(),
        ArtifactError::TooLarge
    );
    let oversized_dir = root.path().join(oversized_session.0.to_string());
    assert!(
        !oversized_dir.exists() || std::fs::read_dir(oversized_dir).unwrap().next().is_none(),
        "an oversized payload must leave no final or temporary files"
    );
}

#[tokio::test]
async fn repeated_generic_bytes_converge_on_one_content_addressed_artifact() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(root.path(), 1024, 4096);
    let session = SessionId::new();
    let page = PageId::new();
    let first = store
        .put(
            &session,
            &page,
            "application/octet-stream",
            "bin",
            b"same",
            1024,
        )
        .await
        .unwrap();
    let second = store
        .put(
            &session,
            &page,
            "application/octet-stream",
            "bin",
            b"same",
            1024,
        )
        .await
        .unwrap();
    assert_eq!(first.artifact_id, first.sha256);
    assert_eq!(second.artifact_id, first.artifact_id);
    assert_eq!(
        std::fs::read_dir(root.path().join(session.0.to_string()))
            .unwrap()
            .count(),
        1
    );
}

#[tokio::test]
async fn content_addressed_pending_bytes_are_invisible_until_commit() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(root.path(), 1024, 4096);
    let session = SessionId::new();
    let page = PageId::new();

    let pending = store
        .put_pending(
            &session,
            &page,
            "application/octet-stream",
            "bin",
            b"guarded",
            1024,
        )
        .await
        .unwrap();
    let dropped_id = pending.record().artifact_id.clone();
    assert!(!root
        .path()
        .join(session.0.to_string())
        .join(&dropped_id)
        .exists());
    assert_eq!(
        store.get(&session, &dropped_id).await.unwrap_err(),
        ArtifactError::NotFound
    );
    drop(pending);
    assert_eq!(
        store.get(&session, &dropped_id).await.unwrap_err(),
        ArtifactError::NotFound
    );
    assert_eq!(
        std::fs::read_dir(root.path().join(session.0.to_string()))
            .unwrap()
            .count(),
        0
    );

    let pending = store
        .put_pending(
            &session,
            &page,
            "application/octet-stream",
            "bin",
            b"committed",
            1024,
        )
        .await
        .unwrap();
    let record = pending.commit().unwrap();
    assert_eq!(
        store.get(&session, &record.artifact_id).await.unwrap(),
        b"committed"
    );
}

#[tokio::test]
async fn identical_bytes_converge_across_pages_and_response_metadata() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(root.path(), 1024, 4096);
    let session = SessionId::new();
    let first = store
        .put(
            &session,
            &PageId::new(),
            "application/octet-stream",
            "bin",
            b"shared-bytes",
            1024,
        )
        .await
        .unwrap();
    let second = store
        .put(
            &session,
            &PageId::new(),
            "text/plain",
            "txt",
            b"shared-bytes",
            1024,
        )
        .await
        .unwrap();
    assert_eq!(first.artifact_id, second.artifact_id);
    assert_eq!(
        store.get(&session, &second.artifact_id).await.unwrap(),
        b"shared-bytes"
    );
}

#[tokio::test]
async fn concurrent_identical_publications_converge_without_loser_deleting_winner() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(root.path(), 1024, 4096);
    let session = SessionId::new();
    let page = PageId::new();

    let (first, second) = tokio::join!(
        store.put_pending(
            &session,
            &page,
            "application/octet-stream",
            "bin",
            b"same-concurrent-bytes",
            1024,
        ),
        store.put_pending(
            &session,
            &page,
            "application/octet-stream",
            "bin",
            b"same-concurrent-bytes",
            1024,
        )
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_eq!(first.record().artifact_id, second.record().artifact_id);
    let record = second.commit().unwrap();
    drop(first);
    assert_eq!(
        store.get(&session, &record.artifact_id).await.unwrap(),
        b"same-concurrent-bytes"
    );
}

#[tokio::test]
async fn durable_staging_id_can_be_finalized_after_process_recovery() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(root.path(), 1024, 4096);
    let session = SessionId::new();
    let pending = store
        .put_pending(
            &session,
            &PageId::new(),
            "application/octet-stream",
            "bin",
            b"recoverable-staging",
            1024,
        )
        .await
        .unwrap();
    let artifact_id = pending.record().artifact_id.clone();
    let sha256 = pending.record().sha256.clone();
    let bytes = pending.record().bytes;
    let staging_id = pending.staging_id().unwrap().to_owned();
    std::mem::forget(pending);

    store
        .finalize_staged(&session, &artifact_id, &staging_id, &sha256, bytes)
        .unwrap();
    assert_eq!(
        store.get(&session, &artifact_id).await.unwrap(),
        b"recoverable-staging"
    );
}

#[tokio::test]
async fn recovery_rejects_tampered_staging_and_cleans_converged_staging() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(root.path(), 1024, 4096);
    let session = SessionId::new();
    let page = PageId::new();
    let pending = store
        .put_pending(
            &session,
            &page,
            "application/octet-stream",
            "bin",
            b"expected",
            1024,
        )
        .await
        .unwrap();
    let artifact_id = pending.record().artifact_id.clone();
    let sha256 = pending.record().sha256.clone();
    let bytes = pending.record().bytes;
    let staging_id = pending.staging_id().unwrap().to_owned();
    let session_dir = root.path().join(session.0.to_string());
    let staging_dir = std::fs::read_dir(&session_dir)
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let data_path = std::fs::read_dir(&staging_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.extension().is_some_and(|extension| extension == "bin"))
        .unwrap();
    std::fs::write(data_path, b"tampered").unwrap();
    std::mem::forget(pending);
    assert_eq!(
        store
            .finalize_staged(&session, &artifact_id, &staging_id, &sha256, bytes)
            .unwrap_err(),
        ArtifactError::InvalidMetadata
    );

    let clean_pending = store
        .put_pending(
            &session,
            &page,
            "application/octet-stream",
            "bin",
            b"winner",
            1024,
        )
        .await
        .unwrap();
    let clean_id = clean_pending.record().artifact_id.clone();
    let clean_sha = clean_pending.record().sha256.clone();
    let clean_bytes = clean_pending.record().bytes;
    let clean_staging = clean_pending.staging_id().unwrap().to_owned();
    std::mem::forget(clean_pending);
    store
        .put(&session, &page, "text/plain", "txt", b"winner", 1024)
        .await
        .unwrap();
    store
        .finalize_staged(&session, &clean_id, &clean_staging, &clean_sha, clean_bytes)
        .unwrap();
    assert!(!session_dir
        .join(format!(".{clean_id}.{clean_staging}.tmp"))
        .exists());
}
