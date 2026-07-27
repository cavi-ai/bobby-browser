use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize, Serializer};

use crate::{AttemptId, CheckpointId, CommandClass, CommandId, PageId, SessionId, WorkflowId};

pub const SKILL_SCHEMA_VERSION: u16 = 1;
const MAX_CAPABILITIES: usize = 32;
const MAX_PREFERRED_ENGINES: usize = 8;
const MAX_PROFILE_VALUES: usize = 64;
const MAX_TACTICS: usize = 32;
const MAX_EVIDENCE_REFS: usize = 128;
const MAX_NAME_BYTES: usize = 128;
const MAX_POSTCONDITION_BYTES: usize = 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "skill", content = "action", rename_all = "camelCase")]
pub enum SkillCommand {
    Ghost(SkillGhostCommand),
    ZigZagZig(SkillZigZagZigCommand),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SkillGhostCommand {
    On,
    Off,
    Status,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SkillZigZagZigCommand {
    Run,
    Status,
    Stop,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SkillCapability {
    EngineSelection,
    ProfilePersistence,
    Locale,
    Timezone,
    Viewport,
    UserAgentConsistency,
    InteractionCadence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SkillFailure {
    UnsupportedCapability,
    ConfigurationConflict,
    DeadlineExceeded,
    TargetDrift,
    PostconditionFailed,
    EffectUncertain,
    CheckpointMismatch,
    StrategyExhausted,
    EngineUnavailable,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SkillTactic {
    ObserveAgain,
    ResolveSemanticTarget,
    ChangeInteractionMethod,
    ReconcileCheckpoint,
    FreshGhostSession,
    SelectCompatibleEngine,
    RestartDurableBoundary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SkillBrowserEngine {
    Firefox,
    Chromium,
    WebKit,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(
    rename_all = "camelCase",
    deny_unknown_fields,
    try_from = "SkillProfileRequestWire"
)]
pub struct SkillProfileRequest {
    pub schema_version: u16,
    pub required: BTreeSet<SkillCapability>,
    pub optional: BTreeSet<SkillCapability>,
    pub preferred_engines: Vec<SkillBrowserEngine>,
    pub values: BTreeMap<String, String>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SkillProfileRequestWire {
    schema_version: u16,
    required: BTreeSet<SkillCapability>,
    optional: BTreeSet<SkillCapability>,
    preferred_engines: Vec<SkillBrowserEngine>,
    values: BTreeMap<String, String>,
}

impl SkillProfileRequest {
    pub const SCHEMA_VERSION: u16 = SKILL_SCHEMA_VERSION;

    pub fn new(
        required: impl IntoIterator<Item = SkillCapability>,
        optional: impl IntoIterator<Item = SkillCapability>,
        preferred_engines: impl IntoIterator<Item = SkillBrowserEngine>,
        values: BTreeMap<String, String>,
    ) -> Result<Self, String> {
        let profile = Self {
            schema_version: Self::SCHEMA_VERSION,
            required: required.into_iter().collect(),
            optional: optional.into_iter().collect(),
            preferred_engines: preferred_engines.into_iter().collect(),
            values,
        };
        profile.validate()?;
        Ok(profile)
    }

    fn validate(&self) -> Result<(), String> {
        validate_schema_version(self.schema_version)?;
        validate_capabilities(&self.required, "required")?;
        validate_capabilities(&self.optional, "optional")?;
        if !self.required.is_disjoint(&self.optional) {
            return Err("required and optional capabilities must not overlap".into());
        }
        if self.required.union(&self.optional).count() > MAX_CAPABILITIES {
            return Err("required and optional capabilities exceed 32 entries".into());
        }
        if self.preferred_engines.len() > MAX_PREFERRED_ENGINES {
            return Err("preferred engines exceed 8 entries".into());
        }
        validate_profile_values(&self.values)?;
        Ok(())
    }
}

impl TryFrom<SkillProfileRequestWire> for SkillProfileRequest {
    type Error = String;

    fn try_from(value: SkillProfileRequestWire) -> Result<Self, Self::Error> {
        let profile = Self {
            schema_version: value.schema_version,
            required: value.required,
            optional: value.optional,
            preferred_engines: value.preferred_engines,
            values: value.values,
        };
        profile.validate()?;
        Ok(profile)
    }
}

impl Serialize for SkillProfileRequest {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        SkillProfileRequestWire {
            schema_version: self.schema_version,
            required: self.required.clone(),
            optional: self.optional.clone(),
            preferred_engines: self.preferred_engines.clone(),
            values: self.values.clone(),
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(
    rename_all = "camelCase",
    deny_unknown_fields,
    try_from = "SkillProfileWire"
)]
pub struct SkillProfile {
    pub schema_version: u16,
    pub version: String,
    pub engine: SkillBrowserEngine,
    pub effective_capabilities: BTreeSet<SkillCapability>,
    pub observable_digest: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SkillProfileWire {
    schema_version: u16,
    version: String,
    engine: SkillBrowserEngine,
    effective_capabilities: BTreeSet<SkillCapability>,
    observable_digest: String,
}

impl SkillProfile {
    pub const SCHEMA_VERSION: u16 = SKILL_SCHEMA_VERSION;

    pub fn new(
        version: impl Into<String>,
        engine: SkillBrowserEngine,
        effective_capabilities: impl IntoIterator<Item = SkillCapability>,
        observable_digest: impl Into<String>,
    ) -> Result<Self, String> {
        let profile = Self {
            schema_version: Self::SCHEMA_VERSION,
            version: version.into(),
            engine,
            effective_capabilities: effective_capabilities.into_iter().collect(),
            observable_digest: observable_digest.into(),
        };
        profile.validate()?;
        Ok(profile)
    }

    fn validate(&self) -> Result<(), String> {
        validate_schema_version(self.schema_version)?;
        validate_version(&self.version, "profile version")?;
        validate_capabilities(&self.effective_capabilities, "effective capabilities")?;
        validate_observable_digest(&self.observable_digest)
    }
}

impl TryFrom<SkillProfileWire> for SkillProfile {
    type Error = String;

    fn try_from(value: SkillProfileWire) -> Result<Self, Self::Error> {
        let profile = Self {
            schema_version: value.schema_version,
            version: value.version,
            engine: value.engine,
            effective_capabilities: value.effective_capabilities,
            observable_digest: value.observable_digest,
        };
        profile.validate()?;
        Ok(profile)
    }
}

impl Serialize for SkillProfile {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        SkillProfileWire {
            schema_version: self.schema_version,
            version: self.version.clone(),
            engine: self.engine,
            effective_capabilities: self.effective_capabilities.clone(),
            observable_digest: self.observable_digest.clone(),
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(
    rename_all = "camelCase",
    deny_unknown_fields,
    try_from = "SkillDecisionWire"
)]
pub struct SkillDecision {
    pub tactic: SkillTactic,
    pub trigger: SkillFailure,
    pub expected_postcondition: String,
    pub remaining_deadline_ms: u64,
    pub tactic_budget_ms: u64,
    pub checkpoint_id: Option<CheckpointId>,
    pub selected_engine: Option<SkillBrowserEngine>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SkillDecisionWire {
    tactic: SkillTactic,
    trigger: SkillFailure,
    expected_postcondition: String,
    remaining_deadline_ms: u64,
    tactic_budget_ms: u64,
    checkpoint_id: Option<CheckpointId>,
    selected_engine: Option<SkillBrowserEngine>,
}

impl SkillDecision {
    pub fn new(
        tactic: SkillTactic,
        trigger: SkillFailure,
        expected_postcondition: impl Into<String>,
        remaining_deadline_ms: u64,
        tactic_budget_ms: u64,
        checkpoint_id: Option<CheckpointId>,
        selected_engine: Option<SkillBrowserEngine>,
    ) -> Result<Self, String> {
        let decision = Self {
            tactic,
            trigger,
            expected_postcondition: expected_postcondition.into(),
            remaining_deadline_ms,
            tactic_budget_ms,
            checkpoint_id,
            selected_engine,
        };
        decision.validate()?;
        Ok(decision)
    }

    fn validate(&self) -> Result<(), String> {
        validate_postcondition(&self.expected_postcondition)?;
        if self.tactic_budget_ms > self.remaining_deadline_ms {
            return Err("tactic budget must not exceed the remaining deadline".into());
        }
        match (self.tactic, self.selected_engine) {
            (SkillTactic::SelectCompatibleEngine, None) => {
                return Err("engine selection requires a selected engine".into());
            }
            (SkillTactic::SelectCompatibleEngine, Some(_)) | (_, None) => {}
            (_, Some(_)) => return Err("only engine selection may select an engine".into()),
        }
        Ok(())
    }
}

impl TryFrom<SkillDecisionWire> for SkillDecision {
    type Error = String;

    fn try_from(value: SkillDecisionWire) -> Result<Self, Self::Error> {
        Self::new(
            value.tactic,
            value.trigger,
            value.expected_postcondition,
            value.remaining_deadline_ms,
            value.tactic_budget_ms,
            value.checkpoint_id,
            value.selected_engine,
        )
    }
}

impl Serialize for SkillDecision {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        SkillDecisionWire {
            tactic: self.tactic,
            trigger: self.trigger,
            expected_postcondition: self.expected_postcondition.clone(),
            remaining_deadline_ms: self.remaining_deadline_ms,
            tactic_budget_ms: self.tactic_budget_ms,
            checkpoint_id: self.checkpoint_id.clone(),
            selected_engine: self.selected_engine,
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    deny_unknown_fields,
    try_from = "SkillOutcomeWire"
)]
pub enum SkillOutcome {
    Applied {
        evidence: Vec<SkillEvidenceRef>,
    },
    Adapted {
        tactic: SkillTactic,
        evidence: Vec<SkillEvidenceRef>,
    },
    Degraded {
        unsupported: BTreeSet<SkillCapability>,
        evidence: Vec<SkillEvidenceRef>,
    },
    Stopped {
        evidence: Vec<SkillEvidenceRef>,
    },
    Failed {
        failure: SkillFailure,
        evidence: Vec<SkillEvidenceRef>,
    },
}

#[derive(Deserialize, Serialize)]
#[serde(tag = "status", rename_all = "camelCase", deny_unknown_fields)]
enum SkillOutcomeWire {
    Applied {
        evidence: Vec<SkillEvidenceRef>,
    },
    Adapted {
        tactic: SkillTactic,
        evidence: Vec<SkillEvidenceRef>,
    },
    Degraded {
        unsupported: BTreeSet<SkillCapability>,
        evidence: Vec<SkillEvidenceRef>,
    },
    Stopped {
        evidence: Vec<SkillEvidenceRef>,
    },
    Failed {
        failure: SkillFailure,
        evidence: Vec<SkillEvidenceRef>,
    },
}

impl SkillOutcome {
    pub fn applied(evidence: Vec<SkillEvidenceRef>) -> Result<Self, String> {
        Self::validate_evidence(&evidence)?;
        Ok(Self::Applied { evidence })
    }

    pub fn adapted(tactic: SkillTactic, evidence: Vec<SkillEvidenceRef>) -> Result<Self, String> {
        Self::validate_evidence(&evidence)?;
        Ok(Self::Adapted { tactic, evidence })
    }

    pub fn degraded(
        unsupported: BTreeSet<SkillCapability>,
        evidence: Vec<SkillEvidenceRef>,
    ) -> Result<Self, String> {
        validate_capabilities(&unsupported, "unsupported")?;
        Self::validate_evidence(&evidence)?;
        Ok(Self::Degraded {
            unsupported,
            evidence,
        })
    }

    pub fn stopped(evidence: Vec<SkillEvidenceRef>) -> Result<Self, String> {
        Self::validate_evidence(&evidence)?;
        Ok(Self::Stopped { evidence })
    }

    pub fn failed(failure: SkillFailure, evidence: Vec<SkillEvidenceRef>) -> Result<Self, String> {
        Self::validate_evidence(&evidence)?;
        Ok(Self::Failed { failure, evidence })
    }

    fn validate_evidence(evidence: &[SkillEvidenceRef]) -> Result<(), String> {
        if evidence.len() > MAX_EVIDENCE_REFS {
            return Err("evidence references exceed 128 entries".into());
        }
        for evidence_ref in evidence {
            evidence_ref.validate()?;
        }
        Ok(())
    }

    fn validate(&self) -> Result<(), String> {
        match self {
            Self::Applied { evidence }
            | Self::Adapted { evidence, .. }
            | Self::Stopped { evidence }
            | Self::Failed { evidence, .. } => Self::validate_evidence(evidence),
            Self::Degraded {
                unsupported,
                evidence,
            } => {
                validate_capabilities(unsupported, "unsupported")?;
                Self::validate_evidence(evidence)
            }
        }
    }
}

impl Serialize for SkillOutcome {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        match self {
            Self::Applied { evidence } => SkillOutcomeWire::Applied {
                evidence: evidence.clone(),
            },
            Self::Adapted { tactic, evidence } => SkillOutcomeWire::Adapted {
                tactic: *tactic,
                evidence: evidence.clone(),
            },
            Self::Degraded {
                unsupported,
                evidence,
            } => SkillOutcomeWire::Degraded {
                unsupported: unsupported.clone(),
                evidence: evidence.clone(),
            },
            Self::Stopped { evidence } => SkillOutcomeWire::Stopped {
                evidence: evidence.clone(),
            },
            Self::Failed { failure, evidence } => SkillOutcomeWire::Failed {
                failure: *failure,
                evidence: evidence.clone(),
            },
        }
        .serialize(serializer)
    }
}

impl TryFrom<SkillOutcomeWire> for SkillOutcome {
    type Error = String;

    fn try_from(value: SkillOutcomeWire) -> Result<Self, Self::Error> {
        match value {
            SkillOutcomeWire::Applied { evidence } => Self::applied(evidence),
            SkillOutcomeWire::Adapted { tactic, evidence } => Self::adapted(tactic, evidence),
            SkillOutcomeWire::Degraded {
                unsupported,
                evidence,
            } => Self::degraded(unsupported, evidence),
            SkillOutcomeWire::Stopped { evidence } => Self::stopped(evidence),
            SkillOutcomeWire::Failed { failure, evidence } => Self::failed(failure, evidence),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(
    rename_all = "camelCase",
    deny_unknown_fields,
    try_from = "SkillEvidenceRefWire"
)]
pub struct SkillEvidenceRef {
    pub artifact_id: String,
    pub sha256: String,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SkillEvidenceRefWire {
    artifact_id: String,
    sha256: String,
}

impl SkillEvidenceRef {
    pub fn new(artifact_id: impl Into<String>, sha256: impl Into<String>) -> Result<Self, String> {
        let evidence = Self {
            artifact_id: artifact_id.into(),
            sha256: sha256.into(),
        };
        evidence.validate()?;
        Ok(evidence)
    }

    fn validate(&self) -> Result<(), String> {
        validate_artifact_id(&self.artifact_id)?;
        if self.sha256.len() != 64
            || !self.sha256.bytes().all(|byte| {
                byte.is_ascii_digit() || (byte.is_ascii_lowercase() && byte.is_ascii_hexdigit())
            })
        {
            return Err("sha256 must be exactly 64 lowercase hexadecimal characters".into());
        }
        Ok(())
    }
}

impl TryFrom<SkillEvidenceRefWire> for SkillEvidenceRef {
    type Error = String;

    fn try_from(value: SkillEvidenceRefWire) -> Result<Self, Self::Error> {
        Self::new(value.artifact_id, value.sha256)
    }
}

impl Serialize for SkillEvidenceRef {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        SkillEvidenceRefWire {
            artifact_id: self.artifact_id.clone(),
            sha256: self.sha256.clone(),
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(
    rename_all = "camelCase",
    deny_unknown_fields,
    try_from = "SkillCheckpointProofWire"
)]
pub struct SkillCheckpointProof {
    pub checkpoint_id: CheckpointId,
    pub session_id: SessionId,
    pub verified_at: DateTime<Utc>,
    pub attestation: SkillEvidenceRef,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SkillCheckpointProofWire {
    checkpoint_id: CheckpointId,
    session_id: SessionId,
    verified_at: DateTime<Utc>,
    attestation: SkillEvidenceRef,
}

impl SkillCheckpointProof {
    const MAX_AGE_MINUTES: i64 = 15;

    pub fn new(
        checkpoint_id: CheckpointId,
        session_id: SessionId,
        verified_at: DateTime<Utc>,
        attestation: SkillEvidenceRef,
    ) -> Result<Self, String> {
        let proof = Self {
            checkpoint_id,
            session_id,
            verified_at,
            attestation,
        };
        proof.validate()?;
        Ok(proof)
    }

    pub fn is_fresh_at(&self, now: DateTime<Utc>) -> bool {
        now >= self.verified_at
            && now.signed_duration_since(self.verified_at)
                <= chrono::Duration::minutes(Self::MAX_AGE_MINUTES)
    }

    fn validate(&self) -> Result<(), String> {
        self.attestation.validate()?;
        if !self.is_fresh_at(Utc::now()) {
            return Err("checkpoint proof is stale or from the future".into());
        }
        Ok(())
    }
}

impl TryFrom<SkillCheckpointProofWire> for SkillCheckpointProof {
    type Error = String;

    fn try_from(value: SkillCheckpointProofWire) -> Result<Self, Self::Error> {
        Self::new(
            value.checkpoint_id,
            value.session_id,
            value.verified_at,
            value.attestation,
        )
    }
}

impl Serialize for SkillCheckpointProof {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        SkillCheckpointProofWire {
            checkpoint_id: self.checkpoint_id.clone(),
            session_id: self.session_id.clone(),
            verified_at: self.verified_at,
            attestation: self.attestation.clone(),
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(
    rename_all = "camelCase",
    deny_unknown_fields,
    try_from = "SkillIssuedDecisionWire"
)]
pub struct SkillIssuedDecision {
    pub reservation_id: CommandId,
    pub session_id: SessionId,
    #[serde(default)]
    pub command_identity: Option<SkillCommandIdentity>,
    pub decision: SkillDecision,
    pub checkpoint_proof: Option<SkillCheckpointProof>,
    pub issued_at: DateTime<Utc>,
    pub deadline: DateTime<Utc>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SkillIssuedDecisionWire {
    reservation_id: CommandId,
    session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    command_identity: Option<SkillCommandIdentity>,
    decision: SkillDecision,
    checkpoint_proof: Option<SkillCheckpointProof>,
    issued_at: DateTime<Utc>,
    deadline: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SkillCommandIdentity {
    pub command_id: CommandId,
    pub workflow_id: WorkflowId,
    pub attempt_id: AttemptId,
    pub session_id: SessionId,
    pub page_id: Option<PageId>,
    pub command_class: CommandClass,
    pub command_sha256: String,
}

impl SkillCommandIdentity {
    pub fn new(
        command_id: CommandId,
        workflow_id: WorkflowId,
        attempt_id: AttemptId,
        session_id: SessionId,
        page_id: Option<PageId>,
        command_class: CommandClass,
        command_sha256: impl Into<String>,
    ) -> Result<Self, String> {
        let identity = Self {
            command_id,
            workflow_id,
            attempt_id,
            session_id,
            page_id,
            command_class,
            command_sha256: command_sha256.into(),
        };
        identity.validate()?;
        Ok(identity)
    }

    fn validate(&self) -> Result<(), String> {
        if self.command_sha256.len() != 64
            || !self
                .command_sha256
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        {
            return Err("skill command identity requires a lowercase SHA-256 digest".into());
        }
        Ok(())
    }
}

impl SkillIssuedDecision {
    pub fn new(
        reservation_id: CommandId,
        session_id: SessionId,
        decision: SkillDecision,
        checkpoint_proof: Option<SkillCheckpointProof>,
        issued_at: DateTime<Utc>,
        deadline: DateTime<Utc>,
    ) -> Result<Self, String> {
        Self::new_inner(
            reservation_id,
            session_id,
            None,
            decision,
            checkpoint_proof,
            issued_at,
            deadline,
        )
    }

    pub fn new_for_command(
        reservation_id: CommandId,
        session_id: SessionId,
        command_identity: SkillCommandIdentity,
        decision: SkillDecision,
        checkpoint_proof: Option<SkillCheckpointProof>,
        issued_at: DateTime<Utc>,
        deadline: DateTime<Utc>,
    ) -> Result<Self, String> {
        if command_identity.session_id != session_id {
            return Err("issued decision command identity belongs to another session".into());
        }
        Self::new_inner(
            reservation_id,
            session_id,
            Some(command_identity),
            decision,
            checkpoint_proof,
            issued_at,
            deadline,
        )
    }

    fn new_inner(
        reservation_id: CommandId,
        session_id: SessionId,
        command_identity: Option<SkillCommandIdentity>,
        decision: SkillDecision,
        checkpoint_proof: Option<SkillCheckpointProof>,
        issued_at: DateTime<Utc>,
        deadline: DateTime<Utc>,
    ) -> Result<Self, String> {
        let issued = Self {
            reservation_id,
            session_id,
            command_identity,
            decision,
            checkpoint_proof,
            issued_at,
            deadline,
        };
        issued.validate()?;
        Ok(issued)
    }

    pub fn is_active_at(&self, now: DateTime<Utc>) -> bool {
        now >= self.issued_at && now <= self.deadline
    }

    fn validate(&self) -> Result<(), String> {
        self.decision.validate()?;
        if let Some(identity) = &self.command_identity {
            identity.validate()?;
            if identity.session_id != self.session_id {
                return Err("issued decision command identity belongs to another session".into());
            }
        }
        let interval_ms = issued_interval_ms(self.issued_at, self.deadline)?;
        if self.decision.remaining_deadline_ms != interval_ms {
            return Err(
                "issued decision remaining deadline does not match its issuance interval".into(),
            );
        }
        if self.decision.tactic_budget_ms > interval_ms {
            return Err("issued decision tactic budget exceeds its issuance interval".into());
        }
        match (
            tactic_requires_checkpoint(self.decision.tactic),
            &self.checkpoint_proof,
        ) {
            (true, Some(proof))
                if proof.session_id == self.session_id
                    && self.decision.checkpoint_id.as_ref() == Some(&proof.checkpoint_id) =>
            {
                proof.validate()?;
            }
            (true, _) => {
                return Err("recovery decision requires its verified checkpoint proof".into())
            }
            (false, None) if self.decision.checkpoint_id.is_none() => {}
            (false, _) => return Err("non-recovery decision cannot carry checkpoint proof".into()),
        }
        Ok(())
    }
}

impl TryFrom<SkillIssuedDecisionWire> for SkillIssuedDecision {
    type Error = String;

    fn try_from(value: SkillIssuedDecisionWire) -> Result<Self, Self::Error> {
        match value.command_identity {
            Some(identity) => Self::new_for_command(
                value.reservation_id,
                value.session_id,
                identity,
                value.decision,
                value.checkpoint_proof,
                value.issued_at,
                value.deadline,
            ),
            None => Self::new(
                value.reservation_id,
                value.session_id,
                value.decision,
                value.checkpoint_proof,
                value.issued_at,
                value.deadline,
            ),
        }
    }
}

impl Serialize for SkillIssuedDecision {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        SkillIssuedDecisionWire {
            reservation_id: self.reservation_id.clone(),
            session_id: self.session_id.clone(),
            command_identity: self.command_identity.clone(),
            decision: self.decision.clone(),
            checkpoint_proof: self.checkpoint_proof.clone(),
            issued_at: self.issued_at,
            deadline: self.deadline,
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(
    rename_all = "camelCase",
    deny_unknown_fields,
    try_from = "SkillSessionStateWire"
)]
pub struct SkillSessionState {
    pub schema_version: u16,
    pub session_id: SessionId,
    pub active_versions: BTreeMap<String, String>,
    pub effective_profile: Option<SkillProfile>,
    pub last_checkpoint_id: Option<CheckpointId>,
    pub verified_checkpoint: Option<SkillCheckpointProof>,
    pub reserved_tactic: Option<SkillTactic>,
    pub pending_issuance: Option<SkillIssuedDecision>,
    pub attempted_tactics: Vec<SkillTactic>,
    pub evidence: Vec<SkillEvidenceRef>,
    pub deadline: DateTime<Utc>,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SkillSessionStateWire {
    schema_version: u16,
    session_id: SessionId,
    active_versions: BTreeMap<String, String>,
    effective_profile: Option<SkillProfile>,
    last_checkpoint_id: Option<CheckpointId>,
    verified_checkpoint: Option<SkillCheckpointProof>,
    reserved_tactic: Option<SkillTactic>,
    pending_issuance: Option<SkillIssuedDecision>,
    attempted_tactics: Vec<SkillTactic>,
    evidence: Vec<SkillEvidenceRef>,
    deadline: DateTime<Utc>,
}

impl SkillSessionState {
    pub const SCHEMA_VERSION: u16 = SKILL_SCHEMA_VERSION;

    #[allow(clippy::too_many_arguments)] // The versioned wire contract maps one-to-one here.
    pub fn new(
        session_id: SessionId,
        active_versions: BTreeMap<String, String>,
        effective_profile: Option<SkillProfile>,
        last_checkpoint_id: Option<CheckpointId>,
        verified_checkpoint: Option<SkillCheckpointProof>,
        reserved_tactic: Option<SkillTactic>,
        pending_issuance: Option<SkillIssuedDecision>,
        attempted_tactics: Vec<SkillTactic>,
        evidence: Vec<SkillEvidenceRef>,
        deadline: DateTime<Utc>,
    ) -> Result<Self, String> {
        let state = Self {
            schema_version: Self::SCHEMA_VERSION,
            session_id,
            active_versions,
            effective_profile,
            last_checkpoint_id,
            verified_checkpoint,
            reserved_tactic,
            pending_issuance,
            attempted_tactics,
            evidence,
            deadline,
        };
        state.validate()?;
        Ok(state)
    }

    fn validate(&self) -> Result<(), String> {
        validate_schema_version(self.schema_version)?;
        validate_active_versions(&self.active_versions)?;
        if self.attempted_tactics.len() > MAX_TACTICS {
            return Err("attempted tactics exceed 32 entries".into());
        }
        if let Some(profile) = &self.effective_profile {
            profile.validate()?;
        }
        match (&self.last_checkpoint_id, &self.verified_checkpoint) {
            (None, None) => {}
            (Some(checkpoint_id), Some(proof))
                if proof.checkpoint_id == *checkpoint_id && proof.session_id == self.session_id =>
            {
                proof.validate()?;
            }
            (Some(_), None) => return Err("checkpoint requires a verified proof".into()),
            (None, Some(_)) => return Err("verified proof requires a checkpoint".into()),
            (Some(_), Some(_)) => {
                return Err("verified proof does not match session checkpoint".into())
            }
        }
        match (&self.reserved_tactic, &self.pending_issuance) {
            (None, None) => {}
            (Some(tactic), Some(issued))
                if issued.session_id == self.session_id
                    && issued.decision.tactic == *tactic
                    && self
                        .attempted_tactics
                        .iter()
                        .filter(|attempted| **attempted == *tactic)
                        .count()
                        == 1 =>
            {
                issued.validate()?;
                if issued.deadline != self.deadline {
                    return Err("issued decision deadline does not match session deadline".into());
                }
                if tactic_requires_checkpoint(*tactic)
                    && issued.checkpoint_proof != self.verified_checkpoint
                {
                    return Err("issued decision proof does not match session proof".into());
                }
            }
            (Some(_), None) => return Err("tactic reservation requires an issued decision".into()),
            (None, Some(_)) => return Err("issued decision requires a tactic reservation".into()),
            (Some(_), Some(_)) => {
                return Err("issued decision does not match its reservation".into())
            }
        }
        SkillOutcome::validate_evidence(&self.evidence)
    }
}

impl TryFrom<SkillSessionStateWire> for SkillSessionState {
    type Error = String;

    fn try_from(value: SkillSessionStateWire) -> Result<Self, Self::Error> {
        let state = Self {
            schema_version: value.schema_version,
            session_id: value.session_id,
            active_versions: value.active_versions,
            effective_profile: value.effective_profile,
            last_checkpoint_id: value.last_checkpoint_id,
            verified_checkpoint: value.verified_checkpoint,
            reserved_tactic: value.reserved_tactic,
            pending_issuance: value.pending_issuance,
            attempted_tactics: value.attempted_tactics,
            evidence: value.evidence,
            deadline: value.deadline,
        };
        state.validate()?;
        Ok(state)
    }
}

impl Serialize for SkillSessionState {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.validate().map_err(serde::ser::Error::custom)?;
        SkillSessionStateWire {
            schema_version: self.schema_version,
            session_id: self.session_id.clone(),
            active_versions: self.active_versions.clone(),
            effective_profile: self.effective_profile.clone(),
            last_checkpoint_id: self.last_checkpoint_id.clone(),
            verified_checkpoint: self.verified_checkpoint.clone(),
            reserved_tactic: self.reserved_tactic,
            pending_issuance: self.pending_issuance.clone(),
            attempted_tactics: self.attempted_tactics.clone(),
            evidence: self.evidence.clone(),
            deadline: self.deadline,
        }
        .serialize(serializer)
    }
}

fn tactic_requires_checkpoint(tactic: SkillTactic) -> bool {
    matches!(
        tactic,
        SkillTactic::ReconcileCheckpoint
            | SkillTactic::FreshGhostSession
            | SkillTactic::SelectCompatibleEngine
            | SkillTactic::RestartDurableBoundary
    )
}

fn issued_interval_ms(issued_at: DateTime<Utc>, deadline: DateTime<Utc>) -> Result<u64, String> {
    let interval = deadline.signed_duration_since(issued_at);
    if interval < chrono::Duration::zero() {
        return Err("issued decision is after the workflow deadline".into());
    }
    // Wire durations are whole milliseconds; sub-millisecond remainder is consistently floored.
    u64::try_from(interval.num_milliseconds())
        .map_err(|_| "issued decision interval is not representable in milliseconds".into())
}

fn validate_schema_version(schema_version: u16) -> Result<(), String> {
    if schema_version != SKILL_SCHEMA_VERSION {
        return Err("skill schema version must be 1".into());
    }
    Ok(())
}

fn validate_capabilities(
    capabilities: &BTreeSet<SkillCapability>,
    field: &str,
) -> Result<(), String> {
    if capabilities.len() > MAX_CAPABILITIES {
        return Err(format!("{field} capabilities exceed 32 entries"));
    }
    Ok(())
}

fn validate_profile_values(values: &BTreeMap<String, String>) -> Result<(), String> {
    if values.len() > MAX_PROFILE_VALUES {
        return Err("profile values exceed 64 entries".into());
    }
    for (name, value) in values {
        match name.as_str() {
            "locale" => validate_locale(value)?,
            "timezone" => validate_timezone(value)?,
            "userAgentConsistency" => validate_user_agent(value)?,
            "engineSelection" | "profilePersistence" | "viewport" | "interactionCadence" => {
                validate_version(value, name)?
            }
            _ => return Err(format!("unsupported profile setting: {name}")),
        }
    }
    Ok(())
}

fn validate_active_versions(values: &BTreeMap<String, String>) -> Result<(), String> {
    if values.len() > MAX_PROFILE_VALUES {
        return Err("active versions exceed 64 entries".into());
    }
    for (name, version) in values {
        if name.len() > MAX_NAME_BYTES
            || name.strip_prefix("Skill").is_none_or(|suffix| {
                suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_alphanumeric())
            })
        {
            return Err("active version name must be a Skill identifier".into());
        }
        validate_version(version, "active version")?;
    }
    Ok(())
}

fn validate_artifact_id(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_NAME_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
        || value.contains("..")
        || credential_or_token_prefix(value)
    {
        return Err("artifact id must be an opaque identifier".into());
    }
    Ok(())
}

fn validate_version(value: &str, field: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_NAME_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-'))
        || credential_or_token_prefix(value)
    {
        return Err(format!("{field} must use the conservative version grammar"));
    }
    Ok(())
}

fn validate_locale(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_NAME_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err("locale is invalid".into());
    }
    Ok(())
}

fn validate_timezone(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_NAME_BYTES
        || value.starts_with('/')
        || value.contains("..")
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b'_' | b'+' | b'-'))
    {
        return Err("timezone is invalid".into());
    }
    Ok(())
}

fn validate_user_agent(value: &str) -> Result<(), String> {
    if value.is_empty()
        || value.len() > MAX_NAME_BYTES
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
    {
        return Err("user agent is invalid".into());
    }
    validate_display_text_safety(value, "user agent")
}

fn validate_postcondition(value: &str) -> Result<(), String> {
    if value.is_empty() || value.len() > MAX_POSTCONDITION_BYTES {
        return Err("expected postcondition must be between 1 and 1024 bytes".into());
    }
    validate_display_text_safety(value, "expected postcondition")
}

fn validate_observable_digest(value: &str) -> Result<(), String> {
    validate_display_text_safety(value, "observable digest")
}

fn validate_display_text_safety(value: &str, field: &str) -> Result<(), String> {
    if value.chars().any(char::is_control)
        || value.contains("..")
        || value.contains("~/")
        || value.contains("~\\")
        || value.starts_with('/')
        || value.starts_with('\\')
        || value.contains(" /")
        || value.contains("\\\\")
        || contains_forbidden_secret_term(value)
        || contains_windows_drive_path(value)
        || value.contains("://")
        || credential_or_token_prefix(value)
        || credential_assignment(value)
    {
        return Err(format!("{field} contains unsafe wire metadata"));
    }
    Ok(())
}

fn contains_forbidden_secret_term(value: &str) -> bool {
    value
        .to_ascii_lowercase()
        .split(|character: char| !character.is_ascii_alphanumeric())
        .any(|token| {
            matches!(
                token,
                "cookie" | "password" | "authorization" | "bearer" | "token"
            )
        })
}

fn contains_windows_drive_path(value: &str) -> bool {
    value.as_bytes().windows(3).any(|window| {
        window[0].is_ascii_alphabetic() && window[1] == b':' && matches!(window[2], b'/' | b'\\')
    })
}

fn credential_or_token_prefix(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "basic ",
        "bearer ",
        "authorization:",
        "authorization ",
        "ghp_",
        "gho_",
        "ghs_",
        "github_pat_",
        "sk-",
        "token",
    ]
    .iter()
    .any(|prefix| lower.contains(prefix))
}

fn credential_assignment(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    [
        "authorization=",
        "cookie=",
        "password=",
        "token=",
        "api_key=",
        "apikey=",
    ]
    .iter()
    .any(|prefix| lower.contains(prefix))
}
