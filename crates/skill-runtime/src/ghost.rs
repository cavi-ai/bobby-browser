use std::{
    collections::{BTreeSet, HashMap},
    sync::Arc,
};

use async_trait::async_trait;
use tokio::sync::Mutex;
use types::{
    SessionId, SkillBrowserEngine, SkillCapability, SkillCommand, SkillFailure, SkillGhostCommand,
    SkillOutcome, SkillProfile, SkillProfileRequest,
};

use crate::{Skill, SkillContext, SkillStateStore, SkillStateStoreError};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveSkillProfile {
    pub profile: SkillProfile,
    pub unsupported_optional: BTreeSet<SkillCapability>,
    pub restart_required: bool,
}

#[async_trait]
pub trait SkillEngineAdapter: Send + Sync {
    fn engine(&self) -> SkillBrowserEngine;

    async fn resolve_profile(
        &self,
        request: &SkillProfileRequest,
    ) -> Result<EffectiveSkillProfile, SkillFailure>;

    async fn prepare_restart(&self, session: &SessionId) -> Result<(), SkillFailure>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillGhostStatus {
    pub active: bool,
    pub profile: Option<SkillProfile>,
    pub unsupported_optional: BTreeSet<SkillCapability>,
    pub restart_required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillGhostResult {
    pub outcome: SkillOutcome,
    pub status: SkillGhostStatus,
}

#[derive(Clone)]
struct LiveGhostProfile {
    active: bool,
    effective: EffectiveSkillProfile,
}

pub struct SkillGhost {
    store: Arc<SkillStateStore>,
    adapters: Vec<Arc<dyn SkillEngineAdapter>>,
    live: Mutex<HashMap<SessionId, Arc<Mutex<Option<LiveGhostProfile>>>>>,
}

impl SkillGhost {
    pub const NAME: &'static str = "SkillGhost";
    pub const ALIAS: &'static str = "/ghost";
    pub const VERSION: &'static str = "1.0.0";

    pub fn new(store: Arc<SkillStateStore>, adapters: Vec<Arc<dyn SkillEngineAdapter>>) -> Self {
        Self {
            store,
            adapters,
            live: Mutex::new(HashMap::new()),
        }
    }

    pub async fn on(&self, context: &SkillContext) -> Result<SkillGhostResult, SkillFailure> {
        let session = context
            .session_id()
            .ok_or(SkillFailure::ConfigurationConflict)?;
        let request = context
            .ghost_profile_request()
            .ok_or(SkillFailure::ConfigurationConflict)?;
        let requested: BTreeSet<_> = request.required.union(&request.optional).copied().collect();
        if !requested.is_subset(context.granted_capabilities()) {
            return Err(SkillFailure::UnsupportedCapability);
        }
        self.store.get(session).map_err(store_failure)?;
        let gate = self.session_gate(session).await;
        let mut live = gate.lock().await;
        let effective = self.resolve(request).await?;

        if let Some(current) = live.as_ref() {
            if current.effective.restart_required
                || current.effective.profile != effective.profile
                || current.effective.unsupported_optional != effective.unsupported_optional
            {
                return Err(SkillFailure::ConfigurationConflict);
            }
        }

        self.store
            .transition(session, |state| {
                match &state.effective_profile {
                    None => state.effective_profile = Some(effective.profile.clone()),
                    Some(existing) if existing == &effective.profile => {}
                    Some(_) => return Err(SkillStateStoreError::ProfileFrozen),
                }
                state
                    .active_versions
                    .insert(Self::NAME.into(), Self::VERSION.into());
                Ok(())
            })
            .map_err(store_failure)?;

        let live_profile = LiveGhostProfile {
            active: true,
            effective: EffectiveSkillProfile {
                restart_required: false,
                ..effective
            },
        };
        let result = result_for(&live_profile, false)?;
        *live = Some(live_profile);
        Ok(result)
    }

    pub async fn off(&self, context: &SkillContext) -> Result<SkillGhostResult, SkillFailure> {
        let session = context
            .session_id()
            .ok_or(SkillFailure::ConfigurationConflict)?;
        let gate = self
            .existing_session_gate(session)
            .await
            .ok_or(SkillFailure::ConfigurationConflict)?;
        let mut live = gate.lock().await;
        let current = live.as_mut().ok_or(SkillFailure::ConfigurationConflict)?;
        self.store
            .transition(session, |state| {
                state.active_versions.remove(Self::NAME);
                Ok(())
            })
            .map_err(store_failure)?;
        current.active = false;
        current.effective.restart_required = true;
        result_for(current, true)
    }

    pub async fn status(&self, context: &SkillContext) -> Result<SkillGhostStatus, SkillFailure> {
        let session = context
            .session_id()
            .ok_or(SkillFailure::ConfigurationConflict)?;
        if let Some(gate) = self.existing_session_gate(session).await {
            if let Some(current) = gate.lock().await.as_ref() {
                return Ok(status_for(current));
            }
        }
        let state = self.store.get(session).map_err(store_failure)?;
        Ok(SkillGhostStatus {
            active: state.active_versions.contains_key(Self::NAME),
            profile: state.effective_profile,
            unsupported_optional: BTreeSet::new(),
            restart_required: false,
        })
    }

    pub async fn replace_session(
        &self,
        context: &SkillContext,
    ) -> Result<SkillGhostResult, SkillFailure> {
        let session = context
            .session_id()
            .ok_or(SkillFailure::ConfigurationConflict)?;
        let gate = self
            .existing_session_gate(session)
            .await
            .ok_or(SkillFailure::ConfigurationConflict)?;
        let mut live = gate.lock().await;
        let current = live
            .as_ref()
            .cloned()
            .ok_or(SkillFailure::ConfigurationConflict)?;
        if current.active || !current.effective.restart_required {
            return Err(SkillFailure::ConfigurationConflict);
        }
        let adapter = self
            .adapters
            .iter()
            .find(|adapter| adapter.engine() == current.effective.profile.engine)
            .ok_or(SkillFailure::EngineUnavailable)?;
        adapter.prepare_restart(session).await?;
        self.store
            .transition(session, |state| {
                state.active_versions.remove(Self::NAME);
                state.effective_profile = None;
                Ok(())
            })
            .map_err(store_failure)?;
        *live = None;
        SkillGhostResult::stopped(SkillGhostStatus {
            active: false,
            profile: None,
            unsupported_optional: BTreeSet::new(),
            restart_required: false,
        })
    }

    async fn resolve(
        &self,
        request: &SkillProfileRequest,
    ) -> Result<EffectiveSkillProfile, SkillFailure> {
        let engines: Vec<_> = if request.preferred_engines.is_empty() {
            self.adapters
                .iter()
                .map(|adapter| adapter.engine())
                .collect()
        } else {
            request.preferred_engines.clone()
        };
        let mut unsupported = false;
        for engine in engines {
            for adapter in self
                .adapters
                .iter()
                .filter(|adapter| adapter.engine() == engine)
            {
                match adapter.resolve_profile(request).await {
                    Ok(effective) => return Ok(effective),
                    Err(SkillFailure::UnsupportedCapability) => unsupported = true,
                    Err(SkillFailure::EngineUnavailable) => {}
                    Err(error) => return Err(error),
                }
            }
        }
        if unsupported {
            Err(SkillFailure::UnsupportedCapability)
        } else {
            Err(SkillFailure::EngineUnavailable)
        }
    }

    async fn session_gate(&self, session: &SessionId) -> Arc<Mutex<Option<LiveGhostProfile>>> {
        self.live
            .lock()
            .await
            .entry(session.clone())
            .or_insert_with(|| Arc::new(Mutex::new(None)))
            .clone()
    }

    async fn existing_session_gate(
        &self,
        session: &SessionId,
    ) -> Option<Arc<Mutex<Option<LiveGhostProfile>>>> {
        self.live.lock().await.get(session).cloned()
    }
}

impl SkillGhostResult {
    fn stopped(status: SkillGhostStatus) -> Result<Self, SkillFailure> {
        Ok(Self {
            outcome: SkillOutcome::stopped(vec![])
                .map_err(|_| SkillFailure::ConfigurationConflict)?,
            status,
        })
    }
}

fn result_for(live: &LiveGhostProfile, stopped: bool) -> Result<SkillGhostResult, SkillFailure> {
    let status = status_for(live);
    let outcome = if stopped {
        SkillOutcome::stopped(vec![])
    } else if live.effective.unsupported_optional.is_empty() {
        SkillOutcome::applied(vec![])
    } else {
        SkillOutcome::degraded(live.effective.unsupported_optional.clone(), vec![])
    }
    .map_err(|_| SkillFailure::ConfigurationConflict)?;
    Ok(SkillGhostResult { outcome, status })
}

fn status_for(live: &LiveGhostProfile) -> SkillGhostStatus {
    SkillGhostStatus {
        active: live.active,
        profile: Some(live.effective.profile.clone()),
        unsupported_optional: live.effective.unsupported_optional.clone(),
        restart_required: live.effective.restart_required,
    }
}

fn store_failure(_error: SkillStateStoreError) -> SkillFailure {
    SkillFailure::ConfigurationConflict
}

#[async_trait]
impl Skill for SkillGhost {
    fn name(&self) -> &'static str {
        Self::NAME
    }

    fn alias(&self) -> &'static str {
        Self::ALIAS
    }

    fn version(&self) -> &'static str {
        Self::VERSION
    }

    fn capabilities(&self) -> BTreeSet<SkillCapability> {
        BTreeSet::from([
            SkillCapability::EngineSelection,
            SkillCapability::ProfilePersistence,
            SkillCapability::Locale,
            SkillCapability::Timezone,
            SkillCapability::Viewport,
            SkillCapability::UserAgentConsistency,
            SkillCapability::InteractionCadence,
        ])
    }

    fn requested_capabilities(
        &self,
        command: &SkillCommand,
        context: &SkillContext,
    ) -> Result<BTreeSet<SkillCapability>, SkillFailure> {
        match command {
            SkillCommand::Ghost(SkillGhostCommand::On) => {
                let request = context
                    .ghost_profile_request()
                    .ok_or(SkillFailure::ConfigurationConflict)?;
                Ok(request.required.union(&request.optional).copied().collect())
            }
            SkillCommand::Ghost(SkillGhostCommand::Off | SkillGhostCommand::Status) => {
                Ok(BTreeSet::new())
            }
            SkillCommand::ZigZagZig(_) => Err(SkillFailure::ConfigurationConflict),
        }
    }

    async fn execute(
        &self,
        command: SkillCommand,
        context: &SkillContext,
    ) -> Result<SkillOutcome, SkillFailure> {
        match command {
            SkillCommand::Ghost(SkillGhostCommand::On) => Ok(self.on(context).await?.outcome),
            SkillCommand::Ghost(SkillGhostCommand::Off) => Ok(self.off(context).await?.outcome),
            SkillCommand::Ghost(SkillGhostCommand::Status) => {
                status_outcome(self.status(context).await?)
            }
            SkillCommand::ZigZagZig(_) => Err(SkillFailure::ConfigurationConflict),
        }
    }
}

fn status_outcome(status: SkillGhostStatus) -> Result<SkillOutcome, SkillFailure> {
    if !status.active {
        SkillOutcome::stopped(vec![])
    } else if status.unsupported_optional.is_empty() {
        SkillOutcome::applied(vec![])
    } else {
        SkillOutcome::degraded(status.unsupported_optional, vec![])
    }
    .map_err(|_| SkillFailure::ConfigurationConflict)
}
