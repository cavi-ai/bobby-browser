use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use skill_runtime::{
    EffectiveSkillProfile, SessionId, Skill, SkillBrowserEngine, SkillCapability, SkillCommand,
    SkillContext, SkillEngineAdapter, SkillFailure, SkillGhost, SkillGhostCommand, SkillOutcome,
    SkillProfile, SkillProfileRequest, SkillRegistry, SkillSessionState, SkillStateStore,
};
use tokio::sync::Semaphore;

struct FakeAdapter {
    engine: SkillBrowserEngine,
    supported: BTreeSet<SkillCapability>,
    resolutions: AtomicUsize,
    restarts: AtomicUsize,
}

struct BlockingRestartAdapter {
    inner: FakeAdapter,
    restart_started: Arc<Semaphore>,
    restart_finish: Arc<Semaphore>,
}

impl FakeAdapter {
    fn supporting(
        engine: SkillBrowserEngine,
        supported: impl IntoIterator<Item = SkillCapability>,
    ) -> Self {
        Self {
            engine,
            supported: supported.into_iter().collect(),
            resolutions: AtomicUsize::new(0),
            restarts: AtomicUsize::new(0),
        }
    }
}

#[async_trait]
impl SkillEngineAdapter for FakeAdapter {
    fn engine(&self) -> SkillBrowserEngine {
        self.engine
    }

    async fn resolve_profile(
        &self,
        request: &SkillProfileRequest,
    ) -> Result<EffectiveSkillProfile, SkillFailure> {
        self.resolutions.fetch_add(1, Ordering::SeqCst);
        if !request.required.is_subset(&self.supported) {
            return Err(SkillFailure::UnsupportedCapability);
        }
        let effective_capabilities = request
            .required
            .union(&request.optional)
            .copied()
            .filter(|capability| self.supported.contains(capability))
            .collect::<BTreeSet<_>>();
        Ok(EffectiveSkillProfile {
            profile: SkillProfile::new(
                "1.0.0",
                self.engine,
                effective_capabilities,
                format!("{:064x}", request.required.len() + request.optional.len()),
            )
            .unwrap(),
            unsupported_optional: request
                .optional
                .difference(&self.supported)
                .copied()
                .collect(),
            restart_required: false,
        })
    }

    async fn prepare_restart(&self, _session: &SessionId) -> Result<(), SkillFailure> {
        self.restarts.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}

#[async_trait]
impl SkillEngineAdapter for BlockingRestartAdapter {
    fn engine(&self) -> SkillBrowserEngine {
        self.inner.engine()
    }

    async fn resolve_profile(
        &self,
        request: &SkillProfileRequest,
    ) -> Result<EffectiveSkillProfile, SkillFailure> {
        self.inner.resolve_profile(request).await
    }

    async fn prepare_restart(&self, _session: &SessionId) -> Result<(), SkillFailure> {
        self.restart_started.add_permits(1);
        self.restart_finish.acquire().await.unwrap().forget();
        Ok(())
    }
}

fn request(
    required: impl IntoIterator<Item = SkillCapability>,
    optional: impl IntoIterator<Item = SkillCapability>,
    engine: SkillBrowserEngine,
) -> SkillProfileRequest {
    SkillProfileRequest::new(required, optional, [engine], BTreeMap::new()).unwrap()
}

fn session_state(session_id: SessionId) -> SkillSessionState {
    SkillSessionState::new(
        session_id,
        BTreeMap::new(),
        None,
        None,
        None,
        None,
        None,
        vec![],
        vec![],
        Utc::now() + Duration::minutes(5),
    )
    .unwrap()
}

fn context(
    session: SessionId,
    granted: impl IntoIterator<Item = SkillCapability>,
    profile: Option<SkillProfileRequest>,
) -> SkillContext {
    SkillContext::with_granted_capabilities([], granted).with_ghost(session, profile)
}

#[tokio::test]
async fn ghost_profile_is_frozen_until_safe_restart() {
    let store = Arc::new(SkillStateStore::new());
    let session = SessionId::new();
    store.insert(session_state(session.clone())).unwrap();
    let adapter = Arc::new(FakeAdapter::supporting(
        SkillBrowserEngine::Firefox,
        [SkillCapability::Locale, SkillCapability::Timezone],
    ));
    let ghost = SkillGhost::new(store.clone(), vec![adapter]);
    let locale = context(
        session.clone(),
        [SkillCapability::Locale],
        Some(request(
            [SkillCapability::Locale],
            [],
            SkillBrowserEngine::Firefox,
        )),
    );

    let first = ghost.on(&locale).await.unwrap();
    assert!(!first.status.restart_required);
    assert!(first.status.active);

    assert_eq!(
        ghost
            .on(&context(
                session.clone(),
                [SkillCapability::Timezone],
                Some(request(
                    [SkillCapability::Timezone],
                    [],
                    SkillBrowserEngine::Firefox,
                )),
            ))
            .await
            .unwrap_err(),
        SkillFailure::ConfigurationConflict
    );
    assert_eq!(
        store.get(&session).unwrap().effective_profile,
        first.status.profile
    );
}

#[tokio::test]
async fn optional_degradation_is_visible_and_off_retains_profile_until_replacement() {
    let store = Arc::new(SkillStateStore::new());
    let session = SessionId::new();
    store.insert(session_state(session.clone())).unwrap();
    let adapter = Arc::new(FakeAdapter::supporting(
        SkillBrowserEngine::Chromium,
        [SkillCapability::Locale],
    ));
    let ghost = SkillGhost::new(store.clone(), vec![adapter.clone()]);

    let enabled = ghost
        .on(&context(
            session.clone(),
            [SkillCapability::Locale, SkillCapability::Timezone],
            Some(request(
                [SkillCapability::Locale],
                [SkillCapability::Timezone],
                SkillBrowserEngine::Chromium,
            )),
        ))
        .await
        .unwrap();
    assert_eq!(
        enabled.status.unsupported_optional,
        BTreeSet::from([SkillCapability::Timezone])
    );

    let session_context = context(session.clone(), [], None);
    let disabled = ghost.off(&session_context).await.unwrap();
    assert!(!disabled.status.active);
    assert!(disabled.status.restart_required);
    assert!(store.get(&session).unwrap().effective_profile.is_some());

    let replaced = ghost.replace_session(&session_context).await.unwrap();
    assert!(!replaced.status.restart_required);
    assert!(replaced.status.profile.is_none());
    assert!(store.get(&session).unwrap().effective_profile.is_none());
    assert_eq!(adapter.restarts.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_pending_restart_cannot_be_cancelled_by_reenabling_the_frozen_profile() {
    let store = Arc::new(SkillStateStore::new());
    let session = SessionId::new();
    store.insert(session_state(session.clone())).unwrap();
    let adapter = Arc::new(FakeAdapter::supporting(
        SkillBrowserEngine::Chromium,
        [SkillCapability::Locale],
    ));
    let ghost = SkillGhost::new(store, vec![adapter]);
    let enabled_context = context(
        session.clone(),
        [SkillCapability::Locale],
        Some(request(
            [SkillCapability::Locale],
            [],
            SkillBrowserEngine::Chromium,
        )),
    );
    let session_context = context(session.clone(), [], None);

    ghost.on(&enabled_context).await.unwrap();
    ghost.off(&session_context).await.unwrap();
    assert_eq!(
        ghost.on(&enabled_context).await.unwrap_err(),
        SkillFailure::ConfigurationConflict
    );
    assert!(
        ghost
            .status(&session_context)
            .await
            .unwrap()
            .restart_required
    );
}

#[tokio::test]
async fn registry_executes_ghost_on_status_and_off_with_session_context() {
    let store = Arc::new(SkillStateStore::new());
    let session = SessionId::new();
    store.insert(session_state(session.clone())).unwrap();
    let adapter = Arc::new(FakeAdapter::supporting(
        SkillBrowserEngine::Chromium,
        [SkillCapability::Locale],
    ));
    let ghost = Arc::new(SkillGhost::new(store.clone(), vec![adapter]));
    let mut registry = SkillRegistry::new();
    registry.register(ghost).unwrap();
    let on_context = context(
        session.clone(),
        [SkillCapability::Locale],
        Some(request(
            [SkillCapability::Locale],
            [],
            SkillBrowserEngine::Chromium,
        )),
    );
    let session_context = context(session.clone(), [], None);

    assert!(registry.resolve("/ghost", &session_context).is_ok());

    assert!(matches!(
        registry
            .execute(SkillCommand::Ghost(SkillGhostCommand::On), &on_context)
            .await,
        Ok(SkillOutcome::Applied { .. })
    ));
    assert!(matches!(
        registry
            .execute(
                SkillCommand::Ghost(SkillGhostCommand::Status),
                &session_context,
            )
            .await,
        Ok(SkillOutcome::Applied { .. })
    ));
    assert!(matches!(
        registry
            .execute(
                SkillCommand::Ghost(SkillGhostCommand::Off),
                &session_context,
            )
            .await,
        Ok(SkillOutcome::Stopped { .. })
    ));
    assert!(!ghost_status(&store, &session));
}

#[tokio::test]
async fn registry_rejects_missing_or_ungranted_ghost_profile_before_resolution() {
    let store = Arc::new(SkillStateStore::new());
    let session = SessionId::new();
    store.insert(session_state(session.clone())).unwrap();
    let adapter = Arc::new(FakeAdapter::supporting(
        SkillBrowserEngine::Chromium,
        [SkillCapability::Locale],
    ));
    let ghost = Arc::new(SkillGhost::new(store.clone(), vec![adapter.clone()]));
    let mut registry = SkillRegistry::new();
    registry.register(ghost).unwrap();

    assert_eq!(
        registry
            .execute(
                SkillCommand::Ghost(SkillGhostCommand::On),
                &context(session.clone(), [SkillCapability::Locale], None),
            )
            .await,
        Err(SkillFailure::ConfigurationConflict)
    );
    assert_eq!(adapter.resolutions.load(Ordering::SeqCst), 0);

    let ungranted = context(
        session.clone(),
        [],
        Some(request(
            [],
            [SkillCapability::Locale],
            SkillBrowserEngine::Chromium,
        )),
    );
    assert_eq!(
        registry
            .execute(SkillCommand::Ghost(SkillGhostCommand::On), &ungranted)
            .await,
        Err(SkillFailure::UnsupportedCapability)
    );
    assert_eq!(adapter.resolutions.load(Ordering::SeqCst), 0);
    assert!(store.get(&session).unwrap().effective_profile.is_none());
}

#[tokio::test]
async fn on_waiting_for_replacement_remains_attached_to_the_session_gate() {
    let store = Arc::new(SkillStateStore::new());
    let session = SessionId::new();
    store.insert(session_state(session.clone())).unwrap();
    let restart_started = Arc::new(Semaphore::new(0));
    let restart_finish = Arc::new(Semaphore::new(0));
    let adapter = Arc::new(BlockingRestartAdapter {
        inner: FakeAdapter::supporting(SkillBrowserEngine::Chromium, [SkillCapability::Locale]),
        restart_started: restart_started.clone(),
        restart_finish: restart_finish.clone(),
    });
    let ghost = Arc::new(SkillGhost::new(store, vec![adapter]));
    let on_context = context(
        session.clone(),
        [SkillCapability::Locale],
        Some(request(
            [SkillCapability::Locale],
            [],
            SkillBrowserEngine::Chromium,
        )),
    );
    let session_context = context(session, [], None);
    ghost.on(&on_context).await.unwrap();
    ghost.off(&session_context).await.unwrap();

    let replacing_ghost = ghost.clone();
    let replacing_context = session_context.clone();
    let replacement =
        tokio::spawn(async move { replacing_ghost.replace_session(&replacing_context).await });
    restart_started.acquire().await.unwrap().forget();
    let enabling_ghost = ghost.clone();
    let enabling_context = on_context.clone();
    let enabling = tokio::spawn(async move { enabling_ghost.on(&enabling_context).await });

    restart_finish.add_permits(1);
    replacement.await.unwrap().unwrap();
    enabling.await.unwrap().unwrap();
    assert!(ghost.off(&session_context).await.is_ok());
}

fn ghost_status(store: &SkillStateStore, session: &SessionId) -> bool {
    store
        .get(session)
        .unwrap()
        .active_versions
        .contains_key(SkillGhost::NAME)
}

#[test]
fn ghost_implements_the_registered_skill_contract() {
    fn assert_skill<T: Skill>() {}
    assert_skill::<SkillGhost>();
}
