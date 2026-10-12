use artifact_store::ArtifactStore;
use types::SessionId;
use uuid::Uuid;

fn staging_name(artifact: &str) -> String {
    format!(".{artifact}.{}.tmp", Uuid::new_v4())
}

fn directory_with_canary(path: &std::path::Path) {
    std::fs::create_dir_all(path).unwrap();
    std::fs::write(path.join("canary"), b"preserve").unwrap();
}

#[test]
fn startup_removes_only_recognized_staging_directories() {
    let root = tempfile::tempdir().unwrap();
    let session = root.path().join(SessionId::new().0.to_string());
    std::fs::create_dir(&session).unwrap();
    let orphans = [
        session.join(staging_name(&Uuid::new_v4().to_string())),
        session.join(staging_name(&"a".repeat(64))),
    ];
    for path in &orphans {
        directory_with_canary(path);
    }
    let preserved = [
        session.join(".unrelated.tmp"),
        session.join(staging_name("invalid-artifact")),
        session.join(format!(".{}.invalid-staging.tmp", Uuid::new_v4())),
        session.join(Uuid::new_v4().to_string()),
        root.path()
            .join("unrelated")
            .join(staging_name(&Uuid::new_v4().to_string())),
    ];
    for path in &preserved {
        directory_with_canary(path);
    }
    let regular_file = session.join(staging_name(&Uuid::new_v4().to_string()));
    std::fs::write(&regular_file, b"regular file").unwrap();

    let _store = ArtifactStore::new(root.path(), 1024, 4096);

    for path in &orphans {
        assert!(!path.exists(), "orphan remains: {}", path.display());
    }
    for path in &preserved {
        assert_eq!(std::fs::read(path.join("canary")).unwrap(), b"preserve");
    }
    assert_eq!(std::fs::read(regular_file).unwrap(), b"regular file");
}

#[cfg(unix)]
#[test]
fn startup_preserves_linked_session_and_staging_directories() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let external = tempfile::tempdir().unwrap();
    let external_staging = external
        .path()
        .join(staging_name(&Uuid::new_v4().to_string()));
    directory_with_canary(&external_staging);
    let linked_session = root.path().join(SessionId::new().0.to_string());
    symlink(external.path(), &linked_session).unwrap();

    let session = root.path().join(SessionId::new().0.to_string());
    std::fs::create_dir(&session).unwrap();
    let linked_staging = session.join(staging_name(&Uuid::new_v4().to_string()));
    symlink(&external_staging, &linked_staging).unwrap();

    let _store = ArtifactStore::new(root.path(), 1024, 4096);

    assert_eq!(
        std::fs::read(external_staging.join("canary")).unwrap(),
        b"preserve"
    );
    for link in [&linked_session, &linked_staging] {
        assert!(std::fs::symlink_metadata(link)
            .unwrap()
            .file_type()
            .is_symlink());
    }
}
