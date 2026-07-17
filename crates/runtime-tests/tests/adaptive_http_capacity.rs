use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use network_engine::state::HttpStateSnapshot;
use network_engine::{DirectHttpExecutor, NetworkPolicy};
use types::{Evidence, InspectCommand, NavigateCommand, PageId, SessionId, WaitUntil};
use worker_pool::{ChromiumWorkerFactory, WorkerFactory};

fn snapshot(url: String) -> HttpStateSnapshot {
    HttpStateSnapshot {
        version: 1,
        current_url: url,
        cookies: Vec::new(),
        cache_validators: BTreeMap::new(),
        user_agent: "capacity-proof".into(),
        language: "en-US".into(),
    }
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort_unstable();
    samples[samples.len() / 2]
}

#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn eight_inspections_complete_with_a_peak_of_four_and_no_browser_dispatch() {
    let site = test_site::spawn().await;
    let executor = Arc::new(DirectHttpExecutor::new(NetworkPolicy {
        allow_loopback: true,
        max_concurrent_requests: 4,
        request_timeout_ms: 5_000,
        ..NetworkPolicy::default()
    }));
    let mut tasks = Vec::new();
    for _ in 0..8 {
        let executor = executor.clone();
        let state = snapshot(format!("{}/slow", site.base_url()));
        tasks.push(tokio::spawn(async move {
            let started = Instant::now();
            let candidate = executor.inspect(&state, &InspectCommand::default()).await;
            (started.elapsed(), candidate)
        }));
    }

    let mut direct_samples = Vec::new();
    for task in tasks {
        let (elapsed, candidate) = task.await.unwrap();
        let candidate = candidate.expect("eligible inspection completes");
        match candidate {
            network_engine::HttpCandidate::Inspection {
                evidence: Evidence::Inspection { .. },
                meta,
                ..
            } => direct_samples.push(Duration::from_millis(meta.elapsed_ms)),
            _ => panic!("unexpected direct candidate"),
        }
        assert!(elapsed < Duration::from_secs(5));
    }
    assert_eq!(site.peak_requests(), 4);

    let root = tempfile::tempdir().unwrap();
    let factory = ChromiumWorkerFactory::new(config::BrowserConfig {
        executable: Some(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        )),
        profiles_dir: root.path().join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.path().join("uploads")],
        downloads_dir: root.path().join("downloads"),
        artifacts_dir: root.path().join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
    });
    let session = SessionId::new();
    let page = PageId::new();
    let worker = factory
        .launch(&session)
        .await
        .expect("launch installed Chrome");
    worker.open_page(page.clone()).await.unwrap();
    let mut chromium_samples = Vec::new();
    for _ in 0..direct_samples.len() {
        let started = Instant::now();
        worker
            .navigate(
                &page,
                &NavigateCommand {
                    url: format!("{}/slow", site.base_url()),
                    wait_until: WaitUntil::Interactive,
                    timeout_ms: 5_000,
                },
            )
            .await
            .expect("Chromium workload completes");
        worker
            .inspect(&page, &InspectCommand::default())
            .await
            .expect("Chromium inspection completes");
        chromium_samples.push(started.elapsed());
    }
    worker.close().await.unwrap();
    let direct_median = median(direct_samples);
    let chromium_median = median(chromium_samples);
    println!(
        "capacity measurements: peak={} direct_http_median_ms={} chromium_only_median_ms={} completed=8 chromium_dispatches=0",
        site.peak_requests(),
        direct_median.as_millis(),
        chromium_median.as_millis()
    );
    assert!(
        direct_median < chromium_median,
        "direct HTTP median {direct_median:?} must remain below measured Chromium median {chromium_median:?}"
    );
    assert!(direct_median < Duration::from_secs(5));
}
