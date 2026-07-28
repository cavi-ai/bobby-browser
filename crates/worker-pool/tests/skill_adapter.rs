use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use companion_protocol::{BrowserEngine, CompanionCapabilities};
use skill_runtime::{
    SkillBrowserEngine, SkillCapability, SkillEngineAdapter, SkillFailure, SkillProfileRequest,
};
use tokio::{sync::Semaphore, time::Duration};
use types::{CommandError, Evidence, InspectCommand, NavigateCommand, PageId, SessionId, WorkerId};
use worker_pool::{
    BrowserWorker, BrowserWorkerSelector, ChromiumSkillAdapter, EnginePreference,
    FactoryRegistration, FirefoxSkillAdapter, RequiredCapabilities, SelectedWorkerFactory,
    WorkerFactory, WorkerPool,
};

struct CountingFactory {
    name: &'static str,
    launches: Arc<AtomicUsize>,
    releases: Arc<AtomicUsize>,
}

struct NamedWorker {
    id: WorkerId,
    name: &'static str,
    terminations: Option<Arc<AtomicUsize>>,
}

struct DelayedReleaseFactory {
    name: &'static str,
    release_started: Arc<Semaphore>,
    release_finish: Arc<Semaphore>,
    release_mutations: Arc<AtomicUsize>,
    terminations: Arc<AtomicUsize>,
}

#[async_trait]
impl WorkerFactory for DelayedReleaseFactory {
    async fn launch(
        &self,
        _session_id: &SessionId,
    ) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(Arc::new(NamedWorker {
            id: WorkerId::new(),
            name: self.name,
            terminations: Some(self.terminations.clone()),
        }))
    }

    async fn release_session(&self, _session_id: &SessionId) {
        self.release_mutations.fetch_add(1, Ordering::SeqCst);
        self.release_started.add_permits(1);
        self.release_finish.acquire().await.unwrap().forget();
    }
}

#[async_trait]
impl WorkerFactory for CountingFactory {
    async fn launch(
        &self,
        _session_id: &SessionId,
    ) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        self.launches.fetch_add(1, Ordering::SeqCst);
        Ok(Arc::new(NamedWorker {
            id: WorkerId::new(),
            name: self.name,
            terminations: None,
        }))
    }

    async fn release_session(&self, _session_id: &SessionId) {
        self.releases.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl BrowserWorker for NamedWorker {
    fn worker_id(&self) -> WorkerId {
        self.id.clone()
    }

    fn profile_dir(&self) -> &Path {
        Path::new(self.name)
    }

    async fn open_page(&self, _page_id: PageId) -> Result<(), CommandError> {
        Ok(())
    }

    async fn navigate(
        &self,
        _page_id: &PageId,
        _command: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![])
    }

    async fn inspect(
        &self,
        _page_id: &PageId,
        _command: &InspectCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![])
    }

    async fn click(
        &self,
        _page_id: &PageId,
        _command: &types::ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![])
    }

    async fn type_text(
        &self,
        _page_id: &PageId,
        _command: &types::TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![])
    }

    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }

    async fn terminate(&self) -> Result<(), CommandError> {
        if let Some(terminations) = &self.terminations {
            terminations.fetch_add(1, Ordering::SeqCst);
        }
        Ok(())
    }
}

fn capabilities() -> CompanionCapabilities {
    CompanionCapabilities {
        observe: true,
        navigate: true,
        native_input: true,
        tabs: true,
        frames: true,
        native_dialogs: true,
    }
}

fn registration(
    engine: BrowserEngine,
    name: &'static str,
    launches: Arc<AtomicUsize>,
    releases: Arc<AtomicUsize>,
) -> FactoryRegistration {
    FactoryRegistration::new(
        engine,
        None,
        capabilities(),
        Arc::new(CountingFactory {
            name,
            launches,
            releases,
        }),
    )
}

fn request(
    required: impl IntoIterator<Item = SkillCapability>,
    optional: impl IntoIterator<Item = SkillCapability>,
    engine: SkillBrowserEngine,
    values: BTreeMap<String, String>,
) -> SkillProfileRequest {
    SkillProfileRequest::new(required, optional, [engine], values).unwrap()
}

fn selected_pool(
    selector: Arc<BrowserWorkerSelector>,
    preference: EnginePreference,
    timeout: Duration,
) -> Arc<WorkerPool> {
    Arc::new(WorkerPool::with_replacement_timeout(
        8,
        Arc::new(SelectedWorkerFactory::new(selector, preference)),
        timeout,
    ))
}

#[tokio::test]
#[cfg(feature = "test-support")]
async fn required_capability_fails_before_launch_and_optional_degradation_is_reported() {
    let launches = Arc::new(AtomicUsize::new(0));
    let selector = Arc::new(BrowserWorkerSelector::new(
        vec![registration(
            BrowserEngine::Chromium,
            "chromium",
            launches.clone(),
            Arc::new(AtomicUsize::new(0)),
        )],
        RequiredCapabilities::default(),
    ));
    let adapter = ChromiumSkillAdapter::for_test(
        selected_pool(
            selector,
            EnginePreference::ManagedChromium,
            Duration::from_secs(1),
        ),
        "chromium-v1",
        [SkillCapability::Locale],
        BTreeMap::from([("locale".into(), "en-US".into())]),
    )
    .unwrap();

    let required = request(
        [SkillCapability::Timezone],
        [],
        SkillBrowserEngine::Chromium,
        BTreeMap::from([("timezone".into(), "Europe/London".into())]),
    );
    assert_eq!(
        adapter.resolve_profile(&required).await.unwrap_err(),
        SkillFailure::UnsupportedCapability
    );
    assert_eq!(launches.load(Ordering::SeqCst), 0);

    let optional = request(
        [SkillCapability::Locale],
        [SkillCapability::Timezone],
        SkillBrowserEngine::Chromium,
        BTreeMap::new(),
    );
    let effective = adapter.resolve_profile(&optional).await.unwrap();
    assert_eq!(
        effective.unsupported_optional,
        [SkillCapability::Timezone].into_iter().collect()
    );
    assert_eq!(launches.load(Ordering::SeqCst), 0);
}

#[tokio::test]
#[cfg(feature = "test-support")]
async fn an_unregistered_engine_is_unavailable_before_profile_resolution() {
    let adapter = FirefoxSkillAdapter::for_test(
        selected_pool(
            Arc::new(BrowserWorkerSelector::new(
                vec![],
                RequiredCapabilities::default(),
            )),
            EnginePreference::ManagedChromium,
            Duration::from_secs(1),
        ),
        "firefox-v1",
        [SkillCapability::Locale],
        BTreeMap::from([("locale".into(), "en-US".into())]),
    )
    .unwrap();
    let result = adapter
        .resolve_profile(&request(
            [SkillCapability::Locale],
            [],
            SkillBrowserEngine::Firefox,
            BTreeMap::new(),
        ))
        .await;
    assert_eq!(result.unwrap_err(), SkillFailure::EngineUnavailable);
}

#[tokio::test]
#[cfg(feature = "test-support")]
async fn adapters_use_actual_values_for_a_stable_secret_free_canonical_digest() {
    let launches = Arc::new(AtomicUsize::new(0));
    let selector = Arc::new(BrowserWorkerSelector::new(
        vec![registration(
            BrowserEngine::Firefox,
            "firefox",
            launches,
            Arc::new(AtomicUsize::new(0)),
        )],
        RequiredCapabilities::default(),
    ));
    let firefox = FirefoxSkillAdapter::for_test(
        selected_pool(
            selector,
            EnginePreference::ManagedChromium,
            Duration::from_secs(1),
        ),
        "firefox-v1",
        [SkillCapability::Locale, SkillCapability::Timezone],
        BTreeMap::from([
            ("timezone".into(), "America/New_York".into()),
            ("locale".into(), "en-US".into()),
        ]),
    )
    .unwrap();
    let first = request(
        [SkillCapability::Locale, SkillCapability::Timezone],
        [],
        SkillBrowserEngine::Firefox,
        BTreeMap::from([
            ("locale".into(), "fr-FR".into()),
            ("timezone".into(), "Europe/Paris".into()),
        ]),
    );
    let second = request(
        [SkillCapability::Locale, SkillCapability::Timezone],
        [],
        SkillBrowserEngine::Firefox,
        BTreeMap::from([
            ("timezone".into(), "Asia/Tokyo".into()),
            ("locale".into(), "de-DE".into()),
        ]),
    );

    let first = firefox.resolve_profile(&first).await.unwrap();
    let second = firefox.resolve_profile(&second).await.unwrap();
    assert_eq!(
        first.profile.observable_digest,
        second.profile.observable_digest
    );
    assert_eq!(first.profile.observable_digest.len(), 64);
    assert!(first
        .profile
        .observable_digest
        .bytes()
        .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase()));
}

#[tokio::test]
#[cfg(feature = "test-support")]
async fn firefox_and_chromium_share_the_engine_neutral_contract() {
    let launches = Arc::new(AtomicUsize::new(0));
    let selector = Arc::new(BrowserWorkerSelector::new(
        vec![
            registration(
                BrowserEngine::Firefox,
                "firefox",
                launches.clone(),
                Arc::new(AtomicUsize::new(0)),
            ),
            registration(
                BrowserEngine::Chromium,
                "chromium",
                launches,
                Arc::new(AtomicUsize::new(0)),
            ),
        ],
        RequiredCapabilities::default(),
    ));
    let supported = [SkillCapability::Locale, SkillCapability::Viewport];
    let actual = BTreeMap::from([
        ("locale".into(), "en-US".into()),
        ("viewport".into(), "desktop".into()),
    ]);
    let pool = selected_pool(
        selector,
        EnginePreference::ManagedChromium,
        Duration::from_secs(1),
    );
    let firefox =
        FirefoxSkillAdapter::for_test(pool.clone(), "firefox-v1", supported, actual.clone())
            .unwrap();
    let chromium = ChromiumSkillAdapter::for_test(pool, "chromium-v1", supported, actual).unwrap();

    for (adapter, engine) in [
        (
            &firefox as &dyn SkillEngineAdapter,
            SkillBrowserEngine::Firefox,
        ),
        (
            &chromium as &dyn SkillEngineAdapter,
            SkillBrowserEngine::Chromium,
        ),
    ] {
        let effective = adapter
            .resolve_profile(&request(
                [SkillCapability::Locale],
                [SkillCapability::Viewport],
                engine,
                BTreeMap::new(),
            ))
            .await
            .unwrap();
        assert_eq!(adapter.engine(), engine);
        assert_eq!(effective.profile.engine, engine);
        assert_eq!(effective.profile.effective_capabilities.len(), 2);
        assert!(effective.unsupported_optional.is_empty());
    }
}

#[tokio::test]
async fn selector_replacement_releases_the_old_choice_before_using_the_new_one() {
    let firefox_launches = Arc::new(AtomicUsize::new(0));
    let firefox_releases = Arc::new(AtomicUsize::new(0));
    let chromium_launches = Arc::new(AtomicUsize::new(0));
    let selector = BrowserWorkerSelector::new(
        vec![
            registration(
                BrowserEngine::Firefox,
                "firefox",
                firefox_launches.clone(),
                firefox_releases.clone(),
            ),
            registration(
                BrowserEngine::Chromium,
                "chromium",
                chromium_launches.clone(),
                Arc::new(AtomicUsize::new(0)),
            ),
        ],
        RequiredCapabilities::default(),
    );
    let session = SessionId::new();
    selector
        .select(
            &session,
            &EnginePreference::Prefer {
                engines: vec![BrowserEngine::Firefox],
            },
        )
        .await
        .unwrap()
        .launch(&session)
        .await
        .unwrap();

    let replacement = selector
        .replace_session(&session, &EnginePreference::ManagedChromium)
        .await
        .unwrap();
    assert_eq!(firefox_releases.load(Ordering::SeqCst), 1);
    assert_eq!(
        replacement.launch(&session).await.unwrap().profile_dir(),
        Path::new("chromium")
    );
    assert_eq!(firefox_launches.load(Ordering::SeqCst), 1);
    assert_eq!(chromium_launches.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn concurrent_select_waits_for_old_release_then_observes_only_the_replacement() {
    let release_started = Arc::new(Semaphore::new(0));
    let release_finish = Arc::new(Semaphore::new(0));
    let selector = Arc::new(BrowserWorkerSelector::with_replacement_timeout(
        vec![
            FactoryRegistration::new(
                BrowserEngine::Firefox,
                None,
                capabilities(),
                Arc::new(DelayedReleaseFactory {
                    name: "firefox",
                    release_started: release_started.clone(),
                    release_finish: release_finish.clone(),
                    release_mutations: Arc::new(AtomicUsize::new(0)),
                    terminations: Arc::new(AtomicUsize::new(0)),
                }),
            ),
            registration(
                BrowserEngine::Chromium,
                "chromium",
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
            ),
        ],
        RequiredCapabilities::default(),
        Duration::from_secs(1),
    ));
    let session = SessionId::new();
    selector
        .select(
            &session,
            &EnginePreference::Prefer {
                engines: vec![BrowserEngine::Firefox],
            },
        )
        .await
        .unwrap()
        .launch(&session)
        .await
        .unwrap();

    let replacing_selector = selector.clone();
    let replacing_session = session.clone();
    let replace = tokio::spawn(async move {
        replacing_selector
            .replace_session(&replacing_session, &EnginePreference::ManagedChromium)
            .await
    });
    release_started.acquire().await.unwrap().forget();

    let selecting_selector = selector.clone();
    let selecting_session = session.clone();
    let mut select = tokio::spawn(async move {
        selecting_selector
            .select(
                &selecting_session,
                &EnginePreference::Prefer {
                    engines: vec![BrowserEngine::Firefox],
                },
            )
            .await
    });
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut select)
        .await
        .is_err());

    release_finish.add_permits(1);
    let replacement = replace.await.unwrap().unwrap();
    let selected = select.await.unwrap().unwrap();
    assert!(Arc::ptr_eq(&replacement, &selected));
    assert_eq!(
        selected.launch(&session).await.unwrap().profile_dir(),
        Path::new("chromium")
    );
}

#[tokio::test]
async fn release_keeps_one_session_gate_across_waiting_replace_and_select() {
    let release_started = Arc::new(Semaphore::new(0));
    let release_finish = Arc::new(Semaphore::new(0));
    let selector = Arc::new(BrowserWorkerSelector::with_replacement_timeout(
        vec![
            FactoryRegistration::new(
                BrowserEngine::Firefox,
                None,
                capabilities(),
                Arc::new(DelayedReleaseFactory {
                    name: "firefox",
                    release_started: release_started.clone(),
                    release_finish: release_finish.clone(),
                    release_mutations: Arc::new(AtomicUsize::new(0)),
                    terminations: Arc::new(AtomicUsize::new(0)),
                }),
            ),
            registration(
                BrowserEngine::Chromium,
                "chromium",
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
            ),
        ],
        RequiredCapabilities::default(),
        Duration::from_secs(1),
    ));
    let session = SessionId::new();
    selector
        .select(
            &session,
            &EnginePreference::Prefer {
                engines: vec![BrowserEngine::Firefox],
            },
        )
        .await
        .unwrap()
        .launch(&session)
        .await
        .unwrap();

    let releasing_selector = selector.clone();
    let releasing_session = session.clone();
    let release = tokio::spawn(async move {
        releasing_selector.release_session(&releasing_session).await;
    });
    release_started.acquire().await.unwrap().forget();

    let replacing_selector = selector.clone();
    let replacing_session = session.clone();
    let mut replace = tokio::spawn(async move {
        replacing_selector
            .replace_session(&replacing_session, &EnginePreference::ManagedChromium)
            .await
    });
    tokio::task::yield_now().await;
    let selecting_selector = selector.clone();
    let selecting_session = session.clone();
    let mut select = tokio::spawn(async move {
        selecting_selector
            .select(
                &selecting_session,
                &EnginePreference::Prefer {
                    engines: vec![BrowserEngine::Firefox],
                },
            )
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut replace)
            .await
            .is_err()
    );
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut select)
        .await
        .is_err());

    release_finish.add_permits(1);
    release.await.unwrap();
    let replacement = replace.await.unwrap().unwrap();
    let selected = select.await.unwrap().unwrap();
    assert!(Arc::ptr_eq(&replacement, &selected));
    assert_eq!(
        selected.launch(&session).await.unwrap().profile_dir(),
        Path::new("chromium")
    );
}

#[tokio::test]
async fn replacement_timeout_keeps_selection_unavailable_until_owned_cleanup_finishes() {
    let release_started = Arc::new(Semaphore::new(0));
    let release_finish = Arc::new(Semaphore::new(0));
    let release_mutations = Arc::new(AtomicUsize::new(0));
    let selector = Arc::new(BrowserWorkerSelector::with_replacement_timeout(
        vec![
            FactoryRegistration::new(
                BrowserEngine::Firefox,
                None,
                capabilities(),
                Arc::new(DelayedReleaseFactory {
                    name: "firefox",
                    release_started: release_started.clone(),
                    release_finish: release_finish.clone(),
                    release_mutations: release_mutations.clone(),
                    terminations: Arc::new(AtomicUsize::new(0)),
                }),
            ),
            registration(
                BrowserEngine::Chromium,
                "chromium",
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
            ),
        ],
        RequiredCapabilities::default(),
        Duration::from_millis(20),
    ));
    let session = SessionId::new();
    let old = selector
        .select(
            &session,
            &EnginePreference::Prefer {
                engines: vec![BrowserEngine::Firefox],
            },
        )
        .await
        .unwrap();
    old.launch(&session).await.unwrap();

    let error = match selector
        .replace_session(&session, &EnginePreference::ManagedChromium)
        .await
    {
        Err(error) => error,
        Ok(_) => panic!("replacement unexpectedly ignored the cleanup timeout"),
    };
    assert_eq!(error.code, types::ErrorCode::DeadlineExceeded);
    assert_eq!(release_mutations.load(Ordering::SeqCst), 1);

    let selecting_selector = selector.clone();
    let selecting_session = session.clone();
    let mut selecting = tokio::spawn(async move {
        selecting_selector
            .select(&selecting_session, &EnginePreference::ManagedChromium)
            .await
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(20), &mut selecting)
            .await
            .is_err()
    );

    release_finish.add_permits(1);
    let replacement = selecting.await.unwrap().unwrap();
    assert!(!Arc::ptr_eq(&old, &replacement));
    assert_eq!(
        replacement.launch(&session).await.unwrap().profile_dir(),
        Path::new("chromium")
    );
}

#[tokio::test]
async fn pool_owned_replacement_times_out_without_exposing_a_cached_or_new_worker() {
    let release_started = Arc::new(Semaphore::new(0));
    let release_finish = Arc::new(Semaphore::new(0));
    let release_mutations = Arc::new(AtomicUsize::new(0));
    let terminations = Arc::new(AtomicUsize::new(0));
    let selector = Arc::new(BrowserWorkerSelector::with_replacement_timeout(
        vec![
            FactoryRegistration::new(
                BrowserEngine::Firefox,
                None,
                capabilities(),
                Arc::new(DelayedReleaseFactory {
                    name: "firefox",
                    release_started: release_started.clone(),
                    release_finish: release_finish.clone(),
                    release_mutations: release_mutations.clone(),
                    terminations: terminations.clone(),
                }),
            ),
            registration(
                BrowserEngine::Chromium,
                "chromium",
                Arc::new(AtomicUsize::new(0)),
                Arc::new(AtomicUsize::new(0)),
            ),
        ],
        RequiredCapabilities::default(),
        Duration::from_secs(1),
    ));
    let pool = selected_pool(
        selector.clone(),
        EnginePreference::Prefer {
            engines: vec![BrowserEngine::Firefox],
        },
        Duration::from_millis(20),
    );
    let adapter = ChromiumSkillAdapter::production(pool.clone()).unwrap();
    let session = SessionId::new();
    let old = pool.lease(session.clone()).await.unwrap();
    let old_id = old.worker_id();
    assert_eq!(old.profile_dir(), Path::new("firefox"));

    assert_eq!(
        adapter.prepare_restart(&session).await.unwrap_err(),
        SkillFailure::DeadlineExceeded
    );
    assert_eq!(terminations.load(Ordering::SeqCst), 0);
    assert_eq!(release_mutations.load(Ordering::SeqCst), 0);

    let leasing_pool = pool.clone();
    let leasing_session = session.clone();
    let mut lease = tokio::spawn(async move { leasing_pool.lease(leasing_session).await });
    assert!(tokio::time::timeout(Duration::from_millis(20), &mut lease)
        .await
        .is_err());

    drop(old);
    release_started.acquire().await.unwrap().forget();
    assert_eq!(release_mutations.load(Ordering::SeqCst), 1);
    assert_eq!(terminations.load(Ordering::SeqCst), 1);

    release_finish.add_permits(1);
    let replacement = lease.await.unwrap().unwrap();
    assert_ne!(replacement.worker_id(), old_id);
    assert_eq!(replacement.profile_dir(), Path::new("chromium"));
}

#[tokio::test]
#[cfg(feature = "test-support")]
async fn released_selector_sessions_are_reclaimed_and_can_be_reused() {
    let selector = BrowserWorkerSelector::new(
        vec![registration(
            BrowserEngine::Chromium,
            "chromium",
            Arc::new(AtomicUsize::new(0)),
            Arc::new(AtomicUsize::new(0)),
        )],
        RequiredCapabilities::default(),
    );

    for _ in 0..64 {
        let session = SessionId::new();
        selector
            .select(&session, &EnginePreference::ManagedChromium)
            .await
            .unwrap();
        selector.release_session(&session).await;
    }
    assert_eq!(selector.retained_session_count().await, 0);

    let reused = SessionId::new();
    selector
        .select(&reused, &EnginePreference::ManagedChromium)
        .await
        .unwrap();
    assert_eq!(selector.retained_session_count().await, 1);
    selector.release_session(&reused).await;
    assert_eq!(selector.retained_session_count().await, 0);
}

#[tokio::test]
async fn production_adapters_claim_only_capabilities_known_from_runtime_construction() {
    let launches = Arc::new(AtomicUsize::new(0));
    let selector = Arc::new(BrowserWorkerSelector::new(
        vec![
            registration(
                BrowserEngine::Firefox,
                "firefox",
                launches.clone(),
                Arc::new(AtomicUsize::new(0)),
            ),
            registration(
                BrowserEngine::Chromium,
                "chromium",
                launches,
                Arc::new(AtomicUsize::new(0)),
            ),
        ],
        RequiredCapabilities::default(),
    ));

    let adapters: Vec<(Box<dyn SkillEngineAdapter>, SkillBrowserEngine)> = vec![
        (
            Box::new(
                FirefoxSkillAdapter::production(selected_pool(
                    selector.clone(),
                    EnginePreference::ManagedChromium,
                    Duration::from_secs(1),
                ))
                .unwrap(),
            ),
            SkillBrowserEngine::Firefox,
        ),
        (
            Box::new(
                ChromiumSkillAdapter::production(selected_pool(
                    selector.clone(),
                    EnginePreference::ManagedChromium,
                    Duration::from_secs(1),
                ))
                .unwrap(),
            ),
            SkillBrowserEngine::Chromium,
        ),
    ];
    for (adapter, engine) in adapters {
        let effective = adapter
            .resolve_profile(&request(
                [
                    SkillCapability::EngineSelection,
                    SkillCapability::ProfilePersistence,
                ],
                [SkillCapability::Locale],
                engine,
                BTreeMap::new(),
            ))
            .await
            .unwrap();
        assert_eq!(
            effective.unsupported_optional,
            [SkillCapability::Locale].into_iter().collect()
        );
    }
}
