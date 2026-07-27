use std::{
    collections::{BTreeMap, BTreeSet},
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use async_trait::async_trait;
use skill_runtime::{
    Skill, SkillCapability, SkillCommand, SkillContext, SkillFailure, SkillOutcome, SkillRegistry,
    SkillRegistryError,
};

struct FakeSkill {
    name: &'static str,
    alias: &'static str,
    version: &'static str,
    capabilities: BTreeSet<SkillCapability>,
    calls: AtomicUsize,
}

impl FakeSkill {
    fn new(name: &'static str, alias: &'static str) -> Self {
        Self {
            name,
            alias,
            version: "1.0.0",
            capabilities: BTreeSet::new(),
            calls: AtomicUsize::new(0),
        }
    }

    fn with_capabilities(
        name: &'static str,
        alias: &'static str,
        capabilities: impl IntoIterator<Item = SkillCapability>,
    ) -> Self {
        Self {
            name,
            alias,
            version: "1.0.0",
            capabilities: capabilities.into_iter().collect(),
            calls: AtomicUsize::new(0),
        }
    }

    fn with_version(name: &'static str, alias: &'static str, version: &'static str) -> Self {
        Self {
            name,
            alias,
            version,
            capabilities: BTreeSet::new(),
            calls: AtomicUsize::new(0),
        }
    }

    fn calls(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

#[async_trait]
impl Skill for FakeSkill {
    fn name(&self) -> &'static str {
        self.name
    }

    fn alias(&self) -> &'static str {
        self.alias
    }

    fn version(&self) -> &'static str {
        self.version
    }

    fn capabilities(&self) -> BTreeSet<SkillCapability> {
        self.capabilities.clone()
    }

    async fn execute(
        &self,
        _command: SkillCommand,
        _context: &SkillContext,
    ) -> Result<SkillOutcome, SkillFailure> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        SkillOutcome::applied(Vec::new()).map_err(|_| SkillFailure::ConfigurationConflict)
    }
}

#[tokio::test]
async fn registry_rejects_duplicate_names_and_aliases() {
    let mut registry = SkillRegistry::new();
    registry
        .register(Arc::new(FakeSkill::new("SkillGhost", "/ghost")))
        .unwrap();
    assert!(matches!(
        registry.register(Arc::new(FakeSkill::new("SkillGhost", "/ghost"))),
        Err(SkillRegistryError::DuplicateName(_))
    ));
}

#[test]
fn registry_admits_only_documented_skill_name_alias_pairs() {
    let mut registry = SkillRegistry::new();

    assert!(matches!(
        registry.register(Arc::new(FakeSkill::new("SkillGhost", "/other"))),
        Err(SkillRegistryError::AliasMismatch { .. })
    ));
    assert!(matches!(
        registry.register(Arc::new(FakeSkill::new("SkillGhost", "/zigzagzig"))),
        Err(SkillRegistryError::AliasMismatch { .. })
    ));
    assert!(matches!(
        registry.register(Arc::new(FakeSkill::new("SkillZigZagZig", "/ghost"))),
        Err(SkillRegistryError::AliasMismatch { .. })
    ));
    assert!(matches!(
        registry.register(Arc::new(FakeSkill::new("SkillOther", "/other"))),
        Err(SkillRegistryError::UnsupportedSkillName(_))
    ));
    registry
        .register(Arc::new(FakeSkill::new("SkillGhost", "/ghost")))
        .unwrap();
    registry
        .register(Arc::new(FakeSkill::new("SkillZigZagZig", "/zigzagzig")))
        .unwrap();
}

#[tokio::test]
async fn registry_fails_closed_when_required_capabilities_are_missing() {
    let mut registry = SkillRegistry::new();
    registry
        .register(Arc::new(FakeSkill::with_capabilities(
            "SkillGhost",
            "/ghost",
            [SkillCapability::Locale],
        )))
        .unwrap();

    let context = SkillContext::new([SkillCapability::Timezone]);
    assert!(matches!(
        registry.resolve("/ghost", &context),
        Err(SkillRegistryError::MissingRequiredCapability { .. })
    ));
}

#[tokio::test]
async fn ungranted_skill_capabilities_never_reach_the_skill_body() {
    let mut registry = SkillRegistry::new();
    let skill = Arc::new(FakeSkill::with_capabilities(
        "SkillGhost",
        "/ghost",
        [SkillCapability::Locale],
    ));
    registry.register(skill.clone()).unwrap();

    assert_eq!(
        registry
            .execute(
                SkillCommand::Ghost(skill_runtime::SkillGhostCommand::On),
                &SkillContext::new([]),
            )
            .await,
        Err(SkillFailure::UnsupportedCapability)
    );
    assert_eq!(skill.calls(), 0);
}

#[tokio::test]
async fn explicitly_granted_capabilities_allow_skill_execution() {
    let mut registry = SkillRegistry::new();
    let skill = Arc::new(FakeSkill::with_capabilities(
        "SkillGhost",
        "/ghost",
        [SkillCapability::Locale],
    ));
    registry.register(skill.clone()).unwrap();
    let context = SkillContext::with_granted_capabilities(
        [SkillCapability::Locale],
        [SkillCapability::Locale],
    );

    assert!(registry
        .execute(
            SkillCommand::Ghost(skill_runtime::SkillGhostCommand::On),
            &context,
        )
        .await
        .is_ok());
    assert_eq!(skill.calls(), 1);
}

#[tokio::test]
async fn failed_required_and_version_resolution_never_reach_the_skill_body() {
    let mut registry = SkillRegistry::new();
    let required = Arc::new(FakeSkill::with_capabilities(
        "SkillGhost",
        "/ghost",
        [SkillCapability::Locale],
    ));
    registry.register(required.clone()).unwrap();

    assert_eq!(
        registry
            .execute(
                SkillCommand::Ghost(skill_runtime::SkillGhostCommand::On),
                &SkillContext::new([SkillCapability::Timezone]),
            )
            .await,
        Err(SkillFailure::UnsupportedCapability)
    );
    assert_eq!(required.calls(), 0);

    let mut registry = SkillRegistry::new();
    let versioned = Arc::new(FakeSkill::with_version("SkillGhost", "/ghost", "1.0.0"));
    registry.register(versioned.clone()).unwrap();
    let context = SkillContext::with_active_versions(
        [],
        BTreeMap::from([("SkillGhost".into(), "2.0.0".into())]),
    );
    assert_eq!(
        registry
            .execute(
                SkillCommand::Ghost(skill_runtime::SkillGhostCommand::On),
                &context,
            )
            .await,
        Err(SkillFailure::ConfigurationConflict)
    );
    assert_eq!(versioned.calls(), 0);
}
