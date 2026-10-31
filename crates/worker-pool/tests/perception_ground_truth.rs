//! Perception ground-truth collector. Each fixture page is captured with the
//! truth read from its DOM at the same moment: visible interactables with
//! boxes, single-line text runs with boxes, and the challenge class named by
//! structural markers. Rows feed the grounding, text-reading and
//! challenge-classification suites.
//!
//!   BOBBY_PERCEPTION_OUT=<rows.jsonl> \
//!   cargo test -p worker-pool --test perception_ground_truth -- --ignored

use std::io::Write as _;
use std::path::PathBuf;

use base64::Engine as _;
use config::BrowserConfig;
use test_site::{FixtureSite, Route};
use types::{EvaluateJavaScriptCommand, Evidence, NavigateCommand, PageId, SessionId, WaitUntil};
use worker_pool::{ChromiumWorkerFactory, WorkerFactory};

const PROBE: &str = include_str!("perception/probe.js");

/// Path, document, and the challenge class its markers name.
const PAGES: &[(&str, &str, &str)] = &[
    ("/sign-in", include_str!("perception/sign-in.html"), "none"),
    (
        "/overview",
        include_str!("perception/dashboard.html"),
        "none",
    ),
    (
        "/settings",
        include_str!("perception/settings.html"),
        "none",
    ),
    ("/shop", include_str!("perception/products.html"), "none"),
    ("/customers", include_str!("perception/table.html"), "none"),
    ("/reports", include_str!("perception/dialog.html"), "none"),
    ("/blog", include_str!("perception/article.html"), "none"),
    ("/search", include_str!("perception/search.html"), "none"),
    (
        "/contact",
        include_str!("perception/challenge-checkbox.html"),
        "recaptchaV2Checkbox",
    ),
    (
        "/newsletter",
        include_str!("perception/challenge-score.html"),
        "recaptchaV3",
    ),
    (
        "/checkout",
        include_str!("perception/challenge-grid.html"),
        "imageGridCaptcha",
    ),
    (
        "/register",
        include_str!("perception/challenge-text.html"),
        "textCaptcha",
    ),
    (
        "/verify",
        include_str!("perception/challenge-code.html"),
        "mfaCodeEntry",
    ),
];

fn chrome_executable() -> PathBuf {
    std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        })
}

#[tokio::test]
#[ignore = "requires installed Chrome; writes rows to BOBBY_PERCEPTION_OUT"]
async fn collect_perception_ground_truth() {
    let out = std::env::var_os("BOBBY_PERCEPTION_OUT")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            panic!("BOBBY_PERCEPTION_OUT unset; this harness collects nothing without it")
        });
    let mut routes: Vec<(&str, Route)> = PAGES
        .iter()
        .map(|(path, html, _)| (*path, Route::Html((*html).into())))
        .collect();
    routes.push((
        "/app.css",
        Route::Raw {
            content_type: "text/css",
            body: include_str!("perception/app.css").into(),
        },
    ));
    routes.push((
        "/recaptcha/api2/anchor",
        Route::Html(include_str!("perception/anchor.html").into()),
    ));
    routes.push((
        "/recaptcha/api2/bframe",
        Route::Html(include_str!("perception/bframe.html").into()),
    ));
    let site = FixtureSite::spawn(routes).await;

    let root = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(chrome_executable()),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 4 * 1024 * 1024,
        max_js_timeout_ms: 30_000,
    });
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let page = PageId::new();
    worker_pool::tabs_or_default(worker.tabs())
        .open_page(page.clone())
        .await
        .unwrap();

    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    let mut rows = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&out)
        .unwrap();
    for (path, _, challenge) in PAGES {
        worker_pool::navigation_or_default(worker.navigation())
            .navigate(
                &page,
                &NavigateCommand {
                    url: site.url(path),
                    wait_until: WaitUntil::NetworkIdle,
                    timeout_ms: 15_000,
                },
            )
            .await
            .unwrap();
        let png = worker_pool::capture_or_default(worker.capture())
            .screenshot_bytes(&page)
            .await
            .unwrap();
        let evidence = worker_pool::javascript_or_default(worker.javascript())
            .evaluate_javascript(
                &page,
                &EvaluateJavaScriptCommand {
                    expression: PROBE.into(),
                    timeout_ms: 10_000,
                    await_promise: false,
                },
            )
            .await
            .unwrap();
        let truth = match evidence.as_slice() {
            [Evidence::JavaScriptResult {
                value,
                truncated: false,
            }] => value.clone(),
            other => panic!("{path}: expected one complete probe result, got {other:?}"),
        };

        assert_eq!(truth["challenge"]["kind"], *challenge, "{path}");
        // A challenge overlay may cover the whole page; a clean page may not
        // come back empty.
        assert!(
            *challenge != "none"
                || (!truth["elements"].as_array().unwrap().is_empty()
                    && !truth["textRuns"].as_array().unwrap().is_empty()),
            "{path}: probe found no elements or text runs"
        );
        if *path == "/reports" {
            // Controls under the modal backdrop are not visible targets.
            let names: Vec<&str> = truth["elements"]
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|element| element["name"].as_str())
                .collect();
            assert!(!names.contains(&"Open report"), "{names:?}");
            assert!(names.contains(&"Accept all"), "{names:?}");
        }
        if *path == "/sign-in" {
            // The fixture pins this button with fixed positioning.
            let pinned = truth["elements"]
                .as_array()
                .unwrap()
                .iter()
                .find(|element| element["name"] == "Contact sales")
                .expect("pinned button");
            let bbox = ["x", "y", "w", "h"].map(|key| pinned["bbox"][key].as_f64().unwrap());
            assert_eq!(bbox, [40.0, 120.0, 132.0, 38.0]);
        }

        let row = serde_json::json!({
            "schema": "bobby-perception/1",
            "page": path,
            "imageB64": base64::engine::general_purpose::STANDARD.encode(&png),
            "viewport": truth["viewport"],
            "elements": truth["elements"],
            "textRuns": truth["textRuns"],
            "challenge": truth["challenge"],
        });
        writeln!(rows, "{row}").unwrap();
    }
    worker.close().await.unwrap();
}
