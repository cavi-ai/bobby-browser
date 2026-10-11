//! The shared await, gather, resolve, and escalation pipeline.
//!
//! Intent-specific actions stay in the executor. This module owns the policy
//! differences that affect resolution and keeps their failure evidence stable.

use super::*;

/// Non-escalating resolution retains the full census for near-miss reporting.
/// Gather and resolver failures remain distinct: callers intentionally handle
/// them differently during reveal waits and structured extraction.
pub(super) struct Decision {
    pub census: Vec<Candidate>,
    pub resolution: Result<ResolutionDecision, dom_engine::ResolutionError>,
}

pub(super) async fn decide(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    target: &TargetSpec,
    action: Option<&ControlAction>,
) -> Result<Decision, CommandError> {
    let census = browser.collect_candidates(page_id, target).await?;
    let resolution = decide_candidates(target, &census, action);
    Ok(Decision { census, resolution })
}

pub(super) fn decide_candidates(
    target: &TargetSpec,
    census: &[Candidate],
    action: Option<&ControlAction>,
) -> Result<ResolutionDecision, dom_engine::ResolutionError> {
    let compatible_pool = action
        .map(|action| {
            census
                .iter()
                .filter(|candidate| compatible(action, candidate))
                .cloned()
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let pool = if compatible_pool.is_empty() {
        census
    } else {
        &compatible_pool
    };
    resolve_candidates(target, pool, &ResolutionPolicy::default())
}

pub(super) fn decide_file_candidates(
    target: &TargetSpec,
    candidates: &[Candidate],
) -> Result<ResolutionDecision, dom_engine::ResolutionError> {
    resolve_candidates(
        target,
        candidates,
        &ResolutionPolicy {
            require_visible: false,
            ..ResolutionPolicy::default()
        },
    )
}

pub(super) enum LocateMode<'a> {
    Standard,
    Purpose,
    SubmitPurpose,
    Fill(&'a ControlAction),
    Reveal,
}

pub(super) struct LocateRequest<'a> {
    pub intent_kind: &'a str,
    pub purpose: Option<&'a str>,
    pub plan_summary: &'a str,
    pub target: &'a TargetSpec,
    pub mode: LocateMode<'a>,
}

pub(super) struct LocatedTarget {
    pub candidate: Box<Candidate>,
    pub evidence: types::CandidateEvidence,
    pub best_match_authorized: bool,
    /// The dialogs open when the target resolved.
    pub open_dialogs: Vec<types::CandidateEvidence>,
}

/// Each visible dialog in a candidate census, by role and name.
pub(super) fn open_dialogs(census: &[Candidate]) -> Vec<types::CandidateEvidence> {
    census
        .iter()
        .filter(|candidate| {
            candidate.state.visible
                && candidate.role.as_deref().is_some_and(|role| {
                    role.eq_ignore_ascii_case("dialog") || role.eq_ignore_ascii_case("alertdialog")
                })
        })
        .map(|candidate| types::CandidateEvidence {
            role: candidate.role.clone(),
            name: candidate.name.clone(),
            score: 0,
            reasons: vec!["openDialog".into()],
        })
        .collect()
}

pub(super) async fn locate(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    request: LocateRequest<'_>,
) -> Result<LocatedTarget, IntentOutcome> {
    let reveal = matches!(request.mode, LocateMode::Reveal);
    let failure = |error, verification| {
        non_escalating_failure(
            error,
            intent_evidence(execution_record(
                request.intent_kind,
                request.purpose.map(str::to_owned),
                request.plan_summary,
                Vec::new(),
                None,
                verification,
            )),
        )
    };
    browser.await_target(page_id, request.target).await;
    let census = browser
        .collect_candidates(page_id, request.target)
        .await
        .map_err(|error| {
            failure(
                error,
                if reveal {
                    "revealGatherFailed"
                } else {
                    "gatherFailed"
                },
            )
        })?;
    // Only resolution sees the compatible pool. Vision must retain the full
    // census, and an empty compatible pool must retain action-mismatch errors.
    let compatible_pool = match request.mode {
        LocateMode::Fill(action) => census
            .iter()
            .filter(|candidate| compatible(action, candidate))
            .cloned()
            .collect::<Vec<_>>(),
        _ => Vec::new(),
    };
    let pool = if compatible_pool.is_empty() {
        &census
    } else {
        &compatible_pool
    };
    let decision = decide_candidates(request.target, pool, None).map_err(|error| {
        failure(
            CommandError {
                code: ErrorCode::InvalidRequest,
                message: error.to_string(),
                layer: ErrorLayer::Page,
                retryable: false,
            },
            if reveal {
                "revealResolveFailed"
            } else {
                "resolveFailed"
            },
        )
    })?;
    let decision = match request.mode {
        LocateMode::Purpose if !matches!(decision, ResolutionDecision::Resolved { .. }) => request
            .purpose
            .and_then(|purpose| disambiguate_by_purpose(request.target, pool, purpose))
            .unwrap_or(decision),
        LocateMode::SubmitPurpose => request
            .purpose
            .and_then(|purpose| disambiguate_submit_by_purpose(request.target, pool, purpose))
            .unwrap_or_else(|| match decision {
                ResolutionDecision::Resolved { evidence, .. } => ResolutionDecision::Ambiguous {
                    candidates: vec![evidence],
                },
                unresolved => unresolved,
            }),
        _ => decision,
    };
    let (kind, candidates, verification) = match decision {
        ResolutionDecision::Resolved {
            candidate,
            evidence,
            best_match_authorized,
        } => {
            return Ok(LocatedTarget {
                candidate,
                evidence,
                best_match_authorized,
                open_dialogs: open_dialogs(&census),
            });
        }
        ResolutionDecision::NotFound => {
            if let LocateMode::Fill(action) = request.mode {
                if !matches!(action, ControlAction::SetFiles { .. })
                    && targets_file_control(request.target, &census)
                {
                    return Err(file_control_failure(
                        request.purpose.map(str::to_owned),
                        request.plan_summary.to_owned(),
                        Vec::new(),
                    ));
                }
            }
            let candidates =
                if request.intent_kind == "locate" || matches!(request.mode, LocateMode::Fill(_)) {
                    ranked_near_miss_window(&census, request.purpose)
                } else {
                    Vec::new()
                };
            (
                StuckKind::TargetMissing,
                candidates,
                if reveal {
                    "revealTargetNotFound"
                } else {
                    "targetNotFound"
                },
            )
        }
        ResolutionDecision::Ambiguous { candidates } => (
            StuckKind::TargetAmbiguous,
            candidates,
            if reveal {
                "revealTargetAmbiguous"
            } else {
                "targetAmbiguous"
            },
        ),
    };
    let fill_payload = match request.mode {
        LocateMode::Fill(action) if !matches!(action, ControlAction::Activate) => {
            Some(VisionFillPayload {
                action: action.clone(),
            })
        }
        _ => None,
    };
    Err(stuck_outcome(
        StuckReport {
            intent_kind: request.intent_kind,
            kind,
            purpose: request.purpose.map(str::to_owned),
            plan_summary: request.plan_summary.to_owned(),
            candidates,
            verification,
            fill_payload,
        },
        page_id,
        browser,
        vision,
    )
    .await)
}
