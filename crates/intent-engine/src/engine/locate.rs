//! The shared await, gather, resolve, and escalation pipeline.
//!
//! Intent-specific actions stay in the executor. This module owns the policy
//! differences that affect resolution and keeps their failure evidence stable.

use std::borrow::Cow;
use std::collections::{btree_map::Entry, BTreeMap};

use dom_engine::rank_candidates;

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
    let pool = resolution_pool(census, action);
    resolve_candidates(target, &pool, &ResolutionPolicy::default())
}

pub(super) fn resolution_pool<'a>(
    census: &'a [Candidate],
    action: Option<&ControlAction>,
) -> Cow<'a, [Candidate]> {
    let Some(action) = action else {
        return Cow::Borrowed(census);
    };
    let compatible_pool = census
        .iter()
        .filter(|candidate| compatible(action, candidate))
        .cloned()
        .collect::<Vec<_>>();
    if compatible_pool.is_empty() {
        // Preserve action-mismatch errors when there are no compatible controls.
        Cow::Borrowed(census)
    } else {
        Cow::Owned(compatible_pool)
    }
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
            ordinal: None,
            score: 0,
            reasons: vec!["openDialog".into()],
        })
        .collect()
}

struct RetryMatches {
    unnumbered: Option<Box<Candidate>>,
    numbered: Option<Vec<Candidate>>,
}

async fn near_miss_evidence(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    request: &LocateRequest<'_>,
    census: &[Candidate],
    action: Option<&ControlAction>,
) -> Vec<types::CandidateEvidence> {
    let mut matches_by_name = BTreeMap::new();
    let mut evidence = Vec::new();
    for (candidate, mut row) in ranked_near_miss_window(census, action, request.purpose) {
        if !candidate.state.visible {
            evidence.push(row);
            continue;
        }

        // The browser may gather different candidates for the suggested
        // role and name. Chromium also changes accessibility alignment
        // when an ordinal is supplied, so probe each form separately.
        let mut retry = TargetSpec {
            role: row.role.clone(),
            accessible_name: row.name.clone(),
            frame_path: request.target.frame_path.clone(),
            shadow_path: request.target.shadow_path.clone(),
            ..TargetSpec::default()
        };
        let matches = match matches_by_name.entry((row.role.clone(), row.name.clone())) {
            Entry::Occupied(entry) => entry.into_mut(),
            Entry::Vacant(entry) => {
                let unnumbered = browser
                    .collect_candidates(page_id, &retry)
                    .await
                    .ok()
                    .and_then(|census| {
                        match resolve_candidates(&retry, &census, &ResolutionPolicy::default()) {
                            Ok(ResolutionDecision::Resolved { candidate, .. })
                                if action.is_none_or(|action| compatible(action, &candidate)) =>
                            {
                                Some(candidate)
                            }
                            _ => None,
                        }
                    });
                entry.insert(RetryMatches {
                    unnumbered,
                    numbered: None,
                })
            }
        };
        if matches.unnumbered.as_ref().is_some_and(|matched| {
            matched.id == candidate.id && matched.frame_path == candidate.frame_path
        }) {
            evidence.push(row);
            continue;
        }

        if matches.numbered.is_none() {
            // The value only requests ordinal-aware collection. Ranking that
            // census gives the actual index to report for each candidate.
            retry.ordinal = Some(0);
            let numbered = match browser.collect_candidates(page_id, &retry).await {
                Ok(census) => rank_candidates(&retry, &census, &ResolutionPolicy::default())
                    .unwrap_or_default()
                    .into_iter()
                    // Fill resolves against compatible controls first and the
                    // browser then acts using the full ranking. After an
                    // incompatible match those ordinals no longer agree.
                    .take_while(|(matched, _)| {
                        action.is_none_or(|action| compatible(action, matched))
                    })
                    .map(|(matched, _)| matched.clone())
                    .collect(),
                Err(_) => Vec::new(),
            };
            matches.numbered = Some(numbered);
        }
        if let Some(ordinal) = matches.numbered.as_ref().and_then(|ranked| {
            ranked.iter().position(|matched| {
                matched.id == candidate.id && matched.frame_path == candidate.frame_path
            })
        }) {
            row.ordinal = Some(ordinal);
            evidence.push(row);
        }
    }
    evidence
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
    let action = match request.mode {
        LocateMode::Fill(action) => Some(action),
        _ => None,
    };
    let pool = resolution_pool(&census, action);
    let pool = pool.as_ref();
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
                    near_miss_evidence(page_id, browser, &request, &census, action).await
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
