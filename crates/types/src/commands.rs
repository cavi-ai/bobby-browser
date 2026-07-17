use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

use crate::{AttemptId, CommandId, PageId, SessionId, WorkflowId};

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
    pub command: PrimitiveCommand,
}

impl CommandEnvelope {
    pub const SCHEMA_VERSION: u16 = 1;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", content = "input", rename_all = "camelCase")]
pub enum PrimitiveCommand {
    Navigate(NavigateCommand),
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
}

impl PrimitiveCommand {
    pub fn class(&self) -> CommandClass {
        match self {
            Self::Navigate(_)
            | Self::Inspect(_)
            | Self::OpenPage(_)
            | Self::ListPages(_)
            | Self::WaitFor(_)
            | Self::CaptureScreenshot(_) => CommandClass::Replayable,
            Self::TypeText(_) | Self::UploadFiles(_) | Self::ClosePage(_) => {
                CommandClass::Reconciliable
            }
            Self::ClickAndWaitForPopup(_) | Self::ClickAndWaitForDownload(_) => {
                CommandClass::Boundary
            }
            Self::Click(command) if command.boundary => CommandClass::Boundary,
            Self::Click(_) => CommandClass::Reconciliable,
        }
    }
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
    pub include_html: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClickCommand {
    pub selector: String,
    pub boundary: bool,
    pub expected_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TypeTextCommand {
    pub selector: String,
    pub value: String,
    pub clear_first: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UploadFilesCommand {
    pub selector: String,
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
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClickAndWaitForDownloadCommand {
    pub selector: String,
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
pub struct ExtractRequest {
    pub page_id: PageId,
    pub fields: serde_json::Value,
}
