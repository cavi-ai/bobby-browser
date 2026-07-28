use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use async_trait::async_trait;
use companion_protocol::BrowserEngine;
use serde::Serialize;
use sha2::{Digest, Sha256};
use skill_runtime::{EffectiveSkillProfile, SkillEngineAdapter};
use types::{
    SessionId, SkillBrowserEngine, SkillCapability, SkillFailure, SkillProfile, SkillProfileRequest,
};

use crate::{EnginePreference, WorkerPool};

pub const CHROMIUM_PRODUCTION_SKILL_PROFILE_VERSION: &str = "chromium-runtime-v1";
pub const FIREFOX_PRODUCTION_SKILL_PROFILE_VERSION: &str = "firefox-companion-v1";
pub const PRODUCTION_SKILL_CAPABILITIES: [SkillCapability; 2] = [
    SkillCapability::EngineSelection,
    SkillCapability::ProfilePersistence,
];

pub fn skill_engine(value: BrowserEngine) -> SkillBrowserEngine {
    match value {
        BrowserEngine::Firefox => SkillBrowserEngine::Firefox,
        BrowserEngine::Chromium => SkillBrowserEngine::Chromium,
        BrowserEngine::WebKit => SkillBrowserEngine::WebKit,
    }
}

struct EngineSkillAdapter {
    engine: SkillBrowserEngine,
    pool: Arc<WorkerPool>,
    version: String,
    supported: BTreeSet<SkillCapability>,
    effective_values: BTreeMap<String, String>,
}

impl EngineSkillAdapter {
    fn new(
        engine: SkillBrowserEngine,
        pool: Arc<WorkerPool>,
        version: impl Into<String>,
        supported: impl IntoIterator<Item = SkillCapability>,
        effective_values: BTreeMap<String, String>,
    ) -> Result<Self, SkillFailure> {
        SkillProfileRequest::new([], [], [], effective_values.clone())
            .map_err(|_| SkillFailure::ConfigurationConflict)?;
        let version = version.into();
        SkillProfile::new(version.clone(), engine, [], format!("{:064x}", 0))
            .map_err(|_| SkillFailure::ConfigurationConflict)?;
        let mut supported: BTreeSet<_> = supported.into_iter().collect();
        supported.retain(|capability| {
            capability_value_key(*capability).is_none_or(|key| effective_values.contains_key(key))
        });
        Ok(Self {
            engine,
            pool,
            version,
            supported,
            effective_values,
        })
    }

    async fn resolve_profile(
        &self,
        request: &SkillProfileRequest,
    ) -> Result<EffectiveSkillProfile, SkillFailure> {
        if !self.pool.can_select(&EnginePreference::Prefer {
            engines: vec![protocol_engine(self.engine)],
        }) {
            return Err(SkillFailure::EngineUnavailable);
        }
        if !request.preferred_engines.is_empty()
            && !request.preferred_engines.contains(&self.engine)
        {
            return Err(SkillFailure::EngineUnavailable);
        }
        if !request.required.is_subset(&self.supported) {
            return Err(SkillFailure::UnsupportedCapability);
        }
        let requested: BTreeSet<_> = request.required.union(&request.optional).copied().collect();
        let effective_capabilities: BTreeSet<_> =
            requested.intersection(&self.supported).copied().collect();
        let effective_values = self
            .effective_values
            .iter()
            .filter(|(key, _)| {
                effective_capabilities
                    .iter()
                    .any(|capability| capability_value_key(*capability) == Some(key.as_str()))
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect();
        let canonical = CanonicalEffectiveConfiguration {
            schema_version: SkillProfileRequest::SCHEMA_VERSION,
            version: &self.version,
            engine: self.engine,
            effective_capabilities: &effective_capabilities,
            effective_values: &effective_values,
        };
        let bytes =
            serde_json::to_vec(&canonical).map_err(|_| SkillFailure::ConfigurationConflict)?;
        let digest = format!("{:x}", Sha256::digest(bytes));
        let profile = SkillProfile::new(
            self.version.clone(),
            self.engine,
            effective_capabilities,
            digest,
        )
        .map_err(|_| SkillFailure::ConfigurationConflict)?;
        Ok(EffectiveSkillProfile {
            profile,
            unsupported_optional: request
                .optional
                .difference(&self.supported)
                .copied()
                .collect(),
            restart_required: false,
        })
    }

    async fn prepare_restart(&self, session: &SessionId) -> Result<(), SkillFailure> {
        self.pool
            .replace_session(
                session,
                &EnginePreference::Prefer {
                    engines: vec![protocol_engine(self.engine)],
                },
            )
            .await
            .map(|_| ())
            .map_err(|error| match error.code {
                types::ErrorCode::DeadlineExceeded => SkillFailure::DeadlineExceeded,
                types::ErrorCode::PolicyDenied => SkillFailure::EngineUnavailable,
                _ => SkillFailure::ConfigurationConflict,
            })
    }
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct CanonicalEffectiveConfiguration<'a> {
    schema_version: u16,
    version: &'a str,
    engine: SkillBrowserEngine,
    effective_capabilities: &'a BTreeSet<SkillCapability>,
    effective_values: &'a BTreeMap<String, String>,
}

fn capability_value_key(capability: SkillCapability) -> Option<&'static str> {
    match capability {
        SkillCapability::EngineSelection | SkillCapability::ProfilePersistence => None,
        SkillCapability::Locale => Some("locale"),
        SkillCapability::Timezone => Some("timezone"),
        SkillCapability::Viewport => Some("viewport"),
        SkillCapability::UserAgentConsistency => Some("userAgentConsistency"),
        SkillCapability::InteractionCadence => Some("interactionCadence"),
    }
}

fn protocol_engine(engine: SkillBrowserEngine) -> BrowserEngine {
    match engine {
        SkillBrowserEngine::Firefox => BrowserEngine::Firefox,
        SkillBrowserEngine::Chromium => BrowserEngine::Chromium,
        SkillBrowserEngine::WebKit => BrowserEngine::WebKit,
    }
}

pub struct ChromiumSkillAdapter(EngineSkillAdapter);

impl ChromiumSkillAdapter {
    pub fn production(pool: Arc<WorkerPool>) -> Result<Self, SkillFailure> {
        EngineSkillAdapter::new(
            SkillBrowserEngine::Chromium,
            pool,
            CHROMIUM_PRODUCTION_SKILL_PROFILE_VERSION,
            PRODUCTION_SKILL_CAPABILITIES,
            BTreeMap::new(),
        )
        .map(Self)
    }

    #[cfg(feature = "test-support")]
    /// Test-only constructor for proving adapter behavior with synthetic engine reports.
    pub fn for_test(
        pool: Arc<WorkerPool>,
        version: impl Into<String>,
        supported: impl IntoIterator<Item = SkillCapability>,
        effective_values: BTreeMap<String, String>,
    ) -> Result<Self, SkillFailure> {
        EngineSkillAdapter::new(
            SkillBrowserEngine::Chromium,
            pool,
            version,
            supported,
            effective_values,
        )
        .map(Self)
    }
}

pub struct FirefoxSkillAdapter(EngineSkillAdapter);

impl FirefoxSkillAdapter {
    pub fn production(pool: Arc<WorkerPool>) -> Result<Self, SkillFailure> {
        EngineSkillAdapter::new(
            SkillBrowserEngine::Firefox,
            pool,
            FIREFOX_PRODUCTION_SKILL_PROFILE_VERSION,
            PRODUCTION_SKILL_CAPABILITIES,
            BTreeMap::new(),
        )
        .map(Self)
    }

    #[cfg(feature = "test-support")]
    /// Test-only constructor for proving adapter behavior with synthetic engine reports.
    pub fn for_test(
        pool: Arc<WorkerPool>,
        version: impl Into<String>,
        supported: impl IntoIterator<Item = SkillCapability>,
        effective_values: BTreeMap<String, String>,
    ) -> Result<Self, SkillFailure> {
        EngineSkillAdapter::new(
            SkillBrowserEngine::Firefox,
            pool,
            version,
            supported,
            effective_values,
        )
        .map(Self)
    }
}

macro_rules! impl_skill_adapter {
    ($adapter:ty) => {
        #[async_trait]
        impl SkillEngineAdapter for $adapter {
            fn engine(&self) -> SkillBrowserEngine {
                self.0.engine
            }

            async fn resolve_profile(
                &self,
                request: &SkillProfileRequest,
            ) -> Result<EffectiveSkillProfile, SkillFailure> {
                self.0.resolve_profile(request).await
            }

            async fn prepare_restart(&self, session: &SessionId) -> Result<(), SkillFailure> {
                self.0.prepare_restart(session).await
            }
        }
    };
}

impl_skill_adapter!(ChromiumSkillAdapter);
impl_skill_adapter!(FirefoxSkillAdapter);
