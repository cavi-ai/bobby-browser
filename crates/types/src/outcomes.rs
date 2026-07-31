use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{AttemptId, CommandId, PageId};

/// One node of a compact accessibility tree as returned by the
/// `accessibilitySnapshot` primitive on any engine.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccessibilityNode {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub children: Vec<AccessibilityNode>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(
    tag = "status",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
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
        #[serde(default)]
        evidence: Vec<Evidence>,
    },
    Failed {
        command_id: CommandId,
        error: CommandError,
        #[serde(default)]
        evidence: Vec<Evidence>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase"
)]
pub enum Evidence {
    ExecutionPath {
        path: ExecutionPath,
        reason: ExecutionReason,
        state_version: u64,
        elapsed_ms: u64,
        bytes: Option<u64>,
        sha256: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        final_url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content_type: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        redirect_chain: Vec<String>,
    },
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
    Configuration {
        name: String,
        value: String,
    },
    Resolution {
        target: Box<crate::TargetSpec>,
        fingerprint: Box<TargetFingerprint>,
        candidates: Vec<CandidateEvidence>,
        best_match_authorized: bool,
    },
    Wait {
        condition: crate::WaitCondition,
        elapsed_ms: u64,
        observations: u64,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        excluded_classes: Vec<String>,
    },
    Screenshot {
        artifact_id: String,
        media_type: String,
        width: u32,
        height: u32,
        bytes: u64,
        sha256: String,
    },
    BrowserExecution {
        engine: String,
        browser_version: String,
        profile_id: String,
        interaction_path: String,
    },
    JavaScriptResult {
        value: serde_json::Value,
        truncated: bool,
    },
    AccessibilitySnapshot {
        page_id: PageId,
        nodes: Vec<AccessibilityNode>,
        truncated: bool,
    },
    IntentExecution {
        record: ExecutionRecord,
    },
    /// Result of resolving one named field of an `ExtractIntent`. Emitted
    /// once per field, in field order, alongside a `Resolution` evidence
    /// entry when the field resolved (deterministically or via vision).
    /// `value: None` means the field could not be resolved; `errorCode`
    /// then carries why (e.g. `targetNotFound`, `targetAmbiguous`,
    /// `visionAssistDenied`, `visionAssistFailed`) without failing the rest
    /// of the extraction.
    Extraction {
        field: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        value: Option<String>,
        resolution_path: IntentResolutionPath,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_code: Option<ErrorCode>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum IntentResolutionPath {
    Deterministic,
    VisionFallback,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ExecutionRecord {
    pub intent_kind: String,
    pub purpose: Option<String>,
    pub resolution_path: IntentResolutionPath,
    pub plan_summary: String,
    pub candidates: Vec<CandidateEvidence>,
    pub wait_elapsed_ms: Option<u64>,
    pub verification: String,
    pub artifact_ids: Vec<String>,
    pub vision_proposal_sha256: Option<String>,
}

impl Evidence {
    pub fn journal_safe(&self) -> Self {
        fn safe_url(value: &str) -> String {
            let Ok(mut url) = url::Url::parse(value) else {
                return "[redacted-invalid-url]".into();
            };
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            url.to_string()
        }
        let mut safe = self.clone();
        match &mut safe {
            Self::ExecutionPath {
                final_url,
                redirect_chain,
                ..
            } => {
                if let Some(url) = final_url {
                    *url = safe_url(url);
                }
                for url in redirect_chain {
                    *url = safe_url(url);
                }
            }
            Self::Navigation { url, .. }
            | Self::Inspection { url, .. }
            | Self::Page { url, .. }
            | Self::Popup { url, .. } => *url = safe_url(url),
            Self::Pages { pages } => {
                for page in pages {
                    page.url = safe_url(&page.url);
                }
            }
            Self::Upload { paths, .. } => {
                for (index, path) in paths.iter_mut().enumerate() {
                    *path = format!("upload://evidence/{index}");
                }
            }
            Self::Download { path, sha256, .. } => {
                *path = format!("artifact://sha256/{sha256}");
            }
            Self::BrowserExecution { .. } => {}
            Self::IntentExecution { .. } => {}
            Self::Extraction { .. } => {}
            _ => {}
        }
        safe
    }
}

impl CommandOutcome {
    pub fn journal_safe(&self) -> Self {
        let mut safe = self.clone();
        match &mut safe {
            Self::Completed { evidence, .. }
            | Self::NeedsReconciliation { evidence, .. }
            | Self::Restarted { evidence, .. }
            | Self::Failed { evidence, .. } => {
                *evidence = evidence.iter().map(Evidence::journal_safe).collect();
            }
            _ => {}
        }
        match &mut safe {
            Self::RetryableFailure { error, .. }
            | Self::NeedsReconciliation { error, .. }
            | Self::PolicyDenied { error, .. }
            | Self::ResourceExhausted { error, .. }
            | Self::Failed { error, .. } => error.message = "redacted durable diagnostic".into(),
            _ => {}
        }
        safe
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum ExecutionPath {
    DirectHttp,
    Chromium,
    ChromiumFallback,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum ExecutionReason {
    EligibleStaticDocument,
    EligibleExplicitDownload,
    IneligibleCommand,
    SemanticTargetRequired,
    JavascriptRequired,
    UnsupportedContentType,
    StateConflict,
    PolicyRequired,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct CandidateEvidence {
    pub role: Option<String>,
    pub name: Option<String>,
    pub score: i32,
    pub reasons: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct TargetFingerprint {
    pub page_id: PageId,
    pub frame: Option<String>,
    pub role: Option<String>,
    pub name: Option<String>,
    pub stable_attributes: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct PageEvidence {
    pub page_id: PageId,
    pub url: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct CommandError {
    pub code: ErrorCode,
    pub message: String,
    pub layer: ErrorLayer,
    pub retryable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
    TargetNotFound,
    TargetAmbiguous,
    FrameNotFound,
    ShadowRootUnavailable,
    TargetDetached,
    TargetObscured,
    TargetOutOfBounds,
    WaitConditionTimedOut,
    ScreenshotCaptureFailed,
    NetworkPolicyDenied,
    HttpResponseTooLarge,
    HttpTransferFailed,
    HttpStateConflict,
    HttpEquivalenceUnproven,
    IntentCompileFailed,
    IntentActionMismatch,
    ObstructionSuspected,
    VisionAssistDenied,
    VisionAssistFailed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum CommandPhase {
    Accepted,
    Prepared,
    Executing,
    ResultPrepared,
    Verifying,
    Recovering,
    Completed,
    Failed,
}

#[derive(Debug, Error, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub enum RuntimeError {
    #[error("not found: {0}")]
    NotFound(String),
    #[error("invalid request: {0}")]
    InvalidRequest(String),
    #[error("internal error: {0}")]
    Internal(String),
}
