use context_store::{
    day_since_epoch, ContextStore, ContextStoreError, ControlContext, FormContext, IntentStats,
    PageContext, RecordSource, SiteContext,
};

fn control(name: &str, verified_day: u32) -> ControlContext {
    let mut intents = std::collections::BTreeMap::new();
    intents.insert(
        "fill".to_string(),
        IntentStats {
            success_count: 3,
            failure_count: 1,
            last_verified_day: Some(verified_day),
            source: Some(RecordSource::Observed),
        },
    );
    ControlContext {
        role: "textbox".into(),
        accessible_name: name.into(),
        ordinal: None,
        form_membership: "login".into(),
        intents,
    }
}

fn site(names: &[&str], verified_day: u32) -> SiteContext {
    let mut forms = std::collections::BTreeMap::new();
    forms.insert(
        "login".to_string(),
        FormContext {
            controls: names
                .iter()
                .map(|name| control(name, verified_day))
                .collect(),
        },
    );
    let mut pages = std::collections::BTreeMap::new();
    pages.insert("/login".to_string(), PageContext { forms });
    SiteContext { pages }
}

#[tokio::test]
async fn round_trip_persists_site_structure() {
    let temp = tempfile::tempdir().unwrap();
    let (store, report) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    assert_eq!(report.sites_loaded, 0);
    store
        .upsert_site("https://example.com", site(&["Email", "Password"], 100))
        .await;
    assert!(store.flush().await.is_empty());
    drop(store);

    let (reopened, report) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    assert_eq!(report.sites_loaded, 1);
    assert!(report.skipped.is_empty());
    let loaded = reopened.site("https://example.com").await.unwrap();
    assert_eq!(loaded, site(&["Email", "Password"], 100));
}

#[tokio::test]
async fn corrupt_and_unsupported_files_are_skipped_and_reported() {
    let temp = tempfile::tempdir().unwrap();
    // Literal UTF-8 hex encoding of `profile-a`; keep this independent of the
    // production encoder so a lossy encoding regression cannot bless itself.
    let profile_dir = temp.path().join("70726f66696c652d61");
    std::fs::create_dir_all(&profile_dir).unwrap();
    std::fs::write(profile_dir.join("garbage.json"), b"{not json").unwrap();
    std::fs::write(
        profile_dir.join("future.json"),
        serde_json::to_vec(&serde_json::json!({ "schema": 99, "site_key": "https://future.example", "site": { "pages": {} } })).unwrap(),
    )
    .unwrap();

    let (store, report) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    assert_eq!(report.sites_loaded, 0);
    assert_eq!(report.skipped.len(), 2);
    assert!(store.list_sites().await.is_empty());
}

#[tokio::test]
async fn profiles_are_isolated() {
    let temp = tempfile::tempdir().unwrap();
    let (store_a, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    store_a
        .upsert_site("https://example.com", site(&["Email"], 100))
        .await;
    assert!(store_a.flush().await.is_empty());
    drop(store_a);

    let (store_b, report) = ContextStore::open(temp.path(), "profile-b").await.unwrap();
    assert_eq!(report.sites_loaded, 0);
    assert!(store_b.site("https://example.com").await.is_none());
}

#[tokio::test]
async fn profiles_with_colliding_sanitized_names_are_isolated() {
    let temp = tempfile::tempdir().unwrap();
    let (slash_profile, _) = ContextStore::open(temp.path(), "a/b").await.unwrap();
    slash_profile
        .upsert_site("https://example.com", site(&["Slash profile"], 100))
        .await;
    assert!(slash_profile.flush().await.is_empty());
    drop(slash_profile);

    let (underscore_profile, report) = ContextStore::open(temp.path(), "a_b").await.unwrap();
    assert_eq!(report.sites_loaded, 0);
    assert!(underscore_profile
        .site("https://example.com")
        .await
        .is_none());
}

#[tokio::test]
async fn lock_contention_refuses_the_second_writer() {
    let temp = tempfile::tempdir().unwrap();
    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    let error = match ContextStore::open(temp.path(), "profile-a").await {
        Ok(_) => panic!("second writer must be refused"),
        Err(error) => error,
    };
    assert!(matches!(error, ContextStoreError::AlreadyLocked));
    drop(store);
    let (reopened, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    drop(reopened);
}

#[tokio::test]
async fn stale_lockfile_does_not_block_recovery() {
    let temp = tempfile::tempdir().unwrap();
    let profile_dir = temp.path().join("70726f66696c652d61");
    std::fs::create_dir_all(&profile_dir).unwrap();
    std::fs::write(profile_dir.join(".context-store.lock"), b"stale-pid\n").unwrap();

    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    drop(store);
}

#[tokio::test]
async fn forget_removes_memory_and_file() {
    let temp = tempfile::tempdir().unwrap();
    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    store
        .upsert_site("https://example.com", site(&["Email"], 100))
        .await;
    assert!(store.flush().await.is_empty());
    assert!(!store.list_sites().await.is_empty());

    store.forget("https://example.com").await.unwrap();
    assert!(store.list_sites().await.is_empty());
    assert!(store.site("https://example.com").await.is_none());
    drop(store);

    let (_reopened, report) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    assert_eq!(report.sites_loaded, 0);
}

#[tokio::test]
async fn sweep_drops_only_expired_records() {
    let temp = tempfile::tempdir().unwrap();
    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    let today = day_since_epoch(chrono::Utc::now());
    store
        .upsert_site("https://fresh.example", site(&["Email"], today))
        .await;
    store
        .upsert_site("https://stale.example", site(&["Email"], today - 120))
        .await;
    let mut two = site(&["Old", "New"], today);
    two.pages
        .values_mut()
        .flat_map(|page| page.forms.values_mut())
        .flat_map(|form| form.controls.iter_mut())
        .for_each(|control| {
            if control.accessible_name == "Old" {
                control
                    .intents
                    .values_mut()
                    .for_each(|stats| stats.last_verified_day = Some(today - 120));
            }
        });
    store
        .upsert_site("https://mixed.example", two.clone())
        .await;
    assert!(store.flush().await.is_empty());

    let dropped = store.sweep(90, today).await.unwrap();
    assert_eq!(dropped, 2);
    assert!(store.site("https://fresh.example").await.is_some());
    assert!(store.site("https://stale.example").await.is_none());
    let mixed = store.site("https://mixed.example").await.unwrap();
    let names: Vec<&str> = mixed
        .pages
        .values()
        .flat_map(|page| page.forms.values())
        .flat_map(|form| form.controls.iter())
        .map(|control| control.accessible_name.as_str())
        .collect();
    assert_eq!(names, ["New"]);
    drop(mixed);
    drop(store);

    let (_reopened, report) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    assert_eq!(report.sites_loaded, 2);
}

#[cfg(unix)]
#[tokio::test]
async fn sweep_reports_persistence_failure() {
    let temp = tempfile::tempdir().unwrap();
    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    store
        .upsert_site("https://stale.example", site(&["Email"], 1))
        .await;
    assert!(store.flush().await.is_empty());
    std::fs::remove_dir_all(store.root()).unwrap();

    let error = store.sweep(90, 200).await.unwrap_err();
    assert!(matches!(error, ContextStoreError::Io(_)));
}

#[tokio::test]
async fn failed_flush_keeps_data_session_only() {
    let temp = tempfile::tempdir().unwrap();
    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    store
        .upsert_site("https://example.com", site(&["Email"], 100))
        .await;
    std::fs::remove_dir_all(store.root()).unwrap();
    let failed = store.flush().await;
    assert_eq!(failed, ["https://example.com"]);
    assert!(store.site("https://example.com").await.is_some());
}

#[test]
fn day_precision_is_coarse() {
    let morning = chrono::DateTime::from_timestamp(1_700_000_000, 0).unwrap();
    let later = chrono::DateTime::from_timestamp(1_700_003_600, 0).unwrap();
    assert_eq!(day_since_epoch(morning), day_since_epoch(later));
    assert_eq!(day_since_epoch(morning), 19675);
}

#[test]
fn serialized_envelope_carries_no_values_or_exact_timestamps() {
    let envelope = serde_json::to_value(site(&["Email"], 100)).unwrap();
    let text = envelope.to_string();
    for forbidden in ["value", "password", "screenshot", "timestamp", "journal"] {
        assert!(
            !text.to_ascii_lowercase().contains(forbidden),
            "serialized context must never contain {forbidden:?}: {text}"
        );
    }
}
