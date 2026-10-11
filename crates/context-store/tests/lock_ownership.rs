#![cfg(unix)]

use context_store::{ContextStore, ContextStoreError};
use std::os::unix::fs::{MetadataExt, PermissionsExt};

#[tokio::test]
async fn hard_linked_context_lock_is_refused_without_changing_the_external_file() {
    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("70726f66696c65");
    std::fs::create_dir(&profile).unwrap();
    let external = root.path().join("external.lock");
    let bytes = b"external-file-canary";
    std::fs::write(&external, bytes).unwrap();
    std::fs::set_permissions(&external, std::fs::Permissions::from_mode(0o644)).unwrap();
    let path = profile.join(".context-store.lock");
    std::fs::hard_link(&external, &path).unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().nlink(), 2);

    let result = ContextStore::open(root.path(), "profile").await;
    assert_eq!(
        std::fs::metadata(&external).unwrap().permissions().mode() & 0o777,
        0o644,
        "a refused lock must not change another file's permissions"
    );
    assert_eq!(std::fs::read(&external).unwrap(), bytes);
    assert!(matches!(result, Err(ContextStoreError::LockUnusable(_))));
    assert_eq!(std::fs::metadata(&path).unwrap().nlink(), 2);

    // Removing the invalid alias allows the profile to claim its own lock.
    std::fs::remove_file(&path).unwrap();
    let (store, _) = ContextStore::open(root.path(), "profile").await.unwrap();
    assert_eq!(std::fs::metadata(&path).unwrap().nlink(), 1);
    assert_eq!(std::fs::metadata(&external).unwrap().nlink(), 1);
    assert_eq!(std::fs::read(&external).unwrap(), bytes);
    assert!(matches!(
        ContextStore::open(root.path(), "profile").await,
        Err(ContextStoreError::AlreadyLocked)
    ));
    drop(store);
    assert!(ContextStore::open(root.path(), "profile").await.is_ok());
}

#[tokio::test]
async fn a_refused_second_writer_does_not_change_the_owners_lock_permissions() {
    let root = tempfile::tempdir().unwrap();
    let (store, _) = ContextStore::open(root.path(), "profile").await.unwrap();
    let path = store.root().join(".context-store.lock");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(matches!(
        ContextStore::open(root.path(), "profile").await,
        Err(ContextStoreError::AlreadyLocked)
    ));
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o644
    );
    drop(store);

    let (store, _) = ContextStore::open(root.path(), "profile").await.unwrap();
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600,
        "the successful owner still repairs private permissions"
    );
    drop(store);
}
