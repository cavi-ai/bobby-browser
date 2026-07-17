use config::BrowserConfig;
use network_engine::state::{HttpCookie, ResponseStateDelta};
use std::collections::BTreeMap;
use std::path::PathBuf;
use types::{
    CaptureScreenshotCommand, ClickAndWaitForDownloadCommand, ClickAndWaitForPopupCommand,
    ClickCommand, ClosePageCommand, ElementState, ErrorCode, Evidence, InspectCommand,
    ListPagesCommand, NavigateCommand, OpenPageCommand, PageId, ScreenshotMode, SessionId,
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
        executable: Some(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        )),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
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
        executable: Some(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        )),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
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
        executable: Some(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        )),
        profiles_dir: profiles.path().to_path_buf(),
        headless: true,
        max_active: 8,
        upload_roots: vec![profiles.path().to_path_buf()],
        downloads_dir: profiles.path().join("downloads"),
        artifacts_dir: profiles.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
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

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn semantic_targets_fail_closed_and_reresolve_after_replacement() {
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
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
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
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::TargetAmbiguous);
    worker.close().await.unwrap();
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn waits_for_dynamic_element_content_url_document_and_network_quiet() {
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
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page_id = PageId::new();
    worker.open_page(page_id.clone()).await.unwrap();
    worker
        .navigate(
            &page_id,
            &NavigateCommand {
                url: "data:text/html,<div id=status hidden>booting</div><input aria-label=State value=booting><script>setTimeout(()=>{status.hidden=false;status.textContent='ready';document.querySelector('input').value='ready'},100)</script>".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();
    let status = TargetSpec {
        css: Some("#status".into()),
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
async fn captures_viewport_full_page_element_and_clip_as_private_artifacts() {
    let root = tempfile::tempdir().unwrap();
    let session_id = SessionId::new();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        )),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
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
        executable: Some(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        )),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().to_path_buf()],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
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
