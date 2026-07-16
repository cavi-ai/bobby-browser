use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{AttemptId, CommandId, PageId};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "camelCase")]
pub enum CommandOutcome {
    Completed {
        command_id: CommandId,
        evidence: Vec<Evidence>,
    },
    RetryableFailure {
        command_id: CommandId,
        error: CommandError,
    },
    NeedsReconciliation {
        command_id: CommandId,
        error: CommandError,
        evidence: Vec<Evidence>,
    },
    PolicyDenied {
        command_id: CommandId,
        error: CommandError,
    },
    ResourceExhausted {
        command_id: CommandId,
        error: CommandError,
        retry_after_ms: u64,
    },
    Restarted {
        command_id: CommandId,
        prior_attempt_id: AttemptId,
        attempt_id: AttemptId,
        reason: String,
    },
    Failed {
        command_id: CommandId,
        error: CommandError,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Evidence {
    Navigation {
        url: String,
        title: String,
    },
    Inspection {
        selector: Option<String>,
        url: String,
        title: String,
        text: String,
        html: Option<String>,
    },
    Element {
        selector: String,
        text: Option<String>,
    },
    Upload {
        selector: String,
        paths: Vec<String>,
    },
    Page {
        page_id: PageId,
        url: String,
        title: String,
    },
    Pages {
        pages: Vec<PageEvidence>,
    },
    Popup {
        opener_page_id: PageId,
        page_id: PageId,
        url: String,
        title: String,
    },
    Download {
        filename: String,
        path: String,
        bytes: u64,
        sha256: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct PageEvidence {
    pub page_id: PageId,
    pub url: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandError {
    pub code: ErrorCode,
    pub message: String,
    pub layer: ErrorLayer,
    pub retryable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorCode {
    InvalidRequest,
    NotFound,
    DeadlineExceeded,
    BrowserLaunchFailed,
    BrowserCommandFailed,
    VerificationFailed,
    JournalFailed,
    ResourceExhausted,
    PolicyDenied,
    Internal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ErrorLayer {
    Interface,
    Broker,
    Workflow,
    Page,
    Driver,
    Browser,
    Network,
    Site,
    Journal,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandPhase {
    Accepted,
    Prepared,
    Executing,
    Verifying,
    Recovering,
    Completed,
    Failed,
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
