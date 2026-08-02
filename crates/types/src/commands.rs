use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::{
    AttemptId, CommandId, FormControlTarget, PageId, SessionId, WorkflowId, MAX_FORM_REFERENCES,
    MAX_FORM_VALUE_BYTES,
};

pub const MAX_INTENT_PURPOSE_BYTES: usize = 256;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", content = "input", rename_all = "camelCase")]
pub enum IntentCommand {
    Locate(LocateIntent),
    Fill(FillIntent),
    CompleteForm(CompleteFormIntent),
    SubmitAndVerify(SubmitAndVerifyIntent),
    WaitForState(WaitForStateIntent),
    Follow(FollowIntent),
    DismissObstruction(DismissObstructionIntent),
    Extract(ExtractIntent),
}

impl IntentCommand {
    pub fn class(&self) -> CommandClass {
        match self {
            Self::Locate(_) | Self::WaitForState(_) | Self::Extract(_) => CommandClass::Replayable,
            Self::Fill(_) | Self::CompleteForm(_) | Self::DismissObstruction(_) => {
                CommandClass::Reconciliable
            }
            Self::SubmitAndVerify(_) => CommandClass::Boundary,
            Self::Follow(intent) => {
                if intent.boundary {
                    CommandClass::Boundary
                } else {
                    CommandClass::Reconciliable
                }
            }
        }
    }
}

/// Every field is optional, and `#[serde(default)]` is what makes that true on
/// the wire. Without it a caller sending `{"role":"textbox"}` — exactly what
/// the published schema says is valid, since no hint is required — fails
/// deserialization on the missing collection and boolean fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", default)]
pub struct IntentHints {
    pub role: Option<String>,
    pub near_text: Option<TextMatch>,
    pub ordinal: Option<usize>,
    pub frame_path: Vec<TargetSpec>,
    pub shadow_path: Vec<TargetSpec>,
    pub allow_best_match: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct LocateIntent {
    pub purpose: String,
    #[serde(default)]
    pub hints: IntentHints,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct FillIntent {
    pub purpose: String,
    #[serde(default)]
    pub hints: IntentHints,
    pub value: FillValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct CompleteFormField {
    pub name: String,
    pub purpose: String,
    #[serde(default)]
    pub hints: IntentHints,
    pub value: FillValue,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct CompleteFormIntent {
    pub purpose: String,
    pub fields: Vec<CompleteFormField>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum FillValue {
    Text {
        text: String,
        #[serde(default)]
        #[serde(rename = "clearFirst")]
        clear_first: bool,
    },
    Select {
        option: String,
    },
    Checked {
        checked: bool,
    },
    Files {
        paths: Vec<String>,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct SubmitAndVerifyIntent {
    pub purpose: String,
    #[serde(default)]
    pub hints: IntentHints,
    pub expected_state: WaitForCommand,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct WaitForStateIntent {
    pub condition: WaitCondition,
    pub timeout_ms: u64,
}

/// Activate a described link/control and verify the resulting destination.
///
/// `boundary` mirrors `ClickCommand.boundary`: the caller judges whether the
/// activated control may perform a mutating/side-effecting action (e.g. "Sign
/// out") and therefore needs the pre-established checkpoint gate, or is
/// ordinary navigation that can run without one.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct FollowIntent {
    pub purpose: String,
    #[serde(default)]
    pub hints: IntentHints,
    pub expected_destination: WaitForCommand,
    #[serde(default)]
    pub boundary: bool,
}

pub const DEFAULT_DISMISS_OBSTRUCTION_TIMEOUT_MS: u64 = 5_000;

fn default_dismiss_timeout_ms() -> u64 {
    DEFAULT_DISMISS_OBSTRUCTION_TIMEOUT_MS
}

/// Activate a described dismiss/close control to clear an obstruction (popup,
/// overlay, cookie banner) and verify the target becomes detached or hidden.
///
/// There is no caller-supplied boundary flag: dismissing an obstruction is
/// never treated as a mutating action, so this intent is always
/// `CommandClass::Reconciliable`. Verification is built in — the engine
/// re-resolves the same target after acting and requires it to be gone
/// (removed from the DOM or no longer visible), not a caller-supplied
/// expected post-dismiss state.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct DismissObstructionIntent {
    pub purpose: String,
    #[serde(default)]
    pub hints: IntentHints,
    #[serde(default = "default_dismiss_timeout_ms")]
    pub timeout_ms: u64,
}

/// What to read off a resolved field's candidate. `Href` is a named
/// convenience over the common "attribute=href" case; anything else goes
/// through `Attribute` directly.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ExtractValueKind {
    Text,
    Attribute { attribute: String },
    Href,
}

/// One named field to extract, resolved independently of every other field
/// in the same `ExtractIntent` — effectively a per-field `LocateIntent` that
/// also names what to read off the result.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ExtractField {
    pub name: String,
    pub purpose: String,
    #[serde(default)]
    pub hints: IntentHints,
    pub value: ExtractValueKind,
}

/// Schema-bounded structured extraction: resolve every named field and read
/// its value. Never mutates the page, so this intent is always
/// `CommandClass::Replayable`.
///
/// Resolution is best-effort per field, not all-or-nothing: a field that
/// cannot be resolved (deterministically, or via vision when permitted) is
/// reported missing in that field's evidence rather than failing the whole
/// command. Callers get whatever the page currently offers instead of a
/// result blocked on the least-available field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ExtractIntent {
    pub purpose: String,
    pub fields: Vec<ExtractField>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
    ActivatePage(ActivatePageCommand),
    AccessibilitySnapshot(AccessibilitySnapshotCommand),
    ExtractStructured(ExtractStructuredCommand),
    GetCookies(GetCookiesCommand),
    PrintToPdf(PrintToPdfCommand),
    HandleDialog(HandleDialogCommand),
    Emulate(EmulateCommand),
    NetworkLog(NetworkLogCommand),
    SetCookies(SetCookiesCommand),
    DeleteCookies(DeleteCookiesCommand),
    ClickAndWaitForPopup(ClickAndWaitForPopupCommand),
    ClickAndWaitForDownload(ClickAndWaitForDownloadCommand),
    WaitFor(WaitForCommand),
    CaptureScreenshot(CaptureScreenshotCommand),
    ControlAction(ControlActionCommand),
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
            Self::UploadFiles(command) => {
                for (index, path) in command.paths.iter_mut().enumerate() {
                    *path = format!("upload://input/{index}");
                }
            }
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
            Self::ControlAction(command) => {
                if let ControlAction::SetFiles { paths } = &mut command.action {
                    for (index, path) in paths.iter_mut().enumerate() {
                        *path = format!("upload://input/{index}");
                    }
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
            | Self::ActivatePage(_)
            | Self::Emulate(_)
            | Self::NetworkLog(_)
            | Self::AccessibilitySnapshot(_)
            | Self::ExtractStructured(_)
            | Self::GetCookies(_)
            | Self::PrintToPdf(_)
            | Self::CaptureScreenshot(_) => CommandClass::Replayable,
            Self::SetCookies(_) | Self::DeleteCookies(_) | Self::HandleDialog(_) => {
                CommandClass::Reconciliable
            }
            Self::DownloadUrl(_)
            | Self::TypeText(_)
            | Self::UploadFiles(_)
            | Self::ClosePage(_)
            | Self::EvaluateJavaScript(_) => CommandClass::Reconciliable,
            Self::ControlAction(_) => CommandClass::Reconciliable,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ControlActionCommand {
    pub target: FormControlTarget,
    pub action: ControlAction,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "camelCase", deny_unknown_fields)]
pub enum ControlAction {
    SetText { value: String },
    SetChecked { checked: bool },
    SelectOne { value: String },
    SelectMany { values: Vec<String> },
    SetFiles { paths: Vec<String> },
    Clear,
    Activate,
}

impl ControlAction {
    pub fn validate(&self) -> Result<(), String> {
        fn bounded(value: &str, field: &str) -> Result<(), String> {
            if value.len() > MAX_FORM_VALUE_BYTES {
                return Err(format!("{field} exceeds {MAX_FORM_VALUE_BYTES} bytes"));
            }
            Ok(())
        }

        match self {
            Self::SetText { value } | Self::SelectOne { value } => bounded(value, "value"),
            Self::SelectMany { values } => {
                if values.is_empty() || values.len() > MAX_FORM_REFERENCES {
                    return Err(format!(
                        "values must contain between 1 and {MAX_FORM_REFERENCES} items"
                    ));
                }
                let mut unique = BTreeSet::new();
                for value in values {
                    bounded(value, "selection value")?;
                    if !unique.insert(value) {
                        return Err("selection values must be unique".into());
                    }
                }
                Ok(())
            }
            Self::SetFiles { paths } => {
                if paths.is_empty() || paths.len() > MAX_FORM_REFERENCES {
                    return Err(format!(
                        "paths must contain between 1 and {MAX_FORM_REFERENCES} items"
                    ));
                }
                for path in paths {
                    bounded(path, "file path")?;
                }
                Ok(())
            }
            Self::SetChecked { .. } | Self::Clear | Self::Activate => Ok(()),
        }
    }

    pub fn operation(&self) -> crate::FormControlOperation {
        match self {
            Self::SetText { .. } => crate::FormControlOperation::SetText,
            Self::SetChecked { .. } => crate::FormControlOperation::SetChecked,
            Self::SelectOne { .. } => crate::FormControlOperation::SelectOne,
            Self::SelectMany { .. } => crate::FormControlOperation::SelectMany,
            Self::SetFiles { .. } => crate::FormControlOperation::SetFiles,
            Self::Clear => crate::FormControlOperation::Clear,
            Self::Activate => crate::FormControlOperation::Activate,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct DownloadUrlCommand {
    pub url: String,
    pub expected_content_type: Option<String>,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum CommandClass {
    Replayable,
    Reconciliable,
    Boundary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct NavigateCommand {
    pub url: String,
    pub wait_until: WaitUntil,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum WaitUntil {
    Commit,
    DomContentLoaded,
    Interactive,
    NetworkIdle,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct InspectCommand {
    pub selector: Option<String>,
    pub target: Option<TargetSpec>,
    pub include_html: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ClickCommand {
    pub selector: String,
    pub target: Option<TargetSpec>,
    pub boundary: bool,
    pub expected_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct TypeTextCommand {
    pub selector: String,
    pub target: Option<TargetSpec>,
    pub value: String,
    pub clear_first: bool,
    /// Optional page-identity confirmation: when set, the command fails before
    /// typing unless the page's current URL matches. This is the type-side
    /// counterpart of `ClickCommand.expected_url` — agents that navigate away
    /// mid-flow (or address the wrong session) fail instead of typing into
    /// the wrong page.
    #[serde(default)]
    pub expected_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct UploadFilesCommand {
    pub selector: String,
    pub target: Option<TargetSpec>,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct OpenPageCommand {
    pub url: Option<String>,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ListPagesCommand;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ClosePageCommand {
    pub page_id: PageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ActivatePageCommand {
    pub page_id: PageId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct AccessibilitySnapshotCommand {
    pub max_nodes: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ExtractStructuredCommand {
    pub schema: serde_json::Value,
    pub purpose: Option<String>,
}

/// One cookie as returned by a cookie read on any engine.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CookieRecord {
    pub name: String,
    pub value: String,
    pub domain: String,
    pub path: String,
    pub secure: bool,
    pub http_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub same_site: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_unix: Option<f64>,
}

/// One cookie to store. `url` anchors the cookie's origin; `path`, flags, and
/// expiry are optional per the driver defaults.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetCookieParam {
    pub name: String,
    pub value: String,
    pub url: String,
    #[serde(default)]
    pub path: Option<String>,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub http_only: bool,
    #[serde(default)]
    pub same_site: Option<String>,
    #[serde(default)]
    pub expires_unix: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct NetworkLogCommand {
    /// Clear the recorded log after producing the artifact (default true).
    #[serde(default = "default_true")]
    pub clear: bool,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ViewportSize {
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GeolocationCoordinates {
    pub latitude: f64,
    pub longitude: f64,
    #[serde(default)]
    pub accuracy: Option<f64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct EmulateCommand {
    #[serde(default)]
    pub viewport: Option<ViewportSize>,
    #[serde(default)]
    pub geolocation: Option<GeolocationCoordinates>,
    /// Mobile device-metrics flag (Chromium); harmless elsewhere.
    #[serde(default)]
    pub mobile: Option<bool>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum DialogAction {
    Accept,
    Dismiss,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct HandleDialogCommand {
    pub action: DialogAction,
    #[serde(default)]
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PrintToPdfCommand {
    #[serde(default)]
    pub landscape: bool,
    #[serde(default = "default_print_background")]
    pub print_background: bool,
    #[serde(default)]
    pub scale: Option<f64>,
    #[serde(default)]
    pub page_ranges: Option<String>,
}

fn default_print_background() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GetCookiesCommand {
    /// Restrict to these origin URLs; empty returns all cookies for the page's jar.
    #[serde(default)]
    pub urls: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct SetCookiesCommand {
    pub cookies: Vec<SetCookieParam>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct DeleteCookiesCommand {
    /// Restrict deletion to these origin URLs; empty means every origin.
    #[serde(default)]
    pub urls: Vec<String>,
    /// Restrict deletion to these cookie names; empty means every cookie.
    #[serde(default)]
    pub names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ClickAndWaitForPopupCommand {
    pub selector: String,
    pub target: Option<TargetSpec>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ClickAndWaitForDownloadCommand {
    pub selector: String,
    pub target: Option<TargetSpec>,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", content = "value", rename_all = "camelCase")]
pub enum TextMatch {
    Exact(String),
    Contains(String),
    Regex(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum ElementState {
    Attached,
    Detached,
    Visible,
    Hidden,
    Enabled,
    Disabled,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "PascalCase")]
pub enum NetworkResourceType {
    Document,
    Stylesheet,
    Image,
    Media,
    Font,
    Script,
    TextTrack,
    #[serde(rename = "XHR", alias = "Xhr")]
    Xhr,
    Fetch,
    Prefetch,
    EventSource,
    WebSocket,
    Manifest,
    SignedExchange,
    Ping,
    #[serde(rename = "CSPViolationReport")]
    CspViolationReport,
    Preflight,
    #[serde(rename = "FedCM")]
    FedCm,
    Other,
}

impl NetworkResourceType {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Document => "Document",
            Self::Stylesheet => "Stylesheet",
            Self::Image => "Image",
            Self::Media => "Media",
            Self::Font => "Font",
            Self::Script => "Script",
            Self::TextTrack => "TextTrack",
            Self::Xhr => "XHR",
            Self::Fetch => "Fetch",
            Self::Prefetch => "Prefetch",
            Self::EventSource => "EventSource",
            Self::WebSocket => "WebSocket",
            Self::Manifest => "Manifest",
            Self::SignedExchange => "SignedExchange",
            Self::Ping => "Ping",
            Self::CspViolationReport => "CSPViolationReport",
            Self::Preflight => "Preflight",
            Self::FedCm => "FedCM",
            Self::Other => "Other",
        }
    }
}

impl std::str::FromStr for NetworkResourceType {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "Document" | "document" => Ok(Self::Document),
            "Stylesheet" | "stylesheet" => Ok(Self::Stylesheet),
            "Image" | "image" => Ok(Self::Image),
            "Media" | "media" => Ok(Self::Media),
            "Font" | "font" => Ok(Self::Font),
            "Script" | "script" => Ok(Self::Script),
            "TextTrack" | "textTrack" | "texttrack" => Ok(Self::TextTrack),
            "XHR" | "Xhr" | "xhr" => Ok(Self::Xhr),
            "Fetch" | "fetch" => Ok(Self::Fetch),
            "Prefetch" | "prefetch" => Ok(Self::Prefetch),
            "EventSource" | "eventSource" | "eventsource" => Ok(Self::EventSource),
            "WebSocket" | "webSocket" | "websocket" => Ok(Self::WebSocket),
            "Manifest" | "manifest" => Ok(Self::Manifest),
            "SignedExchange" | "signedExchange" => Ok(Self::SignedExchange),
            "Ping" | "ping" => Ok(Self::Ping),
            "CSPViolationReport" | "cspViolationReport" => Ok(Self::CspViolationReport),
            "Preflight" | "preflight" => Ok(Self::Preflight),
            "FedCM" | "FedCm" | "fedCM" => Ok(Self::FedCm),
            "Other" | "other" => Ok(Self::Other),
            other => Err(format!("unknown network resource type: {other}")),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
        #[serde(rename = "idleMs", alias = "idle_ms")]
        idle_ms: u64,
        #[serde(rename = "maxInFlight", alias = "max_in_flight")]
        max_in_flight: usize,
        #[serde(
            default,
            rename = "ignoreUrlSubstrings",
            alias = "ignore_url_substrings",
            skip_serializing_if = "Vec::is_empty"
        )]
        ignore_url_substrings: Vec<String>,
        #[serde(
            default,
            rename = "ignoreResourceTypes",
            alias = "ignore_resource_types",
            skip_serializing_if = "Vec::is_empty"
        )]
        ignore_resource_types: Vec<NetworkResourceType>,
        #[serde(
            default,
            rename = "ignoreLongLived",
            alias = "ignore_long_lived",
            skip_serializing_if = "std::ops::Not::not"
        )]
        ignore_long_lived: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct WaitForCommand {
    pub condition: WaitCondition,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct CaptureScreenshotCommand {
    pub mode: ScreenshotMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct SetFocusEmulationCommand {
    pub enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct SetEmulatedMediaCommand {
    pub media: String,
    pub features: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
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
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ExecutionPolicy {
    #[serde(default)]
    pub javascript_evaluation: bool,
    #[serde(default)]
    pub vision_assist: bool,
    /// Whether workers leased for this session apply fingerprint spoofing.
    ///
    /// Fingerprinting was a process-wide knob on the worker factory
    /// (`ChromiumWorkerFactory::with_fingerprint`), which meant one operator
    /// decision applied to every principal's sessions and no caller could see
    /// or choose it. It belongs here for the same reason
    /// `javascript_evaluation` does: it changes what the browser presents to
    /// the page, so it is a per-session decision the caller makes explicitly
    /// and the runtime records.
    #[serde(default)]
    pub fingerprint: bool,
    /// Whether workers leased for this session synthesize human-like input
    /// timing (`behavioral-engine`) instead of driving the browser directly.
    ///
    /// Off by default because it changes observable timing on the intent
    /// execution path, and intent verification compares what it asked for
    /// against what the browser did. When on, the synthesized timing is
    /// carried in `Evidence::Humanization` so the two agree.
    #[serde(default)]
    pub humanize: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionRequest {
    pub profile: String,
    pub proxy: Option<String>,
    #[serde(default)]
    pub execution_policy: ExecutionPolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct OpenPageRequest {
    pub session_id: SessionId,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct NavigationRequest {
    pub page_id: PageId,
    pub url: String,
    pub wait_until: Option<String>,
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
pub struct ExtractRequest {
    pub page_id: PageId,
    pub fields: serde_json::Value,
}
