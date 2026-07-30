use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use companion_protocol::{BrowserEngine, CompanionCapabilities};
use tokio::sync::Mutex;
use types::{CommandError, ProfileId, SessionId};

use crate::{policy_error, BrowserWorker, WorkerFactory};

pub const DEFAULT_REPLACEMENT_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

type SessionSelection = Arc<Mutex<Option<Arc<dyn WorkerFactory>>>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnginePreference {
    ManagedChromium,
    Exact {
        engine: BrowserEngine,
        profile_id: Option<ProfileId>,
    },
    Prefer {
        engines: Vec<BrowserEngine>,
    },
}

impl Default for EnginePreference {
    fn default() -> Self {
        Self::Exact {
            engine: BrowserEngine::Firefox,
            profile_id: None,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RequiredCapabilities {
    pub observe: bool,
    pub navigate: bool,
    pub native_input: bool,
    pub tabs: bool,
    pub frames: bool,
    pub native_dialogs: bool,
}

impl RequiredCapabilities {
    pub fn are_met_by(self, available: &CompanionCapabilities) -> bool {
        (!self.observe || available.observe)
            && (!self.navigate || available.navigate)
            && (!self.native_input || available.native_input)
            && (!self.tabs || available.tabs)
            && (!self.frames || available.frames)
            && (!self.native_dialogs || available.native_dialogs)
    }
}

#[derive(Clone)]
pub struct FactoryRegistration {
    engine: BrowserEngine,
    profile_id: Option<ProfileId>,
    capabilities: Option<CompanionCapabilities>,
    factory: Arc<dyn WorkerFactory>,
}

impl FactoryRegistration {
    pub fn new(
        engine: BrowserEngine,
        profile_id: Option<ProfileId>,
        capabilities: CompanionCapabilities,
        factory: Arc<dyn WorkerFactory>,
    ) -> Self {
        Self {
            engine,
            profile_id,
            capabilities: Some(capabilities),
            factory,
        }
    }

    pub fn negotiated(
        engine: BrowserEngine,
        profile_id: Option<ProfileId>,
        factory: Arc<dyn WorkerFactory>,
    ) -> Self {
        Self {
            engine,
            profile_id,
            capabilities: None,
            factory,
        }
    }

    fn satisfies(&self, required: RequiredCapabilities) -> bool {
        self.capabilities
            .as_ref()
            .is_none_or(|capabilities| required.are_met_by(capabilities))
    }
}

pub struct BrowserWorkerSelector {
    registrations: Vec<FactoryRegistration>,
    required: RequiredCapabilities,
    selected: Arc<Mutex<HashMap<SessionId, SessionSelection>>>,
    replacement_cleanup_timeout: Duration,
}

impl BrowserWorkerSelector {
    pub fn new(registrations: Vec<FactoryRegistration>, required: RequiredCapabilities) -> Self {
        Self::with_replacement_timeout(registrations, required, DEFAULT_REPLACEMENT_CLEANUP_TIMEOUT)
    }

    pub fn with_replacement_timeout(
        registrations: Vec<FactoryRegistration>,
        required: RequiredCapabilities,
        replacement_cleanup_timeout: Duration,
    ) -> Self {
        Self {
            registrations,
            required,
            selected: Arc::new(Mutex::new(HashMap::new())),
            replacement_cleanup_timeout,
        }
    }

    pub async fn select(
        &self,
        session_id: &SessionId,
        preference: &EnginePreference,
    ) -> Result<Arc<dyn WorkerFactory>, CommandError> {
        let selection = self.session_selection(session_id).await;
        let mut selected = selection.lock().await;
        if let Some(factory) = selected.as_ref() {
            return Ok(Arc::clone(factory));
        }

        let factory = self.factory_for(preference)?;
        *selected = Some(Arc::clone(&factory));
        Ok(factory)
    }

    pub async fn release_session(&self, session_id: &SessionId) {
        let selection = self.session_selection(session_id).await;
        let selections = Arc::clone(&self.selected);
        let session_id = session_id.clone();
        let cleanup = tokio::spawn(async move {
            let mut selected = selection.lock().await;
            if let Some(factory) = selected.as_ref() {
                factory.release_session(&session_id).await;
            }
            *selected = None;
            drop(selected);

            let mut registered = selections.lock().await;
            let is_current = registered
                .get(&session_id)
                .is_some_and(|current| Arc::ptr_eq(current, &selection));
            let only_registry_and_cleanup = Arc::strong_count(&selection) == 2;
            let state_is_empty = selection
                .try_lock()
                .is_ok_and(|selected| selected.is_none());
            if is_current && only_registry_and_cleanup && state_is_empty {
                registered.remove(&session_id);
            }
        });
        let _ = cleanup.await;
    }

    pub async fn replace_session(
        &self,
        session_id: &SessionId,
        preference: &EnginePreference,
    ) -> Result<Arc<dyn WorkerFactory>, CommandError> {
        let mut replacement = self.start_replacement(session_id, preference).await?;
        match tokio::time::timeout(self.replacement_cleanup_timeout, &mut replacement).await {
            Ok(result) => Ok(result
                .map_err(|error| policy_error(format!("replacement task failed: {error}")))?),
            Err(_) => Err(replacement_timeout_error()),
        }
    }

    pub fn can_select(&self, preference: &EnginePreference) -> bool {
        !self.find(preference).is_empty()
    }

    #[cfg(feature = "test-support")]
    pub async fn retained_session_count(&self) -> usize {
        self.selected.lock().await.len()
    }

    async fn session_selection(&self, session_id: &SessionId) -> SessionSelection {
        self.selected
            .lock()
            .await
            .entry(session_id.clone())
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    }

    async fn replace_session_to_completion(
        &self,
        session_id: &SessionId,
        preference: &EnginePreference,
    ) -> Result<Arc<dyn WorkerFactory>, CommandError> {
        self.start_replacement(session_id, preference)
            .await?
            .await
            .map_err(|error| policy_error(format!("replacement task failed: {error}")))
    }

    async fn start_replacement(
        &self,
        session_id: &SessionId,
        preference: &EnginePreference,
    ) -> Result<tokio::task::JoinHandle<Arc<dyn WorkerFactory>>, CommandError> {
        let replacement = self.factory_for(preference)?;
        let selection = self.session_selection(session_id).await;
        let session_id = session_id.clone();
        Ok(tokio::spawn(async move {
            let mut selected = selection.lock().await;
            if let Some(previous) = selected.as_ref() {
                previous.release_session(&session_id).await;
            }
            *selected = Some(Arc::clone(&replacement));
            replacement
        }))
    }

    fn find(&self, preference: &EnginePreference) -> Vec<&FactoryRegistration> {
        match preference {
            EnginePreference::ManagedChromium => self
                .registrations
                .iter()
                .filter(|registration| {
                    registration.engine == BrowserEngine::Chromium
                        && registration.profile_id.is_none()
                        && registration.satisfies(self.required)
                })
                .take(1)
                .collect(),
            EnginePreference::Exact { engine, profile_id } => self
                .matching(engine, profile_id.as_ref())
                .into_iter()
                .take(1)
                .collect(),
            EnginePreference::Prefer { engines } => engines
                .iter()
                .flat_map(|engine| self.matching(engine, None))
                .collect(),
        }
    }

    fn factory_for(
        &self,
        preference: &EnginePreference,
    ) -> Result<Arc<dyn WorkerFactory>, CommandError> {
        let registrations = self.find(preference);
        if registrations.is_empty() {
            return Err(policy_error(
                "no browser worker satisfies the requested engine, profile, and capabilities",
            ));
        }
        Ok(Arc::new(PreferenceWorkerFactory {
            factories: registrations
                .into_iter()
                .map(|registration| Arc::clone(&registration.factory))
                .collect(),
            launched: Mutex::new(HashMap::new()),
        }))
    }

    fn matching(
        &self,
        engine: &BrowserEngine,
        profile_id: Option<&ProfileId>,
    ) -> Vec<&FactoryRegistration> {
        self.registrations
            .iter()
            .filter(|registration| {
                &registration.engine == engine
                    && profile_id
                        .is_none_or(|wanted| registration.profile_id.as_ref() == Some(wanted))
                    && registration.satisfies(self.required)
            })
            .collect()
    }
}

struct PreferenceWorkerFactory {
    factories: Vec<Arc<dyn WorkerFactory>>,
    launched: Mutex<HashMap<SessionId, Arc<dyn WorkerFactory>>>,
}

#[async_trait]
impl WorkerFactory for PreferenceWorkerFactory {
    async fn launch(&self, session_id: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        if let Some(factory) = self.launched.lock().await.get(session_id).cloned() {
            return factory.launch(session_id).await;
        }

        let mut last_error = None;
        for factory in &self.factories {
            match factory.launch(session_id).await {
                Ok(worker) => {
                    self.launched
                        .lock()
                        .await
                        .insert(session_id.clone(), Arc::clone(factory));
                    return Ok(worker);
                }
                Err(error) => last_error = Some(error),
            }
        }
        Err(last_error.unwrap_or_else(|| policy_error("no browser worker factory is available")))
    }

    async fn release_session(&self, session_id: &SessionId) {
        let mut launched = self.launched.lock().await;
        if let Some(factory) = launched.get(session_id).cloned() {
            factory.release_session(session_id).await;
            launched.remove(session_id);
        }
    }
}

fn replacement_timeout_error() -> CommandError {
    CommandError {
        code: types::ErrorCode::DeadlineExceeded,
        message: "browser worker replacement cleanup exceeded its deadline".into(),
        layer: types::ErrorLayer::Driver,
        retryable: true,
    }
}

pub struct SelectedWorkerFactory {
    selector: Arc<BrowserWorkerSelector>,
    preference: EnginePreference,
}

impl SelectedWorkerFactory {
    pub fn new(selector: Arc<BrowserWorkerSelector>, preference: EnginePreference) -> Self {
        Self {
            selector,
            preference,
        }
    }
}

#[async_trait]
impl WorkerFactory for SelectedWorkerFactory {
    async fn launch(&self, session_id: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        self.selector
            .select(session_id, &self.preference)
            .await?
            .launch(session_id)
            .await
    }

    async fn release_session(&self, session_id: &SessionId) {
        self.selector.release_session(session_id).await;
    }

    fn can_select(&self, preference: &EnginePreference) -> bool {
        self.selector.can_select(preference)
    }

    async fn replace_session(
        &self,
        session_id: &SessionId,
        preference: &EnginePreference,
    ) -> Result<(), CommandError> {
        self.selector
            .replace_session_to_completion(session_id, preference)
            .await
            .map(|_| ())
    }
}
