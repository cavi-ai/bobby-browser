use context_store::{ChallengeStats, ContextStore, SiteContext};
use std::collections::BTreeMap;

async fn persisted_site(root: &std::path::Path, challenges: BTreeMap<String, ChallengeStats>) {
    let (store, _) = ContextStore::open(root, "profile").await.unwrap();
    store
        .upsert_site(
            "site",
            SiteContext {
                challenges,
                ..Default::default()
            },
        )
        .await;
    assert!(store.flush().await.is_empty());
}

#[tokio::test]
async fn saturated_challenge_counters_keep_outcomes_and_verification_day_across_reopen() {
    let root = tempfile::tempdir().unwrap();
    persisted_site(
        root.path(),
        BTreeMap::from([(
            "challenge".into(),
            ChallengeStats {
                success_count: u64::MAX,
                failure_count: u64::MAX,
                last_verified_day: Some(10),
            },
        )]),
    )
    .await;
    let (store, report) = ContextStore::open(root.path(), "profile").await.unwrap();
    assert_eq!(report.sites_loaded, 1);
    store.record_challenge("site", "challenge", true, 100).await;
    store
        .record_challenge("site", "challenge", false, 101)
        .await;
    store
        .record_challenge("healthy", "challenge", true, 102)
        .await;
    store
        .record_challenge("healthy", "challenge", false, 103)
        .await;
    assert!(store.flush().await.is_empty());
    drop(store);

    let (store, _) = ContextStore::open(root.path(), "profile").await.unwrap();
    assert_eq!(
        store.site("site").await.unwrap().challenges["challenge"],
        ChallengeStats {
            success_count: u64::MAX,
            failure_count: u64::MAX,
            last_verified_day: Some(100),
        }
    );
    assert_eq!(
        store.site("healthy").await.unwrap().challenges["challenge"],
        ChallengeStats {
            success_count: 1,
            failure_count: 1,
            last_verified_day: Some(102),
        }
    );
}

#[tokio::test]
async fn challenge_prior_orders_attempt_totals_above_u64_without_overflow_or_false_ties() {
    let root = tempfile::tempdir().unwrap();
    let most_attempted = ChallengeStats {
        success_count: u64::MAX,
        failure_count: 2,
        last_verified_day: Some(10),
    };
    persisted_site(
        root.path(),
        BTreeMap::from([
            ("a-most".into(), most_attempted.clone()),
            (
                "b-less".into(),
                ChallengeStats {
                    success_count: u64::MAX,
                    failure_count: 1,
                    last_verified_day: Some(20),
                },
            ),
            (
                "c-normal".into(),
                ChallengeStats {
                    success_count: 100,
                    failure_count: 100,
                    last_verified_day: Some(30),
                },
            ),
        ]),
    )
    .await;
    let (store, _) = ContextStore::open(root.path(), "profile").await.unwrap();
    assert_eq!(
        store.challenge_prior("site").await,
        Some(("a-most".into(), most_attempted))
    );
    assert!(store.challenge_prior("missing").await.is_none());
}
