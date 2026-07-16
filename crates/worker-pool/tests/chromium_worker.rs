use config::BrowserConfig;
use std::path::PathBuf;
use types::{
    ClickAndWaitForDownloadCommand, ClickAndWaitForPopupCommand, ClosePageCommand, InspectCommand,
    ListPagesCommand, NavigateCommand, OpenPageCommand, PageId, SessionId, TypeTextCommand,
    UploadFilesCommand, WaitUntil,
};
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

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn correlates_popup_and_download_before_clicking() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        )),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: fixture.base_url(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

    let popup = worker
        .click_and_wait_for_popup(
            &page_id,
            &ClickAndWaitForPopupCommand {
                selector: "#root-popup".into(),
                timeout_ms: 5_000,
            },
        )
        .await
        .unwrap();
    println!("popup={popup:?}");
    assert!(matches!(&popup[0], types::Evidence::Popup { title, .. } if title == "Popup"));
    let download = worker
        .click_and_wait_for_download(
            &page_id,
            &ClickAndWaitForDownloadCommand {
                selector: "#download".into(),
                timeout_ms: 5_000,
            },
        )
        .await
        .unwrap();
    println!("download={download:?}");
    assert!(
        matches!(&download[0], types::Evidence::Download { filename, bytes, sha256, .. } if filename == "workflow-fixture.bin" && *bytes == 20 && sha256.len() == 64)
    );
    worker.close().await.unwrap();
    println!("closed");
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
    let upload = profiles.path().join("resume.txt");
    std::fs::write(&upload, b"Ada Lovelace").unwrap();
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
                url: "data:text/html,<title>Worker Proof</title><input id='name'><input id='resume' type='file'>".into(),
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
    let upload_evidence = worker
        .upload_files(
            &page_id,
            &UploadFilesCommand {
                selector: "#resume".into(),
                paths: vec![upload.to_string_lossy().into_owned()],
            },
        )
        .await
        .unwrap();
    assert!(format!("{upload_evidence:?}").contains("resume.txt"));
    let opened = worker
        .open_page_command(&OpenPageCommand {
            url: Some("data:text/html,<title>Second Page</title>".into()),
        })
        .await
        .unwrap();
    let second_page = match &opened[0] {
        types::Evidence::Page { page_id, title, .. } => {
            assert_eq!(title, "Second Page");
            page_id.clone()
        }
        other => panic!("unexpected evidence: {other:?}"),
    };
    let listed = worker.list_pages(&ListPagesCommand).await.unwrap();
    assert!(matches!(&listed[0], types::Evidence::Pages { pages } if pages.len() == 2));
    worker
        .close_page_command(&ClosePageCommand {
            page_id: second_page,
        })
        .await
        .unwrap();
    let listed = worker.list_pages(&ListPagesCommand).await.unwrap();
    assert!(matches!(&listed[0], types::Evidence::Pages { pages } if pages.len() == 1));
    worker.close().await.unwrap();
}
