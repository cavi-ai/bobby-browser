use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use chromiumoxide::browser::Browser;
use chromiumoxide::cdp::browser_protocol::dom::{RequestNodeParams, SetFileInputFilesParams};
use chromiumoxide::cdp::browser_protocol::page::{
    CaptureScreenshotFormat, GetFrameTreeParams, Viewport,
};
use chromiumoxide::cdp::browser_protocol::target::GetTargetsParams;
use chromiumoxide::cdp::js_protocol::runtime::{EvaluateParams, ExecutionContextId};
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

#[derive(Clone)]
struct JsLocator {
    context_id: Option<ExecutionContextId>,
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
        let mut params = EvaluateParams::new(expression);
        params.context_id = self.locator.context_id;
        params.return_by_value = Some(false);
        let object_id = page
            .evaluate(params)
            .await
            .map_err(cdp_error)?
            .object()
            .object_id
            .clone()
            .ok_or_else(|| target_error(ErrorCode::TargetDetached, "target has no live object"))?;
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
        evaluate_in_context(page, self.locator.context_id, expression).await
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
    let raw = collect_candidates(
        &scope.execution_page,
        scope.context_id,
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
    scope_id: u64,
    frame_id: chromiumoxide::cdp::browser_protocol::page::FrameId,
    frame_trace: Vec<types::CandidateEvidence>,
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

    for (index, host_target) in target.shadow_path.iter().enumerate() {
        let raw = collect_candidates(&execution_page, context_id, &shadow_hosts, scope_id).await?;
        let (candidate, evidence, _) = choose(host_target, raw, true)?;
        let mut prospective = shadow_hosts.clone();
        prospective.push(candidate.id.clone());
        let has_root: bool = evaluate_in_context(
            &execution_page,
            context_id,
            scope_expression(&prospective, "return !!root")?,
        )
        .await?;
        if !has_root {
            return Err(target_error(
                ErrorCode::ShadowRootUnavailable,
                format!("shadow path component {index} has no open root"),
            ));
        }
        shadow_hosts.push(candidate.id);
        frame_trace.push(evidence);
    }

    Ok(TargetScope {
        execution_page,
        context_id,
        shadow_hosts,
        scope_id,
        frame_id,
        frame_trace,
    })
}

fn into_candidate(item: BrowserCandidate) -> Candidate {
    Candidate {
        id: item.id,
        css: item.css,
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
                context_id: None,
                shadow_hosts: Vec::new(),
                id: String::new(),
            },
            execution_page: None,
            evidence: selector_evidence(page_id, selector),
        });
    };

    let scope = open_target_scope(page, target, browser).await?;
    let deadline = Instant::now() + Duration::from_secs(2);
    let (candidate, evidence, best_match_authorized) = loop {
        let raw = collect_candidates(
            &scope.execution_page,
            scope.context_id,
            &scope.shadow_hosts,
            scope.scope_id,
        )
        .await?;
        match choose(target, raw, require_visible) {
            Ok(resolved) => break resolved,
            Err(error) if error.code == ErrorCode::TargetNotFound && Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(25)).await;
            }
            Err(error) => return Err(error),
        }
    };
    let locator = JsLocator {
        context_id: scope.context_id,
        shadow_hosts: scope.shadow_hosts,
        id: candidate.id.clone(),
    };
    let native = if target.frame_path.is_empty() && target.shadow_path.is_empty() {
        match candidate.css.as_deref() {
            Some(css) => scope.execution_page.find_element(css).await.ok(),
            None => None,
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
    let prefix = format!("bobby-{scope}-");
    let prefix = serde_json::to_string(&prefix)
        .map_err(|error| target_error(ErrorCode::InvalidRequest, error))?;
    let operation = format!(
        r#"let n=0,out=[]; const visit=current=>{{for(const el of current.querySelectorAll('*')){{const id={prefix}+(++n);el.setAttribute('data-bobby-target',id);const style=getComputedStyle(el),rect=el.getBoundingClientRect();const label=el.labels&&el.labels.length?Array.from(el.labels).map(x=>x.innerText.trim()).join(' '):null;const role=el.getAttribute('role')||({{BUTTON:'button',A:'link',IFRAME:'iframe',INPUT:el.type==='checkbox'?'checkbox':'textbox',TEXTAREA:'textbox',SELECT:'combobox'}}[el.tagName]||null);const name=el.getAttribute('aria-label')||label||el.innerText?.trim()||el.value||null;const attributes={{}};for(const a of el.attributes)if(a.name==='name'||a.name==='type'||a.name==='src'||a.name.startsWith('data-'))attributes[a.name]=a.value;const css=el.id?`#${{CSS.escape(el.id)}}`:`[data-bobby-target="${{id}}"]`;out.push({{id,css,testId:el.getAttribute('data-testid'),role,name,label,text:(el.innerText||el.value||'').trim(),attributes,attached:el.isConnected,visible:style.visibility!=='hidden'&&style.display!=='none'&&rect.width>0&&rect.height>0,enabled:!el.disabled}});if(el.shadowRoot)visit(el.shadowRoot)}}}};visit(root);return out"#
    );
    evaluate_in_context(
        page,
        context_id,
        scope_expression(shadow_hosts, &operation)?,
    )
    .await
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

fn scope_expression(shadow_hosts: &[String], operation: &str) -> Result<String, CommandError> {
    let hosts = serde_json::to_string(shadow_hosts)
        .map_err(|error| target_error(ErrorCode::InvalidRequest, error))?;
    Ok(format!(
        r#"(()=>{{const find=(root,id)=>{{for(const el of root.querySelectorAll('*')){{if(el.getAttribute('data-bobby-target')===id)return el;if(el.shadowRoot){{const found=find(el.shadowRoot,id);if(found)return found}}}}return null}};let root=document;for(const id of {hosts}){{const host=find(root,id);if(!host||!host.shadowRoot)return false;root=host.shadowRoot}};{operation}}})()"#
    ))
}

fn locator_expression(locator: &JsLocator, operation: &str) -> Result<String, CommandError> {
    let id = serde_json::to_string(&locator.id)
        .map_err(|error| target_error(ErrorCode::InvalidRequest, error))?;
    scope_expression(
        &locator.shadow_hosts,
        &format!(
            "const el=find(root,{id});if(!el||!el.isConnected)throw new Error('target detached');{operation}"
        ),
    )
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
