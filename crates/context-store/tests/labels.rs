use std::collections::BTreeMap;

use context_store::{
    day_since_epoch, label_key, ContextStore, IntentStats, LabelContext, PageContext, RecordSource,
    SiteContext, MAX_LABEL_BYTES,
};

const SITE: &str = "https://example.com";

fn label(name: &str, verified_day: u32) -> LabelContext {
    LabelContext {
        role: "textbox".into(),
        accessible_name: name.into(),
        ordinal: None,
        stats: IntentStats {
            success_count: 1,
            failure_count: 0,
            last_verified_day: Some(verified_day),
            source: Some(RecordSource::VisionPromoted),
        },
    }
}

fn site(labels: &[(&str, LabelContext)]) -> SiteContext {
    SiteContext {
        pages: BTreeMap::from([(
            "/account".to_string(),
            PageContext {
                labels: labels
                    .iter()
                    .map(|(key, label)| (key.to_string(), label.clone()))
                    .collect(),
                ..PageContext::default()
            },
        )]),
        ..SiteContext::default()
    }
}

#[test]
fn label_keys_are_trimmed_lowercase_and_bounded() {
    assert_eq!(label_key("  Contact Email "), Some("contact email".into()));
    assert_eq!(label_key("   "), None);
    assert_eq!(
        label_key(&"x".repeat(MAX_LABEL_BYTES)).map(|key| key.len()),
        Some(MAX_LABEL_BYTES)
    );
    assert_eq!(label_key(&"x".repeat(MAX_LABEL_BYTES + 1)), None);
}

#[tokio::test]
async fn labels_survive_a_reopen() {
    let temp = tempfile::tempdir().unwrap();
    let today = day_since_epoch(chrono::Utc::now());
    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    let written = site(&[("contact email", label("f17", today))]);
    store.upsert_site(SITE, written.clone()).await;
    assert!(store.flush().await.is_empty());
    drop(store);

    let (reopened, report) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    assert_eq!(report.sites_loaded, 1);
    assert_eq!(reopened.site(SITE).await, Some(written));
}

#[tokio::test]
async fn sweep_drops_expired_labels_and_keeps_fresh_ones() {
    let temp = tempfile::tempdir().unwrap();
    let today = day_since_epoch(chrono::Utc::now());
    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    store
        .upsert_site(
            SITE,
            site(&[
                ("contact email", label("f17", today)),
                ("old field", label("f9", today - 120)),
            ]),
        )
        .await;
    store
        .upsert_site(
            "https://stale.example",
            site(&[("contact email", label("f17", today - 120))]),
        )
        .await;
    assert!(store.flush().await.is_empty());

    assert_eq!(store.sweep(90, today).await.unwrap(), 2);
    assert_eq!(
        store.site(SITE).await,
        Some(site(&[("contact email", label("f17", today))]))
    );
    assert_eq!(store.site("https://stale.example").await, None);
}

#[tokio::test]
async fn an_oversized_label_key_is_refused() {
    let temp = tempfile::tempdir().unwrap();
    let today = day_since_epoch(chrono::Utc::now());
    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    let key = "x".repeat(MAX_LABEL_BYTES + 1);
    store
        .upsert_site(SITE, site(&[(&key, label("f17", today))]))
        .await;

    assert_eq!(store.site(SITE).await, None);
    assert_eq!(store.usage().await.rejected_updates, 1);
}
