use std::collections::BTreeMap;

use chromiumoxide::{Element, Page};
use dom_engine::{
    resolve_candidates, Candidate, CandidateState, ResolutionDecision, ResolutionPolicy,
};
use serde::Deserialize;
use types::{CommandError, ErrorCode, ErrorLayer, Evidence, PageId, TargetFingerprint, TargetSpec};

pub struct ResolvedTarget {
    pub element: Element,
    pub evidence: Evidence,
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

pub async fn resolve_target(
    page_id: &PageId,
    page: &Page,
    selector: &str,
    target: Option<&TargetSpec>,
) -> Result<ResolvedTarget, CommandError> {
    resolve_target_with_visibility(page_id, page, selector, target, true).await
}

pub async fn resolve_target_with_visibility(
    page_id: &PageId,
    page: &Page,
    selector: &str,
    target: Option<&TargetSpec>,
    require_visible: bool,
) -> Result<ResolvedTarget, CommandError> {
    let Some(target) = target else {
        let element = page.find_element(selector).await.map_err(cdp_error)?;
        return Ok(ResolvedTarget {
            element,
            evidence: selector_evidence(page_id, selector),
        });
    };
    if !target.frame_path.is_empty() {
        return Err(target_error(
            ErrorCode::FrameNotFound,
            "frame traversal is unavailable for this target",
        ));
    }
    let script = r#"(() => {
      let n = 0; const out = [];
      const visit = root => {
        for (const el of root.querySelectorAll('*')) {
          const id = `bobby-${++n}`; el.setAttribute('data-bobby-target', id);
          const style = getComputedStyle(el); const rect = el.getBoundingClientRect();
          const label = el.labels && el.labels.length ? Array.from(el.labels).map(x => x.innerText.trim()).join(' ') : null;
          const role = el.getAttribute('role') || ({BUTTON:'button',A:'link',INPUT:el.type === 'checkbox' ? 'checkbox' : 'textbox',TEXTAREA:'textbox',SELECT:'combobox'}[el.tagName] || null);
          const name = el.getAttribute('aria-label') || label || el.innerText?.trim() || el.value || null;
          const attributes = {}; for (const a of el.attributes) if (a.name === 'name' || a.name === 'type' || a.name.startsWith('data-')) attributes[a.name] = a.value;
          out.push({id, css:`[data-bobby-target="${id}"]`, testId:el.getAttribute('data-testid'), role, name, label, text:(el.innerText || el.value || '').trim(), attributes, attached:el.isConnected, visible:style.visibility !== 'hidden' && style.display !== 'none' && rect.width > 0 && rect.height > 0, enabled:!el.disabled});
          if (el.shadowRoot) visit(el.shadowRoot);
        }
      }; visit(document); return out;
    })()"#;
    let raw: Vec<BrowserCandidate> = page
        .evaluate(script)
        .await
        .map_err(cdp_error)?
        .into_value()
        .map_err(|error| target_error(ErrorCode::BrowserCommandFailed, error))?;
    let candidates = raw
        .into_iter()
        .map(|item| Candidate {
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
        })
        .collect::<Vec<_>>();
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
        ResolutionDecision::Ambiguous { candidates } => Err(target_error(
            ErrorCode::TargetAmbiguous,
            format!("target is ambiguous across {} candidates", candidates.len()),
        )),
        ResolutionDecision::Resolved {
            candidate,
            evidence,
            best_match_authorized,
        } => {
            let css = candidate.css.as_deref().ok_or_else(|| {
                target_error(
                    ErrorCode::TargetNotFound,
                    "resolved candidate has no browser selector",
                )
            })?;
            let element = page.find_element(css).await.map_err(cdp_error)?;
            let fingerprint = TargetFingerprint {
                page_id: page_id.clone(),
                frame: None,
                role: candidate.role.clone(),
                name: candidate.name.clone(),
                stable_attributes: candidate.attributes.clone(),
            };
            Ok(ResolvedTarget {
                element,
                evidence: Evidence::Resolution {
                    target: Box::new(target.clone()),
                    fingerprint: Box::new(fingerprint),
                    candidates: vec![evidence],
                    best_match_authorized,
                },
            })
        }
    }
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
