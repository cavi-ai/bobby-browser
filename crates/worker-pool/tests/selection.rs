use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use companion_protocol::{BrowserEngine, CompanionCapabilities};
use types::{
    CommandError, Evidence, InspectCommand, NavigateCommand, PageId, ProfileId, SessionId, WorkerId,
};
use worker_pool::{
    BrowserWorker, BrowserWorkerSelector, EnginePreference, FactoryRegistration,
    RequiredCapabilities, SelectedWorkerFactory, WorkerFactory, WorkerPool,
};

struct FailingFactory(Arc<AtomicUsize>);
struct FailOnceFactory(Arc<AtomicUsize>);

#[async_trait]
impl WorkerFactory for FailingFactory {
    async fn launch(
        &self,
        _session_id: &SessionId,
    ) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(CommandError {
            code: types::ErrorCode::BrowserLaunchFailed,
            message: "unavailable".into(),
            layer: types::ErrorLayer::Driver,
            retryable: true,
        })
    }
}

#[async_trait]
impl WorkerFactory for FailOnceFactory {
    async fn launch(
        &self,
        _session_id: &SessionId,
    ) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
            return Err(CommandError {
                code: types::ErrorCode::BrowserLaunchFailed,
                message: "temporarily unavailable".into(),
                layer: types::ErrorLayer::Driver,
                retryable: true,
            });
        }
        Ok(Arc::new(NamedWorker {
            id: WorkerId::new(),
            name: "recovered-firefox",
        }))
    }
}

struct NamedFactory(&'static str);

struct NamedWorker {
    id: WorkerId,
    name: &'static str,
}

#[async_trait]
impl WorkerFactory for NamedFactory {
    async fn launch(
        &self,
        _session_id: &SessionId,
    ) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(Arc::new(NamedWorker {
            id: WorkerId::new(),
            name: self.0,
        }))
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
}

fn capabilities(native_input: bool) -> CompanionCapabilities {
    CompanionCapabilities {
        observe: true,
        navigate: true,
        native_input,
        tabs: true,
        frames: true,
        native_dialogs: false,
    }
}

fn registration(
    name: &'static str,
    engine: BrowserEngine,
    profile_id: Option<ProfileId>,
    native_input: bool,
) -> FactoryRegistration {
    FactoryRegistration::new(
        engine,
        profile_id,
        capabilities(native_input),
        Arc::new(NamedFactory(name)),
    )
}

fn session() -> SessionId {
    SessionId::new()
}

#[tokio::test]
async fn exact_firefox_never_silently_falls_back_to_chromium() {
    let selector = BrowserWorkerSelector::new(
        vec![registration(
            "chromium",
            BrowserEngine::Chromium,
            None,
            true,
        )],
        RequiredCapabilities::default(),
    );
    let result = selector
        .select(
            &session(),
            &EnginePreference::Exact {
                engine: BrowserEngine::Firefox,
                profile_id: Some(ProfileId::new()),
            },
        )
        .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("exact Firefox unexpectedly fell back"),
    };
    assert_eq!(error.code, types::ErrorCode::PolicyDenied);
}

#[tokio::test]
async fn default_selection_requires_firefox_without_chromium_fallback() {
    let selector = BrowserWorkerSelector::new(
        vec![registration(
            "chromium",
            BrowserEngine::Chromium,
            None,
            true,
        )],
        RequiredCapabilities::default(),
    );
    let result = selector
        .select(&session(), &EnginePreference::default())
        .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("default selection unexpectedly launched Chromium"),
    };
    assert_eq!(error.code, types::ErrorCode::PolicyDenied);
}

#[tokio::test]
async fn exact_firefox_selects_only_the_requested_profile() {
    let wanted = ProfileId::new();
    let selector = BrowserWorkerSelector::new(
        vec![
            registration(
                "other",
                BrowserEngine::Firefox,
                Some(ProfileId::new()),
                true,
            ),
            registration("wanted", BrowserEngine::Firefox, Some(wanted.clone()), true),
            registration("chromium", BrowserEngine::Chromium, None, true),
        ],
        RequiredCapabilities::default(),
    );
    let selected = selector
        .select(
            &session(),
            &EnginePreference::Exact {
                engine: BrowserEngine::Firefox,
                profile_id: Some(wanted),
            },
        )
        .await
        .unwrap();
    let worker = selected.launch(&session()).await.unwrap();
    assert_eq!(worker.profile_dir(), Path::new("wanted"));
}

#[tokio::test]
async fn prefer_firefox_uses_chromium_as_ordered_fallback() {
    let selector = BrowserWorkerSelector::new(
        vec![registration(
            "chromium",
            BrowserEngine::Chromium,
            None,
            true,
        )],
        RequiredCapabilities::default(),
    );
    let selected = selector
        .select(
            &session(),
            &EnginePreference::Prefer {
                engines: vec![BrowserEngine::Firefox, BrowserEngine::Chromium],
            },
        )
        .await
        .unwrap();
    assert_eq!(
        selected.launch(&session()).await.unwrap().profile_dir(),
        Path::new("chromium")
    );
}

#[tokio::test]
async fn managed_chromium_preserves_the_compatibility_path() {
    let selector = BrowserWorkerSelector::new(
        vec![
            registration(
                "firefox",
                BrowserEngine::Firefox,
                Some(ProfileId::new()),
                true,
            ),
            registration("chromium", BrowserEngine::Chromium, None, true),
        ],
        RequiredCapabilities::default(),
    );
    let selected = selector
        .select(&session(), &EnginePreference::ManagedChromium)
        .await
        .unwrap();
    assert_eq!(
        selected.launch(&session()).await.unwrap().profile_dir(),
        Path::new("chromium")
    );
}

#[tokio::test]
async fn managed_chromium_ignores_profile_bound_chromium_companions() {
    let selector = BrowserWorkerSelector::new(
        vec![
            registration(
                "paired-chromium",
                BrowserEngine::Chromium,
                Some(ProfileId::new()),
                true,
            ),
            registration("managed-chromium", BrowserEngine::Chromium, None, true),
        ],
        RequiredCapabilities::default(),
    );
    let selected = selector
        .select(&session(), &EnginePreference::ManagedChromium)
        .await
        .unwrap();
    assert_eq!(
        selected.launch(&session()).await.unwrap().profile_dir(),
        Path::new("managed-chromium")
    );
}

#[tokio::test]
async fn missing_required_capability_is_policy_denied() {
    let selector = BrowserWorkerSelector::new(
        vec![registration(
            "firefox",
            BrowserEngine::Firefox,
            Some(ProfileId::new()),
            false,
        )],
        RequiredCapabilities {
            native_input: true,
            ..RequiredCapabilities::default()
        },
    );
    let result = selector
        .select(
            &session(),
            &EnginePreference::Prefer {
                engines: vec![BrowserEngine::Firefox],
            },
        )
        .await;
    let error = match result {
        Err(error) => error,
        Ok(_) => panic!("missing capability unexpectedly selected"),
    };
    assert_eq!(error.code, types::ErrorCode::PolicyDenied);
}

#[tokio::test]
async fn session_selection_is_sticky_for_its_lifetime() {
    let selector = BrowserWorkerSelector::new(
        vec![
            registration(
                "firefox",
                BrowserEngine::Firefox,
                Some(ProfileId::new()),
                true,
            ),
            registration("chromium", BrowserEngine::Chromium, None, true),
        ],
        RequiredCapabilities::default(),
    );
    let session = session();
    let first = selector
        .select(
            &session,
            &EnginePreference::Prefer {
                engines: vec![BrowserEngine::Firefox],
            },
        )
        .await
        .unwrap();
    let second = selector
        .select(&session, &EnginePreference::ManagedChromium)
        .await
        .unwrap();
    assert!(Arc::ptr_eq(&first, &second));
}

#[tokio::test]
async fn prefer_falls_back_when_the_preferred_factory_fails_to_launch() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let selector = Arc::new(BrowserWorkerSelector::new(
        vec![
            FactoryRegistration::new(
                BrowserEngine::Firefox,
                Some(ProfileId::new()),
                capabilities(true),
                Arc::new(FailingFactory(attempts.clone())),
            ),
            registration("chromium", BrowserEngine::Chromium, None, true),
        ],
        RequiredCapabilities::default(),
    ));
    let pool = WorkerPool::new(
        1,
        Arc::new(SelectedWorkerFactory::new(
            selector,
            EnginePreference::Prefer {
                engines: vec![BrowserEngine::Firefox, BrowserEngine::Chromium],
            },
        )),
    );

    let lease = pool.lease(session()).await.unwrap();
    assert_eq!(lease.profile_dir(), Path::new("chromium"));
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn exact_launch_failure_never_falls_back() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let profile_id = ProfileId::new();
    let selector = Arc::new(BrowserWorkerSelector::new(
        vec![
            FactoryRegistration::new(
                BrowserEngine::Firefox,
                Some(profile_id.clone()),
                capabilities(true),
                Arc::new(FailingFactory(attempts.clone())),
            ),
            registration("chromium", BrowserEngine::Chromium, None, true),
        ],
        RequiredCapabilities::default(),
    ));
    let pool = WorkerPool::new(
        1,
        Arc::new(SelectedWorkerFactory::new(
            selector,
            EnginePreference::Exact {
                engine: BrowserEngine::Firefox,
                profile_id: Some(profile_id),
            },
        )),
    );

    let error = match pool.lease(session()).await {
        Err(error) => error,
        Ok(_) => panic!("exact unexpectedly fell back"),
    };
    assert_eq!(error.code, types::ErrorCode::BrowserLaunchFailed);
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn failed_launch_does_not_leave_a_stale_selected_choice() {
    let attempts = Arc::new(AtomicUsize::new(0));
    let profile_id = ProfileId::new();
    let selector = Arc::new(BrowserWorkerSelector::new(
        vec![FactoryRegistration::new(
            BrowserEngine::Firefox,
            Some(profile_id.clone()),
            capabilities(true),
            Arc::new(FailOnceFactory(attempts.clone())),
        )],
        RequiredCapabilities::default(),
    ));
    let pool = WorkerPool::new(
        1,
        Arc::new(SelectedWorkerFactory::new(
            selector,
            EnginePreference::Exact {
                engine: BrowserEngine::Firefox,
                profile_id: Some(profile_id),
            },
        )),
    );
    let session = session();
    assert!(pool.lease(session.clone()).await.is_err());
    let recovered = pool.lease(session).await.unwrap();
    assert_eq!(recovered.profile_dir(), Path::new("recovered-firefox"));
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}
