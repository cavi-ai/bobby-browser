use config::BrowserConfig;
use network_engine::state::{HttpCookie, ResponseStateDelta};
use std::collections::BTreeMap;
use std::path::PathBuf;
use types::{
    AccessibilityNode, AccessibilitySnapshotCommand, CaptureScreenshotCommand,
    ClickAndWaitForDownloadCommand, ClickAndWaitForPopupCommand, ClickCommand, ClosePageCommand,
    ElementState, ErrorCode, EvaluateJavaScriptCommand, Evidence, InspectCommand, ListPagesCommand,
    NavigateCommand, OpenPageCommand, PageId, ScreenshotMode, SessionId, TargetSpec, TextMatch,
    TypeTextCommand, UploadFilesCommand, WaitCondition, WaitForCommand, WaitUntil,
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
async fn form_controls_have_normalized_roles_names_constraints_and_native_selection() {
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
                url: "data:text/html,<span id=email-label>Email address</span><input id=email aria-labelledby=email-label required pattern='[^@]+@[^@]+' autocomplete=email><label><input id=updates type=checkbox>Product updates</label><label><input id=pro type=radio name=plan value=pro>Professional</label><select id=region aria-label=Region><option value=us>United States</option><option value=ca>Canada</option></select><label for=phone-home>Phone</label><input id=phone-home><label for=phone-work>Phone</label><input id=phone-work><label for=password>Password</label><input id=password type=password autocomplete=current-password value=vault-secret-92 required>".into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap();

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
async fn network_quiet_respects_url_and_long_lived_ignores() {
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
