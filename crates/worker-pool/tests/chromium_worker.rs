use config::BrowserConfig;
use std::path::PathBuf;
use types::{InspectCommand, NavigateCommand, PageId, SessionId, TypeTextCommand, WaitUntil};
use worker_pool::{
    resolve_upload_paths, session_download_dir, ChromiumWorkerFactory, WorkerFactory,
};

#[test]
fn upload_paths_are_canonical_and_confined_to_roots() {
    let root = tempfile::tempdir().unwrap();
    let allowed = root.path().join("allowed");
    std::fs::create_dir(&allowed).unwrap();
    let file = allowed.join("resume.txt");
    std::fs::write(&file, b"Ada").unwrap();
    let outside = root.path().join("outside.txt");
    std::fs::write(&outside, b"nope").unwrap();

    assert_eq!(
        resolve_upload_paths(&[allowed], &[file.clone()]).unwrap(),
        vec![file.canonicalize().unwrap()]
    );
    let error = resolve_upload_paths(&[root.path().join("allowed")], &[outside]).unwrap_err();
    assert_eq!(error.code, types::ErrorCode::PolicyDenied);
}

#[cfg(unix)]
#[test]
fn upload_paths_reject_symlink_escapes() {
    use std::os::unix::fs::symlink;
    let root = tempfile::tempdir().unwrap();
    let allowed = root.path().join("allowed");
    std::fs::create_dir(&allowed).unwrap();
    let outside = root.path().join("outside.txt");
    std::fs::write(&outside, b"nope").unwrap();
    let link = allowed.join("escape.txt");
    symlink(&outside, &link).unwrap();

    assert_eq!(
        resolve_upload_paths(&[allowed], &[link]).unwrap_err().code,
        types::ErrorCode::PolicyDenied
    );
}

#[test]
fn download_directories_are_session_private() {
    let root = PathBuf::from("/downloads");
    let first = SessionId::new();
    let second = SessionId::new();
    assert_ne!(
        session_download_dir(&root, &first),
        session_download_dir(&root, &second)
    );
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn drives_a_real_chromium_page() {
    let profiles = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        )),
        profiles_dir: profiles.path().to_path_buf(),
        headless: true,
        max_active: 8,
        upload_roots: vec![profiles.path().to_path_buf()],
        downloads_dir: profiles.path().join("downloads"),
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: "data:text/html,<title>Worker Proof</title><input id='name'>".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();
    worker
        .type_text(
            &page_id,
            &TypeTextCommand {
                selector: "#name".into(),
                value: "Ada".into(),
                clear_first: true,
            },
        )
        .await
        .unwrap();
    let evidence = worker
        .inspect(
            &page_id,
            &InspectCommand {
                selector: Some("#name".into()),
                include_html: false,
            },
        )
        .await
        .unwrap();
    assert!(format!("{evidence:?}").contains("Ada"));
    worker.close().await.unwrap();
}
