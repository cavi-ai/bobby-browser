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
    SiteContext {
        pages,
        ..SiteContext::default()
    }
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
async fn long_site_keys_with_valid_destination_names_persist_and_remain_erasable() {
    let temp = tempfile::tempdir().unwrap();
    let origin = format!(
        "https://{}.s3.dualstack.ap-southeast-2.amazonaws.com",
        "a".repeat(63)
    );
    let key = context_store::site_key(&format!("{origin}/form")).unwrap();
    assert_eq!(key, origin);
    let name = format!("{}.json", hex::encode(key.as_bytes()));
    assert!(name.len() <= 255);
    assert!(
        key.len() * 2 + 42 > 255,
        "fixture must exceed the old temporary-name limit"
    );
    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    let mut expected = site(&["Fresh", "Expired"], 100);
    expected
        .pages
        .get_mut("/login")
        .unwrap()
        .forms
        .get_mut("login")
        .unwrap()
        .controls[1]
        .intents
        .get_mut("fill")
        .unwrap()
        .last_verified_day = Some(1);
    store.upsert_site(&key, expected.clone()).await;
    store.upsert_site("other", site(&["Other"], 100)).await;
    assert!(
        store.flush().await.is_empty(),
        "a valid destination failed to persist"
    );
    let path = store.root().join(name);
    assert!(context_store::inspect_site_file(&path, Default::default()).is_ok());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }
    drop(store);
    let (store, report) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    assert_eq!(report.sites_loaded, 2);
    assert!(report.skipped.is_empty());
    assert_eq!(store.site(&key).await, Some(expected.clone()));
    assert_eq!(store.sweep(30, 100).await.unwrap(), 1);
    expected
        .pages
        .get_mut("/login")
        .unwrap()
        .forms
        .get_mut("login")
        .unwrap()
        .controls
        .pop();
    assert_eq!(store.site(&key).await, Some(expected.clone()));
    assert!(context_store::inspect_site_file(&path, Default::default()).is_ok());
    store.forget(&key).await.unwrap();
    assert!(!path.exists());

    // Force a rename failure after creating the temporary file. It must be
    // removed and the dirty update must remain retryable.
    store.upsert_site(&key, expected.clone()).await;
    std::fs::create_dir(&path).unwrap();
    assert_eq!(store.flush().await, vec![key.clone()]);
    for entry in std::fs::read_dir(store.root()).unwrap() {
        assert_ne!(
            entry
                .unwrap()
                .path()
                .extension()
                .and_then(|ext| ext.to_str()),
            Some("tmp")
        );
    }
    std::fs::remove_dir(&path).unwrap();
    assert!(store.flush().await.is_empty());
    drop(store);
    let (store, report) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    assert_eq!(report.sites_loaded, 2);
    assert_eq!(store.site(&key).await, Some(expected));
    store.forget(&key).await.unwrap();
    assert_eq!(store.list_sites().await, vec!["other"]);
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
async fn mismatched_site_identity_is_reported_and_cannot_revive_after_forget() {
    let temp = tempfile::tempdir().unwrap();
    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    store.upsert_site("one", site(&["Email"], 100)).await;
    store.upsert_site("other", site(&["Username"], 100)).await;
    assert!(store.flush().await.is_empty());
    // Literal UTF-8 hex for "one", independent of the production encoder.
    let path = store.root().join("6f6e65.json");
    drop(store);
    let mut envelope: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    envelope["site_key"] = serde_json::json!("swapped");
    let original = serde_json::to_vec(&envelope).unwrap();
    std::fs::write(&path, &original).unwrap();

    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    store.forget("swapped").await.unwrap();
    drop(store);

    for _ in 0..2 {
        let (store, report) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
        assert!(
            store.site("swapped").await.is_none(),
            "forgotten context revived from a mismatched file"
        );
        assert!(store.site("one").await.is_none());
        assert_eq!(report.sites_loaded, 1);
        assert_eq!(report.skipped_total, 1);
        assert_eq!(report.skipped.len(), 1);
        assert_eq!(report.skipped[0].file, path);
        assert_eq!(store.list_sites().await, vec!["other"]);
        assert_eq!(store.site("other").await, Some(site(&["Username"], 100)));
        assert!(context_store::inspect_site_file(&path, Default::default()).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), original);
        store.forget("swapped").await.unwrap();
    }

    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    store.upsert_site("fresh", site(&["Fresh"], 100)).await;
    assert!(store.flush().await.is_empty());
    store.forget("other").await.unwrap();
    drop(store);
    let (store, report) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    assert_eq!(store.list_sites().await, vec!["fresh"]);
    assert_eq!(store.site("fresh").await, Some(site(&["Fresh"], 100)));
    assert!(store.site("other").await.is_none());
    assert_eq!(report.skipped_total, 1);
    assert_eq!(std::fs::read(&path).unwrap(), original);
}

#[tokio::test]
async fn oversized_valid_site_is_reported_without_reading_or_erasing_it() {
    use std::io::Write;
    let temp = tempfile::tempdir().unwrap();
    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    store.upsert_site("site", site(&["Email"], 100)).await;
    assert!(store.flush().await.is_empty());
    let path = store.root().join("73697465.json");
    drop(store);
    let mut file = std::fs::OpenOptions::new()
        .append(true)
        .open(&path)
        .unwrap();
    file.write_all(&vec![b' '; 2 * 1024 * 1024]).unwrap();
    drop(file);
    let original = std::fs::read(&path).unwrap();
    let (reopened, report) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    assert_eq!(
        report.sites_loaded, 0,
        "oversized valid JSON must be refused"
    );
    assert_eq!(report.skipped.len(), 1);
    assert!(reopened.site("site").await.is_none());
    assert_eq!(std::fs::read(path).unwrap(), original);
}

#[tokio::test]
async fn cache_eviction_reloads_persisted_sites_and_bounds_dirty_admissions() {
    let temp = tempfile::tempdir().unwrap();
    let limits = context_store::ContextLimits {
        max_resident_sites: 2,
        max_resident_bytes: 64 * 1024,
        ..Default::default()
    };
    let (store, _) = ContextStore::open_with_limits(temp.path(), "profile-a", limits)
        .await
        .unwrap();
    for name in ["one", "two", "three", "four"] {
        store.upsert_site(name, site(&[name], 100)).await;
        let usage = store.usage().await;
        assert!(usage.resident_sites <= 2, "{usage:?}");
        assert!(usage.resident_bytes <= 64 * 1024, "{usage:?}");
    }
    assert!(store.flush().await.is_empty());
    assert_eq!(store.list_sites().await, ["four", "one", "three", "two"]);
    for name in ["one", "two", "three", "four"] {
        assert_eq!(store.site(name).await, Some(site(&[name], 100)));
        assert!(store.usage().await.resident_sites <= 2);
    }
    assert!(store.usage().await.evictions > 0);
    drop(store);
    let (reopened, report) = ContextStore::open_with_limits(temp.path(), "profile-a", limits)
        .await
        .unwrap();
    assert_eq!(report.sites_loaded, 4);
    assert!(reopened.usage().await.resident_sites <= 2);
    assert_eq!(reopened.site("one").await, Some(site(&["one"], 100)));
}

#[tokio::test]
async fn site_structure_and_cache_byte_limits_preserve_the_previous_observation() {
    let temp = tempfile::tempdir().unwrap();
    let limits = context_store::ContextLimits {
        max_site_records: 4,
        max_resident_bytes: 16 * 1024,
        ..Default::default()
    };
    let (store, _) = ContextStore::open_with_limits(temp.path(), "profile-a", limits)
        .await
        .unwrap();
    store.upsert_site("site", site(&["Email"], 100)).await;
    assert!(store.flush().await.is_empty());
    let original = store.site("site").await.unwrap();
    store
        .upsert_site("site", site(&["Email", "Password"], 100))
        .await;
    assert_eq!(store.site("site").await.unwrap(), original);
    store
        .upsert_site("site", site(&[&"x".repeat(32 * 1024)], 100))
        .await;
    assert_eq!(store.site("site").await.unwrap(), original);
    assert_eq!(store.usage().await.rejected_updates, 2);
    assert!(store.usage().await.resident_bytes <= 16 * 1024);
}

#[tokio::test]
async fn failed_pressure_flush_keeps_accepted_dirty_context_and_refuses_growth() {
    let temp = tempfile::tempdir().unwrap();
    let limits = context_store::ContextLimits {
        max_resident_sites: 1,
        ..Default::default()
    };
    let (store, _) = ContextStore::open_with_limits(temp.path(), "profile-a", limits)
        .await
        .unwrap();
    store.upsert_site("one", site(&["Email"], 100)).await;
    let original_root = store.root().to_path_buf();
    let moved = temp.path().join("moved");
    std::fs::rename(&original_root, &moved).unwrap();
    store.upsert_site("two", site(&["Password"], 100)).await;
    assert_eq!(store.site("one").await, Some(site(&["Email"], 100)));
    assert!(store.site("two").await.is_none());
    assert_eq!(store.usage().await.pending_changes, 1);
    assert_eq!(store.usage().await.rejected_updates, 1);
    std::fs::rename(moved, original_root).unwrap();
    assert!(store.flush().await.is_empty());
    drop(store);
    let (reopened, _) = ContextStore::open_with_limits(temp.path(), "profile-a", limits)
        .await
        .unwrap();
    assert_eq!(reopened.site("one").await, Some(site(&["Email"], 100)));
}

#[tokio::test]
async fn retention_and_forget_include_evicted_sites() {
    let temp = tempfile::tempdir().unwrap();
    let limits = context_store::ContextLimits {
        max_resident_sites: 1,
        ..Default::default()
    };
    let (store, _) = ContextStore::open_with_limits(temp.path(), "profile-a", limits)
        .await
        .unwrap();
    for name in ["one", "two", "three"] {
        store.upsert_site(name, site(&[name], 1)).await;
    }
    store.upsert_site("fresh", site(&["Email"], 100)).await;
    assert!(store.flush().await.is_empty());
    assert_eq!(store.sweep(30, 100).await.unwrap(), 3);
    assert_eq!(store.list_sites().await, ["fresh"]);
    store.upsert_site("other", site(&["Password"], 100)).await;
    assert!(store.flush().await.is_empty());
    store.forget("fresh").await.unwrap();
    assert!(store.site("fresh").await.is_none());
    drop(store);
    let (reopened, _) = ContextStore::open_with_limits(temp.path(), "profile-a", limits)
        .await
        .unwrap();
    assert_eq!(reopened.list_sites().await, ["other"]);
}

#[tokio::test]
async fn malformed_file_diagnostics_are_bounded_while_counting_every_skip() {
    let temp = tempfile::tempdir().unwrap();
    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    let root = store.root().to_path_buf();
    drop(store);
    for i in 0..300 {
        std::fs::write(root.join(format!("bad-{i}.json")), b"bad").unwrap();
    }
    let (store, report) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    assert_eq!(report.skipped_total, 300);
    assert!(report.skipped.len() < 300);
    assert_eq!(store.usage().await.resident_sites, 0);
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
    assert!(store.list_sites().await.is_empty());
}

#[tokio::test]
async fn listing_applies_retention_to_resident_context_after_a_failed_sweep() {
    let temp = tempfile::tempdir().unwrap();
    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    store.upsert_site("expired", site(&["Expired"], 69)).await;
    store.upsert_site("boundary", site(&["Boundary"], 70)).await;
    store.upsert_site("fresh", site(&["Fresh"], 100)).await;
    store.upsert_site("empty", SiteContext::default()).await;
    let mut unverified = site(&["Unverified"], 100);
    unverified
        .pages
        .get_mut("/login")
        .unwrap()
        .forms
        .get_mut("login")
        .unwrap()
        .controls[0]
        .intents
        .get_mut("fill")
        .unwrap()
        .last_verified_day = None;
    store.upsert_site("unverified", unverified).await;
    let challenge = SiteContext {
        challenges: std::collections::BTreeMap::from([(
            "challenge".into(),
            context_store::ChallengeStats::default(),
        )]),
        ..Default::default()
    };
    store.upsert_site("challenge", challenge).await;
    assert_eq!(
        store.list_sites().await,
        vec![
            "boundary",
            "challenge",
            "empty",
            "expired",
            "fresh",
            "unverified"
        ]
    );
    // The failure sets the cutoff before persistence and leaves resident
    // snapshots unpruned. Listing must still apply the public retention rule.
    std::fs::remove_dir_all(store.root()).unwrap();
    assert!(store.sweep(30, 100).await.is_err());
    assert_eq!(
        store.list_sites().await,
        vec!["boundary", "challenge", "fresh"]
    );
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

/// A closed store releases its lockfile for the next open in the same process.
///
/// `bobby context forget` opens the store, drops it, and opens it again to
/// confirm the removal took. That handoff is the whole reason the command
/// works, and nothing pinned it.
#[tokio::test]
async fn a_dropped_store_releases_its_lock_for_the_next_open() {
    let root = tempfile::tempdir().unwrap();
    let (first, _) = match ContextStore::open(root.path(), "profile-a").await {
        Ok(opened) => opened,
        Err(error) => panic!("first open must claim the lock: {error}"),
    };
    drop(first);

    let (second, _) = match ContextStore::open(root.path(), "profile-a").await {
        Ok(opened) => opened,
        Err(error) => panic!("a dropped store must release its lock: {error}"),
    };
    drop(second);

    // And again, so the release is not a one-shot.
    let (third, _) = match ContextStore::open(root.path(), "profile-a").await {
        Ok(opened) => opened,
        Err(error) => panic!("the lock must stay reusable: {error}"),
    };
    drop(third);
}

/// A dropped store releases its lock even while a forked child still holds a
/// copy of the lock descriptor.
///
/// On Linux, `std::process::Command` creates the child first and execs it
/// second. Until the exec, the child holds a copy of every descriptor in the
/// process, `O_CLOEXEC` ones included, and a flock belongs to the open file
/// description those copies share. Closing ours released nothing while some
/// other thread's child sat in that window, so `bobby context forget`
/// reported a running bobby that did not exist. `pre_exec` parks the child in
/// the window for as long as the test needs it there.
#[cfg(unix)]
#[tokio::test]
async fn a_dropped_store_releases_its_lock_while_a_forked_child_holds_a_copy() {
    use std::io::{Read, Write};
    use std::os::unix::process::CommandExt;

    let root = tempfile::tempdir().unwrap();
    let (store, _) = ContextStore::open(root.path(), "profile-a").await.unwrap();

    let (mut forked_reader, mut forked_writer) = std::io::pipe().unwrap();
    let (mut release_reader, mut release_writer) = std::io::pipe().unwrap();
    let mut command = std::process::Command::new("true");
    // SAFETY: the closure only issues read and write syscalls on pipes it
    // owns; it neither allocates nor takes a lock.
    unsafe {
        command.pre_exec(move || {
            forked_writer.write_all(&[1])?;
            release_reader.read_exact(&mut [0])?;
            Ok(())
        });
    }
    // Spawning blocks until the child execs, so it runs on its own thread,
    // the way a concurrent test or task spawns in a real process.
    let spawner = std::thread::spawn(move || command.status());
    forked_reader.read_exact(&mut [0]).unwrap();

    drop(store);
    let reopened = ContextStore::open(root.path(), "profile-a").await;

    // The child also holds a copy of the release pipe's write end, so only a
    // written byte, never a close, lets it through to the exec.
    release_writer.write_all(&[1]).unwrap();
    assert!(spawner.join().unwrap().unwrap().success());
    if let Err(error) = reopened {
        panic!("a dropped store must release its lock despite a forked child: {error}");
    }
}

/// A live store still refuses a second writer.
#[tokio::test]
async fn a_live_store_still_refuses_a_second_writer() {
    let root = tempfile::tempdir().unwrap();
    let (held, _) = ContextStore::open(root.path(), "profile-a").await.unwrap();
    let error = match ContextStore::open(root.path(), "profile-a").await {
        Ok(_) => panic!("a second writer must be refused while the first is live"),
        Err(error) => error,
    };
    assert!(
        matches!(error, ContextStoreError::AlreadyLocked),
        "expected contention, got {error}"
    );
    drop(held);
}

/// Concurrent sessions flush the same site: whatever order the writes land in,
/// the file on disk ends up equal to the newest in-memory state.
#[tokio::test(flavor = "multi_thread", worker_threads = 8)]
async fn concurrent_flushes_leave_the_newest_state_on_disk() {
    let temp = tempfile::tempdir().unwrap();
    for round in 0..20 {
        let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
        let store = std::sync::Arc::new(store);
        let mut tasks = Vec::new();
        for writer in 0..32 {
            let store = std::sync::Arc::clone(&store);
            tasks.push(tokio::spawn(async move {
                // Larger files keep each write in flight longer.
                let names = (0..writer * 20)
                    .map(|field| format!("Field {round}-{writer}-{field}"))
                    .collect::<Vec<_>>();
                let names = names.iter().map(String::as_str).collect::<Vec<_>>();
                store
                    .upsert_site("https://example.test", site(&names, 1))
                    .await;
                assert!(store.flush().await.is_empty());
            }));
        }
        for task in tasks {
            task.await.unwrap();
        }
        let newest = store.site("https://example.test").await.unwrap();
        drop(store);
        let (reopened, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
        assert_eq!(
            reopened.site("https://example.test").await.unwrap(),
            newest,
            "round {round}: an older flush overwrote a newer one"
        );
    }
}

#[tokio::test]
async fn sweep_accepts_an_empty_site_that_was_never_flushed() {
    let temp = tempfile::tempdir().unwrap();
    let (store, _) = ContextStore::open(temp.path(), "profile").await.unwrap();
    store
        .upsert_site("https://empty.test", SiteContext::default())
        .await;
    assert_eq!(store.sweep(30, 100).await.unwrap(), 0);
    assert!(store.list_sites().await.is_empty());
    assert!(store.flush().await.is_empty());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_challenges_preserve_every_outcome_and_unrelated_structure() {
    let temp = tempfile::tempdir().unwrap();
    let (store, _) = ContextStore::open(temp.path(), "profile").await.unwrap();
    store
        .upsert_site("https://example.test", site(&["Email"], 100))
        .await;
    let store = std::sync::Arc::new(store);
    let mut tasks = Vec::new();
    for _ in 0..32 {
        let store = store.clone();
        tasks.push(tokio::spawn(async move {
            for _ in 0..10 {
                store
                    .record_challenge("https://example.test", "captcha", true, 100)
                    .await;
            }
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let remembered = store.site("https://example.test").await.unwrap();
    assert_eq!(remembered.challenges["captcha"].success_count, 320);
    assert_eq!(remembered.pages, site(&["Email"], 100).pages);
    assert!(store.flush().await.is_empty());
    drop(store);
    let (reopened, _) = ContextStore::open(temp.path(), "profile").await.unwrap();
    assert_eq!(
        reopened.site("https://example.test").await.unwrap(),
        remembered
    );
}
