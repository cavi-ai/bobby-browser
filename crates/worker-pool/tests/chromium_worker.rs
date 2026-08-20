use config::BrowserConfig;
use std::path::PathBuf;

fn chrome_executable() -> PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

use network_engine::state::{HttpCookie, ResponseStateDelta};
use std::collections::BTreeMap;
use types::{
    AccessibilityNode, AccessibilitySnapshotCommand, CaptureScreenshotCommand,
    ClickAndWaitForDownloadCommand, ClickAndWaitForPopupCommand, ClickCommand, ClickModifier,
    ClosePageCommand, ControlAction, ControlActionCommand, DialogAction, ElementState, ErrorCode,
    EvaluateJavaScriptCommand, Evidence, HandleDialogCommand, InspectCommand, ListPagesCommand,
    NavigateCommand, NetworkLogCommand, OpenPageCommand, PageId, ScreenshotMode, SessionId,
    TargetSpec, TextMatch, TypeTextCommand, UploadFilesCommand, WaitCondition, WaitForCommand,
    WaitUntil,
};
use worker_pool::{
    resolve_upload_paths, session_download_dir, ChromiumWorkerFactory, WorkerFactory,
};

fn cookie(name: &str, value: &str) -> HttpCookie {
    HttpCookie {
        name: name.into(),
        value: value.into(),
        domain: String::new(),
        host_only: true,
        path: "/".into(),
        secure: false,
        http_only: false,
        same_site: None,
        expires_unix: None,
        priority: None,
        source_scheme: None,
        source_port: None,
        partition_key: None,
    }
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn har_reports_request_duration_instead_of_monotonic_uptime() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request).await;
                tokio::time::sleep(std::time::Duration::from_millis(120)).await;
                stream
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: 9\r\nConnection: close\r\n\r\n<p>ok</p>",
                    )
                    .await
                    .unwrap();
            });
        }
    });
    let url = format!("http://{address}/timed");
    let root = tempfile::tempdir().unwrap();
    let session_id = SessionId::new();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&session_id).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .network_log(&page_id, &NetworkLogCommand { clear: true })
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: url.clone(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let evidence = worker
        .network_log(&page_id, &NetworkLogCommand { clear: false })
        .await
        .unwrap();
    let artifact_id = match &evidence[0] {
        Evidence::HarArtifact {
            artifact_id,
            entries,
            ..
        } => {
            assert!(*entries > 0, "the delayed request must be recorded");
            artifact_id
        }
        other => panic!("unexpected evidence: {other:?}"),
    };
    let artifact = root
        .path()
        .join("artifacts")
        .join(session_id.0.to_string())
        .join(artifact_id)
        .join(format!("{artifact_id}.har"));
    let document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(artifact).unwrap()).unwrap();
    let entry = document["log"]["entries"]
        .as_array()
        .unwrap()
        .iter()
        .find(|entry| entry["request"]["url"] == url)
        .expect("HAR contains the delayed navigation request");
    let elapsed_ms = entry["time"].as_f64().unwrap();
    assert!(
        (100.0..2_000.0).contains(&elapsed_ms),
        "HAR time must be an elapsed request duration, got {elapsed_ms}ms"
    );
    assert_eq!(
        entry["response"]["statusText"], "OK",
        "HAR preserves the HTTP response status text"
    );

    worker.close().await.unwrap();
    fixture.abort();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn har_preserves_redirect_responses_that_reuse_a_request_id() {
    let fixture = test_site::spawn().await;
    let redirect_url = format!("{}/redirect-static", fixture.base_url());
    let final_url = format!("{}/static", fixture.base_url());
    let root = tempfile::tempdir().unwrap();
    let session_id = SessionId::new();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&session_id).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .network_log(&page_id, &NetworkLogCommand { clear: true })
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: redirect_url.clone(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let evidence = worker
        .network_log(&page_id, &NetworkLogCommand { clear: false })
        .await
        .unwrap();
    let artifact_id = match &evidence[0] {
        Evidence::HarArtifact { artifact_id, .. } => artifact_id,
        other => panic!("unexpected evidence: {other:?}"),
    };
    let artifact = root
        .path()
        .join("artifacts")
        .join(session_id.0.to_string())
        .join(artifact_id)
        .join(format!("{artifact_id}.har"));
    let document: serde_json::Value =
        serde_json::from_slice(&std::fs::read(artifact).unwrap()).unwrap();
    let entries = document["log"]["entries"].as_array().unwrap();
    let redirect = entries
        .iter()
        .find(|entry| entry["request"]["url"] == redirect_url)
        .expect("HAR retains the redirect request");
    assert_eq!(redirect["response"]["status"], 302);
    assert_eq!(
        redirect["response"]["redirectURL"], final_url,
        "HAR records the redirect destination"
    );
    assert!(
        entries
            .iter()
            .any(|entry| entry["request"]["url"] == final_url),
        "HAR retains the final request"
    );

    worker.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn navigating_to_a_download_response_fails_fast_instead_of_hanging_to_the_deadline() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut request = [0_u8; 1024];
                let read = stream.read(&mut request).await.unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..read]);
                if request.starts_with("GET /ok") {
                    let body = "<!doctype html><title>OK</title><p id=\"ok\">ok fixture</p>";
                    let response = format!(
                        "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                        body.len(),
                        body
                    );
                    stream.write_all(response.as_bytes()).await.unwrap();
                } else {
                    stream
                        .write_all(
                            b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=file.bin\r\nContent-Length: 3\r\nConnection: close\r\n\r\nbin",
                        )
                        .await
                        .unwrap();
                }
            });
        }
    });
    let download_url = format!("http://{address}/file.bin");
    let ok_url = format!("http://{address}/ok");
    let root = tempfile::tempdir().unwrap();
    let session_id = SessionId::new();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&session_id).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();

    let started = std::time::Instant::now();
    let result = worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: download_url.clone(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await;
    let elapsed = started.elapsed();
    let error = result.expect_err("navigating to a download response must not report success");
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "navigation to a download must fail fast instead of waiting for the deadline, took {elapsed:?}"
    );
    assert!(
        !error.retryable,
        "an aborted download navigation is not retryable, retrying reaches the same download"
    );
    assert!(
        error.message.contains("download_url"),
        "error must point the caller at download_url, got: {}",
        error.message
    );

    let started = std::time::Instant::now();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: ok_url.clone(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "the navigation after an aborted download must not wait on the freed slot, took {elapsed:?}"
    );
    let observed = worker
        .inspect(
            &page_id,
            &InspectCommand {
                selector: Some("#ok".into()),
                target: None,
                include_html: false,
            },
        )
        .await
        .unwrap();
    assert!(
        observed
            .iter()
            .any(|item| matches!(item, Evidence::Inspection { text, .. } if text == "ok fixture")),
        "expected the /ok fixture text, got {observed:?}"
    );

    worker.close().await.unwrap();
    fixture.abort();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn network_log_reports_networkrecordingstarted_only_on_the_attaching_call() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let session_id = SessionId::new();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&session_id).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();

    let first = worker
        .network_log(&page_id, &NetworkLogCommand { clear: false })
        .await
        .unwrap();
    assert!(
        first.iter().any(|evidence| matches!(
            evidence,
            Evidence::Configuration { name, value }
                if name == "networkRecordingStarted" && value == "true"
        )),
        "the attaching call must report networkRecordingStarted: {first:?}"
    );

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
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let second = worker
        .network_log(&page_id, &NetworkLogCommand { clear: false })
        .await
        .unwrap();
    let entries = match &second[0] {
        Evidence::HarArtifact { entries, .. } => *entries,
        other => panic!("unexpected evidence: {other:?}"),
    };
    assert!(entries > 0, "the navigation request must be recorded");
    assert!(
        !second.iter().any(|evidence| matches!(
            evidence,
            Evidence::Configuration { name, .. } if name == "networkRecordingStarted"
        )),
        "an already-attached collector must not report networkRecordingStarted again: {second:?}"
    );

    worker.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn synchronizes_versioned_http_state() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let fixture = tokio::spawn(async move {
        loop {
            let (mut stream, _) = listener.accept().await.unwrap();
            tokio::spawn(async move {
                use tokio::io::{AsyncReadExt, AsyncWriteExt};
                let mut request = [0_u8; 1024];
                let _ = stream.read(&mut request).await;
                stream
                    .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nSet-Cookie: session=alpha; HttpOnly; SameSite=Lax; Path=/\r\nContent-Length: 32\r\nConnection: close\r\n\r\n<title>HTTP state fixture</title>")
                    .await
                    .unwrap();
            });
        }
    });
    let state_url = format!("http://{address}/state");
    let root = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: state_url.clone(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

    let snapshot = worker.http_state(&page_id).await.unwrap();
    assert_eq!(snapshot.current_url, state_url);
    assert!(snapshot
        .cookies
        .iter()
        .any(|cookie| { cookie.name == "session" && cookie.value == "alpha" && cookie.http_only }));
    assert!(!snapshot.user_agent.is_empty());

    let mut unrelated = cookie("unrelated", "blocked");
    unrelated.domain = "example.invalid".into();
    let mut nonfinite = cookie("nonfinite", "blocked");
    nonfinite.expires_unix = Some(f64::NAN);
    let mut invalid_expiry = cookie("invalid-expiry", "blocked");
    invalid_expiry.expires_unix = Some(-1.0);
    let mut secure_over_http = cookie("secure", "blocked");
    secure_over_http.secure = true;
    for rejected in [unrelated, nonfinite, invalid_expiry, secure_over_http] {
        let error = worker
            .commit_http_state(
                &page_id,
                snapshot.version,
                ResponseStateDelta {
                    cookies: vec![rejected],
                    cache_validators: BTreeMap::new(),
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::InvalidRequest);
        let unchanged = worker.http_state(&page_id).await.unwrap();
        assert_eq!(unchanged.version, snapshot.version);
        assert_eq!(unchanged.cookies.len(), snapshot.cookies.len());
    }

    worker
        .commit_http_state(
            &page_id,
            snapshot.version,
            ResponseStateDelta {
                cookies: Vec::new(),
                cache_validators: BTreeMap::from([("state".into(), "fixture-v1".into())]),
            },
        )
        .await
        .expect("empty cookie delta commits validators without invalid CDP call");
    let snapshot = worker.http_state(&page_id).await.unwrap();
    assert_eq!(
        snapshot.cache_validators.get("state").unwrap(),
        "fixture-v1"
    );

    let mut direct = cookie("direct", "beta");
    direct.source_scheme = Some("NonSecure".into());
    worker
        .commit_http_state(
            &page_id,
            snapshot.version,
            ResponseStateDelta {
                cookies: vec![direct],
                cache_validators: BTreeMap::new(),
            },
        )
        .await
        .unwrap();
    let committed = worker.http_state(&page_id).await.unwrap();
    assert!(committed
        .cookies
        .iter()
        .any(|cookie| cookie.name == "direct" && cookie.value == "beta"));

    let conflict = worker
        .commit_http_state(
            &page_id,
            snapshot.version,
            ResponseStateDelta {
                cookies: Vec::new(),
                cache_validators: BTreeMap::new(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(conflict.code, ErrorCode::HttpStateConflict);

    for sequence in 0..16 {
        let before = worker.http_state(&page_id).await.unwrap();
        let name = format!("coherent-{sequence}");
        let expected_name = name.clone();
        let (observed, committed) = tokio::join!(
            worker.http_state(&page_id),
            worker.commit_http_state(
                &page_id,
                before.version,
                ResponseStateDelta {
                    cookies: vec![cookie(&name, "set")],
                    cache_validators: BTreeMap::new(),
                },
            )
        );
        committed.unwrap();
        let observed = observed.unwrap();
        let contains_commit = observed
            .cookies
            .iter()
            .any(|cookie| cookie.name == expected_name);
        assert!(
            (observed.version == before.version && !contains_commit)
                || (observed.version == before.version + 1 && contains_commit),
            "snapshot mixed cookie state and version"
        );
    }
    worker.close().await.unwrap();
    fixture.abort();
}

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
        resolve_upload_paths(&[allowed], std::slice::from_ref(&file)).unwrap(),
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
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
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
                target: None,
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
                target: None,
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
        executable: Some(chrome_executable()),
        profiles_dir: profiles.path().to_path_buf(),
        headless: true,
        max_active: 8,
        upload_roots: vec![profiles.path().to_path_buf()],
        downloads_dir: profiles.path().join("downloads"),
        artifacts_dir: profiles.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
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
                target: None,
                value: "Ada".into(),
                clear_first: true,
                expected_url: None,
            },
        )
        .await
        .unwrap();
    let evidence = worker
        .inspect(
            &page_id,
            &InspectCommand {
                selector: Some("#name".into()),
                target: None,
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
                target: None,
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

/// Typing into a readonly, disabled, or otherwise non-editable control must
/// fail fast with `InvalidRequest` rather than dispatch key events at it —
/// on macOS headless Chrome, dispatching keys into a target that never
/// accepts them pins the browser process at 100% CPU permanently.
#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn refuses_to_type_into_readonly_and_disabled_controls() {
    let profiles = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: profiles.path().to_path_buf(),
        headless: true,
        max_active: 8,
        upload_roots: vec![profiles.path().to_path_buf()],
        downloads_dir: profiles.path().join("downloads"),
        artifacts_dir: profiles.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: "data:text/html,<input id=ro readonly value=ro><input id=dis disabled><input id=ok>".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

    fn type_command(selector: &str) -> TypeTextCommand {
        TypeTextCommand {
            selector: selector.into(),
            target: None,
            value: "x".into(),
            clear_first: false,
            expected_url: None,
        }
    }

    let readonly_result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        worker.type_text(&page_id, &type_command("#ro")),
    )
    .await
    .expect("typing into a readonly input must not hang");
    let error = readonly_result.expect_err("typing into a readonly input must be refused");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(!error.retryable);

    let disabled_result = tokio::time::timeout(
        std::time::Duration::from_secs(15),
        worker.type_text(&page_id, &type_command("#dis")),
    )
    .await
    .expect("typing into a disabled input must not hang");
    let error = disabled_result.expect_err("typing into a disabled input must be refused");
    assert_eq!(error.code, ErrorCode::InvalidRequest);
    assert!(!error.retryable);

    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        worker.type_text(&page_id, &type_command("#ok")),
    )
    .await
    .expect("typing into an editable input must not hang")
    .expect("typing into an editable input must succeed");

    worker.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn types_unicode_and_newlines_into_a_textarea() {
    let profiles = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: profiles.path().to_path_buf(),
        headless: true,
        max_active: 8,
        upload_roots: vec![profiles.path().to_path_buf()],
        downloads_dir: profiles.path().join("downloads"),
        artifacts_dir: profiles.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let worker = factory.launch(&SessionId::new()).await.unwrap();
        let page_id = PageId::new();
        worker.open_page(page_id.clone()).await.unwrap();
        worker
            .navigate(
                &page_id,
                &NavigateCommand {
                    url: "data:text/html,<textarea id='t'></textarea><input id='i'>".into(),
                    wait_until: WaitUntil::Interactive,
                    timeout_ms: 10_000,
                },
            )
            .await
            .unwrap();
        let textarea_value = "Zoë 日本 🧪\nline2";
        worker
            .type_text(
                &page_id,
                &TypeTextCommand {
                    selector: "#t".into(),
                    target: None,
                    value: textarea_value.into(),
                    clear_first: true,
                    expected_url: None,
                },
            )
            .await
            .unwrap();
        let input_value = "naïve";
        worker
            .type_text(
                &page_id,
                &TypeTextCommand {
                    selector: "#i".into(),
                    target: None,
                    value: input_value.into(),
                    clear_first: true,
                    expected_url: None,
                },
            )
            .await
            .unwrap();
        let textarea_evidence = worker
            .inspect(
                &page_id,
                &InspectCommand {
                    selector: Some("#t".into()),
                    target: None,
                    include_html: false,
                },
            )
            .await
            .unwrap();
        let textarea_text = match &textarea_evidence[0] {
            Evidence::Inspection { text, .. } => text.clone(),
            other => panic!("unexpected evidence: {other:?}"),
        };
        assert_eq!(textarea_text, textarea_value);
        let input_evidence = worker
            .inspect(
                &page_id,
                &InspectCommand {
                    selector: Some("#i".into()),
                    target: None,
                    include_html: false,
                },
            )
            .await
            .unwrap();
        let input_text = match &input_evidence[0] {
            Evidence::Inspection { text, .. } => text.clone(),
            other => panic!("unexpected evidence: {other:?}"),
        };
        assert_eq!(input_text, input_value);
        worker.close().await.unwrap();
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn type_text_reports_the_typed_control_kind_and_the_committed_value() {
    let profiles = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: profiles.path().to_path_buf(),
        headless: true,
        max_active: 8,
        upload_roots: vec![profiles.path().to_path_buf()],
        downloads_dir: profiles.path().join("downloads"),
        artifacts_dir: profiles.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let worker = factory.launch(&SessionId::new()).await.unwrap();
        let page_id = PageId::new();
        worker.open_page(page_id.clone()).await.unwrap();
        worker
            .navigate(
                &page_id,
                &NavigateCommand {
                    url: "data:text/html,<input id='text' value='prefilled'><input id='cb' type='checkbox'><select id='sel'><option value='basic'>Basic</option><option value='pro'>Pro plan</option></select>".into(),
                    wait_until: WaitUntil::Interactive,
                    timeout_ms: 10_000,
                },
            )
            .await
            .unwrap();

        fn element_text(evidence: &[Evidence]) -> Option<String> {
            evidence.iter().find_map(|item| match item {
                Evidence::Element { text, .. } => text.clone(),
                _ => None,
            })
        }
        fn typed_control_kind(evidence: &[Evidence]) -> Option<String> {
            evidence.iter().find_map(|item| match item {
                Evidence::Configuration { name, value } if name == "typedControlKind" => {
                    Some(value.clone())
                }
                _ => None,
            })
        }

        let text_evidence = worker
            .type_text(
                &page_id,
                &TypeTextCommand {
                    selector: "#text".into(),
                    target: None,
                    value: "x".into(),
                    clear_first: false,
                    expected_url: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(
            element_text(&text_evidence).as_deref(),
            Some("prefilledx")
        );
        assert_eq!(typed_control_kind(&text_evidence).as_deref(), Some("text"));

        let checkbox_evidence = worker
            .type_text(
                &page_id,
                &TypeTextCommand {
                    selector: "#cb".into(),
                    target: None,
                    value: "true".into(),
                    clear_first: false,
                    expected_url: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(element_text(&checkbox_evidence).as_deref(), Some("true"));
        assert_eq!(
            typed_control_kind(&checkbox_evidence).as_deref(),
            Some("checkable")
        );

        let select_evidence = worker
            .type_text(
                &page_id,
                &TypeTextCommand {
                    selector: "#sel".into(),
                    target: None,
                    value: "Pro plan".into(),
                    clear_first: false,
                    expected_url: None,
                },
            )
            .await
            .unwrap();
        assert_eq!(element_text(&select_evidence).as_deref(), Some("pro"));
        assert_eq!(
            typed_control_kind(&select_evidence).as_deref(),
            Some("select")
        );

        worker.close().await.unwrap();
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn control_action_set_text_without_clear_first_still_clears_before_typing() {
    let profiles = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: profiles.path().to_path_buf(),
        headless: true,
        max_active: 1,
        upload_roots: vec![profiles.path().to_path_buf()],
        downloads_dir: profiles.path().join("downloads"),
        artifacts_dir: profiles.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: "data:text/html,<input id=name aria-label=Name value=prefilled>".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

    let form_snapshot = worker.form_snapshot(&page_id, None).await.unwrap();
    let snapshot = form_snapshot
        .iter()
        .find_map(|item| match item {
            Evidence::FormSnapshot { snapshot } => Some(snapshot),
            _ => None,
        })
        .expect("form snapshot evidence");
    let name_field = snapshot
        .unowned_controls
        .iter()
        .find(|control| control.accessible_name.as_deref() == Some("Name"))
        .expect("Name control");
    let target = name_field.target.clone().unwrap();

    // The wire body carries no `clearFirst`; ControlAction::SetText must still
    // default to replace semantics (matches the previously hard-coded true).
    let action: ControlAction =
        serde_json::from_value(serde_json::json!({"kind":"setText","value":"x"})).unwrap();
    let evidence = worker
        .control_action(&page_id, &ControlActionCommand { target, action })
        .await
        .unwrap();
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::ControlAction { action }
            if action.state == types::FormControlState::Text { value: "x".into() }
    )));

    worker.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn control_action_clear_empties_a_text_input() {
    let profiles = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: profiles.path().to_path_buf(),
        headless: true,
        max_active: 1,
        upload_roots: vec![profiles.path().to_path_buf()],
        downloads_dir: profiles.path().join("downloads"),
        artifacts_dir: profiles.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: "data:text/html,<input id=name aria-label=Name value=prefilled>".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

    let form_snapshot = worker.form_snapshot(&page_id, None).await.unwrap();
    let snapshot = form_snapshot
        .iter()
        .find_map(|item| match item {
            Evidence::FormSnapshot { snapshot } => Some(snapshot),
            _ => None,
        })
        .expect("form snapshot evidence");
    let name_field = snapshot
        .unowned_controls
        .iter()
        .find(|control| control.accessible_name.as_deref() == Some("Name"))
        .expect("Name control");
    let target = name_field.target.clone().unwrap();

    // Text inputs also expose a prototype `checked` property; clear must reach
    // the value branch, not the checkbox one.
    let evidence = worker
        .control_action(
            &page_id,
            &ControlActionCommand {
                target,
                action: ControlAction::Clear,
            },
        )
        .await
        .unwrap();
    assert!(evidence.iter().any(|item| matches!(
        item,
        Evidence::ControlAction { action }
            if action.state == types::FormControlState::Empty
                || action.state == types::FormControlState::Text { value: String::new() }
    )));

    worker.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn semantic_targets_fail_closed_and_reresolve_after_replacement() {
    let root = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: "data:text/html,<label for='email'>Email address</label><input id='email'><button aria-label='Continue' onclick=\"this.outerHTML='<button aria-label=Continue>Continue</button>'\">old</button><button aria-label='Duplicate'>one</button><button aria-label='Duplicate'>two</button>".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

    let typed = worker
        .type_text(
            &page_id,
            &TypeTextCommand {
                expected_url: None,
                selector: String::new(),
                target: Some(TargetSpec {
                    label: Some("Email address".into()),
                    ..TargetSpec::default()
                }),
                value: "ada@example.test".into(),
                clear_first: true,
            },
        )
        .await
        .unwrap();
    assert!(typed
        .iter()
        .any(|item| matches!(item, Evidence::Resolution { .. })));

    let continue_target = TargetSpec {
        role: Some("button".into()),
        accessible_name: Some("Continue".into()),
        ..TargetSpec::default()
    };
    for _ in 0..2 {
        worker
            .click(
                &page_id,
                &ClickCommand {
                    selector: String::new(),
                    target: Some(continue_target.clone()),
                    boundary: false,
                    expected_url: None,
                    modifiers: Vec::new(),
                },
            )
            .await
            .unwrap();
    }

    let error = worker
        .click(
            &page_id,
            &ClickCommand {
                selector: String::new(),
                target: Some(TargetSpec {
                    role: Some("button".into()),
                    accessible_name: Some("Duplicate".into()),
                    ..TargetSpec::default()
                }),
                boundary: false,
                expected_url: None,
                modifiers: Vec::new(),
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::TargetAmbiguous);
    worker.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn form_controls_have_normalized_roles_names_constraints_and_native_selection() {
    let root = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: "data:text/html,<span id=email-label>Email address</span><input id=email aria-labelledby=email-label required pattern='[^@]+@[^@]+' autocomplete=email><label><input id=updates type=checkbox>Product updates</label><label><input id=pro type=radio name=plan value=pro>Professional</label><select id=region aria-label=Region><option value=us>United States</option><optgroup label=Blocked disabled><option value=ca>Canada</option></optgroup></select><label for=phone-home>Phone</label><input id=phone-home><label for=phone-work>Phone</label><input id=phone-work><label for=password>Password</label><input id=password type=password autocomplete=current-password value=vault-secret-92 required><form aria-label=Application><button aria-label=Apply>Apply</button><input type=button aria-label=Preview><input role=combobox aria-label=City value=Boston></form>".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

    worker
        .evaluate_javascript(
            &page_id,
            &EvaluateJavaScriptCommand {
                expression: "password.setCustomValidity(password.value); const input=document.createElement('input'); input.setAttribute('aria-label','😀'.repeat(700)+'\\n'); document.body.append(input); true".into(),
                await_promise: false,
                timeout_ms: 5_000,
            },
        )
        .await
        .unwrap();

    let form_snapshot = worker.form_snapshot(&page_id, None).await.unwrap();
    let snapshot = form_snapshot
        .iter()
        .find_map(|item| match item {
            Evidence::FormSnapshot { snapshot } => Some(snapshot),
            _ => None,
        })
        .expect("form snapshot evidence");
    assert_eq!(snapshot.page_id, page_id);
    assert_eq!(snapshot.unowned_controls.len(), 8);
    let email = snapshot
        .unowned_controls
        .iter()
        .find(|control| control.accessible_name.as_deref() == Some("Email address"))
        .expect("aria-labelledby control");
    assert!(email.constraints.required);
    let region = snapshot
        .unowned_controls
        .iter()
        .find(|control| control.accessible_name.as_deref() == Some("Region"))
        .expect("select control");
    assert_eq!(region.options.len(), 2);
    assert!(region.options[1].disabled, "disabled optgroup is effective");
    let application = snapshot.forms.first().expect("owned application form");
    assert_eq!(application.submit_control_ids.len(), 1);
    let preview = application
        .controls
        .iter()
        .find(|control| control.accessible_name.as_deref() == Some("Preview"))
        .unwrap();
    assert_eq!(
        preview.supported_operations,
        vec![types::FormControlOperation::Activate]
    );
    let city = application
        .controls
        .iter()
        .find(|control| control.accessible_name.as_deref() == Some("City"))
        .unwrap();
    assert!(matches!(city.state, types::FormControlState::Text { ref value } if value == "Boston"));
    assert!(city
        .supported_operations
        .contains(&types::FormControlOperation::SetText));

    let updates = snapshot
        .unowned_controls
        .iter()
        .find(|control| control.accessible_name.as_deref() == Some("Product updates"))
        .unwrap();
    let action_evidence = worker
        .control_action(
            &page_id,
            &ControlActionCommand {
                target: updates.target.clone().unwrap(),
                action: ControlAction::SetChecked { checked: true },
            },
        )
        .await
        .unwrap();
    assert!(action_evidence.iter().any(|item| matches!(
        item,
        Evidence::ControlAction { action }
            if action.state == types::FormControlState::Checked { checked: true }
    )));
    let encoded = serde_json::to_string(snapshot).unwrap();
    assert!(!encoded.contains("vault-secret-92"));
    assert!(!encoded.contains("cssPath"));
    assert!(!encoded.contains("selector"));

    let candidates = worker
        .collect_candidates(&page_id, &TargetSpec::default())
        .await
        .unwrap();
    let email = candidates
        .iter()
        .find(|item| item.css.as_deref() == Some("#email"))
        .unwrap();
    assert_eq!(email.name.as_deref(), Some("Email address"));
    assert_eq!(
        email.attributes.get("required").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        email.attributes.get("autocomplete").map(String::as_str),
        Some("email")
    );
    assert_eq!(
        candidates
            .iter()
            .find(|item| item.css.as_deref() == Some("#updates"))
            .and_then(|item| item.role.as_deref()),
        Some("checkbox")
    );
    assert_eq!(
        candidates
            .iter()
            .find(|item| item.css.as_deref() == Some("#pro"))
            .and_then(|item| item.role.as_deref()),
        Some("radio")
    );

    let invalid = worker
        .type_text(
            &page_id,
            &TypeTextCommand {
                expected_url: None,
                selector: String::new(),
                target: Some(TargetSpec {
                    role: Some("textbox".into()),
                    accessible_name: Some("Email address".into()),
                    ..TargetSpec::default()
                }),
                value: "not-an-email".into(),
                clear_first: true,
            },
        )
        .await
        .unwrap();
    assert!(invalid.iter().any(|item| matches!(
        item,
        Evidence::Configuration { name, value }
            if name == "formControlValid" && value == "false"
    )));
    assert!(invalid.iter().any(|item| matches!(
        item,
        Evidence::Configuration { name, value }
            if name == "formControlValidationMessage" && !value.is_empty()
    )));

    let snapshot = worker
        .a11y_snapshot(
            &page_id,
            &AccessibilitySnapshotCommand {
                max_nodes: Some(128),
                target: None,
            },
        )
        .await
        .unwrap();
    let nodes = snapshot
        .iter()
        .find_map(|item| match item {
            Evidence::AccessibilitySnapshot { nodes, .. } => Some(nodes),
            _ => None,
        })
        .expect("accessibility snapshot evidence");
    fn find_form_node<'a>(
        nodes: &'a [AccessibilityNode],
        name: &str,
    ) -> Option<&'a AccessibilityNode> {
        nodes.iter().find_map(|node| {
            (node.name.as_deref() == Some(name) && node.value.is_some())
                .then_some(node)
                .or_else(|| find_form_node(&node.children, name))
        })
    }
    let email = find_form_node(nodes, "Email address").expect("email form node");
    assert_eq!(email.value.as_deref(), Some("not-an-email"));
    assert_eq!(email.required, Some(true));
    assert_eq!(email.invalid, Some(true));
    let password = find_form_node(nodes, "Password").expect("password form node");
    assert_eq!(password.value.as_deref(), Some("[redacted]"));
    assert!(!serde_json::to_string(nodes)
        .unwrap()
        .contains("vault-secret-92"));

    fn collect_named<'a>(
        nodes: &'a [AccessibilityNode],
        name: &str,
        output: &mut Vec<&'a AccessibilityNode>,
    ) {
        for node in nodes {
            if node.name.as_deref() == Some(name) && node.target.is_some() {
                output.push(node);
            }
            collect_named(&node.children, name, output);
        }
    }
    let mut phones = Vec::new();
    collect_named(nodes, "Phone", &mut phones);
    assert_eq!(phones.len(), 2);
    assert_eq!(phones[0].target.as_ref().unwrap().ordinal, Some(0));
    assert_eq!(phones[1].target.as_ref().unwrap().ordinal, Some(1));
    let work_phone = phones[1].target.as_ref().unwrap();
    worker
        .type_text(
            &page_id,
            &TypeTextCommand {
                expected_url: None,
                selector: String::new(),
                target: Some(TargetSpec {
                    role: Some(work_phone.role.clone()),
                    accessible_name: Some(work_phone.accessible_name.clone()),
                    ordinal: work_phone.ordinal,
                    ..TargetSpec::default()
                }),
                value: "555-0102".into(),
                clear_first: true,
            },
        )
        .await
        .unwrap();
    let candidates = worker
        .collect_candidates(&page_id, &TargetSpec::default())
        .await
        .unwrap();
    assert_eq!(
        candidates
            .iter()
            .find(|candidate| candidate.css.as_deref() == Some("#phone-home"))
            .map(|candidate| candidate.text.as_str()),
        Some("")
    );
    assert_eq!(
        candidates
            .iter()
            .find(|candidate| candidate.css.as_deref() == Some("#phone-work"))
            .map(|candidate| candidate.text.as_str()),
        Some("555-0102")
    );

    worker
        .type_text(
            &page_id,
            &TypeTextCommand {
                expected_url: None,
                selector: String::new(),
                target: Some(TargetSpec {
                    role: Some("combobox".into()),
                    accessible_name: Some("Region".into()),
                    ..TargetSpec::default()
                }),
                value: "ca".into(),
                clear_first: true,
            },
        )
        .await
        .unwrap();
    let observed = worker
        .inspect(
            &page_id,
            &InspectCommand {
                selector: Some("#region".into()),
                target: None,
                include_html: false,
            },
        )
        .await
        .unwrap();
    assert!(observed
        .iter()
        .any(|item| matches!(item, Evidence::Inspection { text, .. } if text == "ca")));
    for (role, name) in [("checkbox", "Product updates"), ("radio", "Professional")] {
        let evidence = worker
            .type_text(
                &page_id,
                &TypeTextCommand {
                    expected_url: None,
                    selector: String::new(),
                    target: Some(TargetSpec {
                        role: Some(role.into()),
                        accessible_name: Some(name.into()),
                        ..TargetSpec::default()
                    }),
                    value: "true".into(),
                    clear_first: true,
                },
            )
            .await
            .unwrap();
        assert!(evidence.iter().any(
            |item| matches!(item, Evidence::Element { text: Some(value), .. } if value == "true")
        ));
    }
    worker.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn control_action_activates_a11y_snapshot_button_targets_outside_a_form() {
    let root = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: "data:text/html,<button id=plain>Plain</button><div role=button tabindex=0 id=divbtn>Div</div><a role=button href=%23 id=anchorbtn>Anchor</a><button aria-label=Labelled id=labelled>x</button><button id=padded> Padded </button>".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

    let snapshot = worker
        .a11y_snapshot(
            &page_id,
            &AccessibilitySnapshotCommand {
                max_nodes: Some(128),
                target: None,
            },
        )
        .await
        .unwrap();
    let nodes = snapshot
        .iter()
        .find_map(|item| match item {
            Evidence::AccessibilitySnapshot { nodes, .. } => Some(nodes),
            _ => None,
        })
        .expect("accessibility snapshot evidence");

    fn find_button_node<'a>(
        nodes: &'a [AccessibilityNode],
        name: &str,
    ) -> Option<&'a AccessibilityNode> {
        nodes.iter().find_map(|node| {
            (node.role.as_deref() == Some("button") && node.name.as_deref() == Some(name))
                .then_some(node)
                .or_else(|| find_button_node(&node.children, name))
        })
    }

    let mut failures = Vec::new();
    for name in ["Plain", "Div", "Anchor", "Labelled", "Padded"] {
        let node = find_button_node(nodes, name)
            .unwrap_or_else(|| panic!("no a11y button node named {name}"));
        let ax_target = node
            .target
            .as_ref()
            .unwrap_or_else(|| panic!("button {name} has no command-ready target"));
        let target = types::FormControlTarget {
            role: ax_target.role.clone(),
            accessible_name: ax_target.accessible_name.clone(),
            ordinal: ax_target.ordinal,
            frame_path: ax_target.frame_path.clone(),
            shadow_path: Vec::new(),
        };
        let result = worker
            .control_action(
                &page_id,
                &ControlActionCommand {
                    target,
                    action: ControlAction::Activate,
                },
            )
            .await;
        if let Err(error) = result {
            failures.push(format!("{name}: {error:?}"));
        }
    }
    worker.close().await.unwrap();
    assert!(failures.is_empty(), "activate failed for: {failures:?}");
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn waits_for_dynamic_element_content_url_document_and_network_quiet() {
    let root = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: concat!(
                    "data:text/html,",
                    "<div id=ready-flag hidden>booting</div>",
                    "<input aria-label=State value=booting>",
                    "<script>",
                    "setTimeout(function(){",
                    "var el=document.getElementById('ready-flag');",
                    "el.removeAttribute('hidden');",
                    "el.textContent='ready';",
                    "document.querySelector('input').value='ready';",
                    "},100);",
                    "</script>"
                )
                .into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();
    let status = TargetSpec {
        css: Some("#ready-flag".into()),
        ..TargetSpec::default()
    };
    for condition in [
        WaitCondition::Element {
            target: Box::new(status.clone()),
            state: ElementState::Visible,
        },
        WaitCondition::Text {
            target: Box::new(status),
            matcher: TextMatch::Exact("ready".into()),
        },
        WaitCondition::Value {
            target: Box::new(TargetSpec {
                accessible_name: Some("State".into()),
                ..TargetSpec::default()
            }),
            matcher: TextMatch::Contains("ready".into()),
        },
        WaitCondition::Url {
            matcher: TextMatch::Contains("data:text/html".into()),
        },
        WaitCondition::Document {
            ready: WaitUntil::Interactive,
        },
        WaitCondition::NetworkQuiet {
            idle_ms: 50,
            max_in_flight: 0,
            ignore_url_substrings: Vec::new(),
            ignore_resource_types: Vec::new(),
            ignore_long_lived: false,
        },
    ] {
        let evidence = worker
            .wait_for(
                &page_id,
                &WaitForCommand {
                    condition,
                    timeout_ms: 2_000,
                },
            )
            .await
            .unwrap();
        assert!(matches!(&evidence[0], Evidence::Wait { observations, .. } if *observations > 0));
    }
    worker.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn page_scoped_text_wait_sees_async_body_updates() {
    let root = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: concat!(
                    "data:text/html,",
                    "<section><h1>Atlas Labs</h1>",
                    "<button id=save type=button>Save priority</button>",
                    "</section>",
                    "<script>",
                    "document.getElementById('save').addEventListener('click',function(){",
                    "setTimeout(function(){",
                    "var n=document.createElement('p');",
                    "n.setAttribute('role','status');",
                    "n.textContent='Priority saved';",
                    "document.body.appendChild(n);",
                    "},150);",
                    "});",
                    "</script>"
                )
                .into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();
    worker
        .click(
            &page_id,
            &ClickCommand {
                selector: "#save".into(),
                target: None,
                boundary: false,
                expected_url: None,
                modifiers: Vec::new(),
            },
        )
        .await
        .unwrap();
    for target in [
        TargetSpec {
            role: Some("main".into()),
            ..TargetSpec::default()
        },
        TargetSpec {
            css: Some("body".into()),
            ..TargetSpec::default()
        },
    ] {
        let evidence = worker
            .wait_for(
                &page_id,
                &WaitForCommand {
                    condition: WaitCondition::Text {
                        target: Box::new(target),
                        matcher: TextMatch::Contains("Priority saved".into()),
                    },
                    timeout_ms: 2_000,
                },
            )
            .await
            .unwrap();
        assert!(matches!(
            &evidence[0],
            Evidence::Wait {
                observations,
                ..
            } if *observations > 0
        ));
    }
    worker.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn click_modifiers_reach_chromium_native_mouse_events() {
    let root = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: concat!(
                    "data:text/html,",
                    "<button id=target type=button>click</button><output id=result></output>",
                    "<script>target.addEventListener('click',e=>{result.textContent=JSON.stringify({shift:e.shiftKey,ctrl:e.ctrlKey,alt:e.altKey,meta:e.metaKey})})</script>"
                )
                .into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

    worker
        .click(
            &page_id,
            &ClickCommand {
                selector: "#target".into(),
                target: None,
                boundary: false,
                expected_url: None,
                modifiers: vec![ClickModifier::Shift, ClickModifier::Alt],
            },
        )
        .await
        .unwrap();
    let evidence = worker
        .inspect(
            &page_id,
            &InspectCommand {
                selector: Some("#result".into()),
                target: None,
                include_html: false,
            },
        )
        .await
        .unwrap();
    assert!(matches!(
        &evidence[0],
        Evidence::Inspection { text, .. }
            if text == "{\"shift\":true,\"ctrl\":false,\"alt\":true,\"meta\":false}"
    ));
    worker.close().await.unwrap();
}

/// A click that opens alert()/confirm()/prompt() must not hang the click
/// call itself: the renderer is blocked showing the dialog, so the click's
/// own CDP round trip may never get a response. `click` must return with
/// `dialogOpened` evidence instead, and the dialog it recorded must still be
/// there for a later `dialog` call to consume — the page must not be left
/// dead afterward.
#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn click_returns_when_a_dialog_opens_and_dialog_accepts_it() {
    let root = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: concat!(
                    "data:text/html,",
                    "<button id=a onclick=\"alert('hi')\">A</button><div id=o></div>"
                )
                .into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

    let click_evidence = tokio::time::timeout(
        std::time::Duration::from_secs(20),
        worker.click(
            &page_id,
            &ClickCommand {
                selector: "#a".into(),
                target: None,
                boundary: false,
                expected_url: None,
                modifiers: vec![],
            },
        ),
    )
    .await
    .expect("click must return well before the dialog blocks it forever")
    .expect("click must succeed even though it opened a dialog");
    assert!(
        click_evidence.iter().any(|evidence| matches!(
            evidence,
            Evidence::Configuration { name, .. } if name == "dialogOpened"
        )),
        "click evidence missing dialogOpened: {click_evidence:?}"
    );

    let dialog_evidence = worker
        .handle_dialog(
            &page_id,
            &HandleDialogCommand {
                action: DialogAction::Accept,
                timeout_ms: Some(2_000),
            },
        )
        .await
        .expect("the dialog recorded by the click must still be there to accept");
    assert!(
        dialog_evidence.iter().any(
            |evidence| matches!(evidence, Evidence::Dialog { message, .. } if message == "hi")
        ),
        "dialog evidence missing the alert message: {dialog_evidence:?}"
    );

    worker
        .inspect(
            &page_id,
            &InspectCommand {
                selector: Some("#o".into()),
                target: None,
                include_html: false,
            },
        )
        .await
        .expect("the page must still be alive after the dialog is handled");
    worker.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn plain_click_completes_on_a_background_page() {
    let root = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();

    let page_a = PageId::new();
    worker.open_page(page_a.clone()).await.unwrap();
    worker
        .navigate(
            &page_a,
            &NavigateCommand {
                url: concat!(
                    "data:text/html,",
                    "<button id=b onclick=\"document.getElementById('o').textContent=",
                    "'clicked'\">B</button><div id=o></div>"
                )
                .into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

    // Backgrounds page A: opening a second page brings it to the front.
    let page_b = PageId::new();
    worker.open_page(page_b.clone()).await.unwrap();
    worker
        .navigate(
            &page_b,
            &NavigateCommand {
                url: "data:text/html,<p>B</p>".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

    tokio::time::timeout(
        std::time::Duration::from_secs(15),
        worker.click(
            &page_a,
            &ClickCommand {
                selector: "#b".into(),
                target: None,
                boundary: false,
                expected_url: None,
                modifiers: vec![],
            },
        ),
    )
    .await
    .expect("plain click on a backgrounded page must not hang until the envelope deadline")
    .expect("plain click on a backgrounded page must succeed");

    let inspect_evidence = worker
        .inspect(
            &page_a,
            &InspectCommand {
                selector: Some("#o".into()),
                target: None,
                include_html: false,
            },
        )
        .await
        .unwrap();
    assert!(
        inspect_evidence.iter().any(|evidence| matches!(
            evidence,
            Evidence::Inspection { text, .. } if text == "clicked"
        )),
        "expected #o to read 'clicked' after the background click: {inspect_evidence:?}"
    );

    worker.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn network_quiet_respects_url_and_long_lived_ignores() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: format!("{}/network-quiet", fixture.base_url()),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();
    worker
        .wait_for(
            &page_id,
            &WaitForCommand {
                condition: WaitCondition::Text {
                    target: Box::new(TargetSpec {
                        css: Some("#status".into()),
                        ..TargetSpec::default()
                    }),
                    matcher: TextMatch::Exact("armed".into()),
                },
                timeout_ms: 5_000,
            },
        )
        .await
        .unwrap();

    let without_ignores = worker
        .wait_for(
            &page_id,
            &WaitForCommand {
                condition: WaitCondition::NetworkQuiet {
                    idle_ms: 50,
                    max_in_flight: 0,
                    ignore_url_substrings: Vec::new(),
                    ignore_resource_types: Vec::new(),
                    ignore_long_lived: false,
                },
                timeout_ms: 750,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(without_ignores.code, ErrorCode::WaitConditionTimedOut);

    let evidence = worker
        .wait_for(
            &page_id,
            &WaitForCommand {
                condition: WaitCondition::NetworkQuiet {
                    idle_ms: 50,
                    max_in_flight: 0,
                    ignore_url_substrings: vec!["analytics".into()],
                    ignore_resource_types: Vec::new(),
                    ignore_long_lived: true,
                },
                timeout_ms: 5_000,
            },
        )
        .await
        .unwrap();
    let Evidence::Wait {
        excluded_classes, ..
    } = &evidence[0]
    else {
        panic!("expected wait evidence, got {evidence:?}");
    };
    assert!(
        excluded_classes
            .iter()
            .any(|class| class == "urlSubstring:analytics"),
        "excluded_classes={excluded_classes:?}"
    );
    assert!(
        excluded_classes
            .iter()
            .any(|class| class == "eventSource" || class == "websocket" || class == "longLived"),
        "excluded_classes={excluded_classes:?}"
    );
    worker.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn captures_viewport_full_page_element_and_clip_as_private_artifacts() {
    let root = tempfile::tempdir().unwrap();
    let session_id = SessionId::new();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&session_id).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: "data:text/html,<main style='height:1200px'><button aria-label=Capture>proof</button></main>".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();
    let modes = [
        ScreenshotMode::Viewport,
        ScreenshotMode::FullPage,
        ScreenshotMode::Element {
            target: Box::new(TargetSpec {
                role: Some("button".into()),
                accessible_name: Some("Capture".into()),
                ..TargetSpec::default()
            }),
        },
        ScreenshotMode::Clip {
            x: 0.0,
            y: 0.0,
            width: 100.0,
            height: 100.0,
        },
    ];
    for mode in modes {
        let evidence = worker
            .capture_screenshot(&page_id, &CaptureScreenshotCommand { mode })
            .await
            .unwrap();
        let artifact_id = match &evidence[0] {
            Evidence::Screenshot {
                artifact_id,
                width,
                height,
                bytes,
                sha256,
                ..
            } => {
                assert!(*width > 0 && *height > 0 && *bytes > 0);
                assert_eq!(sha256.len(), 64);
                artifact_id
            }
            other => panic!("unexpected evidence: {other:?}"),
        };
        assert!(root
            .path()
            .join("artifacts")
            .join(session_id.0.to_string())
            .join(artifact_id)
            .join(format!("{artifact_id}.png"))
            .is_file());
    }
    worker.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn resolves_nested_cross_origin_frames_and_open_shadow_roots() {
    let fixture = test_site::spawn().await;
    let host = test_site::spawn_frame_host(&fixture.base_url()).await;
    let root = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: host.base_url(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

    let frame_path = vec![
        Box::new(TargetSpec {
            role: Some("iframe".into()),
            accessible_name: Some("Outer".into()),
            ..TargetSpec::default()
        }),
        Box::new(TargetSpec {
            role: Some("iframe".into()),
            accessible_name: Some("Cross".into()),
            ..TargetSpec::default()
        }),
    ];
    worker
        .type_text(
            &page_id,
            &TypeTextCommand {
                expected_url: None,
                selector: String::new(),
                target: Some(TargetSpec {
                    label: Some("Name".into()),
                    frame_path,
                    ..TargetSpec::default()
                }),
                value: "Ada".into(),
                clear_first: true,
            },
        )
        .await
        .unwrap();

    worker
        .click(
            &page_id,
            &ClickCommand {
                selector: String::new(),
                target: Some(TargetSpec {
                    role: Some("button".into()),
                    accessible_name: Some("Inside".into()),
                    shadow_path: vec![Box::new(TargetSpec {
                        css: Some("#host".into()),
                        ..TargetSpec::default()
                    })],
                    ..TargetSpec::default()
                }),
                boundary: false,
                expected_url: None,
                modifiers: Vec::new(),
            },
        )
        .await
        .unwrap();
    let evidence = worker
        .inspect(
            &page_id,
            &InspectCommand {
                selector: None,
                target: None,
                include_html: false,
            },
        )
        .await
        .unwrap();
    assert!(format!("{evidence:?}").contains("shadow-clicked"));
    worker.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn resolves_ambient_and_explicit_closed_shadow_roots() {
    let fixture = test_site::spawn().await;
    let host = test_site::spawn_frame_host(&fixture.base_url()).await;
    let root = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: host.base_url(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

    // Ambient purpose-based match inside a closed root (no shadow_path).
    worker
        .click(
            &page_id,
            &ClickCommand {
                selector: String::new(),
                target: Some(TargetSpec {
                    role: Some("button".into()),
                    accessible_name: Some("Closed Inside".into()),
                    ..TargetSpec::default()
                }),
                boundary: false,
                expected_url: None,
                modifiers: Vec::new(),
            },
        )
        .await
        .unwrap();
    let evidence = worker
        .inspect(
            &page_id,
            &InspectCommand {
                selector: None,
                target: None,
                include_html: false,
            },
        )
        .await
        .unwrap();
    assert!(format!("{evidence:?}").contains("closed-shadow-clicked"));

    // Explicit shadow_path into a closed root.
    worker
        .click(
            &page_id,
            &ClickCommand {
                selector: String::new(),
                target: Some(TargetSpec {
                    role: Some("button".into()),
                    accessible_name: Some("Closed Inside".into()),
                    shadow_path: vec![Box::new(TargetSpec {
                        css: Some("#closed-host".into()),
                        ..TargetSpec::default()
                    })],
                    ..TargetSpec::default()
                }),
                boundary: false,
                expected_url: None,
                modifiers: Vec::new(),
            },
        )
        .await
        .unwrap();

    // Mixed open-then-closed nesting via explicit shadow_path.
    worker
        .click(
            &page_id,
            &ClickCommand {
                selector: String::new(),
                target: Some(TargetSpec {
                    role: Some("button".into()),
                    accessible_name: Some("Mixed Closed Inside".into()),
                    shadow_path: vec![
                        Box::new(TargetSpec {
                            css: Some("#mixed-host".into()),
                            ..TargetSpec::default()
                        }),
                        Box::new(TargetSpec {
                            css: Some("#inner-closed-host".into()),
                            ..TargetSpec::default()
                        }),
                    ],
                    ..TargetSpec::default()
                }),
                boundary: false,
                expected_url: None,
                modifiers: Vec::new(),
            },
        )
        .await
        .unwrap();
    let evidence = worker
        .inspect(
            &page_id,
            &InspectCommand {
                selector: None,
                target: None,
                include_html: false,
            },
        )
        .await
        .unwrap();
    assert!(format!("{evidence:?}").contains("mixed-closed-clicked"));
    worker.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn evaluates_javascript_bounds_the_result_and_classifies_errors() {
    let root = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 16,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: "data:text/html,<title>JS eval fixture</title>".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

    // A small result passes through untouched.
    let evidence = worker
        .evaluate_javascript(
            &page_id,
            &EvaluateJavaScriptCommand {
                expression: "1 + 1".into(),
                timeout_ms: 5_000,
                await_promise: false,
            },
        )
        .await
        .unwrap();
    match evidence.as_slice() {
        [Evidence::JavaScriptResult { value, truncated }] => {
            assert_eq!(value, &serde_json::json!(2));
            assert!(!truncated);
        }
        other => panic!("expected a single JavaScriptResult evidence, got {other:?}"),
    }

    // A result larger than `max_js_result_bytes` (16 above) is truncated and flagged.
    let evidence = worker
        .evaluate_javascript(
            &page_id,
            &EvaluateJavaScriptCommand {
                expression: "'x'.repeat(1000)".into(),
                timeout_ms: 5_000,
                await_promise: false,
            },
        )
        .await
        .unwrap();
    match evidence.as_slice() {
        [Evidence::JavaScriptResult { value, truncated }] => {
            assert!(truncated);
            assert!(matches!(value, serde_json::Value::String(_)));
        }
        other => panic!("expected a single JavaScriptResult evidence, got {other:?}"),
    }

    // await_promise=true resolves an awaited promise's value.
    let evidence = worker
        .evaluate_javascript(
            &page_id,
            &EvaluateJavaScriptCommand {
                expression: "Promise.resolve(41 + 1)".into(),
                timeout_ms: 5_000,
                await_promise: true,
            },
        )
        .await
        .unwrap();
    match evidence.as_slice() {
        [Evidence::JavaScriptResult { value, truncated }] => {
            assert_eq!(value, &serde_json::json!(42));
            assert!(!truncated);
        }
        other => panic!("expected a single JavaScriptResult evidence, got {other:?}"),
    }

    // A JS exception surfaces as a failed (non-panicking) CommandError, not a panic.
    let error = worker
        .evaluate_javascript(
            &page_id,
            &EvaluateJavaScriptCommand {
                expression: "throw new Error('boom')".into(),
                timeout_ms: 5_000,
                await_promise: false,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::BrowserCommandFailed);

    // A near-zero timeout classifies as a deadline-exceeded, retryable error.
    let error = worker
        .evaluate_javascript(
            &page_id,
            &EvaluateJavaScriptCommand {
                expression: "new Promise(() => {})".into(),
                timeout_ms: 1,
                await_promise: true,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::DeadlineExceeded);
    assert!(error.retryable);

    worker.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn humanized_input_reaches_the_page_with_synthesized_timing() {
    let profiles = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: profiles.path().to_path_buf(),
        headless: true,
        max_active: 1,
        upload_roots: vec![profiles.path().to_path_buf()],
        downloads_dir: profiles.path().join("downloads"),
        artifacts_dir: profiles.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    // The probe counts keydowns and clicks: synthesized input that never
    // reaches the page fails here, not in a Rust-side assertion.
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: "data:text/html,<title>Humanize</title><input id='name'><button id='go'>Go</button><script>window.keydowns=0;window.repeats=0;window.lastkey='';window.clicks=0;document.addEventListener('keydown',(e)=>{window.keydowns++;if(e.repeat)window.repeats++;window.lastkey=e.key;if(!window.t0)window.t0=e.timeStamp;window.t1=e.timeStamp;const b=Math.floor(e.timeStamp/50)*50;window.hist[b]=(window.hist[b]||0)+1;});document.getElementById('go').addEventListener('click',()=>window.clicks++);</script>".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 30_000,
            },
        )
        .await
        .unwrap();
    worker.set_humanization_enabled(true).await.unwrap();

    let started = std::time::Instant::now();
    let evidence = worker
        .type_text(
            &page_id,
            &TypeTextCommand {
                selector: "#name".into(),
                target: None,
                value: "Ada".into(),
                clear_first: true,
                expected_url: None,
            },
        )
        .await
        .unwrap();
    let typed_ms = started.elapsed().as_millis();
    assert!(
        evidence.iter().any(|item| matches!(
            item,
            Evidence::Humanization { engine, actions, synthesized_ms }
                if engine == "behavioral-engine" && *actions > 0 && *synthesized_ms > 0
        )),
        "no Humanization evidence for synthesized typing: {evidence:?}"
    );
    assert!(
        typed_ms >= 300,
        "typing three characters with clear finished in {typed_ms}ms; a human burst cannot be instant"
    );

    worker
        .click(
            &page_id,
            &ClickCommand {
                selector: "#go".into(),
                target: None,
                boundary: false,
                expected_url: None,
                modifiers: Vec::new(),
            },
        )
        .await
        .unwrap();

    let evidence = worker
        .evaluate_javascript(
            &page_id,
            &EvaluateJavaScriptCommand {
                expression: "JSON.stringify({keydowns: window.keydowns, clicks: window.clicks, value: document.getElementById('name').value})".into(),
                timeout_ms: 10_000,
                await_promise: false,
            },
        )
        .await
        .unwrap();
    let probe = evidence
        .iter()
        .find_map(|item| match item {
            Evidence::JavaScriptResult { value, .. } => Some(value.clone()),
            _ => None,
        })
        .expect("javascript result evidence");
    let probe: serde_json::Value =
        serde_json::from_str(probe.as_str().expect("probe json")).unwrap();
    assert!(
        probe["keydowns"].as_u64().unwrap_or(0) >= 3,
        "page saw too few keydowns: {probe}"
    );
    assert_eq!(probe["clicks"].as_u64().unwrap_or(0), 1, "probe: {probe}");
    assert_eq!(probe["value"].as_str().unwrap_or_default(), "Ada");

    // Off means off: no Humanization evidence and direct input speed.
    worker.set_humanization_enabled(false).await.unwrap();
    let direct = worker
        .type_text(
            &page_id,
            &TypeTextCommand {
                selector: "#name".into(),
                target: None,
                value: "Bo".into(),
                clear_first: false,
                expected_url: None,
            },
        )
        .await
        .unwrap();
    assert!(
        !direct
            .iter()
            .any(|item| matches!(item, Evidence::Humanization { .. })),
        "Humanization evidence emitted with humanize off: {direct:?}"
    );
    worker.close().await.unwrap();
}

/// Dogfood the Chromium humanized stream as a detector would: inter-key
/// intervals must vary like a human's (no machine-uniform cadence, no
/// zero-ms chords), and the mouse path must not be a straight line.
#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn dogfood_humanized_stream_biometrics() {
    let profiles = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: profiles.path().to_path_buf(),
        headless: true,
        max_active: 1,
        upload_roots: vec![profiles.path().to_path_buf()],
        downloads_dir: profiles.path().join("downloads"),
        artifacts_dir: profiles.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: "data:text/html,<title>Dogfood</title><input id='f' style='position:absolute;left:400px;top:300px;width:200px'><script>window.keys=[];window.moves=[];document.addEventListener('keydown',e=>window.keys.push({k:e.key,t:e.timeStamp}));document.addEventListener('mousemove',e=>window.moves.push({x:e.clientX,y:e.clientY,t:e.timeStamp}));</script>".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 30_000,
            },
        )
        .await
        .unwrap();
    worker.set_humanization_enabled(true).await.unwrap();
    // Pool several rounds: per-round statistics are small-sample flaky, a
    // detector's cadence check is only meaningful on a pooled stream.
    for value in ["stream", "behavior", "input", "human"] {
        worker
            .type_text(
                &page_id,
                &TypeTextCommand {
                    selector: "#f".into(),
                    target: None,
                    value: value.into(),
                    clear_first: true,
                    expected_url: None,
                },
            )
            .await
            .unwrap();
    }
    worker
        .click(
            &page_id,
            &ClickCommand {
                selector: "#f".into(),
                target: None,
                boundary: false,
                expected_url: None,
                modifiers: Vec::new(),
            },
        )
        .await
        .unwrap();
    let evidence = worker
        .evaluate_javascript(
            &page_id,
            &EvaluateJavaScriptCommand {
                expression: "JSON.stringify({keys: window.keys, moves: window.moves})".into(),
                timeout_ms: 10_000,
                await_promise: false,
            },
        )
        .await
        .unwrap();
    let probe = evidence
        .iter()
        .find_map(|item| match item {
            Evidence::JavaScriptResult { value, .. } => Some(value.clone()),
            _ => None,
        })
        .expect("probe");
    let probe: serde_json::Value = serde_json::from_str(probe.as_str().unwrap()).unwrap();

    let keys = probe["keys"].as_array().expect("keys");
    // Rounds may legitimately produce bursts (a paste has no keydowns) or
    // typo corrections (extra keydowns); the detector-relevant floor is a
    // pooled stream big enough to measure cadence on.
    assert!(keys.len() >= 12, "too few key events: {keys:?}");
    let intervals: Vec<f64> = keys
        .windows(2)
        .map(|pair| pair[1]["t"].as_f64().unwrap() - pair[0]["t"].as_f64().unwrap())
        .collect();
    eprintln!("inter-key intervals (ms): {intervals:?}");
    assert!(
        intervals.iter().all(|interval| *interval > 10.0),
        "zero/burst intervals look synthetic: {intervals:?}"
    );
    let mean = intervals.iter().sum::<f64>() / intervals.len() as f64;
    let variance = intervals
        .iter()
        .map(|interval| (interval - mean).powi(2))
        .sum::<f64>()
        / intervals.len() as f64;
    assert!(
        variance.sqrt() > mean * 0.15,
        "uniform cadence (mean={mean:.1} sd={:.1}) fails a detector's variance check",
        variance.sqrt()
    );

    let moves = probe["moves"].as_array().expect("moves");
    eprintln!("mouse path: {} points", moves.len());
    assert!(
        moves.len() >= 4,
        "a real approach has curved intermediate points, got {moves:?}"
    );
    // Collinearity over the approach segment: walk back from the final move
    // to the last point away from the target; a straight-line approach has
    // every intermediate point on that segment, a bezier does not.
    let last = &moves[moves.len() - 1];
    let (tx, ty) = (last["x"].as_f64().unwrap(), last["y"].as_f64().unwrap());
    // Landings are clicks ending on the target; the approach is the widest
    // run of off-target moves between two of them (focus clicks have no
    // intermediates, the bezier approach does).
    let landings: Vec<usize> = moves
        .iter()
        .enumerate()
        .filter(|(_, point)| {
            (point["x"].as_f64().unwrap() - tx).abs() <= 1.0
                && (point["y"].as_f64().unwrap() - ty).abs() <= 1.0
        })
        .map(|(index, _)| index)
        .collect();
    let (start, end) = landings
        .windows(2)
        .max_by_key(|pair| pair[1] - pair[0])
        .map(|pair| (pair[0] + 1, pair[1]))
        .filter(|(start, end)| end - start >= 2)
        .expect("an approach segment with intermediates");
    let first = &moves[start];
    let (x1, y1) = (first["x"].as_f64().unwrap(), first["y"].as_f64().unwrap());
    let last = &moves[end];
    let (x2, y2) = (last["x"].as_f64().unwrap(), last["y"].as_f64().unwrap());
    let dx = x2 - x1;
    let dy = y2 - y1;
    let off_line = moves[start + 1..end].iter().any(|point| {
        let px = point["x"].as_f64().unwrap() - x1;
        let py = point["y"].as_f64().unwrap() - y1;
        (dx * py - dy * px).abs() > 1.0
    });
    assert!(off_line, "mouse path is a straight line: {moves:?}");
    worker.close().await.unwrap();
}

fn find_by_role<'a>(
    nodes: &'a [AccessibilityNode],
    roles: &[&str],
) -> Option<&'a AccessibilityNode> {
    for node in nodes {
        if node
            .role
            .as_deref()
            .is_some_and(|role| roles.contains(&role))
        {
            return Some(node);
        }
        if let Some(found) = find_by_role(&node.children, roles) {
            return Some(found);
        }
    }
    None
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn implicit_roles_from_the_dom_collector_match_the_a11y_snapshot() {
    let profiles = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: profiles.path().to_path_buf(),
        headless: true,
        max_active: 8,
        upload_roots: vec![profiles.path().to_path_buf()],
        downloads_dir: profiles.path().join("downloads"),
        artifacts_dir: profiles.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let worker = factory.launch(&SessionId::new()).await.unwrap();
        let page_id = PageId::new();
        worker.open_page(page_id.clone()).await.unwrap();
        worker
            .navigate(
                &page_id,
                &NavigateCommand {
                    url: "data:text/html,<h1>Title</h1><ul><li>One</li></ul>\
                          <img alt='Logo' src='data:,'>\
                          <table><tr><th>H</th><td>C</td></tr></table>"
                        .into(),
                    wait_until: WaitUntil::Interactive,
                    timeout_ms: 10_000,
                },
            )
            .await
            .unwrap();
        let snapshot = worker
            .a11y_snapshot(
                &page_id,
                &AccessibilitySnapshotCommand {
                    max_nodes: Some(256),
                    target: None,
                },
            )
            .await
            .unwrap();
        let nodes = snapshot
            .iter()
            .find_map(|item| match item {
                Evidence::AccessibilitySnapshot { nodes, .. } => Some(nodes),
                _ => None,
            })
            .expect("accessibility snapshot evidence");
        for roles in [
            vec!["heading"],
            vec!["listitem"],
            vec!["img", "image"],
            vec!["cell"],
        ] {
            let node = find_by_role(nodes, &roles)
                .unwrap_or_else(|| panic!("no node with role in {roles:?} in {nodes:#?}"));
            let target = TargetSpec {
                role: node.role.clone(),
                accessible_name: node.name.clone(),
                ..TargetSpec::default()
            };
            worker
                .inspect(
                    &page_id,
                    &InspectCommand {
                        selector: None,
                        target: Some(target),
                        include_html: false,
                    },
                )
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "role {roles:?} (emitted {:?}) failed to resolve: {error:?}",
                        node.role
                    )
                });
        }
        worker.close().await.unwrap();
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn a_css_selector_matching_nothing_is_reported_as_no_matching_element() {
    let profiles = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: profiles.path().to_path_buf(),
        headless: true,
        max_active: 8,
        upload_roots: vec![profiles.path().to_path_buf()],
        downloads_dir: profiles.path().join("downloads"),
        artifacts_dir: profiles.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    tokio::time::timeout(std::time::Duration::from_secs(20), async {
        let worker = factory.launch(&SessionId::new()).await.unwrap();
        let page_id = PageId::new();
        worker.open_page(page_id.clone()).await.unwrap();
        worker
            .navigate(
                &page_id,
                &NavigateCommand {
                    url: "data:text/html,<p>fresh page</p>".into(),
                    wait_until: WaitUntil::Interactive,
                    timeout_ms: 10_000,
                },
            )
            .await
            .unwrap();
        let error = worker
            .inspect(
                &page_id,
                &InspectCommand {
                    selector: Some("#nope".into()),
                    target: None,
                    include_html: false,
                },
            )
            .await
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::TargetNotFound);
        assert!(error.message.contains("no element matches selector"));
        assert!(!error.message.contains("stale"));
        worker.close().await.unwrap();
    })
    .await
    .expect("test timed out");
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn descends_unnamed_srcdoc_iframes_and_resolves_the_stamped_ordinal_hop() {
    let root = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: "data:text/html,<iframe srcdoc='<button>One</button>'></iframe><iframe srcdoc='<button>Two</button>'></iframe>".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

    let snapshot = worker
        .a11y_snapshot(
            &page_id,
            &AccessibilitySnapshotCommand {
                max_nodes: Some(128),
                target: None,
            },
        )
        .await
        .unwrap();
    let nodes = snapshot
        .iter()
        .find_map(|item| match item {
            Evidence::AccessibilitySnapshot { nodes, .. } => Some(nodes),
            _ => None,
        })
        .expect("accessibility snapshot evidence");

    fn find_iframe_nodes<'a>(nodes: &'a [AccessibilityNode], out: &mut Vec<&'a AccessibilityNode>) {
        for node in nodes {
            if node.role.as_deref() == Some("Iframe") {
                out.push(node);
            }
            find_iframe_nodes(&node.children, out);
        }
    }
    let mut iframe_nodes = Vec::new();
    find_iframe_nodes(nodes, &mut iframe_nodes);
    assert_eq!(iframe_nodes.len(), 2, "both iframes must appear: {nodes:?}");
    for iframe in &iframe_nodes {
        assert!(
            !iframe.children.is_empty(),
            "unnamed iframe must still be descended into: {iframe:?}"
        );
    }

    fn find_button_node<'a>(
        nodes: &'a [AccessibilityNode],
        name: &str,
    ) -> Option<&'a AccessibilityNode> {
        nodes.iter().find_map(|node| {
            (node.role.as_deref() == Some("button") && node.name.as_deref() == Some(name))
                .then_some(node)
                .or_else(|| find_button_node(&node.children, name))
        })
    }

    let two = find_button_node(nodes, "Two").expect("button Two inside the descended iframe");
    let ax_target = two
        .target
        .as_ref()
        .expect("button Two has a command-ready target");
    assert_eq!(ax_target.frame_path.len(), 1);
    assert_eq!(ax_target.frame_path[0].accessible_name, "");
    assert_eq!(ax_target.frame_path[0].ordinal, Some(1));

    // The snapshot's own stamped target, passed back exactly as an agent
    // would (a JSON round trip through `AccessibilityTarget`'s wire shape),
    // must validate and resolve even though the frame hop carries an empty
    // accessibleName.
    let value = serde_json::to_value(ax_target).expect("serialize accessibility target");
    let verbatim_target: TargetSpec =
        serde_json::from_value(value).expect("deserialize accessibility target as TargetSpec");
    worker
        .click(
            &page_id,
            &ClickCommand {
                selector: String::new(),
                target: Some(verbatim_target),
                boundary: false,
                expected_url: None,
                modifiers: Vec::new(),
            },
        )
        .await
        .unwrap();

    // A hand-built target using only role + ordinal (no accessibleName at
    // all) must resolve the same unnamed iframe by identity, not by the
    // name/src heuristic (both iframes have neither).
    let hand_built = TargetSpec {
        role: Some("button".into()),
        accessible_name: Some("Two".into()),
        frame_path: vec![Box::new(TargetSpec {
            role: Some("iframe".into()),
            ordinal: Some(1),
            ..TargetSpec::default()
        })],
        ..TargetSpec::default()
    };
    worker
        .inspect(
            &page_id,
            &InspectCommand {
                selector: None,
                target: Some(hand_built),
                include_html: false,
            },
        )
        .await
        .unwrap();

    worker.close().await.unwrap();
}

/// A hung `click_and_wait_for_popup` on one page must not block another
/// page's commands: the worker-wide browser mutex used to be held for the
/// whole click-plus-wait, up to `timeout_ms`, serializing every other page.
#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn a_hung_click_and_wait_for_popup_does_not_block_other_pages() {
    let root = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 8,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();

    let page_a = PageId::new();
    worker.open_page(page_a.clone()).await.unwrap();
    worker
        .navigate(
            &page_a,
            &NavigateCommand {
                url: "data:text/html,<button id='continue' type='button'>Continue</button>".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

    let opened = worker
        .open_page_command(&OpenPageCommand {
            url: Some("data:text/html,<title>Page B</title><button id='b'>B</button>".into()),
        })
        .await
        .unwrap();
    let page_b = match &opened[0] {
        types::Evidence::Page { page_id, .. } => page_id.clone(),
        other => panic!("unexpected evidence: {other:?}"),
    };

    // `#continue` never opens a popup, so this waits out the full timeout.
    let popup_worker = worker.clone();
    let popup_page = page_a.clone();
    let popup_task = tokio::spawn(async move {
        popup_worker
            .click_and_wait_for_popup(
                &popup_page,
                &ClickAndWaitForPopupCommand {
                    selector: "#continue".into(),
                    target: None,
                    timeout_ms: 6_000,
                },
            )
            .await
    });

    // Give the click a head start so it is inside its wait loop, then prove
    // page B is not blocked behind page A's still-pending popup wait. The
    // probe resolves a target, the path that used to take the worker-wide
    // browser mutex; a whole-page inspect never did and would pass either way.
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    let started = std::time::Instant::now();
    worker
        .inspect(
            &page_b,
            &InspectCommand {
                selector: None,
                target: Some(TargetSpec {
                    css: Some("#b".into()),
                    ..Default::default()
                }),
                include_html: false,
            },
        )
        .await
        .unwrap();
    let elapsed = started.elapsed();
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "inspect on page B took {elapsed:?} while page A's click_and_wait_for_popup was in \
         flight; the worker-wide browser mutex is still blocking other pages"
    );

    let popup_result = popup_task.await.unwrap();
    let error = popup_result.expect_err("no popup ever opens for #continue");
    assert_eq!(error.code, types::ErrorCode::DeadlineExceeded);

    worker.close().await.unwrap();
}
