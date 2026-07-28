use std::{
    collections::{BTreeMap, BTreeSet},
    sync::Arc,
};

use async_trait::async_trait;
use thiserror::Error;
use types::{
    SessionId, SkillCapability, SkillCommand, SkillFailure, SkillOutcome, SkillProfileRequest,
};

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SkillContext {
    required_capabilities: BTreeSet<SkillCapability>,
    granted_capabilities: BTreeSet<SkillCapability>,
    active_versions: BTreeMap<String, String>,
    session_id: Option<SessionId>,
    ghost_profile_request: Option<SkillProfileRequest>,
}

impl SkillContext {
    pub fn new(required_capabilities: impl IntoIterator<Item = SkillCapability>) -> Self {
        let required_capabilities: BTreeSet<_> = required_capabilities.into_iter().collect();
        Self {
            granted_capabilities: required_capabilities.clone(),
            required_capabilities,
            active_versions: BTreeMap::new(),
            session_id: None,
            ghost_profile_request: None,
        }
    }

    pub fn with_granted_capabilities(
        required_capabilities: impl IntoIterator<Item = SkillCapability>,
        granted_capabilities: impl IntoIterator<Item = SkillCapability>,
    ) -> Self {
        Self {
            required_capabilities: required_capabilities.into_iter().collect(),
            granted_capabilities: granted_capabilities.into_iter().collect(),
            active_versions: BTreeMap::new(),
            session_id: None,
            ghost_profile_request: None,
        }
    }

    pub fn with_active_versions(
        required_capabilities: impl IntoIterator<Item = SkillCapability>,
        active_versions: BTreeMap<String, String>,
    ) -> Self {
        let required_capabilities: BTreeSet<_> = required_capabilities.into_iter().collect();
        Self {
            granted_capabilities: required_capabilities.clone(),
            required_capabilities,
            active_versions,
            session_id: None,
            ghost_profile_request: None,
        }
    }

    pub fn with_ghost(
        mut self,
        session_id: SessionId,
        profile_request: Option<SkillProfileRequest>,
    ) -> Self {
        self.session_id = Some(session_id);
        self.ghost_profile_request = profile_request;
        self
    }

    pub fn with_session(mut self, session_id: SessionId) -> Self {
        self.session_id = Some(session_id);
        self
    }

    pub fn required_capabilities(&self) -> &BTreeSet<SkillCapability> {
        &self.required_capabilities
    }

    pub fn granted_capabilities(&self) -> &BTreeSet<SkillCapability> {
        &self.granted_capabilities
    }

    pub fn session_id(&self) -> Option<&SessionId> {
        self.session_id.as_ref()
    }

    pub fn ghost_profile_request(&self) -> Option<&SkillProfileRequest> {
        self.ghost_profile_request.as_ref()
    }
}

#[async_trait]
pub trait Skill: Send + Sync {
    fn name(&self) -> &'static str;
    fn alias(&self) -> &'static str;
    fn version(&self) -> &'static str;
    fn capabilities(&self) -> BTreeSet<SkillCapability>;
    fn requested_capabilities(
        &self,
        _command: &SkillCommand,
        _context: &SkillContext,
    ) -> Result<BTreeSet<SkillCapability>, SkillFailure> {
        Ok(self.capabilities())
    }
    async fn execute(
        &self,
        command: SkillCommand,
        context: &SkillContext,
    ) -> Result<SkillOutcome, SkillFailure>;
}

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum SkillRegistryError {
    #[error("skill name is invalid: {0}")]
    InvalidName(String),
    #[error("skill alias is invalid: {0}")]
    InvalidAlias(String),
    #[error("skill version is invalid: {0}")]
    InvalidVersion(String),
    #[error("skill name is already registered: {0}")]
    DuplicateName(String),
    #[error("skill alias is already registered: {0}")]
    DuplicateAlias(String),
    #[error("skill alias is not registered: {0}")]
    UnknownAlias(String),
    #[error("skill {skill} is missing required capabilities: {missing:?}")]
    MissingRequiredCapability {
        skill: String,
        missing: BTreeSet<SkillCapability>,
    },
    #[error("skill {skill} version conflicts with the active session version")]
    VersionConflict { skill: String },
    #[error("skill {skill} requests ungranted capabilities: {ungranted:?}")]
    UngrantedCapabilities {
        skill: String,
        ungranted: BTreeSet<SkillCapability>,
    },
    #[error("skill name is unsupported: {0}")]
    UnsupportedSkillName(String),
    #[error("skill {name} must use alias {expected}, not {actual}")]
    AliasMismatch {
        name: String,
        expected: String,
        actual: String,
    },
}

pub struct SkillRegistry {
    by_name: BTreeMap<&'static str, Arc<dyn Skill>>,
    by_alias: BTreeMap<&'static str, &'static str>,
}

impl Default for SkillRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl SkillRegistry {
    pub fn new() -> Self {
        Self {
            by_name: BTreeMap::new(),
            by_alias: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, skill: Arc<dyn Skill>) -> Result<(), SkillRegistryError> {
        validate_registered_pair(skill.name(), skill.alias())?;
        validate_version(skill.version())?;
        if self.by_name.contains_key(skill.name()) {
            return Err(SkillRegistryError::DuplicateName(skill.name().into()));
        }
        if self.by_alias.contains_key(skill.alias()) {
            return Err(SkillRegistryError::DuplicateAlias(skill.alias().into()));
        }
        self.by_alias.insert(skill.alias(), skill.name());
        self.by_name.insert(skill.name(), skill);
        Ok(())
    }

    pub fn get_by_name(&self, name: &str) -> Option<Arc<dyn Skill>> {
        self.by_name.get(name).cloned()
    }

    pub fn get_by_alias(&self, alias: &str) -> Option<Arc<dyn Skill>> {
        let name = self.by_alias.get(alias)?;
        self.get_by_name(name)
    }

    pub fn resolve(
        &self,
        alias: &str,
        context: &SkillContext,
    ) -> Result<Arc<dyn Skill>, SkillRegistryError> {
        self.resolve_base(alias, context)
    }

    fn resolve_base(
        &self,
        alias: &str,
        context: &SkillContext,
    ) -> Result<Arc<dyn Skill>, SkillRegistryError> {
        let skill = self
            .get_by_alias(alias)
            .ok_or_else(|| SkillRegistryError::UnknownAlias(alias.into()))?;
        let missing: BTreeSet<_> = context
            .required_capabilities
            .difference(&skill.capabilities())
            .copied()
            .collect();
        if !missing.is_empty() {
            return Err(SkillRegistryError::MissingRequiredCapability {
                skill: skill.name().into(),
                missing,
            });
        }
        if context
            .active_versions
            .get(skill.name())
            .is_some_and(|version| version != skill.version())
        {
            return Err(SkillRegistryError::VersionConflict {
                skill: skill.name().into(),
            });
        }
        Ok(skill)
    }

    pub async fn execute(
        &self,
        command: SkillCommand,
        context: &SkillContext,
    ) -> Result<SkillOutcome, SkillFailure> {
        let alias = match command {
            SkillCommand::Ghost(_) => "/ghost",
            SkillCommand::ZigZagZig(_) => "/zigzagzig",
        };
        let skill = self
            .resolve_base(alias, context)
            .map_err(|error| match error {
                SkillRegistryError::MissingRequiredCapability { .. }
                | SkillRegistryError::UngrantedCapabilities { .. } => {
                    SkillFailure::UnsupportedCapability
                }
                _ => SkillFailure::ConfigurationConflict,
            })?;
        let requested = skill.requested_capabilities(&command, context)?;
        if !requested.is_subset(&skill.capabilities())
            || !requested.is_subset(context.granted_capabilities())
        {
            return Err(SkillFailure::UnsupportedCapability);
        }
        skill.execute(command, context).await
    }
}

fn validate_registered_pair(name: &str, alias: &str) -> Result<(), SkillRegistryError> {
    let expected = match name {
        "SkillGhost" => "/ghost",
        "SkillZigZagZig" => "/zigzagzig",
        _ => return Err(SkillRegistryError::UnsupportedSkillName(name.into())),
    };
    if alias != expected {
        return Err(SkillRegistryError::AliasMismatch {
            name: name.into(),
            expected: expected.into(),
            actual: alias.into(),
        });
    }
    Ok(())
}

fn validate_version(version: &str) -> Result<(), SkillRegistryError> {
    if version.is_empty()
        || version.len() > 128
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
        || contains_secret_marker(version)
    {
        return Err(SkillRegistryError::InvalidVersion(version.into()));
    }
    Ok(())
}

fn contains_secret_marker(value: &str) -> bool {
    let value = value.to_ascii_lowercase();
    [
        "bearer",
        "authorization",
        "password",
        "cookie",
        "token",
        "sk-",
    ]
    .iter()
    .any(|marker| value.contains(marker))
}
