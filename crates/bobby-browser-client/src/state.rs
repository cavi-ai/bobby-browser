//! Session and page runtime state returned by `/v1` endpoints.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ExecutionPolicy, PageId, SessionId};

/// Page rendering / interaction mode.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PageMode {
    Document,
    Interactive,
    Render,
}

/// `GET /v1/runtime` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RuntimeInfo {
    pub version: String,
    pub capabilities: Vec<String>,
    pub active_sessions: usize,
    pub queued_jobs: usize,
    pub uptime_ms: u64,
}

/// Browser session returned by session create/list endpoints.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct SessionState {
    pub id: SessionId,
    pub profile: String,
    pub proxy: Option<String>,
    pub page_ids: Vec<PageId>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
    pub execution_policy: ExecutionPolicy,
}

/// Page within a session.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct PageState {
    pub id: PageId,
    pub session_id: SessionId,
    pub url: Option<String>,
    pub mode: PageMode,
    pub ready_state: String,
    pub pending_requests: usize,
}

/// Result of a navigation command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NavigationResult {
    pub page_id: PageId,
    pub url: String,
    pub ready_state: String,
}

/// Result of a structured extract command.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ExtractResult {
    pub page_id: PageId,
    pub data: serde_json::Value,
}
