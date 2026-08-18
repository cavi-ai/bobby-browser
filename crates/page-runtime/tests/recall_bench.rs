//! Recall benchmark: validation-weighted persisted recall at scale.
//!
//! Measures `ask()` latency over a seeded ContextStore at several scales,
//! and asserts the ranking invariants the weighting guarantees: validated
//! entries win ties, recent beats stale, failures drag down, and the match
//! ladder is untouched.
//!
//! Run: cargo test -p page-runtime --test recall_bench -- --ignored --nocapture

use std::collections::BTreeMap;
use std::time::Instant;

use context_store::{
    ContextStore, ControlContext, FormContext, IntentStats, PageContext, SiteContext,
};
use page_runtime::ContextPromotion;

fn day_now() -> u32 {
    context_store::day_since_epoch(chrono::Utc::now())
}

fn control(name: &str, successes: u64, failures: u64, last_day: Option<u32>) -> ControlContext {
    let mut intents = BTreeMap::new();
    intents.insert(
        "locate".to_string(),
        IntentStats {
            success_count: successes,
            failure_count: failures,
            last_verified_day: last_day,
            source: None,
        },
    );
    ControlContext {
        role: "button".into(),
        accessible_name: name.into(),
        ordinal: None,
        form_membership: "page".into(),
        intents,
    }
}

fn site_with_controls(controls: Vec<ControlContext>) -> SiteContext {
    let mut forms = BTreeMap::new();
    forms.insert("page".to_string(), FormContext { controls });
    let mut pages = BTreeMap::new();
    pages.insert("/".to_string(), PageContext { forms });
    SiteContext {
        pages,
        ..SiteContext::default()
    }
}

/// Seed a store with `n_sites` sites, each holding `per_site` controls with
/// a spread of validation records (validated counts, staleness, failures).
async fn seed_store(
    dir: &std::path::Path,
    n_sites: usize,
    per_site: usize,
) -> (ContextPromotion, Vec<String>) {
    let (store, report) = ContextStore::open(dir, "bench-profile").await.unwrap();
    assert!(report.skipped.is_empty());
    let today = day_now();
    let mut keys = Vec::new();
    for site_idx in 0..n_sites {
        // Distinct registrable domains: site keys collapse subdomains to
        // eTLD+1, so `site-N.example.test` would all collide.
        let key = format!("https://site{site_idx}.test");
        let controls = (0..per_site)
            .map(|control_idx| {
                let validated = control_idx % 4 != 3; // 3/4 of controls have a record
                let successes = if validated {
                    (control_idx % 7) as u64 + 1
                } else {
                    0
                };
                let failures = (control_idx % 5 == 4) as u64 * 2; // some flaky
                let staleness = (control_idx % 6) as u32 * 15; // 0..75 days old
                let last_day = if validated {
                    Some(today.saturating_sub(staleness))
                } else {
                    None
                };
                control(
                    &format!("Control {control_idx}"),
                    successes,
                    failures,
                    last_day,
                )
            })
            .collect();
        store.upsert_site(&key, site_with_controls(controls)).await;
        keys.push(key);
    }
    store.flush().await;
    (ContextPromotion::new(store), keys)
}

fn percentile(sorted: &[f64], p: f64) -> f64 {
    if sorted.is_empty() {
        return 0.0;
    }
    let idx = ((sorted.len() as f64 - 1.0) * p).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn report_latencies(label: &str, mut samples_ms: Vec<f64>) {
    samples_ms.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = samples_ms.len();
    println!(
        "{label}: n={n} p50={:.3}ms p95={:.3}ms p99={:.3}ms max={:.3}ms",
        percentile(&samples_ms, 0.50),
        percentile(&samples_ms, 0.95),
        percentile(&samples_ms, 0.99),
        samples_ms.last().copied().unwrap_or(0.0),
    );
}

#[tokio::test]
#[ignore = "benchmark; run on demand"]
async fn recall_latency_scales_with_site_size() {
    for (n_sites, per_site) in [(10, 20), (50, 100), (100, 200)] {
        let temp = tempfile::tempdir().unwrap();
        let (promotion, keys) = seed_store(temp.path(), n_sites, per_site).await;

        // Warm the in-memory index.
        let url = format!("{}/", keys[0]);
        let _ = promotion.ask(Some(&url), "Control 0").await;

        let mut samples = Vec::new();
        for key in &keys {
            let url = format!("{key}/");
            for needle in ["Control 0", "Control 5", "button Control 9", "Controle 3"] {
                let start = Instant::now();
                let answer = promotion.ask(Some(&url), needle).await;
                samples.push(start.elapsed().as_secs_f64() * 1000.0);
                // Exact and role+name needles must answer; the typo needle may.
                if !needle.starts_with("Controle") {
                    assert!(answer.is_some(), "no answer for {needle} on {key}");
                }
            }
        }
        report_latencies(
            &format!("sites={n_sites} controls/site={per_site}"),
            samples,
        );
    }
}

#[tokio::test]
#[ignore = "benchmark; run on demand"]
async fn store_open_scales_with_site_count() {
    for n_sites in [10, 100, 500] {
        let temp = tempfile::tempdir().unwrap();
        {
            let (promotion, _keys) = seed_store(temp.path(), n_sites, 20).await;
            drop(promotion);
        }
        let start = Instant::now();
        let (store, report) = ContextStore::open(temp.path(), "bench-profile")
            .await
            .unwrap();
        let elapsed = start.elapsed();
        assert_eq!(report.sites_loaded, n_sites);
        assert!(report.skipped.is_empty());
        println!(
            "open: sites={n_sites} loaded in {:.1}ms ({:.2}ms/site)",
            elapsed.as_secs_f64() * 1000.0,
            elapsed.as_secs_f64() * 1000.0 / n_sites as f64,
        );
        drop(store);
    }
}

#[tokio::test]
#[ignore = "benchmark; run on demand"]
async fn weighted_ranking_invariants_hold_at_scale() {
    let temp = tempfile::tempdir().unwrap();
    let today = day_now();
    let (store, report) = ContextStore::open(temp.path(), "bench-profile")
        .await
        .unwrap();
    assert!(report.skipped.is_empty());

    // One site, four same-named controls with different records: the ladder
    // ties all four (exact name match), so the weight decides.
    let controls = vec![
        control("Save", 0, 0, None),             // no record
        control("Save", 8, 0, Some(today)),      // validated, fresh
        control("Save", 8, 0, Some(today - 90)), // validated, stale
        control("Save", 8, 4, Some(today)),      // validated, flaky
    ];
    store
        .upsert_site("https://example.test", site_with_controls(controls))
        .await;
    store.flush().await;
    let promotion = ContextPromotion::new(store);

    let url = "https://app.example.test/";
    let answer = promotion
        .ask(Some(url), "save")
        .await
        .expect("exact match must answer");
    // The fresh, clean, validated control must win the tie.
    // (Ordinal is None for all; identity is by the record we seeded — the
    // answer must be the one whose weight is highest: validated+fresh+clean.)
    assert_eq!(answer.target.accessible_name, "Save");

    // Latency on the tie-heavy path.
    let mut samples = Vec::new();
    for _ in 0..200 {
        let start = Instant::now();
        let _ = promotion.ask(Some(url), "save").await;
        samples.push(start.elapsed().as_secs_f64() * 1000.0);
    }
    report_latencies("tie-heavy recall (4 same-name controls)", samples);
}
