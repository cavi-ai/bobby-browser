#![cfg(unix)]

use context_store::{inspect_site_file, ContextStore, SiteContext};
use std::os::unix::fs::{symlink, FileTypeExt};

fn envelope(key: &str) -> Vec<u8> {
    serde_json::to_vec(&serde_json::json!({
        "schema": 1,
        "site_key": key,
        "site": {"pages": {}}
    }))
    .unwrap()
}

#[tokio::test]
async fn opening_context_skips_symbolic_links_and_keeps_regular_sites() {
    let root = tempfile::tempdir().unwrap();
    let (store, _) = ContextStore::open(root.path(), "profile").await.unwrap();
    store.upsert_site("healthy", SiteContext::default()).await;
    assert!(store.flush().await.is_empty());
    let path = store.root().join("6c696e6b.json");
    let target = root.path().join("external.json");
    let bytes = envelope("link");
    std::fs::write(&target, &bytes).unwrap();
    symlink(&target, &path).unwrap();
    drop(store);

    let (store, report) = ContextStore::open(root.path(), "profile").await.unwrap();
    assert_eq!(report.sites_loaded, 1);
    assert_eq!(report.skipped_total, 1);
    assert_eq!(report.skipped[0].file, path);
    assert_eq!(store.list_sites().await, ["healthy"]);
    assert!(store.site("link").await.is_none());
    assert!(inspect_site_file(&path, Default::default()).is_err());
    assert!(std::fs::symlink_metadata(&path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read(target).unwrap(), bytes);
}

#[tokio::test]
async fn lazy_reads_and_retention_ignore_symbolic_links_added_after_open() {
    let root = tempfile::tempdir().unwrap();
    let (store, _) = ContextStore::open(root.path(), "profile").await.unwrap();
    let path = store.root().join("6c696e6b.json");
    let target = root.path().join("external.json");
    let bytes = envelope("link");
    std::fs::write(&target, &bytes).unwrap();
    symlink(&target, &path).unwrap();

    assert!(store.site("link").await.is_none());
    assert!(store.list_sites().await.is_empty());
    assert_eq!(store.sweep(30, 100).await.unwrap(), 0);
    assert!(std::fs::symlink_metadata(&path)
        .unwrap()
        .file_type()
        .is_symlink());
    assert_eq!(std::fs::read(target).unwrap(), bytes);
}

#[tokio::test]
async fn context_file_inspection_rejects_directories() {
    let root = tempfile::tempdir().unwrap();
    let path = root.path().join("646972.json");
    std::fs::create_dir(&path).unwrap();
    assert!(inspect_site_file(&path, Default::default()).is_err());
    assert!(std::fs::symlink_metadata(path).unwrap().is_dir());
}

// A subprocess keeps a regressed blocking FIFO open bounded: Tokio cannot
// cancel the underlying blocking filesystem task when its future times out.
#[test]
fn named_pipe_context_entries_do_not_block_readers() {
    use std::os::unix::ffi::OsStrExt;
    use std::process::Command;
    use std::time::{Duration, Instant};

    let root = tempfile::tempdir().unwrap();
    let profile = root.path().join("70726f66696c65");
    std::fs::create_dir(&profile).unwrap();
    let path = profile.join("70697065.json");
    let name = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    // The CString outlives the syscall, and the fixture has no writer.
    assert_eq!(unsafe { libc::mkfifo(name.as_ptr(), 0o600) }, 0);
    for operation in ["open", "site", "list", "sweep", "inspect"] {
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "named_pipe_reader", "--nocapture"])
            .env("CONTEXT_FIFO_ROOT", root.path())
            .env("CONTEXT_FIFO_OPERATION", operation)
            .spawn()
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                assert!(status.success(), "{operation} failed: {status}");
                break;
            }
            if Instant::now() >= deadline {
                child.kill().unwrap();
                child.wait().unwrap();
                panic!("{operation} blocked on a context FIFO");
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }
    assert!(std::fs::symlink_metadata(path)
        .unwrap()
        .file_type()
        .is_fifo());
}

#[tokio::test]
async fn named_pipe_reader() {
    let Some(root) = std::env::var_os("CONTEXT_FIFO_ROOT") else {
        return;
    };
    let operation = std::env::var("CONTEXT_FIFO_OPERATION").unwrap();
    let path = std::path::PathBuf::from(&root).join("70726f66696c65/70697065.json");
    if operation == "inspect" {
        assert!(inspect_site_file(&path, Default::default()).is_err());
        return;
    }
    // Open with the FIFO temporarily absent to exercise post-open readers.
    if operation != "open" {
        std::fs::rename(&path, path.with_extension("hold")).unwrap();
    }
    let (store, report) = ContextStore::open(&root, "profile").await.unwrap();
    if operation != "open" {
        std::fs::rename(path.with_extension("hold"), &path).unwrap();
    }
    match operation.as_str() {
        "open" => assert_eq!(report.skipped_total, 1),
        "site" => assert!(store.site("pipe").await.is_none()),
        "list" => assert!(store.list_sites().await.is_empty()),
        "sweep" => assert_eq!(store.sweep(30, 100).await.unwrap(), 0),
        _ => panic!("unknown reader operation"),
    }
}
