use config::BrowserConfig;
use std::path::PathBuf;
use types::{InspectCommand, NavigateCommand, PageId, SessionId, TypeTextCommand, WaitUntil};
use worker_pool::{ChromiumWorkerFactory, WorkerFactory};

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
