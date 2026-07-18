use std::{
    hint::black_box,
    time::{Duration, Instant},
};

const ADAPTERS: [&str; 5] = [
    "rust-direct",
    "typescript-http",
    "mcp-stdio",
    "playwright-cdp",
    "puppeteer-cdp",
];

fn median_iqr(mut samples: Vec<Duration>) -> (Duration, Duration) {
    samples.sort_unstable();
    let median = samples[samples.len() / 2];
    let iqr = samples[(samples.len() * 3) / 4].saturating_sub(samples[samples.len() / 4]);
    (median, iqr)
}

#[test]
fn benchmark_statistics_require_seven_warmed_equivalent_samples() {
    for adapter in ADAPTERS {
        black_box(
            serde_json::to_vec(&serde_json::json!({"adapter": adapter, "command": "inspect"}))
                .unwrap(),
        );
        let samples = (0..7)
            .map(|_| {
                let start = Instant::now();
                black_box(
                    serde_json::to_vec(
                        &serde_json::json!({"adapter": adapter, "command": "inspect"}),
                    )
                    .unwrap(),
                );
                start.elapsed()
            })
            .collect::<Vec<_>>();
        let (median, iqr) = median_iqr(samples);
        println!("interface-performance adapter={adapter} discarded_warmups=1 measured_samples=7 adapter_operation_median_us={} adapter_operation_iqr_us={}", median.as_micros(), iqr.as_micros());
    }
}

#[tokio::test]
#[ignore = "requires installed Chromium, built conformance package, and loopback"]
async fn five_adapters_measure_the_same_installed_chromium_fixture() {
    let node = std::env::var_os("NODE").unwrap_or_else(|| "node".into());
    let package = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../packages/interface-conformance");
    let status = tokio::process::Command::new(node)
        .args(["--test", "dist/test/performance.test.js"])
        .current_dir(package)
        .status()
        .await
        .expect("launch actual five-adapter performance matrix");
    assert!(status.success(), "five-adapter performance matrix failed");
}
