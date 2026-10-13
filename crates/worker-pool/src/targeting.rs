use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::pointer_hit::{parse_pointer_hit, pointer_hit_function, PointerHit};
use chromiumoxide::browser::BrowserHandle;
use chromiumoxide::cdp::browser_protocol::accessibility::QueryAxTreeParams;
#[cfg(test)]
use chromiumoxide::cdp::browser_protocol::dom::Node as CdpNode;
use chromiumoxide::cdp::browser_protocol::dom::{
    BackendNodeId, DescribeNodeParams, GetBoxModelParams, GetContentQuadsParams, GetDocumentParams,
    ResolveNodeParams, SetFileInputFilesParams, ShadowRootType,
};
use chromiumoxide::cdp::browser_protocol::input::InsertTextParams;
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, GetFrameTreeParams, Viewport,
};
use chromiumoxide::cdp::js_protocol::runtime::{
    CallFunctionOnParams, EvaluateParams, ExecutionContextId, ReleaseObjectGroupParams,
    RemoteObjectId,
};
use chromiumoxide::keys::get_key_definition;
use chromiumoxide::layout::{ElementQuad, Point};
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::{Command, Element, Method, Page};
use dom_engine::{
    accessibility_alignment_unnecessary, apply_accessibility_alignment, rank_candidates,
    resolve_candidates, Candidate, CandidateState, ResolutionDecision, ResolutionPolicy,
};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use types::{CommandError, ErrorCode, ErrorLayer, Evidence, PageId, TargetFingerprint, TargetSpec};

static TARGET_SCOPE: AtomicU64 = AtomicU64::new(1);
pub(crate) const MAX_TARGET_PATH_DEPTH: usize = 8;

pub struct ResolvedTarget {
    native: Option<Element>,
    locator: JsLocator,
    execution_page: Option<Page>,
    /// The frame the resolved element lives in; `None` for the main frame.
    /// The a11y snapshot's scoped mode fetches that frame's AX tree.
    frame_id: Option<chromiumoxide::cdp::browser_protocol::page::FrameId>,
    /// The selected iframe's semantic hop, ranked within its parent scope.
    pub(crate) iframe_hop: Option<types::SemanticTargetSegment>,
    /// Resolved ancestor hops, ranked within each parent frame.
    pub(crate) frame_path: Vec<types::SemanticTargetSegment>,
    /// The iframe element and its parent's session for every hop into a frame
    /// from another site: that frame reports geometry in its own viewport.
    frame_owners: Vec<(Page, BackendNodeId)>,
    pub evidence: Evidence,
}

/// Where a [`JsLocator`]'s `find(root, id)` lookups are rooted.
///
/// `ClosedRoot` anchors execution at a closed shadow root `Element` handle
/// (see [`discover_closed_shadow_roots`]) rather than a document/frame context:
/// `document.querySelector` cannot see into a closed tree, a handle already
/// inside one can.
#[derive(Clone)]
pub(crate) enum LocatorScope {
    Context(Option<ExecutionContextId>),
    ClosedRoot(Arc<Element>),
}

#[derive(Clone)]
struct JsLocator {
    scope: LocatorScope,
    shadow_hosts: Vec<String>,
    id: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct BrowserCandidate {
    id: String,
    css: Option<String>,
    tag: Option<String>,
    test_id: Option<String>,
    role: Option<String>,
    name: Option<String>,
    label: Option<String>,
    text: String,
    attributes: BTreeMap<String, String>,
    attached: bool,
    visible: bool,
    enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FormControlValidity {
    pub valid: bool,
    pub validation_message: String,
}

impl ResolvedTarget {
    /// The session serving the target's frame: `fallback` for the page's own
    /// frames, the frame's own target for a frame from another site.
    pub(crate) fn execution_page<'a>(&'a self, fallback: &'a Page) -> &'a Page {
        self.execution_page.as_ref().unwrap_or(fallback)
    }

    /// The frame holding the resolved element, when it is not the main frame.
    pub(crate) fn frame_id(&self) -> Option<&chromiumoxide::cdp::browser_protocol::page::FrameId> {
        self.frame_id.as_ref()
    }

    /// The element's backend DOM node id, for correlating with AX tree nodes
    /// (`AxNode.backend_dom_node_id`).
    pub(crate) async fn backend_node_id(
        &self,
        page: &Page,
    ) -> Result<chromiumoxide::cdp::browser_protocol::dom::BackendNodeId, CommandError> {
        if let Some(element) = &self.native {
            return Ok(element.backend_node_id);
        }
        let expression = locator_expression(&self.locator, "return el")?;
        let object_id =
            resolve_object_id_scoped(self.execution_page(page), &self.locator.scope, expression)
                .await?;
        Ok(self
            .execution_page(page)
            .execute(DescribeNodeParams::builder().object_id(object_id).build())
            .await
            .map_err(cdp_error)?
            .result
            .node
            .backend_node_id)
    }

    /// The center of the target's border box, for CDP mouse input. `None`
    /// when the element reports no usable rect. Goes through the backend
    /// node id and `DOM.getContentQuads` rather than a JS
    /// `getBoundingClientRect()` eval: the latter is frame-local, so for a
    /// `frame_path`/`shadow_path` target (never `native`, see
    /// `resolve_target_with_visibility`) it returns a point relative to the
    /// wrong origin once the frame or shadow host is offset within its
    /// parent. `DOM.getContentQuads` on a backend node id -- what
    /// `Element::clickable_point` uses for a `native` target -- reports
    /// coordinates in the top-level page's viewport regardless of nesting.
    pub async fn clickable_point(&self, page: &Page) -> Result<Option<Point>, CommandError> {
        if let Some(element) = &self.native {
            return element.clickable_point().await.map(Some).map_err(cdp_error);
        }
        let backend_node_id = self.backend_node_id(page).await?;
        let quads = self
            .execution_page(page)
            .execute(
                GetContentQuadsParams::builder()
                    .backend_node_id(backend_node_id)
                    .build(),
            )
            .await
            .map_err(cdp_error)?;
        let Some(mut point) = quads
            .quads
            .iter()
            .filter(|quad| quad.inner().len() == 8)
            .map(ElementQuad::from_quad)
            .filter(|quad| quad.quad_area() > 1.)
            .map(|quad| quad.quad_center())
            .next()
        else {
            return Ok(None);
        };
        // Each frame from another site reports from its own viewport origin:
        // its iframe's content box places that origin in the parent.
        for (session, owner) in &self.frame_owners {
            let model = session
                .execute(GetBoxModelParams::builder().backend_node_id(*owner).build())
                .await
                .map_err(cdp_error)?
                .result
                .model;
            let [x, y, ..] = model.content.inner().as_slice() else {
                return Ok(None);
            };
            point.x += x;
            point.y += y;
        }
        Ok(Some(point))
    }

    /// Classifies a pointer at the target's center: reachable, covered,
    /// outside the viewport, or detached.
    pub async fn receives_pointer(&self, page: &Page) -> Result<PointerHit, CommandError> {
        let verdict: String = self
            .eval(page, &format!("return ({})(el);", pointer_hit_function()))
            .await?;
        parse_pointer_hit(&verdict).ok_or_else(|| {
            target_error(
                ErrorCode::BrowserCommandFailed,
                "pointer hit check returned an invalid result",
            )
        })
    }

    /// Scrolls the target to the viewport center and reports whether its
    /// center is covered while an open dialog it is not part of is shown.
    pub async fn covered_by_dialog(&self, page: &Page) -> Result<bool, CommandError> {
        let script = format!(
            "el.scrollIntoView({{block:'center',inline:'center',behavior:'instant'}});\
             if(({hit})(el)!=='obscured')return false;\
             const holds=dialog=>{{for(let node=el;node;node=node.parentNode||node.host)if(node===dialog)return true;return false;}};\
             return [...document.querySelectorAll('dialog[open],[role=dialog],[role=alertdialog]')].some(dialog=>dialog.checkVisibility()&&!holds(dialog));",
            hit = pointer_hit_function()
        );
        self.eval(page, &script).await
    }

    /// Scrolls the target into view. A coordinate-based click dispatch
    /// (`clickable_point` followed by `Input.dispatchMouseEvent`) needs the
    /// target on-screen first -- unlike a native `Element::click()`, which
    /// scrolls internally, or a JS `el.click()`, which needs no coordinate
    /// at all. Best-effort at the call site: this only sharpens
    /// `clickable_point`'s result, so a failure here is logged, not fatal.
    pub async fn scroll_into_view(&self, page: &Page) -> Result<(), CommandError> {
        if let Some(element) = &self.native {
            element.scroll_into_view().await.map_err(cdp_error)?;
            return Ok(());
        }
        self.eval::<bool>(
            page,
            "el.scrollIntoView({block:'center',inline:'center',behavior:'instant'}); return true",
        )
        .await?;
        Ok(())
    }

    pub async fn click(&self, page: &Page) -> Result<(), CommandError> {
        if let Some(element) = &self.native {
            element.click().await.map_err(cdp_error)?;
            return Ok(());
        }
        self.eval::<bool>(page, "el.click(); return true").await?;
        Ok(())
    }

    pub async fn click_js(&self, page: &Page) -> Result<(), CommandError> {
        if let Some(element) = &self.native {
            element
                .call_js_fn("function() { this.click(); }", false)
                .await
                .map_err(cdp_error)?;
            return Ok(());
        }
        self.eval::<bool>(page, "el.click(); return true").await?;
        Ok(())
    }

    pub async fn inner_text(&self, page: &Page) -> Result<Option<String>, CommandError> {
        if let Some(element) = &self.native {
            return element.inner_text().await.map_err(cdp_error);
        }
        self.eval(page, "return el.innerText || ''").await.map(Some)
    }

    /// The control's value; empty for an element without one, such as an
    /// editable region.
    pub async fn value(&self, page: &Page) -> Result<Option<String>, CommandError> {
        self.eval(page, "return el.value || ''").await.map(Some)
    }

    pub async fn attribute(&self, page: &Page, name: &str) -> Result<Option<String>, CommandError> {
        let name = serde_json::to_string(name)
            .map_err(|error| target_error(ErrorCode::InvalidRequest, error))?;
        self.eval(page, &format!("return el.getAttribute({name})"))
            .await
    }

    pub async fn outer_html(&self, page: &Page) -> Result<Option<String>, CommandError> {
        if let Some(element) = &self.native {
            return element.outer_html().await.map_err(cdp_error);
        }
        self.eval(page, "return el.outerHTML || ''").await.map(Some)
    }

    pub async fn visible(&self, page: &Page) -> Result<bool, CommandError> {
        self.eval(page, crate::wait::ELEMENT_VISIBILITY_SCRIPT)
            .await
    }

    pub async fn enabled(&self, page: &Page) -> Result<bool, CommandError> {
        self.eval(page, "return !el.disabled").await
    }

    pub async fn form_control_validity(
        &self,
        page: &Page,
    ) -> Result<FormControlValidity, CommandError> {
        self.eval(
            page,
            "const validates=typeof el.willValidate==='boolean'&&el.willValidate;const message=validates&&!el.validity.valid?el.validationMessage:'';return {valid:!validates||el.validity.valid,validationMessage:message.slice(0,1024)}",
        )
        .await
    }

    /// Presses Enter on the focused control.
    pub async fn press_enter(&self, page: &Page) -> Result<(), CommandError> {
        self.execution_page(page)
            .press_key("Enter")
            .await
            .map_err(cdp_error)?;
        Ok(())
    }

    /// The element read as one form control (JSON text), or `None` when it is
    /// detached or not a form control.
    pub async fn form_control(&self, page: &Page) -> Result<Option<String>, CommandError> {
        self.eval(page, &crate::form_control_script()).await
    }

    /// Empties an editable region with select-all and Backspace. Rich-text
    /// editors that own their DOM revert direct edits but accept these keys.
    pub async fn clear_editable(&self, page: &Page) -> Result<(), CommandError> {
        self.eval::<bool>(page, "el.focus(); return true").await?;
        self.clear_focused(page).await
    }

    /// Select-all and Backspace on the focused control.
    async fn clear_focused(&self, page: &Page) -> Result<(), CommandError> {
        use chromiumoxide::cdp::browser_protocol::input::{
            DispatchKeyEventParams, DispatchKeyEventType,
        };
        let page = self.execution_page(page);
        for event in select_all_events() {
            page.execute(event).await.map_err(cdp_error)?;
        }
        // Editors learn the new selection from `selectionchange` a task or more
        // later; Backspace before that deletes from the old selection.
        page.evaluate(crate::typed_focus::PAGE_IDLE_SCRIPT)
            .await
            .map_err(cdp_error)?;
        for event_type in [
            DispatchKeyEventType::RawKeyDown,
            DispatchKeyEventType::KeyUp,
        ] {
            let event = DispatchKeyEventParams::builder()
                .r#type(event_type)
                .key("Backspace")
                .code("Backspace")
                .windows_virtual_key_code(8)
                .build()
                .map_err(|error| target_error(ErrorCode::BrowserCommandFailed, error))?;
            page.execute(event).await.map_err(cdp_error)?;
        }
        Ok(())
    }

    /// The text of the focused control in the target's document: what typing
    /// reached once focus moved off the target to a replacement control.
    pub async fn focused_text(&self, page: &Page) -> Result<String, CommandError> {
        let script = crate::typed_focus::FOCUSED_TEXT_SCRIPT;
        if let Some(element) = &self.native {
            let value = element
                .call_js_fn_by_value(
                    format!("function() {{ const doc=this.ownerDocument; {script} }}"),
                    false,
                )
                .await
                .map_err(cdp_error)?;
            return serde_json::from_value(value)
                .map_err(|error| target_error(ErrorCode::BrowserCommandFailed, error));
        }
        let expression = scoped_expression(
            &self.locator.scope,
            &self.locator.shadow_hosts,
            &format!("const doc=root.ownerDocument||root;{script}"),
        )?;
        eval_scoped(self.execution_page(page), &self.locator.scope, expression).await
    }

    /// Waits for focus to settle after the target was focused; true when the
    /// page swapped in another editable control that took focus. Keys sent
    /// before focus settles land in the field being removed.
    pub async fn settle_focus(&self, page: &Page) -> Result<bool, CommandError> {
        let script = crate::typed_focus::FOCUS_SETTLE_SCRIPT;
        if let Some(element) = &self.native {
            let value = element
                .call_js_fn_by_value(
                    format!(
                        "function() {{ const el=this; const doc=this.ownerDocument; {script} }}"
                    ),
                    true,
                )
                .await
                .map_err(cdp_error)?;
            return serde_json::from_value(value)
                .map_err(|error| target_error(ErrorCode::BrowserCommandFailed, error));
        }
        // The page may already have removed the field; the locator then finds
        // nothing instead of failing.
        let id = serde_json::to_string(&self.locator.id)
            .map_err(|error| target_error(ErrorCode::InvalidRequest, error))?;
        let expression = scoped_expression(
            &self.locator.scope,
            &self.locator.shadow_hosts,
            &format!("const el=find(root,{id});const doc=root.ownerDocument||root;{script}"),
        )?;
        eval_scoped(self.execution_page(page), &self.locator.scope, expression).await
    }

    /// Types into the target and returns whether a page-swapped replacement
    /// control took focus, and the keys, instead of the target.
    pub async fn type_text(
        &self,
        page: &Page,
        value: &str,
        clear_first: bool,
    ) -> Result<bool, CommandError> {
        // Keys go to the focused control: a native target is clicked into, any
        // other is focused through its locator.
        match &self.native {
            Some(element) => {
                element.click().await.map_err(cdp_error)?;
            }
            None => {
                self.eval::<bool>(page, "el.focus(); return true").await?;
            }
        }
        let replaced = self.settle_focus(page).await?;
        if clear_first && replaced {
            self.clear_focused(page).await?;
        } else if clear_first {
            let editable = self
                .eval::<bool>(
                    page,
                    "if (el.isContentEditable) return true; el.value = ''; el.dispatchEvent(new Event('input', { bubbles: true })); return false",
                )
                .await?;
            if editable {
                self.clear_editable(page).await?;
            }
        }
        // Runs of chars the US keyboard layout key-presses, runs that need a
        // caret-level insert (Unicode outside the US map), and line breaks,
        // each the Enter key: it submits a single-line input and, unless the
        // page handles it, breaks the line elsewhere.
        let keyboard = self.execution_page(page);
        let mut runs: Vec<(u8, String)> = Vec::new();
        for ch in value.chars() {
            let kind = if ch == '\n' || ch == '\r' {
                2
            } else {
                u8::from(get_key_definition(ch.to_string()).is_some())
            };
            match runs.last_mut() {
                Some((run_kind, run)) if *run_kind == kind && kind != 2 => run.push(ch),
                _ => runs.push((kind, ch.to_string())),
            }
        }
        for (kind, run) in runs {
            if kind == 2 {
                self.press_enter(page).await?;
            } else if kind == 1 {
                keyboard.type_str(run).await.map_err(cdp_error)?;
            } else {
                keyboard
                    .execute(InsertTextParams::new(run))
                    .await
                    .map_err(cdp_error)?;
            }
        }
        Ok(replaced)
    }

    pub async fn is_select(&self, page: &Page) -> Result<bool, CommandError> {
        self.eval(page, "return el instanceof HTMLSelectElement")
            .await
    }

    /// Anchors with a `download` attribute trigger a download on click, which
    /// a plain click would otherwise drop silently in headless Chromium.
    pub async fn is_download_link(&self, page: &Page) -> Result<bool, CommandError> {
        self.eval(
            page,
            "return el instanceof HTMLAnchorElement && el.hasAttribute('download')",
        )
        .await
    }

    pub async fn is_checkable(&self, page: &Page) -> Result<bool, CommandError> {
        self.eval(
            page,
            "return el instanceof HTMLInputElement && (el.type==='checkbox'||el.type==='radio')",
        )
        .await
    }

    pub async fn set_checked(&self, page: &Page, checked: bool) -> Result<bool, CommandError> {
        let checked = if checked { "true" } else { "false" };
        self.eval(
            page,
            &format!("if(el.type==='radio'&&!{checked})throw new Error('radio controls cannot be unchecked directly');if(el.checked!=={checked}){{el.click()}}return el.checked"),
        )
        .await
    }
    /// Select a native option by exact value, falling back to a trimmed,
    /// case-insensitive label/text match when no value matches — snapshots
    /// surface option labels, so a caller holding only the label must not
    /// have to guess the underlying value. Rereads the committed value.
    pub async fn select_option(&self, page: &Page, value: &str) -> Result<String, CommandError> {
        let value = serde_json::to_string(value).map_err(|error| {
            target_error(
                ErrorCode::InvalidRequest,
                format!("invalid select value: {error}"),
            )
        })?;
        let script = format!(
            "if (!(el instanceof HTMLSelectElement)) throw new Error('resolved control is not a select'); const wanted={value}; const norm=s=>s.trim().toLowerCase(); const byValue=[...el.options].filter(option=>option.value===wanted); const matches=byValue.length?byValue:[...el.options].filter(option=>norm(option.label)===norm(wanted)||norm(option.textContent)===norm(wanted)); if(matches.length!==1||matches[0].disabled) throw new Error('select option is missing, ambiguous, or disabled'); el.value=matches[0].value; el.dispatchEvent(new Event('input',{{bubbles:true}})); el.dispatchEvent(new Event('change',{{bubbles:true}})); return el.value"
        );
        if let Some(element) = &self.native {
            let selected = element
                .call_js_fn_by_value(format!("function() {{ const el=this; {script}; }}"), false)
                .await
                .map_err(cdp_error)?;
            return serde_json::from_value(selected)
                .map_err(|error| target_error(ErrorCode::BrowserCommandFailed, error));
        }
        self.eval(page, &script).await
    }

    pub async fn select_options(
        &self,
        page: &Page,
        values: &[String],
    ) -> Result<Vec<String>, CommandError> {
        let values = serde_json::to_string(values).map_err(|error| {
            target_error(
                ErrorCode::InvalidRequest,
                format!("invalid select values: {error}"),
            )
        })?;
        self.eval(
            page,
            &format!("if(!(el instanceof HTMLSelectElement)||!el.multiple)throw new Error('resolved control is not a multi-select');const norm=s=>s.trim().toLowerCase();const requested=new Set({values});const wanted=new Map();for(const value of requested){{const byValue=[...el.options].filter(option=>option.value===value);const matches=byValue.length?byValue:[...el.options].filter(option=>norm(option.label)===norm(value)||norm(option.textContent)===norm(value));if(matches.length!==1||matches[0].disabled)throw new Error('select option is missing, ambiguous, or disabled');wanted.set(matches[0].value,true)}}for(const option of el.options)option.selected=wanted.has(option.value);el.dispatchEvent(new Event('input',{{bubbles:true}}));el.dispatchEvent(new Event('change',{{bubbles:true}}));return [...el.selectedOptions].map(option=>option.value)"),
        )
        .await
    }

    pub async fn clear_control(&self, page: &Page) -> Result<(), CommandError> {
        self.eval::<bool>(
            page,
            "if(el instanceof HTMLInputElement&&el.type==='file'){throw new Error('file controls require native clear')}if(el instanceof HTMLSelectElement){for(const option of el.options)option.selected=false}else if(el instanceof HTMLInputElement&&(el.type==='checkbox'||el.type==='radio')){el.checked=false}else if('value'in el){el.value=''}else if(el.isContentEditable){el.textContent=''}else{throw new Error('resolved control cannot be cleared')}el.dispatchEvent(new Event('input',{bubbles:true}));el.dispatchEvent(new Event('change',{bubbles:true}));return true",
        )
        .await?;
        Ok(())
    }

    pub async fn screenshot(&self, page: &Page) -> Result<Vec<u8>, CommandError> {
        let page = self.execution_page(page);
        if let Some(element) = &self.native {
            return element
                .screenshot(CaptureScreenshotFormat::Png)
                .await
                .map_err(cdp_error);
        }
        let rect: Rect = self
            .eval(
                page,
                "const r=el.getBoundingClientRect(); return {x:r.x,y:r.y,width:r.width,height:r.height}",
            )
            .await?;
        if rect.width <= 0.0 || rect.height <= 0.0 {
            return Err(target_error(
                ErrorCode::ScreenshotCaptureFailed,
                "resolved target has no visible screenshot bounds",
            ));
        }
        page.screenshot(
            ScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Png)
                .clip(Viewport {
                    x: rect.x,
                    y: rect.y,
                    width: rect.width,
                    height: rect.height,
                    scale: 1.0,
                })
                .build(),
        )
        .await
        .map_err(cdp_error)
    }

    pub async fn set_files(&self, page: &Page, paths: Vec<String>) -> Result<(), CommandError> {
        let page = self.execution_page(page);
        // Raw selectors own a stable native element handle. Semantic targets,
        // however, are gathered and then re-resolved; using the opportunistic
        // native handle here can hand DOM.setFileInputFiles an already-invalid
        // backend node even when the freshly stamped JS locator still resolves.
        if self.locator.id.is_empty() {
            let element = self.native.as_ref().ok_or_else(|| {
                target_error(
                    ErrorCode::TargetNotFound,
                    "file input selector did not resolve to a native element",
                )
            })?;
            page.execute(
                SetFileInputFilesParams::builder()
                    .files(paths)
                    .backend_node_id(element.backend_node_id)
                    .build()
                    .map_err(|error| target_error(ErrorCode::BrowserCommandFailed, error))?,
            )
            .await
            .map_err(cdp_error)?;
            return Ok(());
        }
        let expression = locator_expression(&self.locator, "return el")?;
        let object_id = resolve_object_id_scoped(page, &self.locator.scope, expression).await?;
        let backend_node_id = page
            .execute(DescribeNodeParams::builder().object_id(object_id).build())
            .await
            .map_err(cdp_error)?
            .result
            .node
            .backend_node_id;
        page.execute(
            SetFileInputFilesParams::builder()
                .files(paths)
                .backend_node_id(backend_node_id)
                .build()
                .map_err(|error| target_error(ErrorCode::BrowserCommandFailed, error))?,
        )
        .await
        .map_err(cdp_error)?;
        Ok(())
    }

    /// Whether the target takes typed text; see [`ACCEPTS_TEXT_SCRIPT`].
    pub async fn accepts_text(&self, page: &Page) -> Result<bool, CommandError> {
        self.eval(page, ACCEPTS_TEXT_SCRIPT).await
    }

    async fn eval<T: DeserializeOwned>(
        &self,
        page: &Page,
        operation: &str,
    ) -> Result<T, CommandError> {
        // A returned promise is awaited on every path, as page evaluation
        // already does.
        if let Some(element) = &self.native {
            let value = element
                .call_js_fn_by_value(format!("function() {{ const el=this; {operation} }}"), true)
                .await
                .map_err(cdp_error)?;
            return serde_json::from_value(value)
                .map_err(|error| target_error(ErrorCode::BrowserCommandFailed, error));
        }
        let page = self.execution_page(page);
        let expression = locator_expression(&self.locator, operation)?;
        eval_scoped(page, &self.locator.scope, expression).await
    }
}

/// Function body with `el` bound: whether `el` takes typed text. Checked
/// before any key is dispatched.
pub(crate) const ACCEPTS_TEXT_SCRIPT: &str = "return !el.disabled && !el.closest('fieldset[disabled]') && ((el instanceof HTMLInputElement && !el.readOnly && !['hidden','button','submit','reset','image','file'].includes((el.type||'text').toLowerCase())) || (el instanceof HTMLTextAreaElement && !el.readOnly) || el instanceof HTMLSelectElement || el.isContentEditable);";

/// The DOM backend node id for a gathered candidate, for correlating it with
/// a CDP frame owner (`DOM.getFrameOwner`'s `backend_node_id`) or an AX tree
/// node (`AxNode.backend_dom_node_id`) by identity instead of by name/src
/// heuristics. Mirrors [`ResolvedTarget::backend_node_id`] for a candidate
/// that has not been resolved into a `ResolvedTarget` yet.
pub(crate) async fn candidate_backend_node_id(
    page: &Page,
    scope: &LocatorScope,
    shadow_hosts: &[String],
    candidate: &Candidate,
) -> Result<BackendNodeId, CommandError> {
    let locator = JsLocator {
        scope: scope.clone(),
        shadow_hosts: shadow_hosts.to_vec(),
        id: candidate.id.clone(),
    };
    let expression = locator_expression(&locator, "return el")?;
    let object_id = resolve_object_id_scoped(page, scope, expression).await?;
    Ok(page
        .execute(DescribeNodeParams::builder().object_id(object_id).build())
        .await
        .map_err(cdp_error)?
        .result
        .node
        .backend_node_id)
}

#[derive(Deserialize)]
struct Rect {
    x: f64,
    y: f64,
    width: f64,
    height: f64,
}

pub async fn resolve_target(
    page_id: &PageId,
    page: &Page,
    selector: &str,
    target: Option<&TargetSpec>,
    browser: Option<&BrowserHandle>,
) -> Result<ResolvedTarget, CommandError> {
    resolve_target_with_visibility(page_id, page, selector, target, true, browser).await
}

/// The select-all shortcut, Cmd+A on macOS and Ctrl+A elsewhere. The editing
/// command performs the selection, so the chord carries no text and no native
/// key code: macOS reads code 65 as the keypad decimal key and floods keydowns.
pub(crate) fn select_all_events(
) -> [chromiumoxide::cdp::browser_protocol::input::DispatchKeyEventParams; 2] {
    use chromiumoxide::cdp::browser_protocol::input::{
        DispatchKeyEventParams, DispatchKeyEventType,
    };
    let modifiers = if cfg!(target_os = "macos") { 4 } else { 2 };
    let event = |event_type| {
        DispatchKeyEventParams::builder()
            .r#type(event_type)
            .key("a")
            .code("KeyA")
            .windows_virtual_key_code(65)
            .modifiers(modifiers)
    };
    [
        event(DispatchKeyEventType::RawKeyDown)
            .commands(["selectAll"])
            .build()
            .expect("select-all params carry a type"),
        event(DispatchKeyEventType::KeyUp)
            .build()
            .expect("select-all params carry a type"),
    ]
}

/// An empty string in a target field can never resolve — an exact match
/// against "" — but a wait condition would poll for it until the deadline,
/// reporting a timeout instead of the real mistake. Reject it up front.
pub fn validate_target_spec(target: &TargetSpec) -> Result<(), CommandError> {
    validate_target_spec_hop(target, false)
}

/// `is_frame_hop` is true only for entries reached by descending
/// `frame_path`: an unnamed iframe's stamped hop carries `accessibleName: ""`
/// with `role` set (see `descend_a11y_iframes`), which must validate so the
/// snapshot's own target can be passed back verbatim. `shadow_path` entries
/// and the top-level target keep the strict rule.
fn validate_target_spec_hop(target: &TargetSpec, is_frame_hop: bool) -> Result<(), CommandError> {
    for (field, value) in [
        ("css", &target.css),
        ("testId", &target.test_id),
        ("role", &target.role),
        ("accessibleName", &target.accessible_name),
        ("label", &target.label),
    ] {
        if let Some(value) = value {
            // Only an exact `""` is the stamped-hop shape (`unwrap_or_default`
            // on the candidate's name); a whitespace-only value is not that
            // shape and must still be rejected, matching the exact-`""` check
            // both normalization points (`open_target_scope`, `segment()`)
            // use downstream.
            let unnamed_frame_hop = is_frame_hop
                && field == "accessibleName"
                && target.role.is_some()
                && value.is_empty();
            if value.trim().is_empty() && !unnamed_frame_hop {
                return Err(target_error(
                    ErrorCode::InvalidRequest,
                    format!("target field {field} is empty and can never resolve"),
                ));
            }
        }
    }
    for nested in &target.frame_path {
        validate_target_spec_hop(nested, true)?;
    }
    for nested in &target.shadow_path {
        validate_target_spec_hop(nested, false)?;
    }
    Ok(())
}

/// Iframe elements in the selected context, with the accessible names the a11y
/// snapshot's `Iframe` nodes carry. The snapshot uses these to map each child
/// frame back to its element and stamp in-frame targets with a re-resolvable
/// hop.
pub(crate) async fn iframe_candidates(
    page: &Page,
    context_id: Option<ExecutionContextId>,
) -> Result<Vec<Candidate>, CommandError> {
    let scope_id = TARGET_SCOPE.fetch_add(1, Ordering::Relaxed);
    let raw = collect_candidates(page, context_id, &[], scope_id).await?;
    Ok(raw
        .into_iter()
        .map(into_candidate)
        .filter(|candidate| candidate.role.as_deref() == Some("iframe") && candidate.state.attached)
        .collect())
}

/// A named hop ranks only same-name frames; an unnamed hop ranks every
/// visible iframe because its normalized name applies no filter.
pub(crate) fn iframe_hop_ordinal(
    visible_iframes: &[&Candidate],
    candidate: &Candidate,
) -> Option<usize> {
    let matching: Vec<_> = visible_iframes
        .iter()
        .filter(|other| candidate.name.is_none() || other.name == candidate.name)
        .collect();
    (matching.len() > 1).then(|| {
        matching
            .iter()
            .position(|other| other.id == candidate.id)
            .unwrap_or(0)
    })
}

fn semantic_iframe_hop(
    candidate: &Candidate,
    candidates: &[Candidate],
) -> types::SemanticTargetSegment {
    let visible_iframes: Vec<_> = candidates
        .iter()
        .filter(|other| {
            other.role.as_deref() == Some("iframe") && other.state.attached && other.state.visible
        })
        .collect();
    types::SemanticTargetSegment {
        role: "iframe".into(),
        accessible_name: candidate.name.clone().unwrap_or_default(),
        ordinal: iframe_hop_ordinal(&visible_iframes, candidate),
    }
}

/// Gather DOM candidates for intent resolution without choosing a match.
pub async fn gather_candidates(
    page: &Page,
    target: &TargetSpec,
    browser: Option<&BrowserHandle>,
) -> Result<Vec<Candidate>, CommandError> {
    let scope = open_target_scope(page, target, browser).await?;
    let scope_ref = scope.locator_scope();
    let (raw, _owners) = collect_candidates_merged(
        &scope.execution_page,
        &scope_ref,
        &scope.shadow_hosts,
        scope.scope_id,
    )
    .await?;
    let mut candidates =
        align_with_accessibility(&scope.execution_page, scope.context_id, target, raw).await?;
    // Auto-descend one level into iframes for main-frame intents: agents
    // cannot name a framePath for content they cannot see, so without this
    // every in-frame control resolves as targetNotFound. Each stamped
    // frame_path lets the action path re-resolve through the same frame.
    if target.frame_path.is_empty() {
        let frames: Vec<(Candidate, Box<TargetSpec>)> = candidates
            .iter()
            .filter(|candidate| {
                candidate.role.as_deref() == Some("iframe")
                    && candidate.state.attached
                    && candidate.state.visible
            })
            .take(8)
            .map(|frame| (frame.clone(), frame_hop(frame, &candidates)))
            .collect();
        for (frame, hop) in frames {
            let gathered = gather_frame_candidates(&scope, &frame, &hop, browser).await;
            if let Ok(mut gathered) = gathered {
                candidates.append(&mut gathered);
            }
        }
    }
    Ok(candidates)
}

/// Reads the document body after resolving only the target's frame/shadow
/// scope. Page-scoped targets such as `body` are not interactive candidates,
/// so ordinary candidate selection would falsely report `targetNotFound`.
pub async fn inspect_page_scoped_target(
    page_id: &PageId,
    page: &Page,
    target: &TargetSpec,
    include_html: bool,
    browser: Option<&BrowserHandle>,
) -> Result<(String, Option<String>, Evidence), CommandError> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct ScopedInspection {
        text: String,
        html: Option<String>,
    }

    validate_target_spec(target)?;
    let scope = open_target_scope(page, target, browser).await?;
    let expression = format!(
        "(()=>{{const el=document.body;return {{text:el?(el.innerText||''):'',html:{}&&el?el.outerHTML:null}}}})()",
        if include_html { "true" } else { "false" }
    );
    let observed: ScopedInspection =
        evaluate_in_context(&scope.execution_page, scope.context_id, expression).await?;
    let evidence = Evidence::Resolution {
        target: Box::new(target.clone()),
        fingerprint: Box::new(TargetFingerprint {
            page_id: page_id.clone(),
            frame: (!target.frame_path.is_empty()).then(|| format!("{:?}", scope.frame_id)),
            role: target.role.clone(),
            name: target.accessible_name.clone(),
            stable_attributes: BTreeMap::new(),
        }),
        candidates: scope.frame_trace,
        best_match_authorized: false,
    };
    Ok((observed.text, observed.html, evidence))
}

/// The hop that re-resolves `frame` among `candidates`: role, name and
/// ordinal, the shape every frame path carries.
fn frame_hop(frame: &Candidate, candidates: &[Candidate]) -> Box<TargetSpec> {
    let segment = semantic_iframe_hop(frame, candidates);
    Box::new(TargetSpec {
        role: Some(segment.role),
        accessible_name: (!segment.accessible_name.is_empty()).then_some(segment.accessible_name),
        ordinal: segment.ordinal,
        ..TargetSpec::default()
    })
}

async fn gather_frame_candidates(
    scope: &TargetScope,
    frame: &Candidate,
    hop: &TargetSpec,
    browser: Option<&BrowserHandle>,
) -> Result<Vec<Candidate>, CommandError> {
    let locator_scope = scope.locator_scope();
    let Some(child) = owned_frame(
        &scope.execution_page,
        browser,
        &locator_scope,
        &scope.shadow_hosts,
        frame,
    )
    .await?
    else {
        return Ok(Vec::new());
    };
    let context_id = child
        .session
        .frame_execution_context(child.frame)
        .await
        .map_err(cdp_error)?;
    let nested_scope = TARGET_SCOPE.fetch_add(1, Ordering::Relaxed);
    let raw = collect_candidates(&child.session, context_id, &[], nested_scope).await?;
    Ok(raw
        .into_iter()
        .map(|item| {
            let mut candidate = into_candidate(item);
            candidate.frame_path = vec![Box::new(hop.clone())];
            candidate
        })
        .collect())
}

struct TargetScope {
    execution_page: Page,
    context_id: Option<ExecutionContextId>,
    shadow_hosts: Vec<String>,
    /// Set once shadow_path traversal enters a closed shadow root; from that
    /// point on, further gathers/lookups are scoped to this `Element`
    /// instead of `context_id` (see [`discover_closed_root_for_candidate`]).
    closed_root: Option<Arc<Element>>,
    scope_id: u64,
    frame_id: chromiumoxide::cdp::browser_protocol::page::FrameId,
    frame_trace: Vec<types::CandidateEvidence>,
    frame_path: Vec<types::SemanticTargetSegment>,
    frame_owners: Vec<(Page, BackendNodeId)>,
}

impl TargetScope {
    fn locator_scope(&self) -> LocatorScope {
        match &self.closed_root {
            Some(element) => LocatorScope::ClosedRoot(Arc::clone(element)),
            None => LocatorScope::Context(self.context_id),
        }
    }
}

async fn open_target_scope(
    page: &Page,
    target: &TargetSpec,
    browser: Option<&BrowserHandle>,
) -> Result<TargetScope, CommandError> {
    if target.frame_path.len() > MAX_TARGET_PATH_DEPTH
        || target.shadow_path.len() > MAX_TARGET_PATH_DEPTH
    {
        return Err(target_error(
            ErrorCode::InvalidRequest,
            "target frame or shadow path exceeds configured depth",
        ));
    }

    let scope_id = TARGET_SCOPE.fetch_add(1, Ordering::Relaxed);
    let main_frame = page
        .mainframe()
        .await
        .map_err(cdp_error)?
        .ok_or_else(|| target_error(ErrorCode::FrameNotFound, "page has no main frame"))?;
    let mut frame_id = main_frame;
    let mut execution_page = page.clone();
    let mut context_id = None;
    let mut shadow_hosts = Vec::new();
    let mut frame_trace = Vec::new();
    let mut frame_path = Vec::new();
    let mut frame_owners = Vec::new();

    for (index, frame_target) in target.frame_path.iter().enumerate() {
        let raw = collect_candidates(&execution_page, context_id, &shadow_hosts, scope_id).await?;
        // A hop stamped for an unnamed iframe carries `accessibleName: ""`
        // (see `descend_a11y_iframes`); normalize it to `None` here so
        // `choose` does not filter out every unnamed candidate.
        let mut normalized_frame_target = (**frame_target).clone();
        if normalized_frame_target.accessible_name.as_deref() == Some("") {
            normalized_frame_target.accessible_name = None;
        }
        let candidates: Vec<_> = raw.into_iter().map(into_candidate).collect();
        let (candidate, evidence, _) = choose(&normalized_frame_target, &candidates, true)?;
        frame_path.push(semantic_iframe_hop(&candidate, &candidates));
        frame_trace.push(evidence);
        // The owner can be in the DOM before its document, or a frame from
        // another site before its target attaches.
        let deadline = Instant::now() + Duration::from_secs(2);
        let child = loop {
            let scope = LocatorScope::Context(context_id);
            if let Some(child) =
                owned_frame(&execution_page, browser, &scope, &shadow_hosts, &candidate).await?
            {
                break child;
            }
            if Instant::now() >= deadline {
                return Err(target_error(
                    ErrorCode::FrameNotFound,
                    format!("frame path component {index} did not map to a child frame"),
                ));
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        if child.session.target_id() != execution_page.target_id() {
            frame_owners.push((execution_page.clone(), child.owner));
        }
        execution_page = child.session;
        frame_id = child.frame.clone();
        context_id = execution_page
            .frame_execution_context(child.frame)
            .await
            .map_err(cdp_error)?;
        shadow_hosts.clear();
    }

    let mut closed_root: Option<Arc<Element>> = None;

    for host_target in &target.shadow_path {
        let scope_ref = match &closed_root {
            Some(element) => LocatorScope::ClosedRoot(Arc::clone(element)),
            None => LocatorScope::Context(context_id),
        };
        let raw = match &scope_ref {
            LocatorScope::Context(_) => {
                collect_candidates(&execution_page, context_id, &shadow_hosts, scope_id).await?
            }
            LocatorScope::ClosedRoot(element) => {
                let nested_scope = TARGET_SCOPE.fetch_add(1, Ordering::Relaxed);
                collect_candidates_within(element, &shadow_hosts, nested_scope).await?
            }
        };
        let candidates: Vec<_> = raw.into_iter().map(into_candidate).collect();
        let (candidate, evidence, _) =
            dom_engine::resolve_shadow_segment(host_target, &candidates)?;
        let mut prospective = shadow_hosts.clone();
        prospective.push(candidate.id.clone());
        let has_root: bool = eval_scoped(
            &execution_page,
            &scope_ref,
            scoped_expression(&scope_ref, &prospective, "return !!root")?,
        )
        .await?;
        if has_root {
            shadow_hosts.push(candidate.id);
            frame_trace.push(evidence);
            continue;
        }

        // No open root on this candidate; CDP pierce discovery may still find a
        // closed one on the same host.
        let discovered = discover_closed_root_for_candidate(
            &execution_page,
            &scope_ref,
            &shadow_hosts,
            &candidate.id,
        )
        .await?;
        let Some(root_backend_id) = discovered else {
            return Err(dom_engine::shadow_probe_error("shadow-unavailable").unwrap());
        };
        let element = execution_page
            .element_from_backend_node_id(root_backend_id)
            .await
            .map_err(cdp_error)?;
        closed_root = Some(Arc::new(element));
        shadow_hosts.clear();
        frame_trace.push(evidence);
    }

    Ok(TargetScope {
        execution_page,
        context_id,
        shadow_hosts,
        closed_root,
        scope_id,
        frame_id,
        frame_trace,
        frame_path,
        frame_owners,
    })
}

fn into_candidate(mut item: BrowserCandidate) -> Candidate {
    // `data-bobby-target` is an instrumentation id an element keeps across
    // scans. Never treat it as identity or match on it in a later resolution
    // pass.
    item.attributes.remove("data-bobby-target");
    let css = item.css.filter(|css| !css.contains("data-bobby-target"));
    Candidate {
        id: item.id,
        css,
        tag: item.tag.filter(|tag| !tag.is_empty()),
        test_id: item.test_id,
        role: item.role,
        name: item.name,
        label: item.label,
        text: item.text,
        attributes: item.attributes,
        state: CandidateState {
            attached: item.attached,
            visible: item.visible,
            enabled: item.enabled,
        },
        frame_path: Vec::new(),
    }
}

pub async fn resolve_target_with_visibility(
    page_id: &PageId,
    page: &Page,
    selector: &str,
    target: Option<&TargetSpec>,
    require_visible: bool,
    browser: Option<&BrowserHandle>,
) -> Result<ResolvedTarget, CommandError> {
    let Some(target) = target else {
        let element = query_element(page, None, selector).await?.ok_or_else(|| {
            target_error(
                ErrorCode::TargetNotFound,
                format!("no element matches selector `{selector}`"),
            )
        })?;
        return Ok(ResolvedTarget {
            native: Some(element),
            locator: JsLocator {
                scope: LocatorScope::Context(None),
                shadow_hosts: Vec::new(),
                id: String::new(),
            },
            execution_page: None,
            frame_id: None,
            iframe_hop: None,
            frame_path: Vec::new(),
            frame_owners: Vec::new(),
            evidence: selector_evidence(page_id, selector),
        });
    };
    validate_target_spec(target)?;

    let scope = open_target_scope(page, target, browser).await?;
    let base_scope = scope.locator_scope();
    // One read: a target that is not on the page yet fails at once. Waiting
    // for it is the caller's (`wait_for`, or the runtime before an action).
    let (raw, owners) = collect_candidates_merged(
        &scope.execution_page,
        &base_scope,
        &scope.shadow_hosts,
        scope.scope_id,
    )
    .await?;
    let raw =
        align_with_accessibility(&scope.execution_page, scope.context_id, target, raw).await?;
    let (candidate, evidence, best_match_authorized) = choose(target, &raw, require_visible)?;
    let iframe_hop = (candidate.role.as_deref() == Some("iframe"))
        .then(|| semantic_iframe_hop(&candidate, &raw));
    let owner = owners.get(&candidate.id).cloned();
    // A candidate gathered from a closed shadow root must be located relative
    // to that root's own element handle, not the outer document/frame context.
    let (locator_scope, locator_shadow_hosts) = match &owner {
        Some(element) => (LocatorScope::ClosedRoot(Arc::clone(element)), Vec::new()),
        None => (base_scope.clone(), scope.shadow_hosts.clone()),
    };
    let locator = JsLocator {
        scope: locator_scope,
        shadow_hosts: locator_shadow_hosts,
        id: candidate.id.clone(),
    };
    // A css lookup is only safe for a unique `#id`; the collector
    // stamps `#id` only when the id selects that element first. Tag and
    // `[aria-label=…]` stamps match more than one node; using them here
    // would click the first hit (often a wrapper) instead of the candidate
    // the resolver just chose. The JS locator keys off `data-bobby-target`.
    let native = if target.frame_path.is_empty() && target.shadow_path.is_empty() {
        match (&owner, candidate.css.as_deref()) {
            (Some(element), Some(css)) if css.starts_with('#') => {
                query_element(&scope.execution_page, Some(element), css)
                    .await
                    .ok()
                    .flatten()
            }
            (None, Some(css)) if css.starts_with('#') => {
                query_element(&scope.execution_page, None, css)
                    .await
                    .ok()
                    .flatten()
            }
            _ => None,
        }
    } else {
        None
    };
    let fingerprint = TargetFingerprint {
        page_id: page_id.clone(),
        frame: (!target.frame_path.is_empty()).then(|| format!("{:?}", scope.frame_id)),
        role: candidate.role.clone(),
        name: candidate.name.clone(),
        stable_attributes: candidate.attributes.clone(),
    };
    let mut frame_trace = scope.frame_trace;
    frame_trace.push(evidence);
    let main_frame = page.mainframe().await.map_err(cdp_error)?;
    Ok(ResolvedTarget {
        native,
        locator,
        execution_page: (scope.execution_page.target_id() != page.target_id())
            .then_some(scope.execution_page),
        frame_id: match &main_frame {
            Some(main) if *main == scope.frame_id => None,
            _ => Some(scope.frame_id),
        },
        iframe_hop,
        frame_path: scope.frame_path,
        frame_owners: scope.frame_owners,
        evidence: Evidence::Resolution {
            target: Box::new(target.clone()),
            fingerprint: Box::new(fingerprint),
            candidates: frame_trace,
            best_match_authorized,
        },
    })
}

/// For a `Text`/`Value` wait whose target matches more than one candidate:
/// reads every ranked candidate's live innerText/value instead of erroring
/// with `TargetAmbiguous`, so the caller's matcher (not identity) decides
/// which one satisfies the wait. Returns them best-ranked first; a candidate
/// whose read fails (e.g. it detached between ranking and the read) is
/// skipped rather than failing the whole call. An empty result means no
/// attached (and, if required, visible) candidate could be read at all,
/// which the caller treats the same as `TargetNotFound` (keep polling).
pub async fn resolve_ambiguous_wait_values(
    page: &Page,
    target: &TargetSpec,
    is_value: bool,
    browser: Option<&BrowserHandle>,
) -> Result<Vec<String>, CommandError> {
    validate_target_spec(target)?;
    let scope = open_target_scope(page, target, browser).await?;
    let base_scope = scope.locator_scope();
    let (raw, owners) = collect_candidates_merged(
        &scope.execution_page,
        &base_scope,
        &scope.shadow_hosts,
        scope.scope_id,
    )
    .await?;
    let candidates: Vec<Candidate> = raw.into_iter().map(into_candidate).collect();
    let policy = ResolutionPolicy {
        require_visible: true,
        ..ResolutionPolicy::default()
    };
    let ranked = rank_candidates(target, &candidates, &policy)
        .map_err(|error| target_error(ErrorCode::InvalidRequest, error))?;
    let operation = if is_value {
        "return el.value || ''"
    } else {
        "return el.innerText || ''"
    };
    let mut values = Vec::with_capacity(ranked.len());
    for (candidate, _evidence) in ranked {
        let owner = owners.get(&candidate.id).cloned();
        let (locator_scope, locator_shadow_hosts) = match &owner {
            Some(element) => (LocatorScope::ClosedRoot(Arc::clone(element)), Vec::new()),
            None => (base_scope.clone(), scope.shadow_hosts.clone()),
        };
        let locator = JsLocator {
            scope: locator_scope,
            shadow_hosts: locator_shadow_hosts,
            id: candidate.id.clone(),
        };
        let expression = locator_expression(&locator, operation)?;
        let read: Result<String, CommandError> =
            eval_scoped(&scope.execution_page, &locator.scope, expression).await;
        match read {
            Ok(value) => values.push(value),
            // A candidate that matched at collection time can detach (or the
            // page can re-render it away) before its value is read on this
            // same poll. That is "this candidate did not match on this
            // poll", not a wait failure: skip it and let the remaining
            // candidates — or the next poll, if every candidate here failed
            // — decide.
            Err(error) => {
                tracing::debug!(
                    candidate_id = %candidate.id,
                    error = %error.message,
                    "skipping ambiguous wait candidate that failed to read"
                );
            }
        }
    }
    Ok(values)
}

/// Aligns the gathered candidates for a role and name target with Chrome's
/// accessibility tree, which snapshots report, unless exactly one gathered
/// candidate already matches and no ordinal is asked. The tree's matches take
/// the target's role and name, which can come from content the gathered names
/// do not read, such as an element slotted into a label; gathered matches the
/// tree leaves out, such as controls hidden behind a modal dialog, drop out.
async fn align_with_accessibility(
    page: &Page,
    context_id: Option<ExecutionContextId>,
    target: &TargetSpec,
    raw: Vec<BrowserCandidate>,
) -> Result<Vec<Candidate>, CommandError> {
    let mut candidates: Vec<_> = raw.into_iter().map(into_candidate).collect();
    let (Some(role), Some(name)) = (&target.role, &target.accessible_name) else {
        return Ok(candidates);
    };
    if accessibility_alignment_unnecessary(&candidates, role, name, target.ordinal) {
        return Ok(candidates);
    }
    let exposed = accessibility_matches(page, context_id, role, name).await?;
    apply_accessibility_alignment(&mut candidates, role, name, &exposed);
    Ok(candidates)
}

/// The gathered ids of the elements Chrome's accessibility tree exposes with
/// `role` and `name`.
async fn accessibility_matches(
    page: &Page,
    context_id: Option<ExecutionContextId>,
    role: &str,
    name: &str,
) -> Result<Vec<String>, CommandError> {
    let document = page
        .execute(GetDocumentParams::builder().depth(0).build())
        .await
        .map_err(cdp_error)?
        .result
        .root;
    let backend_node_id = if context_id.is_some() {
        let object_id = scope_root_object_id(page, &LocatorScope::Context(context_id), &[]).await?;
        element_from_remote_object(page, object_id)
            .await?
            .backend_node_id
    } else {
        document.backend_node_id
    };
    let nodes = page
        .execute(
            QueryAxTreeParams::builder()
                .backend_node_id(backend_node_id)
                .accessible_name(name)
                .role(role)
                .build(),
        )
        .await
        .map_err(cdp_error)?
        .result
        .nodes;
    // Each call releases only its own objects; a shared group would free the
    // objects of a concurrent call on the same page.
    let group = format!(
        "{AX_RESOLVE_GROUP}-{}",
        TARGET_SCOPE.fetch_add(1, Ordering::Relaxed)
    );
    let mut ids = Vec::new();
    for backend_node_id in nodes
        .into_iter()
        .filter(|node| !node.ignored)
        .filter_map(|node| node.backend_dom_node_id)
    {
        let object = page
            .execute(
                ResolveNodeParams::builder()
                    .backend_node_id(backend_node_id)
                    .object_group(group.clone())
                    .build(),
            )
            .await
            .map_err(cdp_error)?
            .result
            .object;
        let Some(object_id) = object.object_id else {
            continue;
        };
        let id = page
            .execute(
                CallFunctionOnParams::builder()
                    .object_id(object_id)
                    .function_declaration(
                        "function(){return this.getAttribute('data-bobby-target')}",
                    )
                    .return_by_value(true)
                    .build()
                    .map_err(|error| target_error(ErrorCode::BrowserCommandFailed, error))?,
            )
            .await
            .map_err(cdp_error)?
            .result
            .result
            .value;
        if let Some(id) = id.as_ref().and_then(|id| id.as_str()) {
            ids.push(id.to_owned());
        }
    }
    let _ = page.execute(ReleaseObjectGroupParams::new(group)).await;
    Ok(ids)
}

const AX_RESOLVE_GROUP: &str = "bobby-accessibility-named";

fn choose(
    target: &TargetSpec,
    candidates: &[Candidate],
    require_visible: bool,
) -> Result<(Candidate, types::CandidateEvidence, bool), CommandError> {
    let policy = ResolutionPolicy {
        require_visible,
        ..ResolutionPolicy::default()
    };
    match resolve_candidates(target, candidates, &policy)
        .map_err(|error| target_error(ErrorCode::InvalidRequest, error))?
    {
        ResolutionDecision::NotFound => Err(target_error(
            ErrorCode::TargetNotFound,
            "no target candidate matched",
        )),
        ResolutionDecision::Ambiguous { candidates } => {
            // Machine-readable repair, not a Rust Debug dump: name each
            // contender the way the outcome's resolution evidence does
            // (role/name/score), so the agent reading `error.message` can
            // narrow the target without re-deriving the candidate list.
            let summary = candidates
                .iter()
                .take(10)
                .map(|candidate| {
                    let role = candidate.role.as_deref().unwrap_or("(no role)");
                    let name = candidate.name.as_deref().unwrap_or("(no name)");
                    let frame = candidate
                        .reasons
                        .iter()
                        .find(|reason| reason.starts_with("inside iframe"))
                        .map(|reason| format!(" ({reason})"))
                        .unwrap_or_default();
                    format!("{role} \"{name}\" score={}{frame}", candidate.score)
                })
                .collect::<Vec<_>>()
                .join("; ");
            Err(target_error(
                ErrorCode::TargetAmbiguous,
                format!(
                    "target is ambiguous: {summary}. Narrow the target with an exact \
                     accessibleName and ordinal, or a framePath when a contender sits \
                     inside an iframe, then retry"
                ),
            ))
        }
        ResolutionDecision::Resolved {
            candidate,
            evidence,
            best_match_authorized,
        } => Ok((*candidate, evidence, best_match_authorized)),
    }
}

async fn collect_candidates(
    page: &Page,
    context_id: Option<ExecutionContextId>,
    shadow_hosts: &[String],
    scope: u64,
) -> Result<Vec<BrowserCandidate>, CommandError> {
    let operation = candidate_collector_operation(scope)?;
    evaluate_in_context(
        page,
        context_id,
        scope_expression(shadow_hosts, &operation)?,
    )
    .await
}

/// Same gather as [`collect_candidates`], rooted at an already-resolved closed
/// shadow root `Element` instead of `document`. The collector script is bound
/// as `this`, since a closed root is invisible to `document.querySelector`.
async fn collect_candidates_within(
    element: &Element,
    shadow_hosts: &[String],
    scope: u64,
) -> Result<Vec<BrowserCandidate>, CommandError> {
    let operation = candidate_collector_operation(scope)?;
    let function_declaration = closed_root_scope_expression(shadow_hosts, &operation)?;
    let value = element
        .call_js_fn_by_value(function_declaration, false)
        .await
        .map_err(cdp_error)?;
    serde_json::from_value(value).map_err(|error| target_error(ErrorCode::InvalidRequest, error))
}

/// Gathers candidates at `scope` plus every closed shadow root reachable
/// within it, merged into one list so purpose matching sees closed shadow DOM
/// like open. Also returns candidate id to owning closed-root `Element`, so a
/// winning candidate can be relocated for later Act calls. Only
/// closed-root-origin candidates appear in that map.
async fn collect_candidates_merged(
    page: &Page,
    scope: &LocatorScope,
    shadow_hosts: &[String],
    gather_scope_id: u64,
) -> Result<(Vec<BrowserCandidate>, HashMap<String, Arc<Element>>), CommandError> {
    let mut candidates = match scope {
        LocatorScope::Context(context_id) => {
            collect_candidates(page, *context_id, shadow_hosts, gather_scope_id).await?
        }
        LocatorScope::ClosedRoot(element) => {
            collect_candidates_within(element, shadow_hosts, gather_scope_id).await?
        }
    };

    let mut owners = HashMap::new();
    let closed_roots = discover_closed_shadow_roots(page, scope, shadow_hosts).await?;
    for root in closed_roots {
        let nested_scope = TARGET_SCOPE.fetch_add(1, Ordering::Relaxed);
        let mut nested = collect_candidates_within(&root, &[], nested_scope).await?;
        for candidate in &nested {
            owners.insert(candidate.id.clone(), Arc::clone(&root));
        }
        candidates.append(&mut nested);
    }
    Ok((candidates, owners))
}

/// Discovers every closed shadow root reachable within the given scope.
///
/// One `DOM.describeNode(pierce: true, depth: -1)` of the scan root, decoded
/// as JSON (not the typed CDP `Node` tree). Typed pierce-trees overflow
/// Linux debug serde; `serde_json::Value` does not. Walks children and
/// shadow roots only — never `contentDocument`.
async fn discover_closed_shadow_roots(
    page: &Page,
    scope: &LocatorScope,
    shadow_hosts: &[String],
) -> Result<Vec<Arc<Element>>, CommandError> {
    let scan_root = discovery_scan_root(page, scope, shadow_hosts).await?;
    let described = page
        .execute(DescribeNodeJson {
            object_id: scan_root.remote_object_id.clone(),
            depth: -1,
            pierce: true,
        })
        .await
        .map_err(cdp_error)?;
    let mut found = Vec::new();
    if let Some(node) = described.result.get("node") {
        collect_closed_from_json(node, &mut found);
    }
    drop(described);
    let mut unique = Vec::new();
    let mut seen = HashSet::new();
    for backend_id in found {
        if seen.insert(backend_id) {
            unique.push(backend_id);
        }
    }
    let mut roots = Vec::with_capacity(unique.len());
    for backend_id in unique {
        if backend_id == scan_root.backend_node_id {
            continue;
        }
        let element = page
            .element_from_backend_node_id(backend_id)
            .await
            .map_err(cdp_error)?;
        roots.push(Arc::new(element));
    }
    Ok(roots)
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DescribeNodeJson {
    object_id: RemoteObjectId,
    depth: i64,
    pierce: bool,
}

impl Method for DescribeNodeJson {
    fn identifier(&self) -> chromiumoxide::types::MethodId {
        "DOM.describeNode".into()
    }
}

impl Command for DescribeNodeJson {
    type Response = serde_json::Value;
}

fn collect_closed_from_json(root: &serde_json::Value, out: &mut Vec<BackendNodeId>) {
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        let Some(object) = node.as_object() else {
            continue;
        };
        if object
            .get("shadowRootType")
            .and_then(|value| value.as_str())
            == Some("closed")
        {
            if let Some(id) = object.get("backendNodeId").and_then(|value| value.as_i64()) {
                out.push(BackendNodeId::new(id));
            }
        }
        if let Some(children) = object.get("children").and_then(|value| value.as_array()) {
            for child in children {
                stack.push(child);
            }
        }
        if let Some(roots) = object.get("shadowRoots").and_then(|value| value.as_array()) {
            for child in roots {
                stack.push(child);
            }
        }
    }
}

async fn discovery_scan_root(
    page: &Page,
    scope: &LocatorScope,
    shadow_hosts: &[String],
) -> Result<Element, CommandError> {
    match scope {
        LocatorScope::Context(_) if shadow_hosts.is_empty() => query_element(page, None, "html")
            .await?
            .ok_or_else(|| target_error(ErrorCode::TargetDetached, "page has no html element")),
        LocatorScope::ClosedRoot(element) if shadow_hosts.is_empty() => page
            .element_from_backend_node_id(element.backend_node_id)
            .await
            .map_err(cdp_error),
        _ => {
            element_from_remote_object(page, scope_root_object_id(page, scope, shadow_hosts).await?)
                .await
        }
    }
}

async fn element_from_remote_object(
    page: &Page,
    object_id: RemoteObjectId,
) -> Result<Element, CommandError> {
    // Initialize the DOM domain's document before requesting a frontend node.
    // Script-based resolution can reach this path before any DOM query.
    page.execute(GetDocumentParams::builder().depth(0).build())
        .await
        .map_err(cdp_error)?;
    // Built from the backend node id: requesting the document again, as any
    // concurrent call on the page may, discards every frontend node id.
    let backend_node_id = page
        .execute(
            DescribeNodeParams::builder()
                .object_id(object_id)
                .depth(0)
                .build(),
        )
        .await
        .map_err(cdp_error)?
        .result
        .node
        .backend_node_id;
    page.element_from_backend_node_id(backend_node_id)
        .await
        .map_err(cdp_error)
}

/// The first element `selector` matches under `owner`, or in the page's
/// document without one.
async fn query_element(
    page: &Page,
    owner: Option<&Element>,
    selector: &str,
) -> Result<Option<Element>, CommandError> {
    let selector = serde_json::to_string(selector)
        .map_err(|error| target_error(ErrorCode::InvalidRequest, error))?;
    let object = match owner {
        Some(owner) => {
            owner
                .call_js_fn(
                    format!("function(){{return this.querySelector({selector})}}"),
                    false,
                )
                .await
                .map_err(cdp_error)?
                .result
        }
        None => {
            let mut params = EvaluateParams::new(format!("document.querySelector({selector})"));
            params.return_by_value = Some(false);
            page.evaluate(params)
                .await
                .map_err(cdp_error)?
                .object()
                .clone()
        }
    };
    match object.object_id {
        Some(object_id) => element_from_remote_object(page, object_id).await.map(Some),
        None => Ok(None),
    }
}

#[cfg(test)]
fn pierce_followup_backend_ids(root: &CdpNode) -> Vec<BackendNodeId> {
    let mut out = Vec::new();
    if let Some(roots) = &root.shadow_roots {
        for node in roots {
            if pierce_follow_node(node) {
                out.push(node.backend_node_id);
            }
        }
    }
    if let Some(children) = &root.children {
        for node in children {
            if pierce_follow_node(node) {
                out.push(node.backend_node_id);
            }
        }
    }
    out
}

#[cfg(test)]
fn pierce_follow_node(node: &CdpNode) -> bool {
    matches!(node.node_type, 1 | 9 | 11)
}

#[cfg(test)]
fn dismantle_cdp_node(node: CdpNode) {
    let mut stack = vec![node];
    while let Some(mut node) = stack.pop() {
        if let Some(children) = node.children.take() {
            stack.extend(children);
        }
        if let Some(roots) = node.shadow_roots.take() {
            stack.extend(roots);
        }
        if let Some(document) = node.content_document.take() {
            stack.push(*document);
        }
    }
}

/// Walks a `DOM.describeNode(pierce: true)` tree collecting the
/// `backendNodeId` of every *closed* shadow root found. Uses backend ids
/// (not frontend `nodeId`s) because pierce trees return `nodeId`s that are
/// not registered with the frontend. Must never descend into
/// `content_document`: iframes require an explicit `frame_path`. Only
/// `.children` and `.shadow_roots` are followed.
#[cfg(test)]
fn collect_closed_shadow_root_ids(node: &CdpNode, out: &mut Vec<BackendNodeId>) {
    let mut stack = vec![node];
    while let Some(node) = stack.pop() {
        if let Some(shadow_roots) = &node.shadow_roots {
            for root in shadow_roots {
                if matches!(root.shadow_root_type, Some(ShadowRootType::Closed)) {
                    out.push(root.backend_node_id);
                }
                stack.push(root);
            }
        }
        if let Some(children) = &node.children {
            for child in children.iter().rev() {
                stack.push(child);
            }
        }
    }
}

/// Returns the `BackendNodeId` of a closed shadow root attached to the
/// candidate with this per-gather `data-bobby-target` id, if any. Used by the
/// `shadow_path` fallback when the open-root check fails.
async fn discover_closed_root_for_candidate(
    page: &Page,
    scope: &LocatorScope,
    shadow_hosts: &[String],
    candidate_id: &str,
) -> Result<Option<BackendNodeId>, CommandError> {
    let id = serde_json::to_string(candidate_id)
        .map_err(|error| target_error(ErrorCode::InvalidRequest, error))?;
    let operation = format!("const el=find(root,{id});return el;");
    let expression = scoped_expression(scope, shadow_hosts, &operation)?;
    let object_id = resolve_object_id_scoped(page, scope, expression).await?;
    let described = page
        .execute(
            DescribeNodeParams::builder()
                .object_id(object_id)
                .depth(0)
                .build(),
        )
        .await
        .map_err(cdp_error)?;
    Ok(described
        .result
        .node
        .shadow_roots
        .into_iter()
        .flatten()
        .find(|root| matches!(root.shadow_root_type, Some(ShadowRootType::Closed)))
        .map(|root| root.backend_node_id))
}

fn candidate_collector_operation(scope: u64) -> Result<String, CommandError> {
    let prefix = serde_json::to_string(&format!("bobby-{scope}-"))
        .map_err(|error| target_error(ErrorCode::InvalidRequest, error))?;
    Ok(format!(
        "return ({}) (root, {{prefix:{prefix}}}).candidates",
        dom_engine::candidate_gather_function()
    ))
}

/// A child frame and the page session that serves it: the parent's page for a
/// same-process frame, the frame's own target for a frame from another site.
pub(crate) struct ChildFrame {
    pub frame: chromiumoxide::cdp::browser_protocol::page::FrameId,
    pub session: Page,
    /// The iframe element, in the parent's session.
    pub owner: BackendNodeId,
}

/// The frame an iframe `owner` shows, by DOM identity: `DOM.describeNode` on
/// the owner element names its content frame. `None` while the owner has no
/// document yet or, for a frame from another site, while its target is not
/// attached.
pub(crate) async fn owned_frame(
    page: &Page,
    browser: Option<&BrowserHandle>,
    scope: &LocatorScope,
    shadow_hosts: &[String],
    owner: &Candidate,
) -> Result<Option<ChildFrame>, CommandError> {
    let backend_node_id = candidate_backend_node_id(page, scope, shadow_hosts, owner).await?;
    let node = page
        .execute(
            DescribeNodeParams::builder()
                .backend_node_id(backend_node_id)
                .depth(0)
                .build(),
        )
        .await
        .map_err(cdp_error)?
        .result
        .node;
    let Some(frame) = node.frame_id else {
        return Ok(None);
    };
    Ok(frame_session(page, browser, frame)
        .await?
        .map(|(frame, session)| ChildFrame {
            frame,
            session,
            owner: backend_node_id,
        }))
}

/// The session serving `frame`: `page` for one of its own frames, else the
/// frame's own target, whose id is the frame id. `None` while that target is
/// not attached.
pub(crate) async fn frame_session(
    page: &Page,
    browser: Option<&BrowserHandle>,
    frame: chromiumoxide::cdp::browser_protocol::page::FrameId,
) -> Result<Option<(chromiumoxide::cdp::browser_protocol::page::FrameId, Page)>, CommandError> {
    if page.frames().await.map_err(cdp_error)?.contains(&frame) {
        return Ok(Some((frame, page.clone())));
    }
    let Some(browser) = browser else {
        return Ok(None);
    };
    let target = chromiumoxide::cdp::browser_protocol::target::TargetId::new(frame.inner().clone());
    Ok(browser
        .get_page(target)
        .await
        .ok()
        .map(|session| (frame, session)))
}

/// Every frame id in the page's frame tree.
pub(crate) async fn page_frame_ids(
    page: &Page,
) -> Result<HashSet<chromiumoxide::cdp::browser_protocol::page::FrameId>, CommandError> {
    let tree = page
        .execute(GetFrameTreeParams::default())
        .await
        .map_err(cdp_error)?
        .result
        .frame_tree;
    let mut ids = HashSet::new();
    let mut pending = vec![tree];
    while let Some(node) = pending.pop() {
        ids.insert(node.frame.id.clone());
        pending.extend(node.child_frames.unwrap_or_default());
    }
    Ok(ids)
}

/// The `find`-by-id helper plus the shadow_hosts descent loop, shared by the
/// document-rooted and closed-root-rooted expression builders. Leaves a mutable
/// `root` binding for the caller's `operation`, already descended through the
/// open shadow hosts named in `shadow_hosts`. Open roots nested inside a closed
/// root resolve here too; only the outermost closed boundary needs CDP.
fn descend_shadow_hosts_snippet(shadow_hosts: &[String]) -> Result<String, CommandError> {
    let hosts = serde_json::to_string(shadow_hosts)
        .map_err(|error| target_error(ErrorCode::InvalidRequest, error))?;
    Ok(format!(
        r#"const find=(root,id)=>{{for(const el of root.querySelectorAll('*')){{if(el.getAttribute('data-bobby-target')===id)return el;if(el.shadowRoot){{const found=find(el.shadowRoot,id);if(found)return found}}}}return null}};for(const id of {hosts}){{const host=find(root,id);if(!host||!host.shadowRoot)return false;root=host.shadowRoot}}"#
    ))
}

/// Builds a bare expression suitable for `Runtime.evaluate`, rooted at
/// `document`.
fn scope_expression(shadow_hosts: &[String], operation: &str) -> Result<String, CommandError> {
    let descend = descend_shadow_hosts_snippet(shadow_hosts)?;
    Ok(format!(
        "(()=>{{let root=document;{descend};{operation}}})()"
    ))
}

/// Builds a `function() {...}` declaration suitable for
/// `Runtime.callFunctionOn` against a closed-root `Element`'s object id,
/// rooted at `this`.
fn closed_root_scope_expression(
    shadow_hosts: &[String],
    operation: &str,
) -> Result<String, CommandError> {
    let descend = descend_shadow_hosts_snippet(shadow_hosts)?;
    Ok(format!("function(){{let root=this;{descend};{operation}}}"))
}

fn scoped_expression(
    scope: &LocatorScope,
    shadow_hosts: &[String],
    operation: &str,
) -> Result<String, CommandError> {
    match scope {
        LocatorScope::Context(_) => scope_expression(shadow_hosts, operation),
        LocatorScope::ClosedRoot(_) => closed_root_scope_expression(shadow_hosts, operation),
    }
}

fn locator_expression(locator: &JsLocator, operation: &str) -> Result<String, CommandError> {
    let id = serde_json::to_string(&locator.id)
        .map_err(|error| target_error(ErrorCode::InvalidRequest, error))?;
    let full_operation = format!(
        "const el=find(root,{id});if(!el||!el.isConnected)throw new Error('target detached');{operation}"
    );
    scoped_expression(&locator.scope, &locator.shadow_hosts, &full_operation)
}

/// Evaluates `expression` (already shaped by [`scoped_expression`]/
/// [`locator_expression`] for the given scope) and deserializes the result.
async fn eval_scoped<T: DeserializeOwned>(
    page: &Page,
    scope: &LocatorScope,
    expression: String,
) -> Result<T, CommandError> {
    match scope {
        LocatorScope::Context(context_id) => {
            evaluate_in_context(page, *context_id, expression).await
        }
        LocatorScope::ClosedRoot(element) => {
            let value = element
                .call_js_fn_by_value(expression, true)
                .await
                .map_err(cdp_error)?;
            serde_json::from_value(value)
                .map_err(|error| target_error(ErrorCode::InvalidRequest, error))
        }
    }
}

/// Resolves `expression` to a live object handle (rather than an inlined
/// JSON value), used where a `RemoteObjectId`/`NodeId` is needed for a
/// follow-up CDP call (e.g. `DOM.requestNode`, `DOM.describeNode`).
async fn resolve_object_id_scoped(
    page: &Page,
    scope: &LocatorScope,
    expression: String,
) -> Result<RemoteObjectId, CommandError> {
    match scope {
        LocatorScope::Context(context_id) => {
            let mut params = EvaluateParams::new(expression);
            params.context_id = *context_id;
            params.return_by_value = Some(false);
            page.evaluate(params)
                .await
                .map_err(cdp_error)?
                .object()
                .object_id
                .clone()
                .ok_or_else(|| target_error(ErrorCode::TargetDetached, "target has no live object"))
        }
        LocatorScope::ClosedRoot(element) => element
            .call_js_fn(expression, false)
            .await
            .map_err(cdp_error)?
            .result
            .object_id
            .clone()
            .ok_or_else(|| target_error(ErrorCode::TargetDetached, "target has no live object")),
    }
}

/// Resolves the object id of the current scope's root (`document`, the
/// `root` reached after descending `shadow_hosts`, or `this` for a
/// closed-root scope), used as the anchor for closed-shadow-root discovery.
async fn scope_root_object_id(
    page: &Page,
    scope: &LocatorScope,
    shadow_hosts: &[String],
) -> Result<RemoteObjectId, CommandError> {
    match scope {
        LocatorScope::Context(_) if shadow_hosts.is_empty() => {
            resolve_object_id_scoped(page, scope, "document".to_string()).await
        }
        LocatorScope::Context(_) => {
            resolve_object_id_scoped(page, scope, scope_expression(shadow_hosts, "return root")?)
                .await
        }
        LocatorScope::ClosedRoot(element) if shadow_hosts.is_empty() => {
            Ok(element.remote_object_id.clone())
        }
        LocatorScope::ClosedRoot(_) => {
            resolve_object_id_scoped(
                page,
                scope,
                closed_root_scope_expression(shadow_hosts, "return root")?,
            )
            .await
        }
    }
}

async fn evaluate_in_context<T: DeserializeOwned>(
    page: &Page,
    context_id: Option<ExecutionContextId>,
    expression: String,
) -> Result<T, CommandError> {
    let mut params = EvaluateParams::new(expression);
    params.context_id = context_id;
    params.return_by_value = Some(true);
    page.evaluate(params)
        .await
        .map_err(cdp_error)?
        .into_value()
        .map_err(|error| target_error(ErrorCode::BrowserCommandFailed, error))
}

fn selector_evidence(page_id: &PageId, selector: &str) -> Evidence {
    Evidence::Resolution {
        target: Box::new(TargetSpec {
            css: Some(selector.into()),
            ..TargetSpec::default()
        }),
        fingerprint: Box::new(TargetFingerprint {
            page_id: page_id.clone(),
            frame: None,
            role: None,
            name: None,
            stable_attributes: BTreeMap::new(),
        }),
        candidates: Vec::new(),
        best_match_authorized: false,
    }
}

/// A dead browser target rewritten to this message must still be recognized
/// by `worker_pool::is_dead_worker_error` so click/type/inspect trigger the
/// same revive path as a raw CDP "receiver is gone"; matching on
/// `ErrorCode::TargetDetached` instead would also catch the stale-element
/// case below and relaunch the browser for a handle that just needs a fresh
/// resolve.
pub(crate) const TARGET_GONE_MESSAGE: &str =
    "the browser target is gone (crashed or closed); re-list pages or recover the session before retrying";

fn cdp_error(error: chromiumoxide::error::CdpError) -> CommandError {
    let message = error.to_string();
    // A resolved node id expires when the page re-renders; that is a stale
    // target, not a driver fault — agents repair it with a fresh snapshot.
    if message.contains("Could not find node with given id") {
        return CommandError {
            code: ErrorCode::TargetNotFound,
            message: "resolved node is stale (page re-rendered); take a fresh snapshot and re-resolve the target".into(),
            layer: ErrorLayer::Driver,
            retryable: false,
        };
    }
    // The page's CDP target died (crash or close). Hammering the dead pipe
    // produced the cascade of identical driver errors seen in benchmark
    // runs; say what happened and what to check instead.
    if message.contains("receiver is gone")
        || message.contains("session closed")
        || message.contains("oneshot canceled")
    {
        return CommandError {
            code: ErrorCode::TargetDetached,
            message: TARGET_GONE_MESSAGE.into(),
            layer: ErrorLayer::Driver,
            retryable: true,
        };
    }
    target_error(ErrorCode::BrowserCommandFailed, message)
}

fn target_error(code: ErrorCode, message: impl std::fmt::Display) -> CommandError {
    CommandError {
        code,
        message: message.to_string(),
        layer: ErrorLayer::Driver,
        retryable: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn browser_candidate(
        css: Option<&str>,
        attributes: BTreeMap<String, String>,
    ) -> BrowserCandidate {
        BrowserCandidate {
            id: "1".into(),
            css: css.map(str::to_owned),
            tag: Some("input".into()),
            test_id: None,
            role: Some("textbox".into()),
            name: Some("Name".into()),
            label: None,
            text: String::new(),
            attributes,
            attached: true,
            visible: true,
            enabled: true,
        }
    }

    #[test]
    fn into_candidate_strips_the_per_gather_tracking_attribute_and_selector() {
        let mut attributes = BTreeMap::new();
        attributes.insert("data-bobby-target".to_owned(), "bobby-2-7".to_owned());
        attributes.insert("type".to_owned(), "text".to_owned());
        let raw = browser_candidate(Some("[data-bobby-target=\"bobby-2-7\"]"), attributes);

        let candidate = into_candidate(raw);

        assert_eq!(candidate.css, None);
        assert_eq!(
            candidate.attributes.get("data-bobby-target"),
            None,
            "tracking id is reassigned on every scan and must not be used for matching"
        );
        assert_eq!(candidate.attributes.get("type"), Some(&"text".to_owned()));
    }

    #[test]
    fn into_candidate_preserves_a_real_element_id_selector() {
        let attributes = BTreeMap::new();
        let raw = browser_candidate(Some("#resume-upload"), attributes);

        let candidate = into_candidate(raw);

        assert_eq!(candidate.css, Some("#resume-upload".to_owned()));
    }

    fn cdp_node(
        id: i64,
        shadow_root_type: Option<ShadowRootType>,
        children: Vec<CdpNode>,
        shadow_roots: Vec<CdpNode>,
        content_document: Option<CdpNode>,
    ) -> CdpNode {
        use chromiumoxide::cdp::browser_protocol::dom::BackendNodeId;

        let mut builder = CdpNode::builder()
            .node_id(chromiumoxide::cdp::browser_protocol::dom::NodeId::new(id))
            .backend_node_id(BackendNodeId::new(id))
            .node_type(1)
            .node_name("DIV")
            .local_name("div")
            .node_value("")
            .childrens(children)
            .shadow_roots(shadow_roots);
        if let Some(kind) = shadow_root_type {
            builder = builder.shadow_root_type(kind);
        }
        if let Some(document) = content_document {
            builder = builder.content_document(document);
        }
        builder.build().expect("synthetic CDP node")
    }

    #[test]
    fn collect_closed_from_json_skips_iframe_content_document() {
        let tree = serde_json::json!({
            "backendNodeId": 1,
            "children": [{
                "backendNodeId": 5,
                "shadowRoots": [{
                    "backendNodeId": 4,
                    "shadowRootType": "closed"
                }]
            }, {
                "backendNodeId": 6,
                "contentDocument": {
                    "backendNodeId": 98,
                    "shadowRoots": [{
                        "backendNodeId": 99,
                        "shadowRootType": "closed"
                    }]
                }
            }]
        });
        let mut found = Vec::new();
        collect_closed_from_json(&tree, &mut found);
        assert_eq!(found, vec![BackendNodeId::new(4)]);
    }

    #[test]
    fn collect_closed_shadow_root_ids_finds_closed_roots_and_skips_open_and_iframe_content() {
        // Tree:
        //   document
        //   ├─ host-open  → open shadow (id 2) with child
        //   ├─ host-closed → closed shadow (id 4)
        //   └─ iframe      → content_document with a closed shadow (id 99) that must be ignored
        let open_root = cdp_node(2, Some(ShadowRootType::Open), Vec::new(), Vec::new(), None);
        let closed_root = cdp_node(
            4,
            Some(ShadowRootType::Closed),
            Vec::new(),
            Vec::new(),
            None,
        );
        let nested_closed_in_iframe = cdp_node(
            99,
            Some(ShadowRootType::Closed),
            Vec::new(),
            Vec::new(),
            None,
        );
        let iframe_document = cdp_node(
            98,
            None,
            vec![cdp_node(
                97,
                None,
                Vec::new(),
                vec![nested_closed_in_iframe],
                None,
            )],
            Vec::new(),
            None,
        );
        let tree = cdp_node(
            1,
            None,
            vec![
                cdp_node(3, None, Vec::new(), vec![open_root], None),
                cdp_node(5, None, Vec::new(), vec![closed_root], None),
                cdp_node(6, None, Vec::new(), Vec::new(), Some(iframe_document)),
            ],
            Vec::new(),
            None,
        );

        let mut found = Vec::new();
        collect_closed_shadow_root_ids(&tree, &mut found);

        assert_eq!(found, vec![BackendNodeId::new(4)]);
    }

    #[test]
    fn collect_closed_shadow_root_ids_walks_nested_closed_roots() {
        let inner_closed = cdp_node(
            20,
            Some(ShadowRootType::Closed),
            Vec::new(),
            Vec::new(),
            None,
        );
        let outer_closed = cdp_node(
            10,
            Some(ShadowRootType::Closed),
            vec![cdp_node(11, None, Vec::new(), vec![inner_closed], None)],
            Vec::new(),
            None,
        );
        let tree = cdp_node(
            1,
            None,
            vec![cdp_node(2, None, Vec::new(), vec![outer_closed], None)],
            Vec::new(),
            None,
        );

        let mut found = Vec::new();
        collect_closed_shadow_root_ids(&tree, &mut found);

        assert_eq!(found, vec![BackendNodeId::new(10), BackendNodeId::new(20)]);
    }

    #[test]
    fn pierce_followup_backend_ids_lists_element_children_and_skips_iframe_documents() {
        let iframe_document = cdp_node(
            98,
            None,
            vec![cdp_node(97, None, Vec::new(), Vec::new(), None)],
            Vec::new(),
            None,
        );
        let closed_root = cdp_node(
            4,
            Some(ShadowRootType::Closed),
            Vec::new(),
            Vec::new(),
            None,
        );
        let tree = cdp_node(
            1,
            None,
            vec![
                cdp_node(5, None, Vec::new(), vec![closed_root], None),
                cdp_node(6, None, Vec::new(), Vec::new(), Some(iframe_document)),
            ],
            Vec::new(),
            None,
        );
        let followups = pierce_followup_backend_ids(&tree);
        assert_eq!(
            followups,
            vec![BackendNodeId::new(5), BackendNodeId::new(6)]
        );
    }

    #[test]
    fn dismantle_cdp_node_unwinds_nested_and_iframe_trees() {
        let iframe_document = cdp_node(
            98,
            None,
            vec![cdp_node(
                97,
                None,
                Vec::new(),
                vec![cdp_node(
                    99,
                    Some(ShadowRootType::Closed),
                    Vec::new(),
                    Vec::new(),
                    None,
                )],
                None,
            )],
            Vec::new(),
            None,
        );
        let tree = cdp_node(
            1,
            None,
            vec![cdp_node(
                6,
                None,
                Vec::new(),
                Vec::new(),
                Some(iframe_document),
            )],
            Vec::new(),
            None,
        );
        dismantle_cdp_node(tree);
    }

    #[test]
    fn empty_target_fields_are_rejected_before_resolution() {
        let err = validate_target_spec(&TargetSpec {
            accessible_name: Some(String::new()),
            role: Some("main".into()),
            ..TargetSpec::default()
        })
        .expect_err("empty accessibleName must fail");
        assert_eq!(err.code, ErrorCode::InvalidRequest);
        assert!(err.message.contains("accessibleName"));

        let err = validate_target_spec(&TargetSpec {
            frame_path: vec![Box::new(TargetSpec {
                css: Some("   ".into()),
                ..TargetSpec::default()
            })],
            ..TargetSpec::default()
        })
        .expect_err("empty nested css must fail");
        assert!(err.message.contains("css"));
    }

    #[test]
    fn unnamed_iframe_hop_may_carry_an_empty_accessible_name() {
        validate_target_spec(&TargetSpec {
            frame_path: vec![Box::new(TargetSpec {
                role: Some("iframe".into()),
                accessible_name: Some(String::new()),
                ..TargetSpec::default()
            })],
            ..TargetSpec::default()
        })
        .expect("unnamed iframe hop");

        let err = validate_target_spec(&TargetSpec {
            shadow_path: vec![Box::new(TargetSpec {
                role: Some("group".into()),
                accessible_name: Some(String::new()),
                ..TargetSpec::default()
            })],
            ..TargetSpec::default()
        })
        .expect_err("shadow hop stays strict");
        assert!(err.message.contains("accessibleName"));
    }

    #[test]
    fn implicit_role_covers_headings_lists_images_and_tables() {
        let operation = candidate_collector_operation(1).expect("collector operation");
        for mapping in [
            "if(/^H[1-6]$/.test(el.tagName))return 'heading';",
            "if(el.tagName==='UL'||el.tagName==='OL'||el.tagName==='MENU'||el.tagName==='DL')return 'list';",
            "if(el.tagName==='LI')return 'listitem';",
            "if(el.tagName==='IMG')return 'img';",
            "if(el.tagName==='OPTION')return 'option';",
            "if(el.tagName==='TABLE')return 'table';",
            "if(el.tagName==='TR')return 'row';",
            "if(el.tagName==='TD')return 'cell';",
            "if(el.tagName==='TH')return 'columnheader';",
            "if(el.tagName==='P')return 'paragraph';",
            "if(el.tagName==='HR')return 'separator';",
            "if(el.tagName==='FIELDSET'||el.tagName==='DETAILS')return 'group';",
            "if(el.tagName==='ASIDE')return 'complementary';",
            "if(el.tagName==='HEADER')return 'banner';",
            "if(el.tagName==='FOOTER')return 'contentinfo';",
            "if(el.tagName==='PROGRESS')return 'progressbar';",
            "if(el.tagName==='METER')return 'meter';",
            "if(el.tagName==='OUTPUT')return 'status';",
            "if(el.tagName==='SUMMARY')return 'button';",
            "if(el.tagName==='FIGURE')return 'figure';",
            "if(el.tagName==='BLOCKQUOTE')return 'blockquote';",
            "if(el.tagName==='DT')return 'term';",
            "if(el.tagName==='DD')return 'definition';",
            "if(el.tagName==='AREA'&&el.hasAttribute('href'))return 'link';",
            "if(el.tagName==='TIME')return 'time';",
        ] {
            assert!(
                operation.contains(mapping),
                "missing implicit role mapping: {mapping}"
            );
        }
    }

    fn ambiguous_candidate(
        role: Option<&str>,
        name: Option<&str>,
        _score: i32,
    ) -> BrowserCandidate {
        BrowserCandidate {
            id: "1".into(),
            css: None,
            tag: Some("button".into()),
            test_id: None,
            role: role.map(str::to_owned),
            name: name.map(str::to_owned),
            label: None,
            text: String::new(),
            attributes: BTreeMap::new(),
            attached: true,
            visible: true,
            enabled: true,
        }
    }

    /// The `TargetAmbiguous` message is machine-readable repair, not a Rust
    /// `Debug` dump: contenders read like the resolution evidence (role,
    /// quoted name, score), and the message ends with the narrowing step.
    #[test]
    fn ambiguous_resolution_message_names_role_name_score_and_the_narrowing_step() {
        let target = TargetSpec {
            role: Some("button".into()),
            ..TargetSpec::default()
        };
        let raw = vec![
            ambiguous_candidate(Some("button"), Some("Submit"), 80),
            ambiguous_candidate(Some("button"), Some("Submit now"), 80),
        ];
        let error = choose(
            &target,
            &raw.into_iter().map(into_candidate).collect::<Vec<_>>(),
            true,
        )
        .expect_err("two equal candidates");
        assert_eq!(error.code, ErrorCode::TargetAmbiguous);
        assert!(
            error.message.contains("button \"Submit\" score=")
                && error.message.contains("button \"Submit now\" score="),
            "message names each contender semantically: {}",
            error.message
        );
        assert!(
            !error.message.contains("Some(") && !error.message.contains("None("),
            "message must not leak Rust Debug formatting: {}",
            error.message
        );
        assert!(
            error.message.contains("Narrow the target"),
            "message carries the narrowing repair: {}",
            error.message
        );
    }
}
