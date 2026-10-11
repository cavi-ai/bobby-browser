//! Typed engine views borrowing the same worker; page lookup stays in operations.
use crate::{unsupported_error, wait::WaitObserver};
use async_trait::async_trait;
use network_engine::state::{HttpStateSnapshot, ResponseStateDelta};
use types::{
    CaptureScreenshotCommand, ClickAndWaitForDownloadCommand, ClickAndWaitForPopupCommand,
    ClickCommand, ClosePageCommand, CommandError, EvaluateJavaScriptCommand, Evidence,
    InspectCommand, ListPagesCommand, NavigateCommand, OpenPageCommand, PageId,
    SetEmulatedMediaCommand, SetFocusEmulationCommand, TargetSpec, TypeTextCommand,
    UploadFilesCommand,
};

#[async_trait]
pub trait SessionSettings: Send + Sync {
    /// Toggle fingerprint spoofing. Implementations that register preload
    /// scripts should apply/remove them immediately (not only on next page).
    async fn set_fingerprint_enabled(&self, _enabled: bool) -> Result<(), CommandError> {
        Ok(())
    }

    /// Whether fingerprint spoofing is currently enabled.
    fn fingerprint_enabled(&self) -> bool {
        false
    }

    /// Toggle human-like input synthesis (`behavioral-engine`). Engines with no
    /// synthesizer accept the call and stay direct (the default below), so the
    /// executor can write session policy onto any worker.
    async fn set_humanization_enabled(&self, _enabled: bool) -> Result<(), CommandError> {
        Ok(())
    }

    /// Whether human-like input synthesis is currently enabled.
    fn humanization_enabled(&self) -> bool {
        false
    }
}

#[async_trait]
pub trait TabsEngine: Send + Sync {
    async fn open_page(&self, page_id: PageId) -> Result<(), CommandError>;

    async fn open_page_command(
        &self,
        _command: &OpenPageCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn list_pages(&self, _command: &ListPagesCommand) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn activate_page(
        &self,
        _command: &types::ActivatePageCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn close_page_command(
        &self,
        _command: &ClosePageCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
}

#[async_trait]
pub trait NavigationEngine: Send + Sync {
    async fn navigate(
        &self,
        page_id: &PageId,
        command: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError>;

    /// The page's URL and title once its document has stopped changing,
    /// read by the probe `navigate` settles with and bounded by `budget`; at
    /// the budget, the URL and title the page shows then. `requested_url` is
    /// the URL the caller expects, when it has one. `None` when the worker
    /// cannot settle a page or the page cannot be read.
    async fn settle_page(
        &self,
        _page_id: &PageId,
        _budget: std::time::Duration,
        _requested_url: Option<&str>,
    ) -> Option<(String, String)> {
        None
    }
}

#[async_trait]
pub trait ObservationEngine: Send + Sync {
    async fn inspect(
        &self,
        page_id: &PageId,
        command: &InspectCommand,
    ) -> Result<Vec<Evidence>, CommandError>;

    async fn a11y_snapshot(
        &self,
        _page_id: &PageId,
        _command: &types::AccessibilitySnapshotCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn form_snapshot(
        &self,
        _page_id: &PageId,
        _max_controls: Option<u32>,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn collect_candidates(
        &self,
        _page_id: &PageId,
        _target: &TargetSpec,
    ) -> Result<Vec<dom_engine::Candidate>, CommandError> {
        Err(unsupported_error())
    }

    /// A target naming only the element resolved now. Workers whose
    /// candidates carry element identity return the original target.
    async fn pin_target(&self, _page_id: &PageId, target: &TargetSpec) -> TargetSpec {
        target.clone()
    }

    /// The value of `attribute` on the element `target` resolves to; `None`
    /// when the element has no such attribute.
    async fn read_attribute(
        &self,
        _page_id: &PageId,
        _target: &TargetSpec,
        _attribute: &str,
    ) -> Result<Option<String>, CommandError> {
        Err(unsupported_error())
    }

    /// Best-effort accessible identity of the interactive element at viewport
    /// point (x, y): role + name, matching the shape a11y candidates carry.
    /// Used by the vision corpus collector to ground a verified click back to
    /// the candidate list. Runs on the worker's internal DOM channel (the same
    /// path as the targeting bounds probes), never through the policy-gated
    /// `evaluate_javascript` primitive. Default: unsupported — not `Ok(None)`,
    /// which would look like "nothing at this point."
    async fn element_at_point(
        &self,
        _page_id: &PageId,
        _x: f64,
        _y: f64,
    ) -> Result<Option<(String, String)>, CommandError> {
        Err(unsupported_error())
    }
}

#[async_trait]
pub trait InputEngine: Send + Sync {
    async fn click(
        &self,
        page_id: &PageId,
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError>;

    /// Coordinate click used by vision fallback proposals.
    async fn click_xy(
        &self,
        _page_id: &PageId,
        _x: f64,
        _y: f64,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn type_text(
        &self,
        page_id: &PageId,
        command: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError>;

    async fn upload_files(
        &self,
        _page_id: &PageId,
        _command: &UploadFilesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn control_action(
        &self,
        _page_id: &PageId,
        _command: &types::ControlActionCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    /// Verify a typed value inside a private frame without returning its
    /// contents to the runtime. Workers without this capability use Inspect.
    async fn verify_framed_typed_value(
        &self,
        _page_id: &PageId,
        _command: &TypeTextCommand,
        _observed: Option<&str>,
        _kind: &str,
    ) -> Result<Option<Vec<Evidence>>, CommandError> {
        Ok(None)
    }
}

#[async_trait]
pub trait EventsEngine: Send + Sync {
    /// In-memory viewport PNG for machine consumers (vision assist). Unlike
    /// `capture_screenshot`, no artifact is persisted and no evidence emitted.
    async fn network_log(
        &self,
        _page_id: &PageId,
        _command: &types::NetworkLogCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn handle_dialog(
        &self,
        _page_id: &PageId,
        _command: &types::HandleDialogCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn click_and_wait_for_popup(
        &self,
        _page_id: &PageId,
        _command: &ClickAndWaitForPopupCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn click_and_wait_for_download(
        &self,
        _page_id: &PageId,
        _command: &ClickAndWaitForDownloadCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
}

#[async_trait]
pub trait CaptureEngine: Send + Sync {
    async fn screenshot_bytes(&self, _page_id: &PageId) -> Result<Vec<u8>, CommandError> {
        Err(unsupported_error())
    }

    async fn capture_screenshot(
        &self,
        _page_id: &PageId,
        _command: &CaptureScreenshotCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn print_to_pdf(
        &self,
        _page_id: &PageId,
        _command: &types::PrintToPdfCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
}

#[async_trait]
pub trait PageConfigurationEngine: Send + Sync {
    async fn emulate(
        &self,
        _page_id: &PageId,
        _command: &types::EmulateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn set_focus_emulation(
        &self,
        _page_id: &PageId,
        _command: &SetFocusEmulationCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn set_emulated_media(
        &self,
        _page_id: &PageId,
        _command: &SetEmulatedMediaCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
}

#[async_trait]
pub trait JavaScriptEngine: Send + Sync {
    // ChromiumWorker overrides this via chromiumoxide EvaluateParams, bounded by
    // `timeout_ms` and result-shaped through `js_engine::bound_result`. Every other
    // worker keeps this default and refuses JS execution.
    async fn evaluate_javascript(
        &self,
        _page_id: &PageId,
        _command: &EvaluateJavaScriptCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
}

#[async_trait]
pub trait WebStateEngine: Send + Sync {
    async fn get_cookies(
        &self,
        _page_id: &PageId,
        _command: &types::GetCookiesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn set_cookies(
        &self,
        _page_id: &PageId,
        _command: &types::SetCookiesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    async fn delete_cookies(
        &self,
        _page_id: &PageId,
        _command: &types::DeleteCookiesCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }

    /// Whether this worker mirrors page HTTP state, and can therefore serve the
    /// direct-HTTP execution path. Workers that keep the `http_state` default below
    /// must report `false`, so the adaptive executor routes the command through the
    /// browser instead of failing it on an unsupported primitive.
    fn supports_http_state(&self) -> bool {
        false
    }

    async fn http_state(&self, _page_id: &PageId) -> Result<HttpStateSnapshot, CommandError> {
        Err(unsupported_error())
    }

    async fn commit_http_state(
        &self,
        _page_id: &PageId,
        _expected_version: u64,
        _delta: ResponseStateDelta,
    ) -> Result<(), CommandError> {
        Err(unsupported_error())
    }
}

pub trait WaitProvider: Send + Sync {
    fn observer<'a>(&'a self, page_id: &'a PageId) -> Box<dyn WaitObserver + 'a>;
}
/// Missing domains retain the existing unsupported result.
pub fn require_domain<T: ?Sized>(domain: Option<&T>) -> Result<&T, CommandError> {
    domain.ok_or_else(unsupported_error)
}

struct DefaultDomains;
#[async_trait]
impl TabsEngine for DefaultDomains {
    async fn open_page(&self, _: PageId) -> Result<(), CommandError> {
        Err(unsupported_error())
    }
}
#[async_trait]
impl NavigationEngine for DefaultDomains {
    async fn navigate(
        &self,
        _: &PageId,
        _: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
}
#[async_trait]
impl ObservationEngine for DefaultDomains {
    async fn inspect(&self, _: &PageId, _: &InspectCommand) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
}
#[async_trait]
impl InputEngine for DefaultDomains {
    async fn click(&self, _: &PageId, _: &ClickCommand) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
    async fn type_text(
        &self,
        _: &PageId,
        _: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(unsupported_error())
    }
}
#[async_trait]
impl SessionSettings for DefaultDomains {}
/// Preserve the existing defaults when the entire domain is absent.
pub fn session_settings_or_default(domain: Option<&dyn SessionSettings>) -> &dyn SessionSettings {
    domain.unwrap_or(&DefaultDomains)
}
/// Preserve the existing defaults when the entire domain is absent.
pub fn tabs_or_default(domain: Option<&dyn TabsEngine>) -> &dyn TabsEngine {
    domain.unwrap_or(&DefaultDomains)
}
/// Preserve the existing defaults when the entire domain is absent.
pub fn navigation_or_default(domain: Option<&dyn NavigationEngine>) -> &dyn NavigationEngine {
    domain.unwrap_or(&DefaultDomains)
}
/// Preserve the existing defaults when the entire domain is absent.
pub fn observation_or_default(domain: Option<&dyn ObservationEngine>) -> &dyn ObservationEngine {
    domain.unwrap_or(&DefaultDomains)
}
/// Preserve the existing defaults when the entire domain is absent.
pub fn input_or_default(domain: Option<&dyn InputEngine>) -> &dyn InputEngine {
    domain.unwrap_or(&DefaultDomains)
}
#[async_trait]
impl EventsEngine for DefaultDomains {}
/// Preserve the existing defaults when the entire domain is absent.
pub fn events_or_default(domain: Option<&dyn EventsEngine>) -> &dyn EventsEngine {
    domain.unwrap_or(&DefaultDomains)
}
#[async_trait]
impl CaptureEngine for DefaultDomains {}
/// Preserve the existing defaults when the entire domain is absent.
pub fn capture_or_default(domain: Option<&dyn CaptureEngine>) -> &dyn CaptureEngine {
    domain.unwrap_or(&DefaultDomains)
}
#[async_trait]
impl PageConfigurationEngine for DefaultDomains {}
/// Preserve the existing defaults when the entire domain is absent.
pub fn page_configuration_or_default(
    domain: Option<&dyn PageConfigurationEngine>,
) -> &dyn PageConfigurationEngine {
    domain.unwrap_or(&DefaultDomains)
}
#[async_trait]
impl JavaScriptEngine for DefaultDomains {}
/// Preserve the existing defaults when the entire domain is absent.
pub fn javascript_or_default(domain: Option<&dyn JavaScriptEngine>) -> &dyn JavaScriptEngine {
    domain.unwrap_or(&DefaultDomains)
}
#[async_trait]
impl WebStateEngine for DefaultDomains {}
/// Preserve the existing defaults when the entire domain is absent.
pub fn web_state_or_default(domain: Option<&dyn WebStateEngine>) -> &dyn WebStateEngine {
    domain.unwrap_or(&DefaultDomains)
}
