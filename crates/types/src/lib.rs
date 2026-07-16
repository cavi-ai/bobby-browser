use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct SessionId(pub Uuid);

impl Default for SessionId {
    fn default() -> Self {
        Self(Uuid::new_v4())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct PageId(pub Uuid);

impl Default for PageId {
    fn default() -> Self {
        Self(Uuid::new_v4())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct JobId(pub Uuid);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Hash)]
pub struct ArtifactId(pub Uuid);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum PageMode {
    Document,
    Interactive,
    Render,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub version: String,
    pub capabilities: Vec<String>,
    pub active_sessions: usize,
    pub queued_jobs: usize,
    pub uptime_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionState {
    pub id: SessionId,
    pub profile: String,
    pub proxy: Option<String>,
    pub page_ids: Vec<PageId>,
    pub created_at: DateTime<Utc>,
    pub last_used_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PageState {
    pub id: PageId,
    pub session_id: SessionId,
    pub url: Option<String>,
    pub mode: PageMode,
    pub ready_state: String,
    pub pending_requests: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionRequest {
    pub profile: String,
    pub proxy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenPageRequest {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavigationRequest {
    pub page_id: PageId,
    pub url: String,
    pub wait_until: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NavigationResult {
    pub page_id: PageId,
    pub url: String,
    pub ready_state: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractRequest {
    pub page_id: PageId,
    pub fields: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractResult {
    pub page_id: PageId,
    pub data: serde_json::Value,
}

#[derive(Debug, Error, Serialize, Deserialize)]
pub enum RuntimeError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("internal error: {0}")]
    Internal(String),
}
