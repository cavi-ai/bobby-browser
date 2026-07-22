use std::{collections::HashMap, sync::Arc};

use async_trait::async_trait;
use companion_protocol::{BrowserEngine, CompanionCapabilities};
use tokio::sync::Mutex;
use types::{CommandError, ProfileId, SessionId};

use crate::{policy_error, BrowserWorker, WorkerFactory};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum EnginePreference {
    #[default]
    ManagedChromium,
    Exact {
        engine: BrowserEngine,
        profile_id: Option<ProfileId>,
    },
    Prefer {
        engines: Vec<BrowserEngine>,
    },
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
    selected: Mutex<HashMap<SessionId, Arc<dyn WorkerFactory>>>,
}

impl BrowserWorkerSelector {
    pub fn new(registrations: Vec<FactoryRegistration>, required: RequiredCapabilities) -> Self {
        Self {
            registrations,
            required,
            selected: Mutex::new(HashMap::new()),
        }
    }

    pub async fn select(
        &self,
        session_id: &SessionId,
        preference: &EnginePreference,
    ) -> Result<Arc<dyn WorkerFactory>, CommandError> {
        let mut selected = self.selected.lock().await;
        if let Some(factory) = selected.get(session_id) {
            return Ok(Arc::clone(factory));
        }

        let registrations = self.find(preference);
        if registrations.is_empty() {
            return Err(policy_error(
                "no browser worker satisfies the requested engine, profile, and capabilities",
            ));
        }
        let factory: Arc<dyn WorkerFactory> = Arc::new(PreferenceWorkerFactory {
            factories: registrations
                .into_iter()
                .map(|registration| Arc::clone(&registration.factory))
                .collect(),
            launched: Mutex::new(HashMap::new()),
        });
        selected.insert(session_id.clone(), Arc::clone(&factory));
        Ok(factory)
    }

    pub async fn release_session(&self, session_id: &SessionId) {
        let factory = { self.selected.lock().await.remove(session_id) };
        if let Some(factory) = factory {
            factory.release_session(session_id).await;
        }
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
        let factory = self.launched.lock().await.remove(session_id);
        if let Some(factory) = factory {
            factory.release_session(session_id).await;
        }
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
}
