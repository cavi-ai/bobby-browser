use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use chromiumoxide::browser::Browser;
use chromiumoxide::cdp::browser_protocol::dom::{
    BackendNodeId, DescribeNodeParams, Node as CdpNode, RequestNodeParams, SetFileInputFilesParams,
    ShadowRootType,
};
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, GetFrameTreeParams, Viewport,
};
use chromiumoxide::cdp::browser_protocol::target::GetTargetsParams;
use chromiumoxide::cdp::js_protocol::runtime::{
    EvaluateParams, ExecutionContextId, RemoteObjectId,
};
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::{Element, Page};
use dom_engine::{
    resolve_candidates, Candidate, CandidateState, ResolutionDecision, ResolutionPolicy,
};
use serde::de::DeserializeOwned;
use serde::Deserialize;
use types::{CommandError, ErrorCode, ErrorLayer, Evidence, PageId, TargetFingerprint, TargetSpec};

static TARGET_SCOPE: AtomicU64 = AtomicU64::new(1);

pub struct ResolvedTarget {
    native: Option<Element>,
    locator: JsLocator,
    execution_page: Option<Page>,
    pub evidence: Evidence,
}

/// Where a [`JsLocator`]'s `find(root, id)` lookups are rooted.
///
/// `ClosedRoot` anchors execution at a specific closed shadow root `Element`
/// handle (resolved via CDP-native traversal, see [`discover_closed_shadow_roots`]
/// and [`discover_closed_root_for_candidate`]) instead of a document/frame
/// execution context, since `document.querySelector` and friends cannot see
/// into closed shadow trees but a live handle already inside one can.
#[derive(Clone)]
enum LocatorScope {
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
    fn execution_page<'a>(&'a self, fallback: &'a Page) -> &'a Page {
        self.execution_page.as_ref().unwrap_or(fallback)
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

    pub async fn value(&self, page: &Page) -> Result<Option<String>, CommandError> {
        if let Some(element) = &self.native {
            return element.string_property("value").await.map_err(cdp_error);
        }
        self.eval(page, "return el.value || ''").await.map(Some)
    }

    pub async fn outer_html(&self, page: &Page) -> Result<Option<String>, CommandError> {
        if let Some(element) = &self.native {
            return element.outer_html().await.map_err(cdp_error);
        }
        self.eval(page, "return el.outerHTML || ''").await.map(Some)
    }

    pub async fn visible(&self, page: &Page) -> Result<bool, CommandError> {
        self.eval(page, "const s=getComputedStyle(el),r=el.getBoundingClientRect(); return s.visibility!=='hidden'&&s.display!=='none'&&r.width>0&&r.height>0")
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

    pub async fn type_text(
        &self,
        page: &Page,
        value: &str,
        clear_first: bool,
    ) -> Result<(), CommandError> {
        if let Some(element) = &self.native {
            element.click().await.map_err(cdp_error)?;
            if clear_first {
                element
                    .call_js_fn(
                        "function() { this.value = ''; this.dispatchEvent(new Event('input', { bubbles: true })); }",
                        false,
                    )
                    .await
                    .map_err(cdp_error)?;
            }
            element.type_str(value).await.map_err(cdp_error)?;
            return Ok(());
        }
        let value = serde_json::to_string(value).map_err(|error| {
            target_error(
                ErrorCode::InvalidRequest,
                format!("invalid input value: {error}"),
            )
        })?;
        let clear = if clear_first { "el.value=''" } else { "" };
        self.eval::<bool>(
            page,
            &format!("{clear}; el.focus(); el.value += {value}; el.dispatchEvent(new Event('input',{{bubbles:true}})); el.dispatchEvent(new Event('change',{{bubbles:true}})); return true"),
        )
        .await?;
        Ok(())
    }

    pub async fn is_select(&self, page: &Page) -> Result<bool, CommandError> {
        self.eval(page, "return el instanceof HTMLSelectElement")
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
    /// Select a native option by exact value and reread the committed value.
    pub async fn select_option(&self, page: &Page, value: &str) -> Result<String, CommandError> {
        let value = serde_json::to_string(value).map_err(|error| {
            target_error(
                ErrorCode::InvalidRequest,
                format!("invalid select value: {error}"),
            )
        })?;
        let script = format!(
            "if (!(el instanceof HTMLSelectElement)) throw new Error('resolved control is not a select'); const matches=[...el.options].filter(option=>option.value==={value}); if(matches.length!==1||matches[0].disabled) throw new Error('select option is missing, ambiguous, or disabled'); el.value={value}; el.dispatchEvent(new Event('input',{{bubbles:true}})); el.dispatchEvent(new Event('change',{{bubbles:true}})); return el.value"
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
        if let Some(element) = &self.native {
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
        let node_id = page
            .execute(RequestNodeParams::new(object_id))
            .await
            .map_err(cdp_error)?
            .result
            .node_id;
        page.execute(
            SetFileInputFilesParams::builder()
                .files(paths)
                .node_id(node_id)
                .build()
                .map_err(|error| target_error(ErrorCode::BrowserCommandFailed, error))?,
        )
        .await
        .map_err(cdp_error)?;
        Ok(())
    }

    async fn eval<T: DeserializeOwned>(
        &self,
        page: &Page,
        operation: &str,
    ) -> Result<T, CommandError> {
        let page = self.execution_page(page);
        let expression = locator_expression(&self.locator, operation)?;
        eval_scoped(page, &self.locator.scope, expression).await
    }
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
    browser: Option<&mut Browser>,
) -> Result<ResolvedTarget, CommandError> {
    resolve_target_with_visibility(page_id, page, selector, target, true, browser).await
}

/// Gather DOM candidates for intent resolution without choosing a match.
pub async fn gather_candidates(
    page: &Page,
    target: &TargetSpec,
    browser: Option<&mut Browser>,
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
    Ok(raw.into_iter().map(into_candidate).collect())
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
    mut browser: Option<&mut Browser>,
) -> Result<TargetScope, CommandError> {
    if target.frame_path.len() > 8 || target.shadow_path.len() > 8 {
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

    for (index, frame_target) in target.frame_path.iter().enumerate() {
        let raw = collect_candidates(&execution_page, context_id, &shadow_hosts, scope_id).await?;
        let (candidate, evidence, _) = choose(frame_target, raw, true)?;
        frame_trace.push(evidence);
        let deadline = Instant::now() + Duration::from_secs(2);
        let (child, oopif) = loop {
            if let Some(child) = find_child_frame(&execution_page, &frame_id, &candidate).await? {
                break (Some(child), None);
            }
            let found = match browser.as_deref_mut() {
                Some(active_browser) => find_oopif(active_browser, &frame_id, &candidate).await?,
                None => None,
            };
            if found.is_some() || Instant::now() >= deadline {
                break (None, found);
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        };
        if let Some(child) = child {
            frame_id = child.clone();
            context_id = execution_page
                .frame_execution_context(child)
                .await
                .map_err(cdp_error)?;
        } else {
            let (oopif, oopif_frame) = oopif.ok_or_else(|| {
                target_error(
                    ErrorCode::FrameNotFound,
                    format!("frame path component {index} did not map to a child frame"),
                )
            })?;
            execution_page = oopif;
            frame_id = oopif_frame.clone();
            context_id = execution_page
                .frame_execution_context(oopif_frame)
                .await
                .map_err(cdp_error)?;
        }
        shadow_hosts.clear();
    }

    let mut closed_root: Option<Arc<Element>> = None;

    for (index, host_target) in target.shadow_path.iter().enumerate() {
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
        let (candidate, evidence, _) = choose(host_target, raw, true)?;
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

        // The candidate has no *open* root; check whether CDP-native pierce
        // discovery can see a *closed* one directly on this host before
        // giving up (see module docs on `discover_closed_root_for_candidate`).
        let discovered = discover_closed_root_for_candidate(
            &execution_page,
            &scope_ref,
            &shadow_hosts,
            &candidate.id,
        )
        .await?;
        let Some(root_backend_id) = discovered else {
            return Err(target_error(
                ErrorCode::ShadowRootUnavailable,
                format!("shadow path component {index} has no attached shadow root"),
            ));
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
    })
}

fn into_candidate(mut item: BrowserCandidate) -> Candidate {
    // `data-bobby-target` is a per-gather instrumentation id we inject to
    // locate elements within a single scan; it is re-assigned on every scan,
    // so it must never be treated as a stable identity attribute or matched
    // against in a later resolution pass.
    item.attributes.remove("data-bobby-target");
    let css = item.css.filter(|css| !css.contains("data-bobby-target"));
    Candidate {
        id: item.id,
        css,
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
    }
}

pub async fn resolve_target_with_visibility(
    page_id: &PageId,
    page: &Page,
    selector: &str,
    target: Option<&TargetSpec>,
    require_visible: bool,
    browser: Option<&mut Browser>,
) -> Result<ResolvedTarget, CommandError> {
    let Some(target) = target else {
        let element = page.find_element(selector).await.map_err(cdp_error)?;
        return Ok(ResolvedTarget {
            native: Some(element),
            locator: JsLocator {
                scope: LocatorScope::Context(None),
                shadow_hosts: Vec::new(),
                id: String::new(),
            },
            execution_page: None,
            evidence: selector_evidence(page_id, selector),
        });
    };

    let scope = open_target_scope(page, target, browser).await?;
    let base_scope = scope.locator_scope();
    let deadline = Instant::now() + Duration::from_secs(2);
    let (candidate, evidence, best_match_authorized, owner) = loop {
        let (raw, owners) = collect_candidates_merged(
            &scope.execution_page,
            &base_scope,
            &scope.shadow_hosts,
            scope.scope_id,
        )
        .await?;
        match choose(target, raw, require_visible) {
            Ok((candidate, evidence, best_match_authorized)) => {
                let owner = owners.get(&candidate.id).cloned();
                break (candidate, evidence, best_match_authorized, owner);
            }
            Err(error) if error.code == ErrorCode::TargetNotFound && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) => return Err(error),
        }
    };
    // Candidates gathered ambiently from a closed shadow root (see
    // `collect_candidates_merged`) must be located relative to that root's
    // own element handle, not the outer document/frame context.
    let (locator_scope, locator_shadow_hosts) = match &owner {
        Some(element) => (LocatorScope::ClosedRoot(Arc::clone(element)), Vec::new()),
        None => (base_scope.clone(), scope.shadow_hosts.clone()),
    };
    let locator = JsLocator {
        scope: locator_scope,
        shadow_hosts: locator_shadow_hosts,
        id: candidate.id.clone(),
    };
    let native = if target.frame_path.is_empty() && target.shadow_path.is_empty() {
        match (&owner, candidate.css.as_deref()) {
            (Some(element), Some(css)) => element.find_element(css).await.ok(),
            (None, Some(css)) => scope.execution_page.find_element(css).await.ok(),
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
    Ok(ResolvedTarget {
        native,
        locator,
        execution_page: (scope.execution_page.target_id() != page.target_id())
            .then_some(scope.execution_page),
        evidence: Evidence::Resolution {
            target: Box::new(target.clone()),
            fingerprint: Box::new(fingerprint),
            candidates: frame_trace,
            best_match_authorized,
        },
    })
}

fn choose(
    target: &TargetSpec,
    raw: Vec<BrowserCandidate>,
    require_visible: bool,
) -> Result<(Candidate, types::CandidateEvidence, bool), CommandError> {
    let candidates = raw.into_iter().map(into_candidate).collect::<Vec<_>>();
    let policy = ResolutionPolicy {
        require_visible,
        ..ResolutionPolicy::default()
    };
    match resolve_candidates(target, &candidates, &policy)
        .map_err(|error| target_error(ErrorCode::InvalidRequest, error))?
    {
        ResolutionDecision::NotFound => Err(target_error(
            ErrorCode::TargetNotFound,
            "no target candidate matched",
        )),
        ResolutionDecision::Ambiguous { candidates } => {
            let summary = candidates
                .iter()
                .take(10)
                .map(|candidate| format!("role={:?},score={}", candidate.role, candidate.score))
                .collect::<Vec<_>>()
                .join(";");
            Err(target_error(
                ErrorCode::TargetAmbiguous,
                format!("target is ambiguous: {summary}"),
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

/// Same gather as [`collect_candidates`], but rooted at an already-resolved
/// closed shadow root `Element` instead of `document` — a closed root's
/// contents are invisible to plain `document.querySelector`, but this
/// existing collector script works unchanged once bound as `this`.
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

/// Gathers candidates at the given scope, then additionally discovers and
/// gathers from every closed shadow root reachable within that scope (see
/// [`discover_closed_shadow_roots`]), merging both into one candidate list
/// so ordinary purpose-based matching sees inside closed shadow DOM the same
/// way it already sees inside open shadow DOM. Returns the merged
/// candidates alongside a map from candidate id to the closed-root `Element`
/// it was gathered from (only populated for closed-root-origin candidates),
/// so a winning candidate can be relocated for later Act calls.
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

/// Discovers every closed shadow root reachable within the given scope via
/// `DOM.describeNode(pierce: true)`, resolving each into a live `Element`
/// handle. CDP can see closed roots at the backend level regardless of the
/// JS-level "closed" restriction — this is exactly how DevTools itself
/// inspects them — so this requires no page-prototype patching.
async fn discover_closed_shadow_roots(
    page: &Page,
    scope: &LocatorScope,
    shadow_hosts: &[String],
) -> Result<Vec<Arc<Element>>, CommandError> {
    let object_id = scope_root_object_id(page, scope, shadow_hosts).await?;
    let described = page
        .execute(
            DescribeNodeParams::builder()
                .object_id(object_id)
                .depth(-1)
                .pierce(true)
                .build(),
        )
        .await
        .map_err(cdp_error)?;
    let mut backend_ids = Vec::new();
    collect_closed_shadow_root_ids(&described.result.node, &mut backend_ids);
    let mut roots = Vec::with_capacity(backend_ids.len());
    for backend_node_id in backend_ids {
        let element = page
            .element_from_backend_node_id(backend_node_id)
            .await
            .map_err(cdp_error)?;
        roots.push(Arc::new(element));
    }
    Ok(roots)
}

/// Walks a `DOM.describeNode(pierce: true)` tree collecting the
/// `backendNodeId` of every *closed* shadow root found. Uses backend ids
/// (not frontend `nodeId`s) because pierce trees return `nodeId`s that are
/// not registered with the frontend. Deliberately never descends into
/// `content_document` (iframe content), preserving the existing "frames
/// require an explicit `frame_path`" boundary — only `.children` (same-tree
/// DOM descendants) and `.shadow_roots` are followed.
fn collect_closed_shadow_root_ids(node: &CdpNode, out: &mut Vec<BackendNodeId>) {
    if let Some(shadow_roots) = &node.shadow_roots {
        for root in shadow_roots {
            if matches!(root.shadow_root_type, Some(ShadowRootType::Closed)) {
                out.push(root.backend_node_id);
            }
            collect_closed_shadow_root_ids(root, out);
        }
    }
    if let Some(children) = &node.children {
        for child in children {
            collect_closed_shadow_root_ids(child, out);
        }
    }
}

/// Checks whether a specific already-resolved candidate (identified by its
/// per-gather `data-bobby-target` id) itself hosts a closed shadow root,
/// used by the explicit `shadow_path` fallback when the open-root check
/// (`!!root`) fails. Returns the closed root's `BackendNodeId` if one is
/// attached.
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
                .depth(1)
                .pierce(true)
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
    let prefix = format!("bobby-{scope}-");
    let prefix = serde_json::to_string(&prefix)
        .map_err(|error| target_error(ErrorCode::InvalidRequest, error))?;
    Ok(format!(
        r#"let n=0,out=[];
const labelledBy=el=>(el.getAttribute('aria-labelledby')||'').split(/\s+/).filter(Boolean).map(id=>el.ownerDocument.getElementById(id)?.innerText?.trim()||'').filter(Boolean).join(' ')||null;
const implicitRole=el=>{{if(el.tagName==='BUTTON')return 'button';if(el.tagName==='A'&&el.hasAttribute('href'))return 'link';if(el.tagName==='IFRAME')return 'iframe';if(el.tagName==='TEXTAREA'||el.isContentEditable)return 'textbox';if(el.tagName==='SELECT')return el.multiple?'listbox':'combobox';if(el.tagName!=='INPUT')return null;const type=(el.type||'text').toLowerCase();if(['button','submit','reset','image'].includes(type))return 'button';if(type==='checkbox')return 'checkbox';if(type==='radio')return 'radio';if(type==='range')return 'slider';if(type==='number')return 'spinbutton';if(type==='search')return 'searchbox';return type==='hidden'?null:'textbox'}};
const visit=current=>{{for(const el of current.querySelectorAll('*')){{const id={prefix}+(++n);el.setAttribute('data-bobby-target',id);const style=getComputedStyle(el),rect=el.getBoundingClientRect();const label=el.labels&&el.labels.length?Array.from(el.labels).map(x=>x.innerText.trim()).filter(Boolean).join(' '):null;const role=el.getAttribute('role')||implicitRole(el);const name=el.getAttribute('aria-label')||labelledBy(el)||label||el.innerText?.trim()||null;const attributes={{}};for(const a of el.attributes)if(['name','type','src','href','placeholder','autocomplete','pattern','min','max','step','multiple'].includes(a.name)||a.name.startsWith('data-'))attributes[a.name]=a.value;for(const booleanName of ['required','readonly','checked','multiple'])if(el[booleanName]===true)attributes[booleanName]='true';const css=el.id?`#${{CSS.escape(el.id)}}`:`[data-bobby-target="${{id}}"]`;out.push({{id,css,testId:el.getAttribute('data-testid'),role,name,label,text:(el.innerText||el.value||'').trim(),attributes,attached:el.isConnected,visible:style.visibility!=='hidden'&&style.display!=='none'&&rect.width>0&&rect.height>0,enabled:!el.disabled&&el.getAttribute('aria-disabled')!=='true'&&!el.closest('fieldset[disabled]')}});if(el.shadowRoot)visit(el.shadowRoot)}}}};visit(root);return out"#
    ))
}

async fn find_child_frame(
    page: &Page,
    parent: &chromiumoxide::cdp::browser_protocol::page::FrameId,
    candidate: &Candidate,
) -> Result<Option<chromiumoxide::cdp::browser_protocol::page::FrameId>, CommandError> {
    let wanted_name = candidate.attributes.get("name");
    let wanted_src = candidate.attributes.get("src");
    let mut children = Vec::new();
    for frame in page.frames().await.map_err(cdp_error)? {
        if page
            .frame_parent(frame.clone())
            .await
            .map_err(cdp_error)?
            .as_ref()
            != Some(parent)
        {
            continue;
        }
        children.push(frame.clone());
        let name = page.frame_name(frame.clone()).await.map_err(cdp_error)?;
        let url = page.frame_url(frame.clone()).await.map_err(cdp_error)?;
        if wanted_name.is_some_and(|wanted| name.as_ref() == Some(wanted))
            || wanted_src.is_some_and(|wanted| {
                url.as_ref()
                    .is_some_and(|actual| actual == wanted || actual.ends_with(wanted))
            })
        {
            return Ok(Some(frame));
        }
    }
    Ok((children.len() == 1).then(|| children.remove(0)))
}

async fn find_oopif(
    browser: &mut Browser,
    parent: &chromiumoxide::cdp::browser_protocol::page::FrameId,
    candidate: &Candidate,
) -> Result<Option<(Page, chromiumoxide::cdp::browser_protocol::page::FrameId)>, CommandError> {
    let wanted_src = candidate.attributes.get("src");
    let targets = browser
        .execute(GetTargetsParams { filter: None })
        .await
        .map_err(cdp_error)?
        .result
        .target_infos
        .into_iter()
        .filter(|target| {
            target.r#type == "iframe"
                && wanted_src.is_none_or(|wanted| {
                    target.url == *wanted
                        || target.url.trim_end_matches('/') == wanted.trim_end_matches('/')
                        || target.url.ends_with(wanted)
                })
        })
        .collect::<Vec<_>>();
    let target = targets
        .iter()
        .find(|target| target.parent_frame_id.as_ref() == Some(parent))
        .cloned()
        .or_else(|| (targets.len() == 1).then(|| targets[0].clone()));
    let Some(target) = target else {
        return Ok(None);
    };
    let page = browser
        .get_page(target.target_id)
        .await
        .map_err(cdp_error)?;
    let frame = page
        .execute(GetFrameTreeParams::default())
        .await
        .map_err(cdp_error)?
        .result
        .frame_tree
        .frame
        .id;
    Ok(Some((page, frame)))
}

/// The `find`-by-id helper plus the shadow_hosts descent loop, shared by
/// both the document-rooted and closed-root-rooted expression builders.
/// Leaves a mutable `root` binding in scope for the caller's `operation` to
/// use, having already descended through any open shadow hosts named in
/// `shadow_hosts`. Further-nested *open* shadow roots inside a closed root
/// are handled transparently here too, since `el.shadowRoot` still resolves
/// once you're already inside — only the outermost "closed" boundary needs
/// CDP-native traversal to cross.
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
                .call_js_fn_by_value(expression, false)
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

fn cdp_error(error: chromiumoxide::error::CdpError) -> CommandError {
    target_error(ErrorCode::BrowserCommandFailed, error)
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
}
