//! Command envelopes, primitives, intents, and session create/open requests.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

use crate::{
    AttemptId, CommandId, FormControlTarget, PageId, SessionId, WorkflowId, MAX_FORM_REFERENCES,
    MAX_FORM_VALUE_BYTES,
};

/// Maximum UTF-8 byte length for intent `purpose` strings.
pub const MAX_INTENT_PURPOSE_BYTES: usize = 256;

/// Envelope submitted to `POST /v1/commands`.
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

    /// Returns a copy safe for durable journals (URLs sanitized).
    /// Keep the live envelope in memory for execution.
    pub fn journal_safe(&self) -> Self {
        let mut safe = self.clone();
        safe.command.sanitize_urls();
        safe
    }
}

/// Nested command wire shape: `{ kind: "intent" | "primitive", input: … }`.
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
    /// Vision-primary challenge solving (captchas, verification widgets).
    /// Bypasses DOM resolution: the engine loops screenshot → vision proposal
    /// → action until the model reports the challenge solved or the hint
    /// timeout elapses.
    SolveChallenge(crate::challenges::SolveChallengeIntent),
}

impl IntentCommand {
    pub fn class(&self) -> CommandClass {
        match self {
            Self::Locate(_) | Self::WaitForState(_) | Self::Extract(_) => CommandClass::Replayable,
            Self::Fill(_) | Self::CompleteForm(_) | Self::DismissObstruction(_) => {
                CommandClass::Reconciliable
            }
            Self::SolveChallenge(_) => CommandClass::Reconciliable,
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

/// Optional targeting hints for intents. All fields are optional on the wire
/// (`#[serde(default)]`); a body like `{"role":"textbox"}` must deserialize.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase", default)]
pub struct IntentHints {
    pub role: Option<String>,
    pub near_text: Option<TextMatch>,
    /// Accessible name of the control, matched exactly. Accepts an
    /// `a11y_snapshot` node's `target` verbatim, which carries
    /// `{role, accessibleName, ordinal}` -- before this existed the field was
    /// dropped silently and the intent resolved on role and ordinal alone.
    /// Equivalent to `near_text: Exact`; setting both to different values is
    /// refused rather than resolved.
    pub accessible_name: Option<String>,
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

/// Follow a control and wait for the expected destination.
///
/// Set `boundary` when activation may mutate state or trigger a side effect
/// (same meaning as [`ClickCommand::boundary`]).
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

/// Dismiss an obstruction (overlay, cookie banner, etc.).
///
/// Always reconciliable. Verification is built in: after acting, the same
/// target must be detached or hidden.
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

/// Value to extract from a resolved field. `Href` is shorthand for
/// `attribute = "href"`; other attributes use [`ExtractValueKind::Attribute`].
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ExtractValueKind {
    Text,
    Attribute { attribute: String },
    Href,
}

/// One named field within an [`ExtractIntent`], resolved independently of siblings.
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

/// Structured extraction intent. Replayable (does not mutate the page).
///
/// Fields resolve independently: a missing field is reported in that field's
/// evidence rather than failing the whole command.
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
            Self::DownloadUrl(command) => {
                sanitize(&mut command.url);
                if let Some(save_as) = &mut command.save_as {
                    *save_as = "[redacted-download-path]".into();
                }
            }
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
    pub save_as: Option<String>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub enum ClickModifier {
    Shift,
    Ctrl,
    Alt,
    Meta,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct ClickCommand {
    pub selector: String,
    pub target: Option<TargetSpec>,
    pub boundary: bool,
    pub expected_url: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub modifiers: Vec<ClickModifier>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct TypeTextCommand {
    pub selector: String,
    pub target: Option<TargetSpec>,
    pub value: String,
    pub clear_first: bool,
    /// When set, fail before typing unless the page URL matches
    /// (same role as [`ClickCommand::expected_url`]).
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
    /// Scope the tree to the subtree rooted at this target (e.g. the form or
    /// dialog being worked on) instead of paying for the whole page on every
    /// re-read. Accepts the same shape as `wait_for` targets.
    #[serde(default)]
    pub target: Option<TargetSpec>,
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

/// Per-session gate for privileged execution. Defaults to deny; a session must
/// opt in even when the bearer token holds the matching capability.
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
    /// Per session, never process-wide: it changes what the browser presents to
    /// the page, so the caller chooses it and the runtime records the choice.
    #[serde(default)]
    pub fingerprint: bool,
    /// Whether workers leased for this session synthesize human-like input
    /// timing (`behavioral-engine`) instead of driving the browser directly.
    ///
    /// Off by default: it changes observable timing on the intent execution
    /// path. When on, the synthesized timing is carried in
    /// `Evidence::Humanization` so intent verification still agrees.
    #[serde(default)]
    pub humanize: bool,
    /// Name of the registered vision node this session escalates to.
    ///
    /// `None` means no escalation, and naming an unconfigured node declines, so
    /// a session is never redirected to a provider it did not choose.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vision_node: Option<String>,
}

/// `POST /v1/sessions` body.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "schema", derive(schemars::JsonSchema))]
#[serde(rename_all = "camelCase")]
pub struct CreateSessionRequest {
    pub profile: String,
    pub proxy: Option<String>,
    #[serde(default)]
    pub execution_policy: ExecutionPolicy,
}

/// `POST /v1/pages` body.
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
