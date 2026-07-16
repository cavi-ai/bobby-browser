use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{
    AttemptId, CheckpointId, CommandClass, CommandId, Evidence, PageId, SessionId, WorkflowId,
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
    pub created_at: DateTime<Utc>,
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
#[serde(tag = "status", rename_all = "camelCase")]
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
    },
}
