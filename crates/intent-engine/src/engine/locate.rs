//! The shared await, gather, resolve, and escalation pipeline.
//!
//! Intent-specific actions stay in the executor. This module owns the policy
//! differences that affect resolution and keeps their failure evidence stable.

use super::*;

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
    let policy = ResolutionPolicy::default();
    let decision = resolve_candidates(request.target, pool, &policy).map_err(|error| {
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
