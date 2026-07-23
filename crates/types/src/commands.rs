use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::{AttemptId, CommandId, PageId, SessionId, WorkflowId};

pub const MAX_INTENT_PURPOSE_BYTES: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommandEnvelope {
    pub schema_version: u16,
    pub command_id: CommandId,
    pub workflow_id: WorkflowId,
    pub attempt_id: AttemptId,
    pub session_id: SessionId,
    pub page_id: Option<PageId>,
    pub deadline: DateTime<Utc>,
    pub command: RuntimeCommand,
}

impl CommandEnvelope {
    pub const SCHEMA_VERSION: u16 = 2;

    /// Returns an envelope suitable for durable journals and diagnostics.
    /// The live envelope must remain in memory for execution.
    pub fn journal_safe(&self) -> Self {
        let mut safe = self.clone();
        safe.command.sanitize_urls();
        safe
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "input", rename_all = "camelCase")]
pub enum RuntimeCommand {
    Primitive(PrimitiveCommand),
    Intent(IntentCommand),
}

impl RuntimeCommand {
    pub fn class(&self) -> CommandClass {
        match self {
            Self::Primitive(command) => command.class(),
            Self::Intent(command) => command.class(),
        }
    }

    pub fn sanitize_urls(&mut self) {
        match self {
            Self::Primitive(command) => command.sanitize_urls(),
            Self::Intent(_) => {}
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "input", rename_all = "camelCase")]
pub enum IntentCommand {
    Locate(LocateIntent),
    Fill(FillIntent),
    SubmitAndVerify(SubmitAndVerifyIntent),
    WaitForState(WaitForStateIntent),
}

impl IntentCommand {
    pub fn class(&self) -> CommandClass {
        match self {
            Self::Locate(_) | Self::WaitForState(_) => CommandClass::Replayable,
            Self::Fill(_) => CommandClass::Reconciliable,
            Self::SubmitAndVerify(_) => CommandClass::Boundary,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct IntentHints {
    pub role: Option<String>,
    pub near_text: Option<TextMatch>,
    pub frame_path: Vec<TargetSpec>,
    pub shadow_path: Vec<TargetSpec>,
    pub allow_best_match: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LocateIntent {
    pub purpose: String,
    #[serde(default)]
    pub hints: IntentHints,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FillIntent {
    pub purpose: String,
    #[serde(default)]
    pub hints: IntentHints,
    pub value: FillValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum FillValue {
    Text {
        text: String,
        #[serde(default)]
        clear_first: bool,
    },
    Select {
        option: String,
    },
    Files {
        paths: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubmitAndVerifyIntent {
    pub purpose: String,
    #[serde(default)]
    pub hints: IntentHints,
    pub expected_state: WaitForCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitForStateIntent {
    pub condition: WaitCondition,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "input", rename_all = "camelCase")]
pub enum PrimitiveCommand {
    Navigate(NavigateCommand),
    DownloadUrl(DownloadUrlCommand),
    Inspect(InspectCommand),
    Click(ClickCommand),
    TypeText(TypeTextCommand),
    UploadFiles(UploadFilesCommand),
    OpenPage(OpenPageCommand),
    ListPages(ListPagesCommand),
    ClosePage(ClosePageCommand),
    ClickAndWaitForPopup(ClickAndWaitForPopupCommand),
    ClickAndWaitForDownload(ClickAndWaitForDownloadCommand),
    WaitFor(WaitForCommand),
    CaptureScreenshot(CaptureScreenshotCommand),
    SetFocusEmulation(SetFocusEmulationCommand),
    SetEmulatedMedia(SetEmulatedMediaCommand),
    EvaluateJavaScript(EvaluateJavaScriptCommand),
}

impl PrimitiveCommand {
    fn sanitize_urls(&mut self) {
        fn sanitize(value: &mut String) {
            let Ok(mut url) = url::Url::parse(value) else {
                *value = "[redacted-invalid-url]".into();
                return;
            };
            let _ = url.set_username("");
            let _ = url.set_password(None);
            url.set_query(None);
            url.set_fragment(None);
            *value = url.to_string();
        }
        match self {
            Self::Navigate(command) => sanitize(&mut command.url),
            Self::DownloadUrl(command) => sanitize(&mut command.url),
            Self::OpenPage(command) => {
                if let Some(url) = &mut command.url {
                    sanitize(url);
                }
            }
            Self::Click(command) => {
                if let Some(url) = &mut command.expected_url {
                    sanitize(url);
                }
            }
            _ => {}
        }
    }

    pub fn class(&self) -> CommandClass {
        match self {
            Self::Navigate(_)
            | Self::Inspect(_)
            | Self::OpenPage(_)
            | Self::ListPages(_)
            | Self::WaitFor(_)
            | Self::CaptureScreenshot(_) => CommandClass::Replayable,
            Self::DownloadUrl(_)
            | Self::TypeText(_)
            | Self::UploadFiles(_)
            | Self::ClosePage(_)
            | Self::EvaluateJavaScript(_) => CommandClass::Reconciliable,
            Self::ClickAndWaitForPopup(_) | Self::ClickAndWaitForDownload(_) => {
                CommandClass::Boundary
            }
            Self::Click(command) if command.boundary => CommandClass::Boundary,
            Self::Click(_) => CommandClass::Reconciliable,
            Self::SetFocusEmulation(_) => CommandClass::Reconciliable,
            Self::SetEmulatedMedia(_) => CommandClass::Reconciliable,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DownloadUrlCommand {
    pub url: String,
    pub expected_content_type: Option<String>,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum CommandClass {
    Replayable,
    Reconciliable,
    Boundary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NavigateCommand {
    pub url: String,
    pub wait_until: WaitUntil,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WaitUntil {
    Commit,
    DomContentLoaded,
    Interactive,
    NetworkIdle,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InspectCommand {
    pub selector: Option<String>,
    pub target: Option<TargetSpec>,
    pub include_html: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClickCommand {
    pub selector: String,
    pub target: Option<TargetSpec>,
    pub boundary: bool,
    pub expected_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeTextCommand {
    pub selector: String,
    pub target: Option<TargetSpec>,
    pub value: String,
    pub clear_first: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadFilesCommand {
    pub selector: String,
    pub target: Option<TargetSpec>,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenPageCommand {
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct ListPagesCommand;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClosePageCommand {
    pub page_id: PageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClickAndWaitForPopupCommand {
    pub selector: String,
    pub target: Option<TargetSpec>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClickAndWaitForDownloadCommand {
    pub selector: String,
    pub target: Option<TargetSpec>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TargetSpec {
    pub css: Option<String>,
    pub test_id: Option<String>,
    pub role: Option<String>,
    pub accessible_name: Option<String>,
    pub label: Option<String>,
    pub text: Option<TextMatch>,
    #[serde(default)]
    pub attributes: BTreeMap<String, String>,
    #[serde(default)]
    pub frame_path: Vec<Box<TargetSpec>>,
    #[serde(default)]
    pub shadow_path: Vec<Box<TargetSpec>>,
    pub ordinal: Option<usize>,
    #[serde(default)]
    pub allow_best_match: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum TextMatch {
    Exact(String),
    Contains(String),
    Regex(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum ElementState {
    Attached,
    Detached,
    Visible,
    Hidden,
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum WaitCondition {
    Element {
        target: Box<TargetSpec>,
        state: ElementState,
    },
    Text {
        target: Box<TargetSpec>,
        matcher: TextMatch,
    },
    Value {
        target: Box<TargetSpec>,
        matcher: TextMatch,
    },
    Url {
        matcher: TextMatch,
    },
    Document {
        ready: WaitUntil,
    },
    NetworkQuiet {
        idle_ms: u64,
        max_in_flight: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WaitForCommand {
    pub condition: WaitCondition,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ScreenshotMode {
    Viewport,
    FullPage,
    Element {
        target: Box<TargetSpec>,
    },
    Clip {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaptureScreenshotCommand {
    pub mode: ScreenshotMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetFocusEmulationCommand {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SetEmulatedMediaCommand {
    pub media: String,
    pub features: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EvaluateJavaScriptCommand {
    pub expression: String,
    pub timeout_ms: u64,
    #[serde(default)]
    pub await_promise: bool,
}

/// Per-session gate for privileged execution capabilities. Defaults to all-false
/// (deny-by-default): a session must explicitly opt in to run JavaScript, even if
/// the bearer token holds the `javascript:evaluate` capability.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecutionPolicy {
    #[serde(default)]
    pub javascript_evaluation: bool,
    #[serde(default)]
    pub vision_assist: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionRequest {
    pub profile: String,
    pub proxy: Option<String>,
    #[serde(default)]
    pub execution_policy: ExecutionPolicy,
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
pub struct ExtractRequest {
    pub page_id: PageId,
    pub fields: serde_json::Value,
}
