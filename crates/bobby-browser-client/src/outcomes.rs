//! Command outcomes, evidence, and accessibility snapshot nodes.

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::{AttemptId, CommandId, PageId};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControlActionEvidence {
    pub operation: crate::FormControlOperation,
    pub target: crate::FormControlTarget,
    pub state: crate::FormControlState,
    pub validity: crate::FormControlValidity,
    pub node_replaced: bool,
}

/// Semantic target for accessibility-based commands (role + accessible name).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccessibilityTarget {
    pub role: String,
    pub accessible_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<usize>,
}

/// Where the retained page context says a described control is.
///
/// Must stay in `types`: the crate-boundary guard requires every wire-advertised
/// shape to be a `types::` one so the schema parity guard covers it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ContextAnswer {
    pub target: AccessibilityTarget,
    pub confidence: f32,
    /// Whether the answer was observed live this session (stamped with the
    /// page's generation) or remembered from a prior session's persisted
    /// context. Additive; absent means a live generation-0 answer.
    #[serde(default)]
    pub observed_at: ContextObservedAt,
    /// How the underlying record entered the graph. Absent for live answers
    /// (always direct observation).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ContextAnswerSource>,
}

impl Default for ContextObservedAt {
    fn default() -> Self {
        Self::Generation { generation: 0 }
    }
}

/// Provenance of a [`ContextAnswer`]: live-observed under a page generation,
/// or remembered from the persisted per-profile context store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ContextObservedAt {
    Generation { generation: u64 },
    Persisted,
}

/// How a remembered control record entered the graph.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "kebab-case")]
pub enum ContextAnswerSource {
    Observed,
    VisionPromoted,
}

/// The remembered form structure around a located control (`context_neighbors`).
/// Structure only: roles, names, ordinals, and per-intent counters — never
/// values, page text, or timestamps finer than a day.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ContextNeighbors {
    pub answer: ContextAnswer,
    /// Key of the enclosing form within the remembered page.
    pub form: String,
    /// Pattern of the remembered page the form belongs to.
    pub page_pattern: String,
    pub controls: Vec<ContextNeighborControl>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ContextNeighborControl {
    pub role: String,
    pub accessible_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ordinal: Option<usize>,
    /// Per-intent-kind counters, keyed by intent kind.
    pub intents: std::collections::BTreeMap<String, ContextNeighborStats>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ContextNeighborStats {
    pub success_count: u64,
    pub failure_count: u64,
    /// Days since the Unix epoch of the last verified success.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_verified_day: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<ContextAnswerSource>,
}

/// One remembered site's structure: page pattern → form key → controls.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ContextSiteView {
    pub site_key: String,
    pub pages: std::collections::BTreeMap<
        String,
        std::collections::BTreeMap<String, Vec<ContextNeighborControl>>,
    >,
}

/// One node in an `accessibilitySnapshot` result tree.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccessibilityNode {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<AccessibilityTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub invalid: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub autocomplete: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_min: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value_max: Option<String>,
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

/// Upper bound on [`Evidence::Wait`]'s `observed` value, in characters.
///
/// A verification value -- a matched label, a URL, a ready state -- not page
/// text. Truncation is on a character boundary; a byte index can land inside a
/// multi-byte codepoint and panic, which is exactly how extraction used to die
/// on non-ASCII pages.
pub const MAX_WAIT_OBSERVED_CHARS: usize = 512;

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
        /// What the satisfying poll actually read: the element text or value,
        /// the URL, or the document ready state, depending on the condition.
        ///
        /// The wait already reads this to decide whether it is satisfied. It
        /// used to be discarded, so an agent that verified a submit had to
        /// spend a second round trip snapshotting the page to learn what it
        /// had just confirmed. Bounded by [`MAX_WAIT_OBSERVED_CHARS`] -- this
        /// is a verification value, never a page-text dump.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        observed: Option<String>,
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
    FormSnapshot {
        snapshot: crate::FormSnapshot,
    },
    ControlAction {
        action: ControlActionEvidence,
    },
    StructuredExtraction {
        page_id: PageId,
        value: serde_json::Value,
        truncated: bool,
    },
    CookieState {
        page_id: Option<PageId>,
        cookies: Vec<crate::CookieRecord>,
    },
    PdfArtifact {
        artifact_id: String,
        media_type: String,
        bytes: u64,
        sha256: String,
    },
    Dialog {
        dialog_type: String,
        message: String,
        action: String,
    },
    Emulation {
        viewport: Option<crate::ViewportSize>,
        geolocation: Option<crate::GeolocationCoordinates>,
    },
    HarArtifact {
        artifact_id: String,
        media_type: String,
        bytes: u64,
        sha256: String,
        entries: u32,
    },
    IntentExecution {
        record: ExecutionRecord,
    },
    /// Input timing the runtime synthesized rather than observed, emitted when
    /// the session opted into `executionPolicy.humanize`. Lets `intent-engine`
    /// verify an effect against the timing actually issued. Carries no typed
    /// text: action counts and durations only.
    Humanization {
        engine: String,
        actions: u32,
        synthesized_ms: u64,
    },
    /// Result of resolving one named field of an `ExtractIntent`, emitted once
    /// per field in field order. `value: None` means unresolved, with the
    /// reason in `errorCode`; the rest of the extraction still runs.
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
    /// Resolved from a cached vision proposal (lazy batch prefill) rather
    /// than a live stuck-rescue escalation.
    VisionPrefill,
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
    /// The page has mutated since load (a non-read-only command ran against
    /// it), so a whole-page read must come from the live DOM, not a refetch.
    PageMutated,
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
