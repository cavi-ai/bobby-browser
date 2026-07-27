use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{
    AttemptId, CheckpointId, CommandClass, CommandId, CommandOutcome, Evidence, PageId, SessionId,
    SkillDecision, SkillOutcome, WorkflowId,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum CheckpointInvariant {
    Url { value: String },
    Title { value: String },
    Text { selector: String, value: String },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct WorkflowCheckpoint {
    pub schema_version: u16,
    pub checkpoint_id: CheckpointId,
    pub workflow_id: WorkflowId,
    pub attempt_id: AttemptId,
    pub session_id: SessionId,
    pub page_id: PageId,
    pub restart_url: String,
    pub current_url: String,
    pub cursor: Option<CommandId>,
    #[serde(default)]
    pub boundary_command_id: Option<CommandId>,
    pub recovery_class: CommandClass,
    pub invariants: Vec<CheckpointInvariant>,
    pub replayable_inputs: Vec<String>,
    pub evidence: Vec<Evidence>,
    #[serde(default)]
    pub recovery_history: Vec<RecoveryRecord>,
    #[serde(default)]
    pub recovery_receipts: Vec<RecoveryReceipt>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryCommandIdentity {
    pub command_id: CommandId,
    pub workflow_id: WorkflowId,
    pub attempt_id: AttemptId,
    pub session_id: SessionId,
    pub page_id: Option<PageId>,
    pub command_class: CommandClass,
    pub command_sha256: String,
}

impl RecoveryCommandIdentity {
    #[allow(clippy::too_many_arguments)]
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

    pub fn validate(&self) -> Result<(), String> {
        validate_sha256(&self.command_sha256, "recovery command identity")
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum RecoveryReceiptState {
    PendingJournal,
    Committed,
    Unresolved,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryReceipt {
    pub idempotency_key: CommandId,
    pub identity: RecoveryCommandIdentity,
    pub state: RecoveryReceiptState,
    pub reservation_id: CommandId,
    pub decision: SkillDecision,
    pub command_outcome: CommandOutcome,
    pub skill_outcome: SkillOutcome,
    pub tactic_evidence: Vec<Evidence>,
    pub outcome_sha256: String,
    pub recorded_at: DateTime<Utc>,
}

impl RecoveryReceipt {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        idempotency_key: CommandId,
        identity: RecoveryCommandIdentity,
        state: RecoveryReceiptState,
        reservation_id: CommandId,
        decision: SkillDecision,
        command_outcome: CommandOutcome,
        skill_outcome: SkillOutcome,
        tactic_evidence: Vec<Evidence>,
        recorded_at: DateTime<Utc>,
    ) -> Result<Self, String> {
        let outcome_sha256 =
            Self::outcome_digest(&command_outcome, &skill_outcome, &tactic_evidence)?;
        let receipt = Self {
            idempotency_key,
            identity,
            state,
            reservation_id,
            decision,
            command_outcome,
            skill_outcome,
            tactic_evidence,
            outcome_sha256,
            recorded_at,
        };
        receipt.validate()?;
        Ok(receipt)
    }

    pub fn validate(&self) -> Result<(), String> {
        self.identity.validate()?;
        if self.idempotency_key != self.identity.command_id
            || command_outcome_id(&self.command_outcome) != &self.identity.command_id
        {
            return Err("recovery receipt command identity does not match its outcome".into());
        }
        validate_sha256(&self.outcome_sha256, "recovery receipt outcome")?;
        let actual = Self::outcome_digest(
            &self.command_outcome,
            &self.skill_outcome,
            &self.tactic_evidence,
        )?;
        if actual != self.outcome_sha256 {
            return Err("recovery receipt outcome digest changed".into());
        }
        serde_json::to_vec(&self.decision).map_err(|error| error.to_string())?;
        Ok(())
    }

    pub fn outcome_digest(
        command_outcome: &CommandOutcome,
        skill_outcome: &SkillOutcome,
        tactic_evidence: &[Evidence],
    ) -> Result<String, String> {
        let bytes = serde_json::to_vec(&(command_outcome, skill_outcome, tactic_evidence))
            .map_err(|error| error.to_string())?;
        Ok(format!("{:x}", Sha256::digest(bytes)))
    }
}

fn command_outcome_id(outcome: &CommandOutcome) -> &CommandId {
    match outcome {
        CommandOutcome::Completed { command_id, .. }
        | CommandOutcome::RetryableFailure { command_id, .. }
        | CommandOutcome::NeedsReconciliation { command_id, .. }
        | CommandOutcome::PolicyDenied { command_id, .. }
        | CommandOutcome::ResourceExhausted { command_id, .. }
        | CommandOutcome::Restarted { command_id, .. }
        | CommandOutcome::Failed { command_id, .. } => command_id,
    }
}

fn validate_sha256(value: &str, name: &str) -> Result<(), String> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    {
        return Err(format!("{name} requires a lowercase SHA-256 digest"));
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryRecord {
    pub recorded_at: DateTime<Utc>,
    pub decision: RecoveryDecision,
}

impl WorkflowCheckpoint {
    pub const SCHEMA_VERSION: u16 = 1;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryRequest {
    pub workflow_id: WorkflowId,
    pub checkpoint_id: CheckpointId,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RestartLineage {
    pub workflow_id: WorkflowId,
    pub abandoned_attempt_id: AttemptId,
    pub attempt_id: AttemptId,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum RecoveryDecision {
    Resumed {
        checkpoint_id: CheckpointId,
        attempt_id: AttemptId,
        evidence: Vec<Evidence>,
    },
    NeedsReconciliation {
        checkpoint_id: CheckpointId,
        attempt_id: AttemptId,
        reason: String,
        evidence: Vec<Evidence>,
    },
    Restarted {
        checkpoint_id: CheckpointId,
        lineage: RestartLineage,
        #[serde(default)]
        evidence: Vec<Evidence>,
    },
}
