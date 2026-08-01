use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ExecutionPolicy, PageId, SessionId};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum PageMode {
    Document,
    Interactive,
    Render,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct RuntimeInfo {
    pub version: String,
    pub capabilities: Vec<String>,
    pub active_sessions: usize,
    pub queued_jobs: usize,
    pub uptime_ms: u64,
}

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

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NavigationResult {
    pub page_id: PageId,
    pub url: String,
    pub ready_state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ExtractResult {
    pub page_id: PageId,
    pub data: serde_json::Value,
}
