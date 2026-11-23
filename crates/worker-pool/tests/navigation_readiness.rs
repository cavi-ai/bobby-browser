use config::BrowserConfig;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use test_site::{FixtureSite, Route};
use types::{
    ErrorCode, EvaluateJavaScriptCommand, Evidence, NavigateCommand, PageId, SessionId, WaitUntil,
};
use worker_pool::{ChromiumWorkerFactory, WorkerFactory};

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn navigation_readiness_uses_the_command_budget() {
    let site = FixtureSite::spawn(vec![
        (
            "/commit",
            Route::Html(
                "<!doctype html><title>Committed</title><script src='/held?script'></script>"
                    .into(),
            ),
        ),
        (
            "/document",
            Route::Html(
                "<!doctype html><title>DOM ready</title><img src='/held?image'><main>Ready</main>"
                    .into(),
            ),
        ),
        (
            "/held",
            Route::Delayed {
                delay: Duration::from_secs(3_600),
                content_type: "application/octet-stream",
                body: String::new(),
            },
        ),
        (
            "/churn",
            Route::Html("<!doctype html><title>Changing</title><main></main><script>setInterval(()=>document.querySelector('main').append(document.createElement('span')),20)</script>".into()),
        ),
        (
            "/ready",
            Route::Html("<!doctype html><title>Ready</title><main>Ready</main>".into()),
        ),
        (
            "/late",
            Route::Delayed {
                delay: Duration::from_secs(31),
                content_type: "text/html",
                body: "<!doctype html><title>Late document</title><main>Ready</main>".into(),
            },
        ),
    ])
    .await;
    let root = tempfile::tempdir().unwrap();
    let executable = std::env::var("BOBBY_CHROME_EXECUTABLE")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome")
        });
    let factory = ChromiumWorkerFactory::new(BrowserConfig {
        executable: Some(executable),
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
    let mut early = Vec::new();
    for (wait_until, route, expected_state) in [
        (WaitUntil::Commit, "/commit", "loading"),
        (WaitUntil::DomContentLoaded, "/document", "interactive"),
        (WaitUntil::Interactive, "/document", "interactive"),
    ] {
        let page = PageId::new();
        worker_pool::tabs_or_default(worker.tabs())
            .open_page(page.clone())
            .await
            .unwrap();
        let started = Instant::now();
        let result = worker_pool::navigation_or_default(worker.navigation())
            .navigate(
                &page,
                &NavigateCommand {
                    url: site.url(route),
                    wait_until,
                    timeout_ms: 2_000,
                },
            )
            .await;
        let elapsed = started.elapsed();
        let actual_url = result.as_ref().ok().and_then(|items| {
            items.iter().find_map(|item| {
                if let Evidence::Navigation { url, .. } = item {
                    Some(url.clone())
                } else {
                    None
                }
            })
        });
        let observed = worker_pool::javascript_or_default(worker.javascript())
            .evaluate_javascript(
                &page,
                &EvaluateJavaScriptCommand {
                    expression: "document.readyState".into(),
                    timeout_ms: 2_000,
                    await_promise: false,
                },
            )
            .await;
        eprintln!(
            "readiness {wait_until:?}: result={result:?}, state={observed:?}, elapsed={elapsed:?}"
        );
        early.push((
            wait_until,
            expected_state,
            site.url(route),
            actual_url,
            result,
            observed,
            elapsed,
        ));
    }

    let page = PageId::new();
    worker_pool::tabs_or_default(worker.tabs())
        .open_page(page.clone())
        .await
        .unwrap();
    let started = Instant::now();
    let deadline = worker_pool::navigation_or_default(worker.navigation())
        .navigate(
            &page,
            &NavigateCommand {
                url: site.url("/document?token=navigation-canary"),
                wait_until: WaitUntil::NetworkIdle,
                timeout_ms: 750,
            },
        )
        .await;
    let deadline_elapsed = started.elapsed();
    eprintln!("network-idle deadline: result={deadline:?}, elapsed={deadline_elapsed:?}");

    let started = Instant::now();
    let settling = worker_pool::navigation_or_default(worker.navigation())
        .navigate(
            &page,
            &NavigateCommand {
                url: site.url("/churn"),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 750,
            },
        )
        .await;
    let settling_elapsed = started.elapsed();
    eprintln!("settling deadline: result={settling:?}, elapsed={settling_elapsed:?}");

    let idle = worker_pool::navigation_or_default(worker.navigation())
        .navigate(
            &page,
            &NavigateCommand {
                url: site.url("/ready"),
                wait_until: WaitUntil::NetworkIdle,
                timeout_ms: 3_000,
            },
        )
        .await;
    eprintln!("network-idle recovery: result={idle:?}");

    let late_page = PageId::new();
    worker_pool::tabs_or_default(worker.tabs())
        .open_page(late_page.clone())
        .await
        .unwrap();
    let started = Instant::now();
    let late = worker_pool::navigation_or_default(worker.navigation())
        .navigate(
            &late_page,
            &NavigateCommand {
                url: site.url("/late"),
                wait_until: WaitUntil::DomContentLoaded,
                timeout_ms: 35_000,
            },
        )
        .await;
    let late_elapsed = started.elapsed();
    eprintln!("delayed document: result={late:?}, elapsed={late_elapsed:?}");
    let held_requests = site.hits("/held");
    worker.close().await.unwrap();
    drop(site);

    assert!(
        held_requests > 0,
        "the fixture must actually hold subresource requests"
    );
    assert!(
        idle.is_ok(),
        "network idle must succeed on a finite page after prior deadlines: {idle:?}"
    );
    for (mode, expected_state, expected_url, actual_url, result, observed, elapsed) in early {
        assert!(
            result.is_ok(),
            "{mode:?} must not wait for held subresources: {result:?}"
        );
        assert!(
            elapsed < Duration::from_secs(2),
            "{mode:?} exceeded its navigation budget: {elapsed:?}"
        );
        assert_eq!(
            actual_url.as_deref(),
            Some(expected_url.as_str()),
            "{mode:?} must report the committed document"
        );
        assert!(
            observed.as_ref().is_ok_and(|items| {
                items.iter().any(|item| {
                matches!(item, Evidence::JavaScriptResult { value, .. } if value == expected_state)
            })
            }),
            "{mode:?} must finish at its requested document state: {observed:?}"
        );
    }
    let error = deadline.expect_err("network idle cannot finish while an image is held");
    assert_eq!(error.code, ErrorCode::DeadlineExceeded);
    assert!(!error.message.contains("navigation-canary"));
    assert!(
        error.message.contains("network idle"),
        "deadline must name the failed readiness stage: {error:?}"
    );
    assert!(
        deadline_elapsed < Duration::from_millis(1_500),
        "deadline must bound the whole navigation: {deadline_elapsed:?}"
    );
    let error =
        settling.expect_err("continuous document changes must exhaust the remaining budget");
    assert_eq!(error.code, ErrorCode::DeadlineExceeded);
    assert!(
        error.message.contains("document settling"),
        "deadline must name the failed settling stage: {error:?}"
    );
    assert!(
        settling_elapsed < Duration::from_millis(1_500),
        "settling must share the navigation budget: {settling_elapsed:?}"
    );
    assert!(
        late.is_ok(),
        "a 35s budget must allow a document arriving after 31s: {late:?}"
    );
    assert!(
        late_elapsed < Duration::from_secs(35),
        "delayed document exceeded its caller budget: {late_elapsed:?}"
    );
}
