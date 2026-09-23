use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use dom_engine::{resolve_candidates, Candidate, ResolutionDecision, ResolutionPolicy};
use futures_util::{stream, StreamExt};
use observability::{
    ContextCandidateRankingMetric, ContextLookupOutcome, ContextRankedVisionMetric,
    OperationalMetrics, ProviderMode, StructuralContextSource, VerificationMetricResult,
    VisionProposalMetric, VisionProposalOutcome,
};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, ControlAction, ControlActionCommand,
    ElementState, ErrorCode, ErrorLayer, Evidence, ExecutionRecord, ExtractValueKind,
    FormControlTarget, IntentCommand, IntentResolutionPath, PageId, ScreenshotMode,
    SemanticTargetSegment, TargetFingerprint, TargetSpec, TypeTextCommand, UploadFilesCommand,
    WaitCondition, WaitForCommand,
};

use crate::compiler::{compile_intent, CompleteFormFieldPlan, ExtractFieldPlan, IntentPlan};
use crate::stuck::{never_escalates, StuckKind};
use crate::verify::{
    compatible, compatible_role, execution_record, execution_record_with_path, is_file_input,
    summarize_target, verify_fill, ResolutionDetails,
};
use crate::vision::{
    proposal_sha256, VisionAction, VisionAssist, VisionProposeRequest, VISION_CONFIDENCE_FLOOR,
};

#[derive(Clone, Default)]
pub struct VisionContext {
    pub session_ok: bool,
    pub capability_ok: bool,
    pub assist: Option<Arc<dyn VisionAssist>>,
    /// Prefill proposal cache. `None` when `[vision].prefill` is disabled or
    /// either vision gate is closed.
    pub proposals: Option<Arc<dyn crate::ProposalLookup>>,
    /// When set, return the deterministic stuck result without live vision
    /// escalation.
    pub defer_escalation: bool,
    /// Base prompt context (page url, recent command kinds) supplied by the
    /// runtime; the engine merges per-stuck candidates into it. `None`
    /// keeps every request byte-identical to before.
    pub prompt_context: Option<crate::VisionPromptContext>,
    /// Escalation corpus sink (`[vision].corpus_dir`). `None` writes nothing;
    /// the default path is byte-identical to before.
    pub corpus: Option<crate::VisionCorpus>,
    /// Durable context graph for challenge priors. `None` keeps the byte-identical
    /// default path; when present, `solveChallenge` reads the most-attempted
    /// challenge kind for the site and records the outcome after solving.
    pub context_store: Option<Arc<context_store::ContextStore>>,
}

#[derive(Debug, Clone)]
pub enum IntentOutcome {
    Completed {
        evidence: Vec<Evidence>,
    },
    Failed {
        error: CommandError,
        evidence: Vec<Evidence>,
    },
}

#[async_trait]
pub trait IntentBrowser: Send + Sync {
    async fn collect_candidates(
        &self,
        page_id: &PageId,
        target: &TargetSpec,
    ) -> Result<Vec<Candidate>, CommandError>;

    async fn click(
        &self,
        page_id: &PageId,
        command: &ClickCommand,
    ) -> Result<Vec<Evidence>, CommandError>;

    async fn click_xy(
        &self,
        page_id: &PageId,
        x: f64,
        y: f64,
    ) -> Result<Vec<Evidence>, CommandError>;

    /// Accessible identity (role, name) of the interactive element at a
    /// viewport point, used by the vision corpus collector to ground a
    /// verified click back onto the candidate list. Default: unsupported
    /// (`browserCommandFailed`), not `Ok(None)`.
    async fn element_at_point(
        &self,
        _page_id: &PageId,
        _x: f64,
        _y: f64,
    ) -> Result<Option<(String, String)>, CommandError> {
        Err(CommandError {
            code: ErrorCode::BrowserCommandFailed,
            message: "browser primitive is not supported by this worker".into(),
            layer: ErrorLayer::Driver,
            retryable: false,
        })
    }

    async fn type_text(
        &self,
        page_id: &PageId,
        command: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError>;

    async fn upload_files(
        &self,
        page_id: &PageId,
        command: &UploadFilesCommand,
    ) -> Result<Vec<Evidence>, CommandError>;

    async fn control_action(
        &self,
        _page_id: &PageId,
        _command: &ControlActionCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Err(CommandError {
            code: ErrorCode::IntentActionMismatch,
            message: "typed control actions are unavailable".into(),
            layer: ErrorLayer::Page,
            retryable: false,
        })
    }

    async fn wait_for(
        &self,
        page_id: &PageId,
        command: &WaitForCommand,
    ) -> Result<Vec<Evidence>, CommandError>;

    /// Inspect the page after a soft submit postcondition settles. The full
    /// runtime implements this with its bounded page inspection; fakes and
    /// alternate runtimes may omit it without changing older intent behavior.
    async fn inspect_settled_page(&self, _page_id: &PageId) -> Result<Vec<Evidence>, CommandError> {
        Ok(Vec::new())
    }

    async fn capture_screenshot(
        &self,
        page_id: &PageId,
        command: &CaptureScreenshotCommand,
    ) -> Result<(Vec<u8>, Vec<Evidence>), CommandError>;

    async fn capture_sanitized_screenshot(
        &self,
        _page_id: &PageId,
    ) -> Result<Vec<u8>, CommandError> {
        Err(CommandError {
            code: ErrorCode::ScreenshotCaptureFailed,
            message: "sanitized screenshot capture is not supported by this worker".into(),
            layer: ErrorLayer::Driver,
            retryable: false,
        })
    }

    /// Compact, value-free invalid-control evidence after a soft submit wait.
    /// An empty vector means the settled page did not retain a rejected form.
    async fn validation_issues(
        &self,
        _page_id: &PageId,
    ) -> Result<Vec<types::FormValidationIssue>, CommandError> {
        Ok(Vec::new())
    }
}

pub struct IntentEngine;

impl IntentEngine {
    pub async fn execute(
        intent: &IntentCommand,
        page_id: &PageId,
        browser: &dyn IntentBrowser,
        vision: &VisionContext,
    ) -> IntentOutcome {
        let plan = match compile_intent(intent) {
            Ok(plan) => plan,
            Err(error) => {
                return IntentOutcome::Failed {
                    error: CommandError {
                        code: ErrorCode::IntentCompileFailed,
                        message: error.to_string(),
                        layer: ErrorLayer::Page,
                        retryable: false,
                    },
                    evidence: Vec::new(),
                };
            }
        };

        match plan {
            IntentPlan::Locate { target } => {
                execute_locate(intent, page_id, browser, vision, target).await
            }
            IntentPlan::WaitForState {
                condition,
                timeout_ms,
            } => execute_wait_for_state(page_id, browser, condition, timeout_ms).await,
            IntentPlan::Fill { target, value } => {
                execute_fill(intent, page_id, browser, vision, target, value).await
            }
            IntentPlan::CompleteForm { fields } => {
                execute_complete_form(page_id, browser, vision, fields).await
            }
            IntentPlan::SubmitAndVerify {
                target,
                expected_state,
            } => {
                execute_submit_and_verify(intent, page_id, browser, vision, target, expected_state)
                    .await
            }
            IntentPlan::Follow {
                target,
                expected_destination,
                boundary,
            } => {
                execute_follow(
                    intent,
                    page_id,
                    browser,
                    vision,
                    target,
                    expected_destination,
                    boundary,
                )
                .await
            }
            IntentPlan::DismissObstruction { target, timeout_ms } => {
                execute_dismiss_obstruction(intent, page_id, browser, vision, target, timeout_ms)
                    .await
            }
            IntentPlan::Extract { fields } => {
                execute_extract(intent, page_id, browser, vision, fields).await
            }
            IntentPlan::SolveChallenge {
                purpose,
                timeout_ms,
            } => execute_solve_challenge(page_id, browser, vision, purpose, timeout_ms).await,
            IntentPlan::DetectChallenge {
                purpose,
                timeout_ms,
            } => execute_detect_challenge(page_id, browser, vision, purpose, timeout_ms).await,
        }
    }
}

/// How long to wait for a field revealed by `CompleteFormField::revealed_by`
/// to become visible after its reveal control is clicked.
const REVEAL_WAIT_TIMEOUT_MS: u64 = 10_000;

async fn execute_complete_form(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    fields: Vec<CompleteFormFieldPlan>,
) -> IntentOutcome {
    let mut evidence = Vec::new();
    proactive_prefill(page_id, browser, vision, &fields).await;
    for field in &fields {
        if let Some(reveal_target) = &field.revealed_by {
            match reveal_field(
                page_id,
                browser,
                vision,
                &field.purpose,
                reveal_target,
                &field.target,
            )
            .await
            {
                IntentOutcome::Completed {
                    evidence: mut reveal_evidence,
                } => evidence.append(&mut reveal_evidence),
                IntentOutcome::Failed {
                    error,
                    evidence: mut fail_evidence,
                } => {
                    evidence.append(&mut fail_evidence);
                    return IntentOutcome::Failed { error, evidence };
                }
            }
        }
        evidence.push(Evidence::Configuration {
            name: "completeFormField".into(),
            value: field.name.clone(),
        });
        let intent = IntentCommand::Fill(types::FillIntent {
            purpose: field.purpose.clone(),
            hints: types::IntentHints::default(),
            value: field.value.clone(),
        });
        match execute_fill(
            &intent,
            page_id,
            browser,
            vision,
            field.target.clone(),
            field.value.clone(),
        )
        .await
        {
            IntentOutcome::Completed {
                evidence: mut field_evidence,
            } => evidence.append(&mut field_evidence),
            IntentOutcome::Failed {
                error,
                evidence: mut field_evidence,
            } => {
                evidence.append(&mut field_evidence);
                return IntentOutcome::Failed { error, evidence };
            }
        }
    }
    IntentOutcome::Completed { evidence }
}

/// Click the control that reveals a `CompleteForm` field which does not
/// exist in the DOM until the fields filled so far are submitted (for
/// example an MFA code field shown only after email and password are
/// submitted). Resolves `reveal_target` and clicks it the same way
/// `SubmitAndVerify` resolves and clicks its button, then waits for
/// `revealed_field_target` to become visible so the caller can fill it
/// deterministically instead of racing the page.
async fn reveal_field(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    purpose: &str,
    reveal_target: &TargetSpec,
    revealed_field_target: &TargetSpec,
) -> IntentOutcome {
    let plan_summary = format!("reveal {}", summarize_target(reveal_target));
    let candidates = match browser.collect_candidates(page_id, reveal_target).await {
        Ok(candidates) => candidates,
        Err(error) => {
            return non_escalating_failure(
                error,
                intent_evidence(execution_record(
                    "completeForm",
                    Some(purpose.to_owned()),
                    plan_summary,
                    Vec::new(),
                    None,
                    "revealGatherFailed",
                )),
            );
        }
    };

    let decision =
        match resolve_candidates(reveal_target, &candidates, &ResolutionPolicy::default()) {
            Ok(decision) => decision,
            Err(error) => {
                return IntentOutcome::Failed {
                    error: CommandError {
                        code: ErrorCode::InvalidRequest,
                        message: error.to_string(),
                        layer: ErrorLayer::Page,
                        retryable: false,
                    },
                    evidence: vec![intent_evidence(execution_record(
                        "completeForm",
                        Some(purpose.to_owned()),
                        plan_summary,
                        Vec::new(),
                        None,
                        "revealResolveFailed",
                    ))],
                };
            }
        };

    let mut click_evidence = match decision {
        ResolutionDecision::Resolved {
            candidate,
            evidence: candidate_evidence,
            best_match_authorized,
        } => {
            let resolution = Evidence::Resolution {
                target: Box::new(reveal_target.clone()),
                fingerprint: Box::new(fingerprint(page_id, &candidate)),
                candidates: vec![candidate_evidence.clone()],
                best_match_authorized,
            };
            let (selector, action_target) = action_target(&candidate, reveal_target);
            let click = ClickCommand {
                selector,
                target: Some(action_target),
                boundary: true,
                expected_url: None,
                modifiers: Vec::new(),
            };
            match browser.click(page_id, &click).await {
                Ok(mut evidence) => {
                    let mut all = vec![resolution];
                    all.append(&mut evidence);
                    all
                }
                Err(error) => {
                    return IntentOutcome::Failed {
                        error,
                        evidence: vec![
                            resolution,
                            intent_evidence(execution_record(
                                "completeForm",
                                Some(purpose.to_owned()),
                                plan_summary,
                                vec![candidate_evidence],
                                None,
                                "revealActFailed",
                            )),
                        ],
                    };
                }
            }
        }
        ResolutionDecision::NotFound => {
            return stuck_outcome(
                StuckReport {
                    intent_kind: "completeForm",
                    kind: StuckKind::TargetMissing,
                    purpose: Some(purpose.to_owned()),
                    plan_summary,
                    candidates: Vec::new(),
                    verification: "revealTargetNotFound",
                    fill_payload: None,
                },
                page_id,
                browser,
                vision,
            )
            .await;
        }
        ResolutionDecision::Ambiguous { candidates } => {
            return stuck_outcome(
                StuckReport {
                    intent_kind: "completeForm",
                    kind: StuckKind::TargetAmbiguous,
                    purpose: Some(purpose.to_owned()),
                    plan_summary,
                    candidates,
                    verification: "revealTargetAmbiguous",
                    fill_payload: None,
                },
                page_id,
                browser,
                vision,
            )
            .await;
        }
    };

    let wait = WaitForCommand {
        condition: WaitCondition::Element {
            target: Box::new(revealed_field_target.clone()),
            state: ElementState::Visible,
        },
        timeout_ms: REVEAL_WAIT_TIMEOUT_MS,
    };
    match browser.wait_for(page_id, &wait).await {
        Ok(mut wait_evidence) => {
            click_evidence.append(&mut wait_evidence);
            click_evidence.push(intent_evidence(execution_record(
                "completeForm",
                Some(purpose.to_owned()),
                format!("revealed field visible timeout_ms={REVEAL_WAIT_TIMEOUT_MS}"),
                Vec::new(),
                None,
                "revealed",
            )));
            IntentOutcome::Completed {
                evidence: click_evidence,
            }
        }
        Err(error) => {
            let error = recode_postclick_verification_error(
                error,
                "reveal control clicked; revealed field did not become visible",
            );
            click_evidence.push(intent_evidence(execution_record(
                "completeForm",
                Some(purpose.to_owned()),
                "revealed field visibility wait",
                Vec::new(),
                None,
                "revealVerifyFailed",
            )));
            IntentOutcome::Failed {
                error,
                evidence: click_evidence,
            }
        }
    }
}

struct PrefillRequest {
    purpose: String,
    stuck: StuckKind,
    context: Option<crate::VisionPromptContext>,
    context_ranked: bool,
}

const PREFILL_CONCURRENCY_LIMIT: usize = 4;

/// Preflights every field without mutating the page, then asks vision only for
/// fields the deterministic resolver cannot settle. Candidate identities are
/// cached; typed values remain in the runtime and are applied only when the
/// field executes.
async fn proactive_prefill(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    fields: &[CompleteFormFieldPlan],
) {
    let (Some(proposals), Some(assist)) = (&vision.proposals, &vision.assist) else {
        return;
    };
    if !vision.session_ok || !vision.capability_ok {
        return;
    }

    let mut requests = Vec::new();
    for field in fields {
        if field.revealed_by.is_some() {
            // Not yet in the DOM; nothing to preflight until its reveal
            // control is clicked in `execute_complete_form`.
            continue;
        }
        if proposals.proposal_for(page_id, &field.purpose).is_some() {
            continue;
        }
        let Some((stuck, candidates)) = prefill_candidates(page_id, browser, field).await else {
            continue;
        };
        let ranking_started = std::time::Instant::now();
        let (candidates, context_ranking) =
            rank_candidates_from_context(vision, "fill", candidates).await;
        if let (Some(ranking), Some((metrics, _))) = (context_ranking, assist.operational_metrics())
        {
            metrics.record_context_lookup(ranking.outcome);
            metrics.record_context_candidate_ranking(ContextCandidateRankingMetric {
                source: ranking.source,
                outcome: ranking.outcome,
                latency_ms: ranking_started.elapsed().as_millis() as u64,
            });
        }
        let prompt_candidates = candidates
            .iter()
            .filter(|candidate| {
                vision_window_eligible(candidate.role.as_deref(), candidate.name.as_deref())
            })
            .take(CONTEXT_RANK_CANDIDATE_LIMIT)
            .map(|candidate| crate::VisionPromptCandidate {
                role: candidate.role.clone().expect("gated role"),
                name: candidate.name.clone().expect("gated name"),
                ordinal: None,
            })
            .collect::<Vec<_>>();
        if prompt_candidates.is_empty() {
            continue;
        }
        let mut context = vision.prompt_context.clone().unwrap_or_default();
        context.candidates = prompt_candidates;
        requests.push(PrefillRequest {
            purpose: field.purpose.clone(),
            stuck,
            context: Some(context),
            context_ranked: context_ranking.is_some(),
        });
    }
    if requests.is_empty() {
        return;
    }

    let Ok((png, _)) = browser
        .capture_screenshot(
            page_id,
            &CaptureScreenshotCommand {
                mode: ScreenshotMode::Viewport,
            },
        )
        .await
    else {
        return;
    };
    let corpus_screenshot_png = if assist.collects_training_data() {
        capture_sanitized_corpus_screenshot(browser, page_id).await
    } else {
        None
    };
    let metric_context = assist.operational_metrics();
    let batch = stream::iter(requests)
        .map(|request| {
            let assist = Arc::clone(assist);
            let png = png.clone();
            let corpus_screenshot_png = corpus_screenshot_png.clone();
            let metric_context = metric_context.clone();
            async move {
                let propose_started = std::time::Instant::now();
                let proposal = match assist
                    .propose(VisionProposeRequest {
                        purpose: request.purpose.clone(),
                        intent_kind: "fill".to_owned(),
                        screenshot_png: png,
                        corpus_screenshot_png,
                        stuck: request.stuck,
                        context: request.context.clone(),
                    })
                    .await
                {
                    Ok(proposal) => proposal,
                    Err(_) => {
                        record_context_ranked_vision_metric(
                            metric_context.as_ref(),
                            request.context_ranked,
                            propose_started.elapsed().as_millis() as u64,
                            None,
                            VisionProposalOutcome::Failed,
                            None,
                        );
                        return None;
                    }
                };
                let latency_ms = propose_started.elapsed().as_millis() as u64;
                if proposal.confidence < VISION_CONFIDENCE_FLOOR {
                    record_context_ranked_vision_metric(
                        metric_context.as_ref(),
                        request.context_ranked,
                        latency_ms,
                        Some(proposal.confidence),
                        VisionProposalOutcome::Rejected,
                        Some(VerificationMetricResult::OtherRejected),
                    );
                    return None;
                }
                let context = request.context.as_ref()?;
                let VisionAction::TypeIntoCandidate { index } = proposal.action else {
                    record_context_ranked_vision_metric(
                        metric_context.as_ref(),
                        request.context_ranked,
                        latency_ms,
                        Some(proposal.confidence),
                        VisionProposalOutcome::Rejected,
                        Some(VerificationMetricResult::OtherRejected),
                    );
                    return None;
                };
                if index as usize >= context.candidates.len() {
                    record_context_ranked_vision_metric(
                        metric_context.as_ref(),
                        request.context_ranked,
                        latency_ms,
                        Some(proposal.confidence),
                        VisionProposalOutcome::Rejected,
                        Some(VerificationMetricResult::OtherRejected),
                    );
                    return None;
                }
                record_context_ranked_vision_metric(
                    metric_context.as_ref(),
                    request.context_ranked,
                    latency_ms,
                    Some(proposal.confidence),
                    VisionProposalOutcome::Accepted,
                    None,
                );
                Some((
                    request.purpose,
                    crate::CachedProposal {
                        action: crate::CachedProposalAction::TypeIntoCandidate {
                            candidates: context.candidates.clone(),
                            index,
                        },
                        confidence: proposal.confidence,
                    },
                ))
            }
        })
        .buffer_unordered(PREFILL_CONCURRENCY_LIMIT)
        .filter_map(async move |proposal| proposal)
        .collect::<Vec<_>>()
        .await;
    if !batch.is_empty() {
        tracing::info!(recorded = batch.len(), "vision.prefill_batch");
        proposals.record_proposals(page_id, batch);
    } else {
        tracing::info!("vision.prefill_batch_empty");
    }
}

async fn prefill_candidates(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    field: &CompleteFormFieldPlan,
) -> Option<(StuckKind, Vec<types::CandidateEvidence>)> {
    let candidates = browser
        .collect_candidates(page_id, &field.target)
        .await
        .ok()?;
    let window_candidates = candidates.clone();
    let compatible_candidates = candidates
        .iter()
        .filter(|candidate| compatible(&field.value, candidate))
        .cloned()
        .collect::<Vec<_>>();
    let candidates = if compatible_candidates.is_empty() {
        candidates
    } else {
        compatible_candidates
    };
    match resolve_candidates(&field.target, &candidates, &ResolutionPolicy::default()).ok()? {
        ResolutionDecision::Resolved { .. } => None,
        ResolutionDecision::NotFound => {
            if !matches!(field.value, ControlAction::SetFiles { .. })
                && targets_file_control(&field.target, &window_candidates)
            {
                return None;
            }
            Some((
                StuckKind::TargetMissing,
                ranked_near_miss_window(&window_candidates, Some(&field.purpose)),
            ))
        }
        ResolutionDecision::Ambiguous { candidates } => {
            Some((StuckKind::TargetAmbiguous, candidates))
        }
    }
}

async fn execute_locate(
    intent: &IntentCommand,
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    target: TargetSpec,
) -> IntentOutcome {
    let (purpose, purpose_is_implicit_match) = match intent {
        IntentCommand::Locate(locate) => (
            Some(locate.purpose.clone()),
            locate.hints.accessible_name.is_none() && locate.hints.near_text.is_none(),
        ),
        _ => (None, false),
    };
    let plan_summary = summarize_target(&target);
    let candidates = match browser.collect_candidates(page_id, &target).await {
        Ok(candidates) => candidates,
        Err(error) => {
            return non_escalating_failure(
                error,
                intent_evidence(execution_record(
                    "locate",
                    purpose,
                    plan_summary,
                    Vec::new(),
                    None,
                    "gatherFailed",
                )),
            );
        }
    };
    let decision = match resolve_candidates(&target, &candidates, &ResolutionPolicy::default()) {
        Ok(decision) => decision,
        Err(error) => {
            return IntentOutcome::Failed {
                error: CommandError {
                    code: ErrorCode::InvalidRequest,
                    message: error.to_string(),
                    layer: ErrorLayer::Page,
                    retryable: false,
                },
                evidence: vec![intent_evidence(execution_record(
                    "locate",
                    purpose,
                    plan_summary,
                    Vec::new(),
                    None,
                    "resolveFailed",
                ))],
            };
        }
    };
    let decision = match decision {
        unresolved @ (ResolutionDecision::NotFound | ResolutionDecision::Ambiguous { .. })
            if purpose_is_implicit_match =>
        {
            purpose
                .as_deref()
                .and_then(|purpose| disambiguate_by_purpose(&target, &candidates, purpose))
                .unwrap_or(unresolved)
        }
        decision => decision,
    };

    match decision {
        ResolutionDecision::Resolved {
            candidate,
            evidence,
            best_match_authorized,
        } => {
            let fingerprint = fingerprint(page_id, &candidate);
            let resolution = Evidence::Resolution {
                target: Box::new(target),
                fingerprint: Box::new(fingerprint),
                candidates: vec![evidence.clone()],
                best_match_authorized,
            };
            let record = execution_record(
                "locate",
                purpose,
                plan_summary,
                vec![evidence],
                None,
                "resolved",
            );
            IntentOutcome::Completed {
                evidence: vec![resolution, intent_evidence(record)],
            }
        }
        ResolutionDecision::NotFound => {
            // The page's interactive candidates still went unmatched, and the
            // model should see them: a stuck escalation with no candidate
            // list asks the model to pick blind. Attach the near-miss set
            // (capped, score zero) so both the prompt and the corpus carry it.
            // The window is purpose-ranked: DOM order puts sidebar chrome
            // ahead of the page's actionable content and truncates the target
            // out of the top 5 (measured: every live harvest step abstained
            // because its target never entered the window).
            let near_misses = ranked_near_miss_window(&candidates, purpose.as_deref());
            stuck_outcome(
                StuckReport {
                    intent_kind: "locate",
                    kind: StuckKind::TargetMissing,
                    purpose,
                    plan_summary,
                    candidates: near_misses,
                    verification: "targetNotFound",
                    fill_payload: None,
                },
                page_id,
                browser,
                vision,
            )
            .await
        }
        ResolutionDecision::Ambiguous { candidates } => {
            stuck_outcome(
                StuckReport {
                    intent_kind: "locate",
                    kind: StuckKind::TargetAmbiguous,
                    purpose,
                    plan_summary,
                    candidates,
                    verification: "targetAmbiguous",
                    fill_payload: None,
                },
                page_id,
                browser,
                vision,
            )
            .await
        }
    }
}

/// Roles the vision prompt window may carry — the actionable set the
/// training corpus is built from. Landmark/structural rows (main,
/// navigation, region, heading, …) are never valid selections, eat the
/// top-5 window budget, and shift the prompt off the adapter's training
/// distribution (measured: one `main` row flips the adapter to abstain
/// on an otherwise-clean window).
const VISION_WINDOW_ROLES: [&str; 12] = [
    "button",
    "link",
    "textbox",
    "spinbutton",
    "combobox",
    "listbox",
    "checkbox",
    "radio",
    "tab",
    "menuitem",
    "searchbox",
    "switch",
];

const CONTEXT_RANK_CANDIDATE_LIMIT: usize = 10;

fn vision_window_eligible(role: Option<&str>, name: Option<&str>) -> bool {
    name.is_some() && role.is_some_and(|role| VISION_WINDOW_ROLES.contains(&role))
}

/// Purpose-token overlap ranking for the near-miss window. The runtime does
/// not know the target on a stuck step; ranking by lexical plausibility
/// floats rows that share a token with the purpose (role text included, so
/// "…the button…" lifts button rows) above unrelated chrome, and keeps DOM
/// order on ties. The training corpus's windows always contain the target;
/// this approximates that distribution without pretending to know it.
fn ranked_near_miss_window(
    candidates: &[Candidate],
    purpose: Option<&str>,
) -> Vec<types::CandidateEvidence> {
    const STOPWORDS: [&str; 12] = [
        "the", "and", "for", "with", "that", "this", "into", "your", "now", "off", "out", "put",
    ];
    let tokens = purpose
        .map(|purpose| {
            purpose
                .split(|c: char| !c.is_alphanumeric())
                .map(str::to_lowercase)
                .filter(|token| token.len() > 2 && !STOPWORDS.contains(&token.as_str()))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    let mut scored = candidates
        .iter()
        .filter(|candidate| {
            vision_window_eligible(candidate.role.as_deref(), candidate.name.as_deref())
        })
        .map(|candidate| {
            let haystack = format!(
                "{} {}",
                candidate.role.as_deref().unwrap_or_default(),
                candidate.name.as_deref().unwrap_or_default()
            )
            .to_lowercase();
            let score = tokens
                .iter()
                .filter(|token| haystack.contains(token.as_str()))
                .count();
            (score, candidate)
        })
        .collect::<Vec<_>>();
    // Stable sort: equal scores keep DOM order.
    scored.sort_by_key(|(score, _)| std::cmp::Reverse(*score));
    scored
        .into_iter()
        .take(CONTEXT_RANK_CANDIDATE_LIMIT)
        .map(|(_, candidate)| types::CandidateEvidence {
            role: candidate.role.clone(),
            name: candidate.name.clone(),
            score: 0,
            reasons: vec!["noMatch".into()],
        })
        .collect()
}

fn disambiguate_by_purpose(
    target: &TargetSpec,
    candidates: &[Candidate],
    purpose: &str,
) -> Option<ResolutionDecision> {
    if target.css.is_some()
        || target.test_id.is_some()
        || target.accessible_name.is_some()
        || target.label.is_some()
        || !target.attributes.is_empty()
        || target.ordinal.is_some()
    {
        return None;
    }

    let wanted_role = target.role.as_deref();
    let eligible = candidates
        .iter()
        .enumerate()
        .filter(|(_, candidate)| {
            candidate.state.attached
                && candidate.state.visible
                && wanted_role.is_none_or(|wanted| {
                    candidate
                        .role
                        .as_deref()
                        .is_some_and(|role| role.eq_ignore_ascii_case(wanted))
                })
        })
        .collect::<Vec<_>>();
    let exact_matches = eligible
        .iter()
        .filter_map(|(_, candidate)| {
            let purpose = purpose.trim();
            (candidate
                .name
                .as_deref()
                .is_some_and(|name| name.trim().eq_ignore_ascii_case(purpose))
                || candidate
                    .label
                    .as_deref()
                    .is_some_and(|label| label.trim().eq_ignore_ascii_case(purpose))
                || candidate.text.trim().eq_ignore_ascii_case(purpose))
            .then_some(*candidate)
        })
        .collect::<Vec<_>>();
    if let [candidate] = exact_matches.as_slice() {
        let mut reasons = Vec::new();
        let mut score = 100;
        if wanted_role.is_some() {
            reasons.push("exactRole".into());
            score += 30;
        }
        reasons.push("exactPurposeName".into());
        return Some(ResolutionDecision::Resolved {
            candidate: Box::new((*candidate).clone()),
            evidence: types::CandidateEvidence {
                role: candidate.role.clone(),
                name: candidate.name.clone(),
                score,
                reasons,
            },
            best_match_authorized: false,
        });
    }

    let purpose_tokens = semantic_tokens(purpose);
    if purpose_tokens.len() < 2 {
        return None;
    }
    let mut ranked = eligible
        .into_iter()
        .map(|(index, candidate)| {
            let candidate_tokens = semantic_tokens(&format!(
                "{} {} {}",
                candidate.name.as_deref().unwrap_or_default(),
                candidate.label.as_deref().unwrap_or_default(),
                candidate.text
            ));
            let overlap = purpose_tokens.intersection(&candidate_tokens).count();
            (index, candidate, overlap)
        })
        .filter(|(_, _, overlap)| *overlap > 0)
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| right.2.cmp(&left.2).then_with(|| left.0.cmp(&right.0)));
    let (_, candidate, overlap) = ranked.first()?;
    let runner_up = ranked.get(1).map(|item| item.2).unwrap_or_default();
    if *overlap < 2 || *overlap <= runner_up {
        return None;
    }
    let mut reasons = Vec::new();
    let mut score = *overlap as i32 * 10;
    if wanted_role.is_some() {
        reasons.push("exactRole".into());
        score += 30;
    }
    reasons.push(format!("purposeTokenOverlap:{overlap}"));
    Some(ResolutionDecision::Resolved {
        candidate: Box::new((*candidate).clone()),
        evidence: types::CandidateEvidence {
            role: candidate.role.clone(),
            name: candidate.name.clone(),
            score,
            reasons,
        },
        best_match_authorized: false,
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct PersistedCandidateRank {
    net_successes: u64,
    successes: u64,
    last_verified_day: u32,
    observed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ContextCandidateRankingResult {
    outcome: ContextLookupOutcome,
    source: Option<StructuralContextSource>,
}

impl ContextCandidateRankingResult {
    fn without_source(outcome: ContextLookupOutcome) -> Self {
        Self {
            outcome,
            source: None,
        }
    }
}

async fn rank_candidates_from_context(
    vision: &VisionContext,
    intent_kind: &str,
    mut candidates: Vec<types::CandidateEvidence>,
) -> (
    Vec<types::CandidateEvidence>,
    Option<ContextCandidateRankingResult>,
) {
    let Some(store) = &vision.context_store else {
        return (candidates, None);
    };
    let Some(url) = vision
        .prompt_context
        .as_ref()
        .and_then(|context| context.url.as_deref())
    else {
        return (
            candidates,
            Some(ContextCandidateRankingResult::without_source(
                ContextLookupOutcome::Miss,
            )),
        );
    };
    let (Some(site_key), Some(page_pattern)) = (
        context_store::site_key(url),
        context_store::page_pattern(url),
    ) else {
        return (
            candidates,
            Some(ContextCandidateRankingResult::without_source(
                ContextLookupOutcome::Error,
            )),
        );
    };
    let Some(site) = store.site(&site_key).await else {
        return (
            candidates,
            Some(ContextCandidateRankingResult::without_source(
                ContextLookupOutcome::Miss,
            )),
        );
    };
    let Some(page) = site.pages.get(&page_pattern) else {
        return (
            candidates,
            Some(ContextCandidateRankingResult::without_source(
                ContextLookupOutcome::Miss,
            )),
        );
    };

    let outcome = rank_candidates_for_page(page, intent_kind, &mut candidates);
    (candidates, Some(outcome))
}

fn rank_candidates_for_page(
    page: &context_store::PageContext,
    intent_kind: &str,
    candidates: &mut Vec<types::CandidateEvidence>,
) -> ContextCandidateRankingResult {
    let controls = page
        .forms
        .values()
        .flat_map(|form| form.controls.iter())
        .filter_map(|control| {
            let stats = control.intents.get(intent_kind)?;
            if stats.success_count == 0 || stats.success_count <= stats.failure_count {
                return None;
            }
            Some((
                control,
                stats.last_verified_day,
                PersistedCandidateRank {
                    net_successes: stats.success_count - stats.failure_count,
                    successes: stats.success_count,
                    last_verified_day: stats.last_verified_day.unwrap_or_default(),
                    observed: stats.source == Some(context_store::RecordSource::Observed),
                },
                structural_context_source(stats.source),
            ))
        })
        .collect::<Vec<_>>();
    if controls.is_empty() {
        return ContextCandidateRankingResult::without_source(ContextLookupOutcome::Miss);
    }

    let stale_match = controls
        .iter()
        .filter(|(_, last_verified_day, _, _)| last_verified_day.is_none())
        .find(|(control, _, _, _)| {
            candidates
                .iter()
                .any(|candidate| same_identity(control, candidate))
        })
        .map(|(_, _, _, source)| *source);
    let controls = controls
        .into_iter()
        .filter(|(_, last_verified_day, _, _)| last_verified_day.is_some())
        .collect::<Vec<_>>();

    let ranks = candidates
        .iter()
        .map(|candidate| {
            let (Some(role), Some(name)) = (candidate.role.as_deref(), candidate.name.as_deref())
            else {
                return None;
            };
            controls
                .iter()
                .filter(|(control, _, _, _)| {
                    control.role.trim().eq_ignore_ascii_case(role.trim())
                        && control
                            .accessible_name
                            .trim()
                            .eq_ignore_ascii_case(name.trim())
                })
                .map(|(_, _, rank, source)| (*rank, *source))
                .max_by_key(|(rank, _)| *rank)
        })
        .collect::<Vec<_>>();
    let Some(best_rank) = ranks.iter().flatten().map(|(rank, _)| *rank).max() else {
        return if let Some(source) = stale_match {
            ContextCandidateRankingResult {
                outcome: ContextLookupOutcome::StaleRejection,
                source,
            }
        } else {
            ContextCandidateRankingResult::without_source(ContextLookupOutcome::Miss)
        };
    };
    let best = ranks
        .iter()
        .enumerate()
        .filter_map(|(index, rank)| {
            rank.is_some_and(|(rank, _)| rank == best_rank)
                .then_some(index)
        })
        .collect::<Vec<_>>();
    let [best_index] = best.as_slice() else {
        return ContextCandidateRankingResult {
            outcome: ContextLookupOutcome::AmbiguousRefusal,
            source: best
                .first()
                .and_then(|index| ranks[*index].and_then(|(_, source)| source)),
        };
    };
    let source = ranks[*best_index].and_then(|(_, source)| source);
    let best = candidates.remove(*best_index);
    candidates.insert(0, best);
    ContextCandidateRankingResult {
        outcome: ContextLookupOutcome::Hit,
        source,
    }
}

fn same_identity(
    control: &context_store::ControlContext,
    candidate: &types::CandidateEvidence,
) -> bool {
    let (Some(role), Some(name)) = (candidate.role.as_deref(), candidate.name.as_deref()) else {
        return false;
    };
    control.role.trim().eq_ignore_ascii_case(role.trim())
        && control
            .accessible_name
            .trim()
            .eq_ignore_ascii_case(name.trim())
}

fn structural_context_source(
    source: Option<context_store::RecordSource>,
) -> Option<StructuralContextSource> {
    match source {
        Some(context_store::RecordSource::Observed) => Some(StructuralContextSource::Observed),
        Some(context_store::RecordSource::VisionPromoted) => {
            Some(StructuralContextSource::VisionPromoted)
        }
        None => None,
    }
}

#[cfg(test)]
mod context_candidate_ranking_tests {
    use super::*;
    use context_store::{ControlContext, FormContext, IntentStats, RecordSource};
    use std::collections::BTreeMap;

    fn page(control_name: &str) -> context_store::PageContext {
        context_store::PageContext {
            forms: BTreeMap::from([(
                "page".into(),
                FormContext {
                    controls: vec![ControlContext {
                        role: "button".into(),
                        accessible_name: control_name.into(),
                        ordinal: None,
                        form_membership: "page".into(),
                        intents: BTreeMap::from([(
                            "locate".into(),
                            IntentStats {
                                success_count: 2,
                                failure_count: 0,
                                last_verified_day: Some(20_000),
                                source: Some(RecordSource::Observed),
                            },
                        )]),
                    }],
                },
            )]),
        }
    }

    fn candidate(name: &str) -> types::CandidateEvidence {
        types::CandidateEvidence {
            role: Some("button".into()),
            name: Some(name.into()),
            score: 0,
            reasons: vec!["noMatch".into()],
        }
    }

    #[test]
    fn duplicate_semantic_identity_refuses_context_ranking() {
        let mut candidates = vec![candidate("Continue"), candidate("Continue")];
        assert_eq!(
            rank_candidates_for_page(&page("Continue"), "locate", &mut candidates).outcome,
            ContextLookupOutcome::AmbiguousRefusal
        );
    }

    #[test]
    fn missing_live_identity_does_not_rank() {
        let mut candidates = vec![candidate("Cancel")];
        assert_eq!(
            rank_candidates_for_page(&page("Continue"), "locate", &mut candidates).outcome,
            ContextLookupOutcome::Miss
        );
    }

    #[test]
    fn matching_context_without_a_freshness_stamp_is_rejected_as_stale() {
        let mut stale = page("Continue");
        let stats = stale
            .forms
            .get_mut("page")
            .unwrap()
            .controls
            .first_mut()
            .unwrap()
            .intents
            .get_mut("locate")
            .unwrap();
        stats.last_verified_day = None;
        stats.source = Some(RecordSource::VisionPromoted);
        let mut candidates = vec![candidate("Cancel"), candidate("Continue")];

        let result = rank_candidates_for_page(&stale, "locate", &mut candidates);

        assert_eq!(result.outcome, ContextLookupOutcome::StaleRejection);
        assert_eq!(
            result.source,
            Some(observability::StructuralContextSource::VisionPromoted)
        );
        assert_eq!(candidates[0].name.as_deref(), Some("Cancel"));
    }
}

fn semantic_tokens(value: &str) -> BTreeSet<String> {
    const STOP_WORDS: &[&str] = &[
        "a", "an", "the", "in", "inside", "within", "on", "of", "for", "to", "button", "control",
        "element", "field", "iframe", "frame", "link",
    ];
    value
        .split(|character: char| !character.is_alphanumeric())
        .map(str::to_ascii_lowercase)
        .filter(|token| token.len() > 1 && !STOP_WORDS.contains(&token.as_str()))
        .collect()
}

/// The rule and the repair for a fill/select intent that names a file
/// control: static text, since `intent-engine` errors are not subject to the
/// MCP-boundary redaction rule but there is nothing instance-specific worth
/// adding -- the control id space here (`dom_engine::Candidate::id`) is not
/// the `controlId` `upload_files`/`control_action` resolve against (that one
/// comes from `worker-pool`'s form snapshot), so naming a lookup is the
/// correct repair, not echoing an id that would not resolve there.
const FILE_CONTROL_UPLOAD_MESSAGE: &str = "target resolves to a file input; file inputs accept \
    values only through upload_files, never a fill or select intent. Read the control's \
    controlId from workflow_observe (includeForms:true) or form_snapshot, then call \
    upload_files with that controlId.";

/// The typed, deterministic failure for a fill/select intent that resolved
/// to a file control: `IntentActionMismatch` already covers "the action
/// does not match the resolved control's kind" (see the compat check
/// below), is already listed in `never_escalates`, and already carries a
/// generic wire-level repair -- no new `ErrorCode` earns its keep here.
fn file_control_failure(
    purpose: Option<String>,
    plan_summary: String,
    candidates: Vec<types::CandidateEvidence>,
) -> IntentOutcome {
    IntentOutcome::Failed {
        error: CommandError {
            code: ErrorCode::IntentActionMismatch,
            message: FILE_CONTROL_UPLOAD_MESSAGE.to_owned(),
            layer: ErrorLayer::Page,
            retryable: false,
        },
        evidence: vec![intent_evidence(execution_record(
            "fill",
            purpose,
            plan_summary,
            candidates,
            None,
            "fileControlRequiresUpload",
        ))],
    }
}

/// Whether `target` names a file input among `candidates`, resolved with
/// visibility relaxed. A native `<input type="file">` is routinely hidden
/// behind a styled "choose file" button, so the visible-only policy the
/// normal fill path uses can legitimately find nothing for it; that must
/// read as "this is a file control" rather than "target not found" so the
/// caller gets sent to `upload_files`.
fn targets_file_control(target: &TargetSpec, candidates: &[Candidate]) -> bool {
    let file_candidates: Vec<Candidate> = candidates
        .iter()
        .filter(|candidate| is_file_input(candidate))
        .cloned()
        .collect();
    if file_candidates.is_empty() {
        return false;
    }
    let policy = ResolutionPolicy {
        require_visible: false,
        ..ResolutionPolicy::default()
    };
    matches!(
        resolve_candidates(target, &file_candidates, &policy),
        Ok(ResolutionDecision::Resolved { .. })
    )
}

async fn execute_fill(
    intent: &IntentCommand,
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    target: TargetSpec,
    value: ControlAction,
) -> IntentOutcome {
    let purpose = match intent {
        IntentCommand::Fill(fill) => Some(fill.purpose.clone()),
        _ => None,
    };
    let plan_summary = format!("{} value={}", summarize_target(&target), fill_kind(&value));
    let fill_payload = match value {
        ControlAction::Activate => None,
        _ => Some(VisionFillPayload {
            action: value.clone(),
        }),
    };
    let candidates = match browser.collect_candidates(page_id, &target).await {
        Ok(candidates) => candidates,
        Err(error) => {
            return non_escalating_failure(
                error,
                intent_evidence(execution_record(
                    "fill",
                    purpose,
                    plan_summary,
                    Vec::new(),
                    None,
                    "gatherFailed",
                )),
            );
        }
    };
    // Labels and wrapper nodes often share a control's accessible name. They
    // must not make a fill ambiguous when exactly one gathered candidate can
    // perform the requested typed action. Preserve the original pool when no
    // candidate is compatible so the existing action-mismatch diagnostic is
    // still available instead of degrading it to target-not-found.
    //
    // The pool swap is for RESOLUTION only. The escalation window below must
    // carry the full (ranked) census: the adapter trains on full-page
    // windows, and a compatible-only window (often a single row) is so far
    // off that distribution that the model abstains on its only option.
    // Act-time compatibility still fails closed on an incompatible pick.
    let window_candidates = candidates.clone();
    let compatible_candidates = candidates
        .iter()
        .filter(|candidate| compatible(&value, candidate))
        .cloned()
        .collect::<Vec<_>>();
    let candidates = if compatible_candidates.is_empty() {
        candidates
    } else {
        compatible_candidates
    };

    let decision = match resolve_candidates(&target, &candidates, &ResolutionPolicy::default()) {
        Ok(decision) => decision,
        Err(error) => {
            return IntentOutcome::Failed {
                error: CommandError {
                    code: ErrorCode::InvalidRequest,
                    message: error.to_string(),
                    layer: ErrorLayer::Page,
                    retryable: false,
                },
                evidence: vec![intent_evidence(execution_record(
                    "fill",
                    purpose,
                    plan_summary,
                    Vec::new(),
                    None,
                    "resolveFailed",
                ))],
            };
        }
    };

    let (candidate, candidate_evidence, best_match_authorized) = match decision {
        ResolutionDecision::Resolved {
            candidate,
            evidence,
            best_match_authorized,
        } => (candidate, evidence, best_match_authorized),
        ResolutionDecision::NotFound => {
            // A native file input is very often visually hidden behind a
            // styled "choose file" button, so the visible-only resolution
            // above legitimately finds nothing -- but the control is real,
            // and no vision escalation can fill it: `ControlAction::SetFiles`
            // takes paths, which are runtime-only, so only `upload_files`
            // (or `control_action`) can act on it. Catch that here, before
            // the stuck path can turn it into a `targetNotFound` that then
            // escalates to a vision fallback which was never going to help.
            if !matches!(value, ControlAction::SetFiles { .. })
                && targets_file_control(&target, &window_candidates)
            {
                return file_control_failure(purpose, plan_summary, Vec::new());
            }
            // Fill escalations must carry the same purpose-ranked window as
            // locate: an empty window asks the model to pick from nothing,
            // and it correctly abstains — those records are the §4i poison
            // class, not selection signal.
            let near_misses = ranked_near_miss_window(&window_candidates, purpose.as_deref());
            return stuck_outcome(
                StuckReport {
                    intent_kind: "fill",
                    kind: StuckKind::TargetMissing,
                    purpose,
                    plan_summary,
                    candidates: near_misses,
                    verification: "targetNotFound",
                    fill_payload: fill_payload.clone(),
                },
                page_id,
                browser,
                vision,
            )
            .await;
        }
        ResolutionDecision::Ambiguous { candidates } => {
            return stuck_outcome(
                StuckReport {
                    intent_kind: "fill",
                    kind: StuckKind::TargetAmbiguous,
                    purpose,
                    plan_summary,
                    candidates,
                    verification: "targetAmbiguous",
                    fill_payload,
                },
                page_id,
                browser,
                vision,
            )
            .await;
        }
    };

    if !compatible(&value, &candidate) {
        // Resolution found the real control and it is a file input: the
        // generic action-mismatch message below ("wrong role/type") would
        // send an agent hunting for a different role, when the actual fix is
        // a different tool. `is_file_input` implies `value` was not already
        // `SetFiles` -- `compatible` would have accepted that combination.
        if is_file_input(&candidate) {
            return file_control_failure(purpose, plan_summary, vec![candidate_evidence]);
        }
        return IntentOutcome::Failed {
            error: CommandError {
                code: ErrorCode::IntentActionMismatch,
                message: format!(
                    "fill {} is incompatible with resolved control role={:?} type={:?}",
                    fill_kind(&value),
                    candidate.role,
                    candidate.attributes.get("type")
                ),
                layer: ErrorLayer::Page,
                retryable: false,
            },
            evidence: vec![intent_evidence(execution_record(
                "fill",
                purpose,
                plan_summary,
                vec![candidate_evidence],
                None,
                "actionMismatch",
            ))],
        };
    }

    let fingerprint = fingerprint(page_id, &candidate);
    let resolution = Evidence::Resolution {
        target: Box::new(target.clone()),
        fingerprint: Box::new(fingerprint),
        candidates: vec![candidate_evidence.clone()],
        best_match_authorized,
    };

    let mut act_evidence = match act_fill(page_id, browser, &candidate, &target, &value).await {
        Ok(evidence) => evidence,
        Err(error) => {
            return IntentOutcome::Failed {
                error,
                evidence: vec![
                    resolution,
                    intent_evidence(execution_record(
                        "fill",
                        purpose,
                        plan_summary,
                        vec![candidate_evidence],
                        None,
                        "actFailed",
                    )),
                ],
            };
        }
    };

    if let Err(message) = verify_fill(&value, &act_evidence) {
        return IntentOutcome::Failed {
            error: CommandError {
                code: ErrorCode::VerificationFailed,
                message,
                layer: ErrorLayer::Page,
                retryable: false,
            },
            evidence: {
                let mut evidence = vec![resolution];
                evidence.append(&mut act_evidence);
                evidence.push(intent_evidence(execution_record(
                    "fill",
                    purpose,
                    plan_summary,
                    vec![candidate_evidence],
                    None,
                    "verifyFailed",
                )));
                evidence
            },
        };
    }

    let mut evidence = vec![resolution];
    evidence.append(&mut act_evidence);
    evidence.push(intent_evidence(execution_record(
        "fill",
        purpose,
        plan_summary,
        vec![candidate_evidence],
        None,
        "filled",
    )));
    IntentOutcome::Completed { evidence }
}

async fn act_fill(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    candidate: &Candidate,
    intent_target: &TargetSpec,
    value: &ControlAction,
) -> Result<Vec<Evidence>, CommandError> {
    let (selector, target) = action_target(candidate, intent_target);
    match value {
        // Worker-pool has no select API; SelectOne is typed via TypeTextCommand.
        ControlAction::SetText { value, clear_first } => {
            browser
                .type_text(
                    page_id,
                    &TypeTextCommand {
                        selector,
                        target: Some(target),
                        value: value.clone(),
                        clear_first: *clear_first,
                        expected_url: None,
                    },
                )
                .await
        }
        ControlAction::SelectOne { value } => {
            browser
                .control_action(
                    page_id,
                    &ControlActionCommand {
                        target: form_control_target(candidate, intent_target)?,
                        action: ControlAction::SelectOne {
                            value: value.clone(),
                        },
                    },
                )
                .await
        }
        ControlAction::SetChecked { checked } => {
            browser
                .control_action(
                    page_id,
                    &ControlActionCommand {
                        target: form_control_target(candidate, intent_target)?,
                        action: ControlAction::SetChecked { checked: *checked },
                    },
                )
                .await
        }
        ControlAction::SetFiles { paths } => {
            browser
                .upload_files(
                    page_id,
                    &UploadFilesCommand {
                        selector,
                        target: Some(target),
                        paths: paths.clone(),
                    },
                )
                .await
        }
        ControlAction::SelectMany { values } => {
            browser
                .control_action(
                    page_id,
                    &ControlActionCommand {
                        target: form_control_target(candidate, intent_target)?,
                        action: ControlAction::SelectMany {
                            values: values.clone(),
                        },
                    },
                )
                .await
        }
        ControlAction::Clear => {
            browser
                .control_action(
                    page_id,
                    &ControlActionCommand {
                        target: form_control_target(candidate, intent_target)?,
                        action: ControlAction::Clear,
                    },
                )
                .await
        }
        ControlAction::Activate => Err(CommandError {
            code: ErrorCode::InvalidRequest,
            message: "activate is not valid for fill; use control_action instead".into(),
            layer: ErrorLayer::Page,
            retryable: false,
        }),
    }
}

fn form_control_target(
    candidate: &Candidate,
    intent_target: &TargetSpec,
) -> Result<FormControlTarget, CommandError> {
    let segment = |target: &TargetSpec| -> Result<SemanticTargetSegment, CommandError> {
        let role = target.role.clone().ok_or_else(|| CommandError {
            code: ErrorCode::InvalidRequest,
            message: "form control path segment requires a semantic role".into(),
            layer: ErrorLayer::Page,
            retryable: false,
        })?;
        let accessible_name = target.accessible_name.clone().ok_or_else(|| CommandError {
            code: ErrorCode::InvalidRequest,
            message: "form control path segment requires an accessible name".into(),
            layer: ErrorLayer::Page,
            retryable: false,
        })?;
        Ok(SemanticTargetSegment {
            role,
            accessible_name,
            ordinal: target.ordinal,
        })
    };
    Ok(FormControlTarget {
        role: candidate.role.clone().ok_or_else(|| CommandError {
            code: ErrorCode::IntentActionMismatch,
            message: "resolved form control has no semantic role".into(),
            layer: ErrorLayer::Page,
            retryable: false,
        })?,
        accessible_name: candidate.name.clone().ok_or_else(|| CommandError {
            code: ErrorCode::IntentActionMismatch,
            message: "resolved form control has no accessible name".into(),
            layer: ErrorLayer::Page,
            retryable: false,
        })?,
        ordinal: intent_target.ordinal,
        frame_path: intent_target
            .frame_path
            .iter()
            .map(|target| segment(target))
            .collect::<Result<Vec<_>, _>>()?,
        shadow_path: intent_target
            .shadow_path
            .iter()
            .map(|target| segment(target))
            .collect::<Result<Vec<_>, _>>()?,
    })
}

fn action_target(candidate: &Candidate, intent_target: &TargetSpec) -> (String, TargetSpec) {
    let selector = candidate.css.clone().unwrap_or_default();
    // Keep frame/shadow hops from the intent so iframe/shadow fills still land.
    // Do not copy ordinal: the candidate is already chosen; re-resolving with
    // ordinal against a narrowed (often length-1) set fails duplicate-name fills.
    let target = TargetSpec {
        css: candidate.css.clone(),
        test_id: candidate.test_id.clone(),
        role: candidate.role.clone(),
        accessible_name: candidate.name.clone(),
        label: candidate.label.clone(),
        attributes: candidate.attributes.clone(),
        // An explicit frame path on the intent wins; otherwise use the one
        // the gather stamped when it found this candidate inside an iframe.
        frame_path: if intent_target.frame_path.is_empty() {
            candidate.frame_path.clone()
        } else {
            intent_target.frame_path.clone()
        },
        shadow_path: intent_target.shadow_path.clone(),
        ..TargetSpec::default()
    };
    (selector, target)
}

fn fingerprint(page_id: &PageId, candidate: &Candidate) -> TargetFingerprint {
    TargetFingerprint {
        page_id: page_id.clone(),
        frame: None,
        role: candidate.role.clone(),
        name: candidate.name.clone(),
        stable_attributes: candidate.attributes.clone(),
    }
}

fn fill_kind(value: &ControlAction) -> &'static str {
    match value {
        ControlAction::SetText { .. } => "setText",
        ControlAction::SelectOne { .. } => "selectOne",
        ControlAction::SetChecked { .. } => "setChecked",
        ControlAction::SetFiles { .. } => "setFiles",
        ControlAction::SelectMany { .. } => "selectMany",
        ControlAction::Clear => "clear",
        ControlAction::Activate => "activate",
    }
}

async fn execute_submit_and_verify(
    intent: &IntentCommand,
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    target: TargetSpec,
    expected_state: WaitForCommand,
) -> IntentOutcome {
    let (purpose, purpose_is_implicit_match) = match intent {
        IntentCommand::SubmitAndVerify(submit) => (
            Some(submit.purpose.clone()),
            submit.hints.accessible_name.is_none()
                && submit.hints.near_text.is_none()
                && submit.hints.ordinal.is_none(),
        ),
        _ => (None, false),
    };
    let plan_summary = format!(
        "{} expected_state={}",
        summarize_target(&target),
        wait_condition_kind(&expected_state.condition)
    );
    let candidates = match browser.collect_candidates(page_id, &target).await {
        Ok(candidates) => candidates,
        Err(error) => {
            return non_escalating_failure(
                error,
                intent_evidence(execution_record(
                    "submitAndVerify",
                    purpose,
                    plan_summary,
                    Vec::new(),
                    None,
                    "gatherFailed",
                )),
            );
        }
    };

    let decision = match resolve_candidates(&target, &candidates, &ResolutionPolicy::default()) {
        Ok(decision) => decision,
        Err(error) => {
            return IntentOutcome::Failed {
                error: CommandError {
                    code: ErrorCode::InvalidRequest,
                    message: error.to_string(),
                    layer: ErrorLayer::Page,
                    retryable: false,
                },
                evidence: vec![intent_evidence(execution_record(
                    "submitAndVerify",
                    purpose,
                    plan_summary,
                    Vec::new(),
                    None,
                    "resolveFailed",
                ))],
            };
        }
    };
    let decision = if purpose_is_implicit_match {
        purpose
            .as_deref()
            .and_then(|purpose| disambiguate_submit_by_purpose(&target, &candidates, purpose))
            .unwrap_or_else(|| match decision {
                ResolutionDecision::Resolved { evidence, .. } => ResolutionDecision::Ambiguous {
                    candidates: vec![evidence],
                },
                unresolved => unresolved,
            })
    } else {
        decision
    };

    let (candidate, candidate_evidence, best_match_authorized) = match decision {
        ResolutionDecision::Resolved {
            candidate,
            evidence,
            best_match_authorized,
        } => (candidate, evidence, best_match_authorized),
        ResolutionDecision::NotFound => {
            return stuck_outcome(
                StuckReport {
                    intent_kind: "submitAndVerify",
                    kind: StuckKind::TargetMissing,
                    purpose,
                    plan_summary,
                    candidates: Vec::new(),
                    verification: "targetNotFound",
                    fill_payload: None,
                },
                page_id,
                browser,
                vision,
            )
            .await;
        }
        ResolutionDecision::Ambiguous { candidates } => {
            return stuck_outcome(
                StuckReport {
                    intent_kind: "submitAndVerify",
                    kind: StuckKind::TargetAmbiguous,
                    purpose,
                    plan_summary,
                    candidates,
                    verification: "targetAmbiguous",
                    fill_payload: None,
                },
                page_id,
                browser,
                vision,
            )
            .await;
        }
    };

    let fingerprint = fingerprint(page_id, &candidate);
    let resolution = Evidence::Resolution {
        target: Box::new(target.clone()),
        fingerprint: Box::new(fingerprint),
        candidates: vec![candidate_evidence.clone()],
        best_match_authorized,
    };

    let (selector, action_target) = action_target(&candidate, &target);
    let click = ClickCommand {
        selector,
        target: Some(action_target),
        boundary: true,
        expected_url: expected_url_from_wait(&expected_state),
        modifiers: Vec::new(),
    };
    // If a text/element/value expected-state already holds before the click,
    // a post-act pass proves nothing — the matcher hit static page copy and
    // the agent would trust a submit that may never have landed. Url,
    // document, and networkQuiet states legitimately pre-hold, so they skip
    // this check. Fail without clicking.
    //
    // The 2s window is a settle budget, not a retry: page-scoped text reads
    // race the SPA's own data fetch, and a matcher against static copy only
    // shows itself once the app has rendered. A static-copy matcher matches
    // at the FIRST poll (~50ms) — misuse is caught fast; the full window is
    // paid only by correctly-scoped matchers whose state genuinely does not
    // pre-hold, which is the ~1.25s price of not verifying against content
    // that was still loading. Measured in the --runs 3 gauntlet batch: a
    // 750ms window let a static "Atlas" matcher through on a slow-rendering
    // customer page, the submit "verified" nothing, and the agent re-ran the
    // Boundary submit (boundary-once now refuses that, but the first line of
    // defense is a pre-check that outlives the render).
    if matches!(
        expected_state.condition,
        WaitCondition::Text { .. } | WaitCondition::Element { .. } | WaitCondition::Value { .. }
    ) {
        let pre_check = WaitForCommand {
            condition: expected_state.condition.clone(),
            timeout_ms: 2_000,
        };
        if browser.wait_for(page_id, &pre_check).await.is_ok() {
            return IntentOutcome::Failed {
                error: CommandError {
                    code: ErrorCode::ExpectedStatePreSatisfied,
                    message: format!(
                        "the expected post-state ({}) already held before the submit ran; \
                         the matcher likely hits static page copy. Strengthen expectedState \
                         to content that only appears after the submit",
                        wait_condition_kind(&expected_state.condition)
                    ),
                    layer: ErrorLayer::Page,
                    retryable: false,
                },
                evidence: vec![
                    resolution,
                    intent_evidence(execution_record(
                        "submitAndVerify",
                        purpose,
                        plan_summary,
                        vec![candidate_evidence],
                        None,
                        "verifyPreSatisfied",
                    )),
                ],
            };
        }
    }
    let mut click_evidence = match browser.click(page_id, &click).await {
        Ok(evidence) => evidence,
        Err(error) if post_navigation_context_loss(&error) => {
            // The browser destroyed the frame execution context while the
            // click was returning. The side effect may already have landed;
            // only the caller's expected state can decide. Continue to that
            // verification instead of reporting an unverified act failure.
            vec![Evidence::Configuration {
                name: "clickDispatch".into(),
                value: "execution context replaced; verifying expected post-state".into(),
            }]
        }
        Err(error) => {
            return IntentOutcome::Failed {
                error,
                evidence: vec![
                    resolution,
                    intent_evidence(execution_record(
                        "submitAndVerify",
                        purpose,
                        plan_summary,
                        vec![candidate_evidence],
                        None,
                        "actFailed",
                    )),
                ],
            };
        }
    };

    let mut wait_evidence = match browser.wait_for(page_id, &expected_state).await {
        Ok(evidence) => evidence,
        Err(error) => {
            // The boundary click already landed; only the post-state
            // verification failed. Reporting the raw wait error would read as
            // "the submit failed" and invite a blind resubmit that duplicates
            // the POST. Rewrap so the effect and the safe next step are
            // explicit.
            let error = CommandError {
                code: ErrorCode::VerificationFailed,
                message: format!(
                    "submit click landed but the expected post-state did not hold ({}): {}. \
                     Do not resubmit blindly — inspect the page for a server rejection or \
                     confirmation, then re-verify with intent_wait_for_state or correct the \
                     rejected fields",
                    wait_condition_kind(&expected_state.condition),
                    error.message
                ),
                layer: error.layer,
                retryable: false,
            };
            return IntentOutcome::Failed {
                error,
                evidence: {
                    let mut evidence = vec![resolution];
                    evidence.append(&mut click_evidence);
                    evidence.push(intent_evidence(execution_record(
                        "submitAndVerify",
                        purpose,
                        plan_summary,
                        vec![candidate_evidence],
                        None,
                        "verifyFailed",
                    )));
                    evidence
                },
            };
        }
    };

    // A network-quiet postcondition is deliberately copy-independent. Once it
    // settles, return the bounded post-submit inspection and classify visible
    // client-side validation as a recoverable outcome. The boundary click has
    // already happened exactly once, so a validation rejection is data for the
    // caller to repair, not a tool error that could invite a blind resubmit.
    let mut settlement_verification = "submitted";
    if matches!(expected_state.condition, WaitCondition::NetworkQuiet { .. }) {
        // Inspect first. Network-idle can become true just before the browser
        // commits the response-driven DOM update; the bounded inspection is a
        // page round trip that observes that render before validation is
        // classified. Checking aria-invalid first can read the rejected form
        // that is about to be replaced by a success state.
        let mut inspection_evidence = match browser.inspect_settled_page(page_id).await {
            Ok(evidence) => evidence,
            Err(error) => {
                return IntentOutcome::Failed {
                    error,
                    evidence: {
                        let mut evidence = vec![resolution];
                        evidence.append(&mut click_evidence);
                        evidence.append(&mut wait_evidence);
                        evidence.push(intent_evidence(execution_record(
                            "submitAndVerify",
                            purpose,
                            plan_summary,
                            vec![candidate_evidence],
                            None,
                            "inspectFailed",
                        )));
                        evidence
                    },
                };
            }
        };
        let validation_issues = match browser.validation_issues(page_id).await {
            Ok(issues) => issues,
            Err(error) => {
                return IntentOutcome::Failed {
                    error,
                    evidence: {
                        let mut evidence = vec![resolution];
                        evidence.append(&mut click_evidence);
                        evidence.append(&mut wait_evidence);
                        evidence.append(&mut inspection_evidence);
                        evidence.push(intent_evidence(execution_record(
                            "submitAndVerify",
                            purpose,
                            plan_summary,
                            vec![candidate_evidence],
                            None,
                            "verifyFailed",
                        )));
                        evidence
                    },
                };
            }
        };
        let outcome = if validation_issues.is_empty() {
            types::SubmitSettlementOutcome::Settled
        } else {
            settlement_verification = "validationRejected";
            types::SubmitSettlementOutcome::ValidationRejected
        };
        wait_evidence.push(Evidence::SubmitSettlement { outcome });
        if !validation_issues.is_empty() {
            wait_evidence.push(Evidence::FormValidation {
                issues: validation_issues,
            });
        }
        wait_evidence.append(&mut inspection_evidence);
    }

    let wait_elapsed_ms = wait_evidence.iter().find_map(|item| match item {
        Evidence::Wait { elapsed_ms, .. } => Some(*elapsed_ms),
        _ => None,
    });

    let mut evidence = vec![resolution];
    evidence.append(&mut click_evidence);
    evidence.append(&mut wait_evidence);
    evidence.push(intent_evidence(execution_record(
        "submitAndVerify",
        purpose,
        plan_summary,
        vec![candidate_evidence],
        wait_elapsed_ms,
        settlement_verification,
    )));
    IntentOutcome::Completed { evidence }
}

fn post_navigation_context_loss(error: &CommandError) -> bool {
    if error.code != ErrorCode::BrowserCommandFailed {
        return false;
    }
    let message = error.message.to_ascii_lowercase();
    message.contains("cannot find context with specified id")
        || message.contains("execution context was destroyed")
}

fn disambiguate_submit_by_purpose(
    target: &TargetSpec,
    candidates: &[Candidate],
    purpose: &str,
) -> Option<ResolutionDecision> {
    let actionable_candidates = candidates
        .iter()
        .filter(|candidate| {
            candidate.state.attached && candidate.state.visible && candidate.state.enabled
        })
        .cloned()
        .collect::<Vec<_>>();

    if let Some(decision) = disambiguate_by_purpose(target, &actionable_candidates, purpose) {
        return Some(decision);
    }

    let wanted_role = target.role.as_deref();
    let submit_candidates = actionable_candidates
        .iter()
        .filter(|candidate| {
            wanted_role.is_none_or(|wanted| {
                candidate
                    .role
                    .as_deref()
                    .is_some_and(|role| role.eq_ignore_ascii_case(wanted))
            }) && candidate
                .attributes
                .get("type")
                .is_some_and(|value| value.eq_ignore_ascii_case("submit"))
        })
        .collect::<Vec<_>>();
    let [candidate] = submit_candidates.as_slice() else {
        return None;
    };

    let mut reasons = Vec::new();
    let mut score = 40;
    if wanted_role.is_some() {
        reasons.push("exactRole".into());
        score += 30;
    }
    reasons.push("uniqueSubmitControl".into());
    Some(ResolutionDecision::Resolved {
        candidate: Box::new((*candidate).clone()),
        evidence: types::CandidateEvidence {
            role: candidate.role.clone(),
            name: candidate.name.clone(),
            score,
            reasons,
        },
        best_match_authorized: false,
    })
}

/// A post-click wait error whose code only reflects targeting trouble
/// (`TargetAmbiguous`, `TargetNotFound`, `InvalidRequest`) means the effect
/// already happened and only verification could not confirm it — not a fresh,
/// retryable failure. Re-codes it `VerificationFailed` with `prefix` ahead of
/// the original message, so a repair reader sees "landed, unverified" instead
/// of "ambiguous, retry". Any other code (e.g. a real command failure) passes
/// through unchanged.
fn recode_postclick_verification_error(error: CommandError, prefix: &str) -> CommandError {
    match error.code {
        ErrorCode::TargetAmbiguous | ErrorCode::TargetNotFound | ErrorCode::InvalidRequest => {
            CommandError {
                code: ErrorCode::VerificationFailed,
                message: format!("{prefix}: {}", error.message),
                layer: error.layer,
                retryable: false,
            }
        }
        _ => error,
    }
}

async fn execute_follow(
    intent: &IntentCommand,
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    target: TargetSpec,
    expected_destination: WaitForCommand,
    boundary: bool,
) -> IntentOutcome {
    let purpose = match intent {
        IntentCommand::Follow(follow) => Some(follow.purpose.clone()),
        _ => None,
    };
    let plan_summary = format!(
        "{} expected_destination={}",
        summarize_target(&target),
        wait_condition_kind(&expected_destination.condition)
    );
    let candidates = match browser.collect_candidates(page_id, &target).await {
        Ok(candidates) => candidates,
        Err(error) => {
            return non_escalating_failure(
                error,
                intent_evidence(execution_record(
                    "follow",
                    purpose,
                    plan_summary,
                    Vec::new(),
                    None,
                    "gatherFailed",
                )),
            );
        }
    };

    let decision = match resolve_candidates(&target, &candidates, &ResolutionPolicy::default()) {
        Ok(decision) => decision,
        Err(error) => {
            return IntentOutcome::Failed {
                error: CommandError {
                    code: ErrorCode::InvalidRequest,
                    message: error.to_string(),
                    layer: ErrorLayer::Page,
                    retryable: false,
                },
                evidence: vec![intent_evidence(execution_record(
                    "follow",
                    purpose,
                    plan_summary,
                    Vec::new(),
                    None,
                    "resolveFailed",
                ))],
            };
        }
    };

    let (candidate, candidate_evidence, best_match_authorized) = match decision {
        ResolutionDecision::Resolved {
            candidate,
            evidence,
            best_match_authorized,
        } => (candidate, evidence, best_match_authorized),
        ResolutionDecision::NotFound => {
            return stuck_outcome(
                StuckReport {
                    intent_kind: "follow",
                    kind: StuckKind::TargetMissing,
                    purpose,
                    plan_summary,
                    candidates: Vec::new(),
                    verification: "targetNotFound",
                    fill_payload: None,
                },
                page_id,
                browser,
                vision,
            )
            .await;
        }
        ResolutionDecision::Ambiguous { candidates } => {
            return stuck_outcome(
                StuckReport {
                    intent_kind: "follow",
                    kind: StuckKind::TargetAmbiguous,
                    purpose,
                    plan_summary,
                    candidates,
                    verification: "targetAmbiguous",
                    fill_payload: None,
                },
                page_id,
                browser,
                vision,
            )
            .await;
        }
    };

    let fingerprint = fingerprint(page_id, &candidate);
    let resolution = Evidence::Resolution {
        target: Box::new(target.clone()),
        fingerprint: Box::new(fingerprint),
        candidates: vec![candidate_evidence.clone()],
        best_match_authorized,
    };

    let (selector, action_target) = action_target(&candidate, &target);
    let click = ClickCommand {
        selector,
        target: Some(action_target),
        boundary,
        expected_url: expected_url_from_wait(&expected_destination),
        modifiers: Vec::new(),
    };
    let mut click_evidence = match browser.click(page_id, &click).await {
        Ok(evidence) => evidence,
        Err(error) => {
            return IntentOutcome::Failed {
                error,
                evidence: vec![
                    resolution,
                    intent_evidence(execution_record(
                        "follow",
                        purpose,
                        plan_summary,
                        vec![candidate_evidence],
                        None,
                        "actFailed",
                    )),
                ],
            };
        }
    };

    let mut wait_evidence = match browser.wait_for(page_id, &expected_destination).await {
        Ok(evidence) => evidence,
        Err(error) => {
            // The click already landed; a wait error that only reflects the
            // matcher's own targeting trouble (ambiguous/missing/invalid
            // target) is not a fresh failure to retry — it means the
            // post-state could not be verified. Reporting it verbatim reads
            // as a retryable ambiguity and invites re-clicking a control that
            // already fired.
            let error = recode_postclick_verification_error(
                error,
                "activation landed; expectedState could not be verified",
            );
            return IntentOutcome::Failed {
                error,
                evidence: {
                    let mut evidence = vec![resolution];
                    evidence.append(&mut click_evidence);
                    evidence.push(intent_evidence(execution_record(
                        "follow",
                        purpose,
                        plan_summary,
                        vec![candidate_evidence],
                        None,
                        "verifyFailed",
                    )));
                    evidence
                },
            };
        }
    };

    let wait_elapsed_ms = wait_evidence.iter().find_map(|item| match item {
        Evidence::Wait { elapsed_ms, .. } => Some(*elapsed_ms),
        _ => None,
    });

    let mut evidence = vec![resolution];
    evidence.append(&mut click_evidence);
    evidence.append(&mut wait_evidence);
    evidence.push(intent_evidence(execution_record(
        "follow",
        purpose,
        plan_summary,
        vec![candidate_evidence],
        wait_elapsed_ms,
        "followed",
    )));
    IntentOutcome::Completed { evidence }
}

/// Re-resolution poll interval while waiting for a dismissed obstruction to leave the DOM
/// or become hidden. Matches `worker-pool`'s `wait_for` cadence.
const DISMISS_POLL_INTERVAL_MS: u64 = 25;

async fn execute_dismiss_obstruction(
    intent: &IntentCommand,
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    target: TargetSpec,
    timeout_ms: u64,
) -> IntentOutcome {
    let purpose = match intent {
        IntentCommand::DismissObstruction(dismiss) => Some(dismiss.purpose.clone()),
        _ => None,
    };
    let plan_summary = format!("{} timeout_ms={timeout_ms}", summarize_target(&target));
    let candidates = match browser.collect_candidates(page_id, &target).await {
        Ok(candidates) => candidates,
        Err(error) => {
            return non_escalating_failure(
                error,
                intent_evidence(execution_record(
                    "dismissObstruction",
                    purpose,
                    plan_summary,
                    Vec::new(),
                    None,
                    "gatherFailed",
                )),
            );
        }
    };

    let decision = match resolve_candidates(&target, &candidates, &ResolutionPolicy::default()) {
        Ok(decision) => decision,
        Err(error) => {
            return IntentOutcome::Failed {
                error: CommandError {
                    code: ErrorCode::InvalidRequest,
                    message: error.to_string(),
                    layer: ErrorLayer::Page,
                    retryable: false,
                },
                evidence: vec![intent_evidence(execution_record(
                    "dismissObstruction",
                    purpose,
                    plan_summary,
                    Vec::new(),
                    None,
                    "resolveFailed",
                ))],
            };
        }
    };

    let (candidate, candidate_evidence, best_match_authorized) = match decision {
        ResolutionDecision::Resolved {
            candidate,
            evidence,
            best_match_authorized,
        } => (candidate, evidence, best_match_authorized),
        ResolutionDecision::NotFound => {
            return stuck_outcome(
                StuckReport {
                    intent_kind: "dismissObstruction",
                    kind: StuckKind::TargetMissing,
                    purpose,
                    plan_summary,
                    candidates: Vec::new(),
                    verification: "targetNotFound",
                    fill_payload: None,
                },
                page_id,
                browser,
                vision,
            )
            .await;
        }
        ResolutionDecision::Ambiguous { candidates } => {
            return stuck_outcome(
                StuckReport {
                    intent_kind: "dismissObstruction",
                    kind: StuckKind::TargetAmbiguous,
                    purpose,
                    plan_summary,
                    candidates,
                    verification: "targetAmbiguous",
                    fill_payload: None,
                },
                page_id,
                browser,
                vision,
            )
            .await;
        }
    };

    let fingerprint = fingerprint(page_id, &candidate);
    let resolution = Evidence::Resolution {
        target: Box::new(target.clone()),
        fingerprint: Box::new(fingerprint),
        candidates: vec![candidate_evidence.clone()],
        best_match_authorized,
    };

    let (selector, action_target) = action_target(&candidate, &target);
    let click = ClickCommand {
        selector,
        target: Some(action_target),
        // DismissObstructionIntent is always CommandClass::Reconciliable, so the act needs
        // no pre-established checkpoint and takes no caller-supplied boundary flag.
        boundary: false,
        expected_url: None,
        modifiers: Vec::new(),
    };
    let mut click_evidence = match browser.click(page_id, &click).await {
        Ok(evidence) => evidence,
        Err(error) => {
            return IntentOutcome::Failed {
                error,
                evidence: vec![
                    resolution,
                    intent_evidence(execution_record(
                        "dismissObstruction",
                        purpose,
                        plan_summary,
                        vec![candidate_evidence],
                        None,
                        "actFailed",
                    )),
                ],
            };
        }
    };

    let gone = wait_until_gone(page_id, browser, &target, timeout_ms).await;
    if !gone {
        let mut prior_evidence = vec![resolution];
        prior_evidence.append(&mut click_evidence);
        return stuck_outcome_with_prior_evidence(
            StuckReport {
                intent_kind: "dismissObstruction",
                kind: StuckKind::ObstructionSuspected,
                purpose,
                plan_summary,
                candidates: vec![candidate_evidence],
                verification: "obstructionPersisted",
                fill_payload: None,
            },
            page_id,
            browser,
            vision,
            prior_evidence,
        )
        .await;
    }

    let mut evidence = vec![resolution];
    evidence.append(&mut click_evidence);
    evidence.push(intent_evidence(execution_record(
        "dismissObstruction",
        purpose,
        plan_summary,
        vec![candidate_evidence],
        None,
        "dismissed",
    )));
    IntentOutcome::Completed { evidence }
}

/// Polls the acted-on target until it is detached or no longer visible. Both are checked in
/// one pass, unlike `WaitCondition::Element`: dismiss affordances do either and callers
/// supply no expectation.
async fn wait_until_gone(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    target: &TargetSpec,
    timeout_ms: u64,
) -> bool {
    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    loop {
        if is_gone(page_id, browser, target).await {
            return true;
        }
        if std::time::Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(std::time::Duration::from_millis(DISMISS_POLL_INTERVAL_MS)).await;
    }
}

async fn is_gone(page_id: &PageId, browser: &dyn IntentBrowser, target: &TargetSpec) -> bool {
    let Ok(candidates) = browser.collect_candidates(page_id, target).await else {
        return false;
    };
    match resolve_candidates(target, &candidates, &ResolutionPolicy::default()) {
        Ok(ResolutionDecision::NotFound) => true,
        Ok(ResolutionDecision::Resolved { candidate, .. }) => !candidate.state.visible,
        _ => false,
    }
}

/// Schema-bounded structured extraction. Each field resolves independently and an
/// unresolvable field is recorded as missing in its own `Extraction` evidence, so this
/// always returns `Completed`; the caller inspects per-field evidence.
async fn execute_extract(
    intent: &IntentCommand,
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    fields: Vec<ExtractFieldPlan>,
) -> IntentOutcome {
    let purpose = match intent {
        IntentCommand::Extract(extract) => Some(extract.purpose.clone()),
        _ => None,
    };
    let plan_summary = format!(
        "fields=[{}]",
        fields
            .iter()
            .map(|field| field.name.as_str())
            .collect::<Vec<_>>()
            .join(",")
    );

    let mut evidence = Vec::new();
    let mut missing_fields = Vec::new();
    for field in &fields {
        let mut field_evidence = resolve_extract_field(page_id, browser, vision, field).await;
        if matches!(
            field_evidence.last(),
            Some(Evidence::Extraction { value: None, .. })
        ) {
            missing_fields.push(field.name.clone());
        }
        evidence.append(&mut field_evidence);
    }

    let verification = if missing_fields.is_empty() {
        "extracted".to_owned()
    } else {
        format!("extractedPartial:missing={}", missing_fields.join(","))
    };
    evidence.push(intent_evidence(execution_record(
        "extract",
        purpose,
        plan_summary,
        Vec::new(),
        None,
        verification,
    )));
    IntentOutcome::Completed { evidence }
}

/// Resolves and reads one `ExtractIntent` field. Always returns evidence ending in exactly
/// one `Evidence::Extraction` for `field.name`, preceded by an `Evidence::Resolution` when
/// the field was found.
async fn resolve_extract_field(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    field: &ExtractFieldPlan,
) -> Vec<Evidence> {
    let candidates = match browser.collect_candidates(page_id, &field.target).await {
        Ok(candidates) => candidates,
        Err(error) => return vec![missing_extraction(&field.name, Some(error.code))],
    };

    // An `a11y_snapshot` node passed verbatim can name a role the element
    // collector never emits (`StaticText` and friends). Fail typed when the
    // deterministic resolver cannot place the target, before the stuck path
    // reports a generic not-found that would send the agent
    // re-snapshotting, or worse, a green `completed` with a silently
    // missing field.
    let decision = resolve_candidates(&field.target, &candidates, &ResolutionPolicy::default());
    if field.target.role.as_deref().is_some_and(a11y_only_role)
        && !matches!(decision, Ok(ResolutionDecision::Resolved { .. }))
    {
        return a11y_only_role_extraction(field);
    }

    match decision {
        Ok(ResolutionDecision::Resolved {
            candidate,
            evidence,
            best_match_authorized,
        }) => {
            let fingerprint = fingerprint(page_id, &candidate);
            let resolution = Evidence::Resolution {
                target: Box::new(field.target.clone()),
                fingerprint: Box::new(fingerprint),
                candidates: vec![evidence],
                best_match_authorized,
            };
            let value = extract_value_from_candidate(&field.value, &candidate);
            vec![
                resolution,
                Evidence::Extraction {
                    field: field.name.clone(),
                    value,
                    resolution_path: IntentResolutionPath::Deterministic,
                    error_code: None,
                },
            ]
        }
        Ok(ResolutionDecision::NotFound) => {
            escalate_extract_field_with_vision(
                page_id,
                browser,
                vision,
                field,
                &candidates,
                StuckKind::TargetMissing,
                ErrorCode::TargetNotFound,
            )
            .await
        }
        Ok(ResolutionDecision::Ambiguous { .. }) => {
            escalate_extract_field_with_vision(
                page_id,
                browser,
                vision,
                field,
                &candidates,
                StuckKind::TargetAmbiguous,
                ErrorCode::TargetAmbiguous,
            )
            .await
        }
        Err(_) => vec![missing_extraction(
            &field.name,
            Some(ErrorCode::InvalidRequest),
        )],
    }
}

/// Vision fallback for a field the deterministic resolver could not place, under the same
/// double-gate rule as every other vision fallback. Success never touches the page: legacy
/// providers can propose a value, while candidate-index proposals select a runtime-owned value.
async fn escalate_extract_field_with_vision(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    field: &ExtractFieldPlan,
    candidates: &[Candidate],
    stuck: StuckKind,
    deterministic_fallback_code: ErrorCode,
) -> Vec<Evidence> {
    if never_escalates(deterministic_fallback_code) || !stuck.may_escalate_to_vision() {
        return vec![missing_extraction(
            &field.name,
            Some(deterministic_fallback_code),
        )];
    }

    let gates_open = vision.session_ok && vision.capability_ok;
    let Some(assist) = vision.assist.as_ref() else {
        let code = if gates_open {
            ErrorCode::VisionAssistFailed
        } else {
            ErrorCode::VisionAssistDenied
        };
        return vec![missing_extraction(&field.name, Some(code))];
    };
    if !gates_open {
        return vec![missing_extraction(
            &field.name,
            Some(ErrorCode::VisionAssistDenied),
        )];
    }

    let (png, mut screenshot_evidence) = match browser
        .capture_screenshot(
            page_id,
            &CaptureScreenshotCommand {
                mode: ScreenshotMode::Viewport,
            },
        )
        .await
    {
        Ok(result) => result,
        Err(_) => {
            return vec![missing_extraction(
                &field.name,
                Some(ErrorCode::VisionAssistFailed),
            )]
        }
    };

    // The provider sees only structural prompt candidates. Keep the matching full DOM
    // candidates in this exact bounded order so an index can be resolved locally without
    // recollecting or accepting provider-authored text.
    let prompt_window = candidates
        .iter()
        .filter(|candidate| {
            vision_window_eligible(candidate.role.as_deref(), candidate.name.as_deref())
        })
        .take(5)
        .cloned()
        .collect::<Vec<_>>();
    let mut context = vision.prompt_context.clone();
    let block = context.get_or_insert_with(crate::VisionPromptContext::default);
    block.candidates = prompt_window
        .iter()
        .map(|candidate| crate::VisionPromptCandidate {
            role: candidate.role.clone().expect("filtered role"),
            name: candidate.name.clone().expect("filtered name"),
            ordinal: None,
        })
        .collect();

    let corpus_screenshot_png = if vision.corpus.is_some() || assist.collects_training_data() {
        capture_sanitized_corpus_screenshot(browser, page_id).await
    } else {
        None
    };
    let propose_started = std::time::Instant::now();
    let metric_context = assist.operational_metrics();
    let proposal = match assist
        .propose(VisionProposeRequest {
            purpose: field.purpose.clone(),
            intent_kind: "extract".to_owned(),
            screenshot_png: png,
            corpus_screenshot_png: corpus_screenshot_png.clone(),
            stuck,
            context,
        })
        .await
    {
        Ok(proposal) => proposal,
        Err(_) => {
            record_vision_metric(
                metric_context.as_ref(),
                propose_started.elapsed().as_millis() as u64,
                None,
                VisionProposalOutcome::Failed,
                None,
            );
            screenshot_evidence.push(missing_extraction(
                &field.name,
                Some(ErrorCode::VisionAssistFailed),
            ));
            return screenshot_evidence;
        }
    };
    let provider_latency_ms = propose_started.elapsed().as_millis() as u64;

    let value = match &proposal.action {
        VisionAction::ExtractValue { value } if proposal.confidence >= VISION_CONFIDENCE_FLOOR => {
            Some(value.clone())
        }
        VisionAction::ExtractFromCandidate { index }
            if proposal.confidence >= VISION_CONFIDENCE_FLOOR =>
        {
            usize::try_from(*index)
                .ok()
                .and_then(|index| prompt_window.get(index))
                .and_then(|candidate| extract_value_from_candidate(&field.value, candidate))
        }
        _ => None,
    };
    let error_code = value.is_none().then_some(ErrorCode::VisionAssistFailed);
    record_vision_metric(
        metric_context.as_ref(),
        provider_latency_ms,
        Some(proposal.confidence),
        if value.is_some() {
            VisionProposalOutcome::Accepted
        } else {
            VisionProposalOutcome::Rejected
        },
        Some(if value.is_some() {
            VerificationMetricResult::Accepted
        } else {
            VerificationMetricResult::OtherRejected
        }),
    );

    if let (Some(corpus), Some(screenshot_png)) = (&vision.corpus, corpus_screenshot_png) {
        let context_candidates = prompt_window
            .iter()
            .map(|candidate| crate::CorpusCandidate {
                role: candidate.role.clone().expect("filtered role"),
                name: candidate.name.clone().expect("filtered name"),
            })
            .collect::<Vec<_>>();
        if !context_candidates.is_empty() {
            let target_index = match proposal.action {
                VisionAction::ExtractFromCandidate { index } if value.is_some() => {
                    usize::try_from(index)
                        .ok()
                        .filter(|index| *index < context_candidates.len())
                }
                _ => None,
            };
            corpus.record(&crate::CorpusRecord {
                image_b64: base64::Engine::encode(
                    &base64::engine::general_purpose::STANDARD,
                    screenshot_png,
                ),
                purpose: field.purpose.clone(),
                intent_kind: "extract".into(),
                stuck: stuck_label(stuck).into(),
                context_url: vision
                    .prompt_context
                    .as_ref()
                    .and_then(|context| context.url.clone()),
                context_candidates,
                target_index,
                resolved_element: None,
                model_response: crate::corpus::CorpusModelResponse {
                    confidence: proposal.confidence,
                    action: crate::corpus::raw_action(&proposal.action, target_index),
                },
                success: value.is_some(),
                journey: "production".into(),
                step: "extract".into(),
                outcome_stage: if value.is_some() {
                    "visionFallback"
                } else {
                    "visionActFailed"
                }
                .into(),
                error_message: value
                    .is_none()
                    .then(|| "candidate extraction failed".into()),
            });
        }
    }

    let mut evidence = screenshot_evidence;
    evidence.push(Evidence::Extraction {
        field: field.name.clone(),
        value,
        resolution_path: IntentResolutionPath::VisionFallback,
        error_code,
    });
    evidence
}

fn missing_extraction(field: &str, error_code: Option<ErrorCode>) -> Evidence {
    Evidence::Extraction {
        field: field.to_owned(),
        value: None,
        resolution_path: IntentResolutionPath::Deterministic,
        error_code,
    }
}

/// Accessibility-tree roles that can never be a DOM candidate: the semantic
/// resolver gathers *elements* (`crates/worker-pool/src/targeting.rs`'s
/// `implicitRole` map), so roles the AX tree emits for text/layout structure
/// (`StaticText`, its label wrapper, select popups, inline text leaves)
/// resolve to nothing by construction. An `a11y_snapshot` advertises them,
/// so an agent that passes a snapshot node verbatim deserves a typed
/// explanation -- not a bare `targetNotFound` that invites retries and a
/// vision escalation that cannot help either.
fn a11y_only_role(role: &str) -> bool {
    matches!(
        role.to_ascii_lowercase().as_str(),
        "statictext"
            | "labeltext"
            | "inlinetextbox"
            | "menulistpopup"
            | "rootwebarea"
            | "genericcontainer"
            | "ignored"
            | "section"
            | "clientsidepushbutton"
            | "layouttable"
            | "layouttablecell"
            | "layouttablerow"
            | "linebreak"
    )
}

/// The typed extraction miss for an `a11y_snapshot`-shaped role the
/// deterministic resolver can never match: `value: None` (a miss, not a
/// failure — the intent still completes) with the a11y-only reason so the
/// agent repairs once instead of re-snapshotting and retrying.
fn a11y_only_role_extraction(field: &ExtractFieldPlan) -> Vec<Evidence> {
    let role = field.target.role.as_deref().unwrap_or("").to_owned();
    vec![
        Evidence::Configuration {
            name: "a11yOnlyRole".into(),
            value: role,
        },
        Evidence::Extraction {
            field: field.name.clone(),
            value: None,
            resolution_path: IntentResolutionPath::Deterministic,
            error_code: Some(ErrorCode::InvalidRequest),
        },
    ]
}

fn extract_value_from_candidate(kind: &ExtractValueKind, candidate: &Candidate) -> Option<String> {
    match kind {
        ExtractValueKind::Text => Some(candidate.text.clone()),
        ExtractValueKind::Attribute { attribute } => candidate.attributes.get(attribute).cloned(),
        ExtractValueKind::Href => candidate.attributes.get("href").cloned(),
    }
}

fn expected_url_from_wait(wait: &WaitForCommand) -> Option<String> {
    match &wait.condition {
        types::WaitCondition::Url {
            matcher: types::TextMatch::Exact(url),
        } => Some(url.clone()),
        _ => None,
    }
}

fn wait_condition_kind(condition: &types::WaitCondition) -> &'static str {
    match condition {
        types::WaitCondition::Element { .. } => "element",
        types::WaitCondition::Text { .. } => "text",
        types::WaitCondition::Value { .. } => "value",
        types::WaitCondition::Url { .. } => "url",
        types::WaitCondition::Document { .. } => "document",
        types::WaitCondition::NetworkQuiet { .. } => "networkQuiet",
    }
}

async fn execute_wait_for_state(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    condition: types::WaitCondition,
    timeout_ms: u64,
) -> IntentOutcome {
    let command = WaitForCommand {
        condition: condition.clone(),
        timeout_ms,
    };
    let plan_summary = format!("wait timeout_ms={timeout_ms}");
    match browser.wait_for(page_id, &command).await {
        Ok(mut evidence) => {
            let wait_elapsed_ms = evidence.iter().find_map(|item| match item {
                Evidence::Wait { elapsed_ms, .. } => Some(*elapsed_ms),
                _ => None,
            });
            evidence.push(intent_evidence(execution_record(
                "waitForState",
                None,
                plan_summary,
                Vec::new(),
                wait_elapsed_ms,
                "waitSatisfied",
            )));
            IntentOutcome::Completed { evidence }
        }
        Err(error) => IntentOutcome::Failed {
            error,
            evidence: vec![intent_evidence(execution_record(
                "waitForState",
                None,
                plan_summary,
                Vec::new(),
                None,
                "waitFailed",
            ))],
        },
    }
}

fn non_escalating_failure(error: CommandError, evidence: Evidence) -> IntentOutcome {
    IntentOutcome::Failed {
        error,
        evidence: vec![evidence],
    }
}

/// Pause between solve iterations: gives the widget time to react to the
/// last action (checkbox flip, grid round, verify) before the next
/// screenshot re-assessment.
const SOLVE_POLL_INTERVAL_MS: u64 = 750;

/// Vision-primary challenge solving. There is no DOM resolution phase: the
/// loop is screenshot → propose → act → reassess until the model reports
/// `challengeSolved` or the deadline passes. Small local models emit a dud
/// (unparseable, low-confidence) every few rounds, so a transient provider
/// error or a below-floor proposal costs one attempt and the loop
/// reassesses; only the deadline is terminal for those. Fails closed on the
/// paths that would act on an unverifiable proposal: a disallowed action or
/// a failed act ends the intent immediately.
async fn execute_solve_challenge(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    purpose: String,
    timeout_ms: u64,
) -> IntentOutcome {
    let plan_summary = format!("solveChallenge timeout_ms={timeout_ms}");
    // Site-level prior: which challenge kind has been attempted most for
    // this site. Read-only hint; the loop still detects from the frame.
    let challenge_prior = match (&vision.context_store, &vision.prompt_context) {
        (Some(store), Some(ctx)) => match ctx.url.as_deref().and_then(context_store::site_key) {
            Some(key) => store.challenge_prior(&key).await.map(|(kind, _stats)| kind),
            None => None,
        },
        _ => None,
    };
    // Purpose handed to the provider: the caller's purpose plus the site
    // prior when the graph has seen this site before. Evidence keeps the
    // original purpose so journals stay caller-shaped.
    let propose_purpose = match challenge_prior.as_deref() {
        Some(kind) => {
            format!("{purpose} Known challenge type for this site from prior runs: {kind}.")
        }
        None => purpose.clone(),
    };
    let gates_open = vision.session_ok && vision.capability_ok;
    let Some(assist) = vision.assist.as_ref().filter(|_| gates_open) else {
        let reason = if !vision.session_ok {
            "vision assist is off for this session (executionPolicy.visionAssist)"
        } else if !vision.capability_ok {
            "the principal lacks the vision:assist capability"
        } else {
            "no vision provider is configured"
        };
        return IntentOutcome::Failed {
            error: CommandError {
                code: ErrorCode::VisionAssistDenied,
                message: format!("solveChallenge requires vision assist; {reason}"),
                layer: ErrorLayer::Page,
                retryable: false,
            },
            evidence: vec![intent_evidence(execution_record(
                "solveChallenge",
                Some(purpose),
                plan_summary,
                Vec::new(),
                None,
                "visionDenied",
            ))],
        };
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    // Executed actions accumulate across iterations; only the latest
    // screenshot's evidence is reported, so a long grid solve does not
    // flood the journal with stale frames.
    let mut act_evidence: Vec<Evidence> = Vec::new();
    let mut attempts = 0_u32;
    // The most recent transient dud (provider error or below-floor
    // proposal), reported when the deadline is what finally ends the loop.
    let mut last_transient: Option<String> = None;
    loop {
        if std::time::Instant::now() >= deadline {
            let transient = last_transient
                .as_deref()
                .map(|message| format!("; last transient failure: {message}"))
                .unwrap_or_default();
            return IntentOutcome::Failed {
                error: CommandError {
                    code: ErrorCode::DeadlineExceeded,
                    message: format!(
                        "challenge not solved within {timeout_ms}ms ({attempts} attempts){transient}"
                    ),
                    layer: ErrorLayer::Page,
                    retryable: false,
                },
                evidence: {
                    let mut evidence = std::mem::take(&mut act_evidence);
                    evidence.push(intent_evidence(execution_record(
                        "solveChallenge",
                        Some(purpose),
                        plan_summary,
                        Vec::new(),
                        None,
                        format!("solveTimeout attempts={attempts}"),
                    )));
                    evidence
                },
            };
        }
        attempts += 1;

        let (png, mut screenshot_evidence) = match browser
            .capture_screenshot(
                page_id,
                &CaptureScreenshotCommand {
                    mode: ScreenshotMode::Viewport,
                },
            )
            .await
        {
            Ok(result) => result,
            Err(error) => {
                // Transient like any other dud: a renderer crash-and-recover
                // makes one capture fail while the page lives on. A truly
                // dead page keeps failing and the deadline reports it.
                last_transient = Some(format!("vision screenshot failed: {}", error.message));
                tokio::time::sleep(std::time::Duration::from_millis(SOLVE_POLL_INTERVAL_MS)).await;
                continue;
            }
        };

        let propose_started = std::time::Instant::now();
        let metric_context = assist.operational_metrics();
        let proposal = match assist
            .propose(VisionProposeRequest {
                purpose: propose_purpose.clone(),
                intent_kind: "solveChallenge".into(),
                screenshot_png: png,
                corpus_screenshot_png: None,
                stuck: StuckKind::ChallengePresent,
                context: vision.prompt_context.clone(),
            })
            .await
        {
            Ok(proposal) => proposal,
            Err(error) => {
                // Transient: small local models emit an unparseable or
                // off-schema reply every few rounds. Cost one attempt and
                // reassess rather than killing the whole budget.
                record_vision_metric(
                    metric_context.as_ref(),
                    propose_started.elapsed().as_millis() as u64,
                    None,
                    VisionProposalOutcome::Failed,
                    None,
                );
                last_transient = Some(format!("vision propose failed: {}", error.message));
                tokio::time::sleep(std::time::Duration::from_millis(SOLVE_POLL_INTERVAL_MS)).await;
                continue;
            }
        };
        let provider_latency_ms = propose_started.elapsed().as_millis() as u64;
        let proposal_hash = proposal_sha256(&proposal);

        if proposal.confidence < VISION_CONFIDENCE_FLOOR {
            // Below-floor is the model saying "not sure" — the correct next
            // step in a solve loop is a reassess, not a terminal failure.
            record_vision_metric(
                metric_context.as_ref(),
                provider_latency_ms,
                Some(proposal.confidence),
                VisionProposalOutcome::Rejected,
                Some(VerificationMetricResult::OtherRejected),
            );
            last_transient = Some(format!(
                "vision proposal confidence {:.2} below floor {VISION_CONFIDENCE_FLOOR}",
                proposal.confidence
            ));
            tokio::time::sleep(std::time::Duration::from_millis(SOLVE_POLL_INTERVAL_MS)).await;
            continue;
        }

        match &proposal.action {
            VisionAction::ChallengeSolved => {
                record_vision_metric(
                    metric_context.as_ref(),
                    provider_latency_ms,
                    Some(proposal.confidence),
                    VisionProposalOutcome::Accepted,
                    Some(VerificationMetricResult::Accepted),
                );
                let mut evidence = std::mem::take(&mut act_evidence);
                evidence.append(&mut screenshot_evidence);
                let artifact_ids = artifact_ids_from(&evidence);
                evidence.push(intent_evidence(execution_record_with_path(
                    "solveChallenge",
                    Some(purpose),
                    plan_summary,
                    Vec::new(),
                    None,
                    format!("challengeSolved attempts={attempts}"),
                    ResolutionDetails {
                        path: IntentResolutionPath::VisionFallback,
                        vision_proposal_sha256: Some(proposal_hash),
                        artifact_ids,
                    },
                )));
                return IntentOutcome::Completed { evidence };
            }
            action @ VisionAction::Click { .. } => {
                record_vision_metric(
                    metric_context.as_ref(),
                    provider_latency_ms,
                    Some(proposal.confidence),
                    VisionProposalOutcome::Accepted,
                    Some(VerificationMetricResult::Accepted),
                );
                match execute_vision_action(page_id, browser, action, &[], None).await {
                    Ok(mut step_evidence) => {
                        act_evidence.append(&mut step_evidence);
                    }
                    Err(error) => {
                        let mut evidence = std::mem::take(&mut act_evidence);
                        evidence.append(&mut screenshot_evidence);
                        evidence.push(intent_evidence(execution_record_with_path(
                            "solveChallenge",
                            Some(purpose),
                            plan_summary.clone(),
                            Vec::new(),
                            None,
                            format!("visionActFailed attempts={attempts}"),
                            ResolutionDetails {
                                path: IntentResolutionPath::VisionFallback,
                                vision_proposal_sha256: Some(proposal_hash),
                                artifact_ids: artifact_ids_from(&screenshot_evidence),
                            },
                        )));
                        return IntentOutcome::Failed {
                            error: CommandError {
                                code: ErrorCode::VisionAssistFailed,
                                message: format!("vision act failed: {}", error.message),
                                layer: ErrorLayer::Page,
                                retryable: false,
                            },
                            evidence,
                        };
                    }
                }
            }
            other => {
                let mut evidence = std::mem::take(&mut act_evidence);
                evidence.append(&mut screenshot_evidence);
                return IntentOutcome::Failed {
                    error: CommandError {
                        code: ErrorCode::VisionAssistFailed,
                        message: format!(
                            "vision action {other:?} is not allowed for solveChallenge"
                        ),
                        layer: ErrorLayer::Page,
                        retryable: false,
                    },
                    evidence,
                };
            }
        }

        tokio::time::sleep(std::time::Duration::from_millis(SOLVE_POLL_INTERVAL_MS)).await;
    }
}

/// Read-only challenge classification: screenshot → vision classify → report.
/// Never acts on the page. Detection carries no confidence floor — acting is
/// what the floor protects, and a caller choosing whether to solve needs the
/// model's honest uncertainty, not a silent retry loop. The site prior
/// enriches the prompt exactly like the solve path; it never blends into the
/// reported detection, so a clean page stays provably clean.
async fn execute_detect_challenge(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    purpose: String,
    timeout_ms: u64,
) -> IntentOutcome {
    let plan_summary = format!("detectChallenge timeout_ms={timeout_ms}");
    // Site-level prior, same read-only hint contract as the solve loop.
    let challenge_prior = match (&vision.context_store, &vision.prompt_context) {
        (Some(store), Some(ctx)) => match ctx.url.as_deref().and_then(context_store::site_key) {
            Some(key) => store.challenge_prior(&key).await.map(|(kind, _stats)| kind),
            None => None,
        },
        _ => None,
    };
    let propose_purpose = match challenge_prior.as_deref() {
        Some(kind) => {
            format!("{purpose} Known challenge type for this site from prior runs: {kind}.")
        }
        None => purpose.clone(),
    };
    let gates_open = vision.session_ok && vision.capability_ok;
    let Some(assist) = vision.assist.as_ref().filter(|_| gates_open) else {
        let reason = if !vision.session_ok {
            "vision assist is off for this session (executionPolicy.visionAssist)"
        } else if !vision.capability_ok {
            "the principal lacks the vision:assist capability"
        } else {
            "no vision provider is configured"
        };
        return IntentOutcome::Failed {
            error: CommandError {
                code: ErrorCode::VisionAssistDenied,
                message: format!("detectChallenge requires vision assist; {reason}"),
                layer: ErrorLayer::Page,
                retryable: false,
            },
            evidence: vec![intent_evidence(execution_record(
                "detectChallenge",
                Some(purpose),
                plan_summary,
                Vec::new(),
                None,
                "visionDenied",
            ))],
        };
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_millis(timeout_ms);
    let mut attempts = 0_u32;
    let mut last_transient: Option<String> = None;
    loop {
        if std::time::Instant::now() >= deadline {
            let transient = last_transient
                .as_deref()
                .map(|message| format!("; last transient failure: {message}"))
                .unwrap_or_default();
            return IntentOutcome::Failed {
                error: CommandError {
                    code: ErrorCode::DeadlineExceeded,
                    message: format!(
                        "challenge not classified within {timeout_ms}ms ({attempts} attempts){transient}"
                    ),
                    layer: ErrorLayer::Page,
                    retryable: false,
                },
                evidence: vec![intent_evidence(execution_record(
                    "detectChallenge",
                    Some(purpose),
                    plan_summary,
                    Vec::new(),
                    None,
                    format!("detectTimeout attempts={attempts}"),
                ))],
            };
        }
        attempts += 1;

        let (png, mut screenshot_evidence) = match browser
            .capture_screenshot(
                page_id,
                &CaptureScreenshotCommand {
                    mode: ScreenshotMode::Viewport,
                },
            )
            .await
        {
            Ok(result) => result,
            Err(error) => {
                last_transient = Some(format!("vision screenshot failed: {}", error.message));
                tokio::time::sleep(std::time::Duration::from_millis(SOLVE_POLL_INTERVAL_MS)).await;
                continue;
            }
        };

        let propose_started = std::time::Instant::now();
        let metric_context = assist.operational_metrics();
        let proposal = match assist
            .propose(VisionProposeRequest {
                purpose: propose_purpose.clone(),
                intent_kind: "detectChallenge".into(),
                screenshot_png: png,
                corpus_screenshot_png: None,
                stuck: StuckKind::ChallengePresent,
                context: vision.prompt_context.clone(),
            })
            .await
        {
            Ok(proposal) => proposal,
            Err(error) => {
                record_vision_metric(
                    metric_context.as_ref(),
                    propose_started.elapsed().as_millis() as u64,
                    None,
                    VisionProposalOutcome::Failed,
                    None,
                );
                last_transient = Some(format!("vision propose failed: {}", error.message));
                tokio::time::sleep(std::time::Duration::from_millis(SOLVE_POLL_INTERVAL_MS)).await;
                continue;
            }
        };
        let provider_latency_ms = propose_started.elapsed().as_millis() as u64;
        let proposal_hash = proposal_sha256(&proposal);

        let detection = match &proposal.action {
            VisionAction::ChallengeDetected {
                challenge_type,
                region,
                blocking,
            } => Some(types::ChallengeDetection {
                challenge_type: *challenge_type,
                confidence: proposal.confidence,
                region: *region,
                blocking: *blocking,
                hints: None,
            }),
            VisionAction::NoChallengeDetected => None,
            // Off-task answers (click, typeText, …) are an upstream
            // confusion, not a classification: one attempt and reassess.
            other => {
                record_vision_metric(
                    metric_context.as_ref(),
                    provider_latency_ms,
                    Some(proposal.confidence),
                    VisionProposalOutcome::Rejected,
                    Some(VerificationMetricResult::OtherRejected),
                );
                last_transient = Some(format!("vision action {other:?} is not a detection answer"));
                tokio::time::sleep(std::time::Duration::from_millis(SOLVE_POLL_INTERVAL_MS)).await;
                continue;
            }
        };

        record_vision_metric(
            metric_context.as_ref(),
            provider_latency_ms,
            Some(proposal.confidence),
            VisionProposalOutcome::Accepted,
            Some(VerificationMetricResult::Accepted),
        );
        let artifact_ids = artifact_ids_from(&screenshot_evidence);
        let mut evidence = Vec::new();
        evidence.append(&mut screenshot_evidence);
        evidence.push(Evidence::ChallengeDetection {
            confidence: proposal.confidence,
            detection,
            prior_kind: challenge_prior.clone(),
        });
        evidence.push(intent_evidence(execution_record_with_path(
            "detectChallenge",
            Some(purpose),
            plan_summary,
            Vec::new(),
            None,
            format!("challengeClassified attempts={attempts}"),
            ResolutionDetails {
                path: IntentResolutionPath::VisionFallback,
                vision_proposal_sha256: Some(proposal_hash),
                artifact_ids,
            },
        )));
        return IntentOutcome::Completed { evidence };
    }
}

/// The deterministic-path facts a stuck report carries into failure evidence and
/// into any vision escalation.
#[derive(Debug, Clone)]
struct VisionFillPayload {
    action: ControlAction,
}

struct StuckReport<'a> {
    intent_kind: &'a str,
    kind: StuckKind,
    purpose: Option<String>,
    plan_summary: String,
    candidates: Vec<types::CandidateEvidence>,
    verification: &'a str,
    fill_payload: Option<VisionFillPayload>,
}

async fn stuck_outcome(
    report: StuckReport<'_>,
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
) -> IntentOutcome {
    stuck_outcome_with_prior_evidence(report, page_id, browser, vision, Vec::new()).await
}

/// Same as `stuck_outcome`, but preserves evidence gathered before the intent got stuck,
/// such as a resolution plus a completed act that had no effect.
async fn stuck_outcome_with_prior_evidence(
    report: StuckReport<'_>,
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    prior_evidence: Vec<Evidence>,
) -> IntentOutcome {
    let intent_kind = report.intent_kind;
    let verification = report.verification;
    let stuck_code = report.kind.error_code();
    let stuck_evidence = intent_evidence(execution_record(
        intent_kind,
        report.purpose.clone(),
        report.plan_summary.clone(),
        report.candidates.clone(),
        None,
        verification,
    ));

    if never_escalates(stuck_code) || !report.kind.may_escalate_to_vision() {
        let mut evidence = prior_evidence;
        evidence.push(stuck_evidence);
        return IntentOutcome::Failed {
            error: CommandError {
                code: stuck_code,
                message: verification.to_owned(),
                layer: ErrorLayer::Page,
                retryable: false,
            },
            evidence,
        };
    }

    if vision.defer_escalation {
        let mut evidence = prior_evidence;
        evidence.push(stuck_evidence);
        return IntentOutcome::Failed {
            error: CommandError {
                code: stuck_code,
                message: verification.to_owned(),
                layer: ErrorLayer::Page,
                retryable: false,
            },
            evidence,
        };
    }

    let gates_open = vision.session_ok && vision.capability_ok;
    let Some(assist) = vision.assist.as_ref() else {
        return vision_denied_or_unavailable(
            vision,
            prior_evidence,
            stuck_evidence,
            verification,
            stuck_code,
        );
    };
    if !gates_open {
        let mut evidence = prior_evidence;
        evidence.push(stuck_evidence);
        tracing::warn!(intent = intent_kind, "policy.vision_denied");
        return IntentOutcome::Failed {
            error: CommandError {
                code: stuck_code,
                message: vision_denied_message(vision, verification),
                layer: ErrorLayer::Page,
                retryable: false,
            },
            evidence,
        };
    }

    // Prefill cache consult, before any screenshot: a remembered proposal
    // answers the stuck field for free. Only reachable when
    // `[vision].prefill` threaded a cache through an open gate.
    if let Some(proposals) = &vision.proposals {
        let key = report.purpose.clone().unwrap_or_default();
        if let Some(cached) = proposals.proposal_for(page_id, &key) {
            let (action, prompt_candidates) = match cached.action {
                crate::CachedProposalAction::Click { x, y } if report.fill_payload.is_none() => {
                    (VisionAction::Click { x, y }, Vec::new())
                }
                crate::CachedProposalAction::Click { .. } => {
                    proposals.drop_proposal(page_id, &key);
                    if let Some((metrics, _)) = assist.operational_metrics() {
                        metrics.record_prefill(observability::PrefillOutcome::DroppedEntry);
                    }
                    return escalate_with_vision(
                        report,
                        stuck_evidence,
                        prior_evidence,
                        page_id,
                        browser,
                        assist.as_ref(),
                        vision,
                    )
                    .await;
                }
                crate::CachedProposalAction::ClickCandidate { candidates, index } => (
                    VisionAction::ClickCandidate { index },
                    cached_candidate_evidence(candidates),
                ),
                crate::CachedProposalAction::TypeIntoCandidate { candidates, index } => (
                    VisionAction::TypeIntoCandidate { index },
                    cached_candidate_evidence(candidates),
                ),
            };
            match execute_vision_action(
                page_id,
                browser,
                &action,
                &prompt_candidates,
                report.fill_payload.as_ref(),
            )
            .await
            {
                Ok(mut act_evidence) => {
                    tracing::info!(intent = intent_kind, "vision.prefill_hit");
                    if let Some((metrics, _)) = assist.operational_metrics() {
                        metrics.record_prefill(observability::PrefillOutcome::Hit);
                        metrics.record_verification(VerificationMetricResult::Accepted);
                    }
                    let mut evidence = prior_evidence;
                    evidence.append(&mut act_evidence);
                    let artifact_ids = artifact_ids_from(&evidence);
                    evidence.push(intent_evidence(execution_record_with_path(
                        report.intent_kind,
                        report.purpose.clone(),
                        report.plan_summary.clone(),
                        report.candidates.clone(),
                        None,
                        "visionPrefill",
                        ResolutionDetails {
                            path: IntentResolutionPath::VisionPrefill,
                            vision_proposal_sha256: None,
                            artifact_ids,
                        },
                    )));
                    return IntentOutcome::Completed { evidence };
                }
                Err(_) => {
                    // A cached proposal that cannot be executed is dropped,
                    // never retried; live escalation proceeds unchanged.
                    tracing::info!(intent = intent_kind, "vision.prefill_entry_dropped");
                    if let Some((metrics, _)) = assist.operational_metrics() {
                        metrics.record_prefill(observability::PrefillOutcome::DroppedEntry);
                    }
                    proposals.drop_proposal(page_id, &key);
                }
            }
        } else if let Some((metrics, _)) = assist.operational_metrics() {
            metrics.record_prefill(observability::PrefillOutcome::Miss);
        }
    }

    escalate_with_vision(
        report,
        stuck_evidence,
        prior_evidence,
        page_id,
        browser,
        assist.as_ref(),
        vision,
    )
    .await
}

fn cached_candidate_evidence(
    candidates: Vec<crate::VisionPromptCandidate>,
) -> Vec<types::CandidateEvidence> {
    candidates
        .into_iter()
        .map(|candidate| types::CandidateEvidence {
            role: Some(candidate.role),
            name: Some(candidate.name),
            score: 0,
            reasons: vec!["visionPrefill".into()],
        })
        .collect()
}

/// The session-level vision gate sentence, shared verbatim with
/// [`vision_gate_closed`] so the ACP gateway can recognize a closed gate by
/// message content when the stuck path already claimed the error code.
pub const VISION_SESSION_GATE_MESSAGE: &str =
    "vision assist is off for this session (executionPolicy.visionAssist)";

/// The capability-level vision gate sentence, shared verbatim with
/// [`vision_gate_closed`].
pub const VISION_CAPABILITY_GATE_MESSAGE: &str = "the principal lacks the vision:assist capability";

/// The `visionAssistDenied` message leads with the deterministic stuck
/// reason (the actionable part: which target was missing or ambiguous) and
/// then names the closed gate, so an agent that never asked for vision can
/// repair the target instead of reading the code as a policy wall. The code
/// is the stuck kind's own code (`targetNotFound`, `targetAmbiguous`,
/// `obstructionSuspected`), not `visionAssistDenied` -- that code is
/// reserved for the tools where vision *is* the operation itself
/// (`extract_structured`, `intent_solve_challenge`, `intent_detect_challenge`).
fn vision_denied_message(vision: &VisionContext, verification: &str) -> String {
    let gate = if !vision.session_ok {
        VISION_SESSION_GATE_MESSAGE
    } else {
        VISION_CAPABILITY_GATE_MESSAGE
    };
    format!("{verification}; no vision fallback ran because {gate}")
}

/// True when `error` reports a vision gate the ACP gateway's "ask the human
/// for vision, then retry" consent escalation can unblock: either the
/// dedicated `VisionAssistDenied` code, or a deterministic stuck code
/// (`TargetNotFound` | `TargetAmbiguous` | `ObstructionSuspected`) whose
/// message carries one of the gate sentences above. A stuck code whose
/// message instead says no vision provider is configured returns false,
/// since approval cannot help there. The ACP gateway
/// (`crates/acp-gateway/src/server.rs`) keys its consent escalation on this
/// predicate rather than on the error code alone, so prose drift in the
/// stuck path can no longer silently break the escalation.
pub fn vision_gate_closed(error: &types::CommandError) -> bool {
    if error.code == types::ErrorCode::VisionAssistDenied {
        return true;
    }
    let is_escalatable_stuck_code = matches!(
        error.code,
        types::ErrorCode::TargetNotFound
            | types::ErrorCode::TargetAmbiguous
            | types::ErrorCode::ObstructionSuspected
    );
    is_escalatable_stuck_code
        && (error.message.contains(VISION_SESSION_GATE_MESSAGE)
            || error.message.contains(VISION_CAPABILITY_GATE_MESSAGE))
}

#[cfg(test)]
mod vision_gate_closed_tests {
    use super::*;

    fn error(code: ErrorCode, message: &str) -> types::CommandError {
        types::CommandError {
            code,
            message: message.to_owned(),
            layer: ErrorLayer::Page,
            retryable: false,
        }
    }

    #[test]
    fn vision_assist_denied_code_is_always_closed() {
        assert!(vision_gate_closed(&error(
            ErrorCode::VisionAssistDenied,
            "anything"
        )));
    }

    #[test]
    fn target_not_found_with_session_gate_sentence_is_closed() {
        assert!(vision_gate_closed(&error(
            ErrorCode::TargetNotFound,
            &format!("stuck; no vision fallback ran because {VISION_SESSION_GATE_MESSAGE}")
        )));
    }

    #[test]
    fn target_ambiguous_with_capability_gate_sentence_is_closed() {
        assert!(vision_gate_closed(&error(
            ErrorCode::TargetAmbiguous,
            &format!("stuck; no vision fallback ran because {VISION_CAPABILITY_GATE_MESSAGE}")
        )));
    }

    #[test]
    fn target_not_found_with_no_provider_configured_is_not_closed() {
        assert!(!vision_gate_closed(&error(
            ErrorCode::TargetNotFound,
            "stuck; no vision fallback ran because no vision provider is configured"
        )));
    }

    #[test]
    fn unrelated_code_with_gate_sentence_is_not_closed() {
        assert!(!vision_gate_closed(&error(
            ErrorCode::VerificationFailed,
            &format!("stuck; no vision fallback ran because {VISION_SESSION_GATE_MESSAGE}")
        )));
    }
}

fn vision_denied_or_unavailable(
    vision: &VisionContext,
    prior_evidence: Vec<Evidence>,
    stuck_evidence: Evidence,
    verification: &str,
    stuck_code: ErrorCode,
) -> IntentOutcome {
    let mut evidence = prior_evidence;
    evidence.push(stuck_evidence);
    if vision.session_ok && vision.capability_ok {
        IntentOutcome::Failed {
            error: CommandError {
                code: stuck_code,
                message: format!(
                    "{verification}; no vision fallback ran because no vision provider is configured"
                ),
                layer: ErrorLayer::Page,
                retryable: false,
            },
            evidence,
        }
    } else {
        tracing::warn!("policy.vision_denied");
        IntentOutcome::Failed {
            error: CommandError {
                code: stuck_code,
                message: vision_denied_message(vision, verification),
                layer: ErrorLayer::Page,
                retryable: false,
            },
            evidence,
        }
    }
}

async fn escalate_with_vision(
    report: StuckReport<'_>,
    stuck_evidence: Evidence,
    prior_evidence: Vec<Evidence>,
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    assist: &dyn VisionAssist,
    vision: &VisionContext,
) -> IntentOutcome {
    let prompt_context = vision.prompt_context.clone();
    let corpus = vision.corpus.clone();
    let StuckReport {
        intent_kind,
        kind,
        purpose,
        plan_summary,
        candidates,
        verification,
        fill_payload,
    } = report;
    let ranking_started = std::time::Instant::now();
    let (candidates, context_ranking) =
        rank_candidates_from_context(vision, intent_kind, candidates).await;
    if let (Some(ranking), Some((metrics, _))) = (context_ranking, assist.operational_metrics()) {
        metrics.record_context_lookup(ranking.outcome);
        metrics.record_context_candidate_ranking(ContextCandidateRankingMetric {
            source: ranking.source,
            outcome: ranking.outcome,
            latency_ms: ranking_started.elapsed().as_millis() as u64,
        });
    }
    let context_ranked = context_ranking.is_some();
    tracing::info!(intent = intent_kind, trigger = "stuck", "vision.escalation");
    // `stuck_evidence` is prefixed onto failure evidence only, never onto a Completed one.
    let mut base_evidence = prior_evidence.clone();
    base_evidence.push(stuck_evidence);

    let (png, mut screenshot_evidence) = match browser
        .capture_screenshot(
            page_id,
            &CaptureScreenshotCommand {
                mode: ScreenshotMode::Viewport,
            },
        )
        .await
    {
        Ok(result) => result,
        Err(error) => {
            return IntentOutcome::Failed {
                error: CommandError {
                    code: ErrorCode::VisionAssistFailed,
                    message: format!("vision screenshot failed: {}", error.message),
                    layer: ErrorLayer::Page,
                    retryable: false,
                },
                evidence: base_evidence,
            };
        }
    };

    let mut context = prompt_context;
    if !candidates.is_empty() {
        let block = context.get_or_insert_with(crate::VisionPromptContext::default);
        block.candidates = candidates
            .iter()
            .filter(|candidate| {
                vision_window_eligible(candidate.role.as_deref(), candidate.name.as_deref())
            })
            .take(5)
            .map(|candidate| crate::VisionPromptCandidate {
                role: candidate.role.clone().expect("gated role"),
                name: candidate.name.clone().expect("gated name"),
                ordinal: None,
            })
            .collect();
    }
    // Corpus capture: snapshot the exact prompt inputs before they move into
    // the request, so the record shows what the model actually saw.
    let corpus_screenshot_png = if corpus.is_some() || assist.collects_training_data() {
        capture_sanitized_corpus_screenshot(browser, page_id).await
    } else {
        None
    };
    let corpus_inputs = corpus_screenshot_png.as_ref().map(|image| {
        (
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, image),
            context.as_ref().and_then(|c| c.url.clone()),
            context
                .as_ref()
                .map(|c| {
                    c.candidates
                        .iter()
                        .map(|p| crate::CorpusCandidate {
                            role: p.role.clone(),
                            name: p.name.clone(),
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default(),
        )
    });
    let propose_started = std::time::Instant::now();
    let metric_context = assist.operational_metrics();
    let proposal = match assist
        .propose(VisionProposeRequest {
            purpose: purpose.clone().unwrap_or_default(),
            intent_kind: intent_kind.to_owned(),
            screenshot_png: png,
            corpus_screenshot_png,
            stuck: kind,
            context,
        })
        .await
    {
        Ok(proposal) => proposal,
        Err(error) => {
            record_context_ranked_vision_metric(
                metric_context.as_ref(),
                context_ranked,
                propose_started.elapsed().as_millis() as u64,
                None,
                VisionProposalOutcome::Failed,
                None,
            );
            let mut evidence = base_evidence;
            evidence.append(&mut screenshot_evidence);
            return IntentOutcome::Failed {
                error: CommandError {
                    code: ErrorCode::VisionAssistFailed,
                    message: format!("vision propose failed: {}", error.message),
                    layer: ErrorLayer::Page,
                    retryable: false,
                },
                evidence,
            };
        }
    };

    let provider_latency_ms = propose_started.elapsed().as_millis() as u64;
    tracing::info!(
        intent = intent_kind,
        latency_ms = provider_latency_ms,
        "vision.provider_round_trip"
    );
    let proposal_hash = proposal_sha256(&proposal);
    if proposal.confidence < VISION_CONFIDENCE_FLOOR {
        record_context_ranked_vision_metric(
            metric_context.as_ref(),
            context_ranked,
            provider_latency_ms,
            Some(proposal.confidence),
            VisionProposalOutcome::Rejected,
            Some(VerificationMetricResult::OtherRejected),
        );
        tracing::info!(
            intent = intent_kind,
            confidence = proposal.confidence,
            "vision.rejection_floor"
        );
        record_escalation(
            &corpus,
            corpus_inputs.as_ref(),
            &purpose,
            intent_kind,
            kind,
            &proposal,
            false,
            "visionRejectionFloor",
            Some(format!(
                "proposal confidence {:.2} below floor {VISION_CONFIDENCE_FLOOR}",
                proposal.confidence
            )),
            None,
        );
        let mut evidence = base_evidence;
        evidence.append(&mut screenshot_evidence);
        evidence.push(intent_evidence(execution_record_with_path(
            intent_kind,
            purpose,
            plan_summary,
            candidates,
            None,
            format!(
                "visionConfidenceBelowFloor:{:.2}<{VISION_CONFIDENCE_FLOOR}",
                proposal.confidence
            ),
            ResolutionDetails {
                path: IntentResolutionPath::VisionFallback,
                vision_proposal_sha256: Some(proposal_hash),
                artifact_ids: artifact_ids_from(&screenshot_evidence),
            },
        )));
        return IntentOutcome::Failed {
            error: CommandError {
                code: ErrorCode::VisionAssistFailed,
                message: format!(
                    "vision proposal confidence {:.2} below floor {VISION_CONFIDENCE_FLOOR}",
                    proposal.confidence
                ),
                layer: ErrorLayer::Page,
                retryable: false,
            },
            evidence,
        };
    }

    // The model indexes into the exact list the prompt carried: top 5
    // window-eligible candidates. Resolve against that same view.
    let prompt_candidates: Vec<types::CandidateEvidence> = candidates
        .iter()
        .filter(|candidate| {
            vision_window_eligible(candidate.role.as_deref(), candidate.name.as_deref())
        })
        .take(5)
        .cloned()
        .collect();
    let mut act_evidence = match execute_vision_action(
        page_id,
        browser,
        &proposal.action,
        &prompt_candidates,
        fill_payload.as_ref(),
    )
    .await
    {
        Ok(evidence) => evidence,
        Err(error) => {
            record_context_ranked_vision_metric(
                metric_context.as_ref(),
                context_ranked,
                provider_latency_ms,
                Some(proposal.confidence),
                VisionProposalOutcome::Rejected,
                Some(VerificationMetricResult::OtherRejected),
            );
            record_escalation(
                &corpus,
                corpus_inputs.as_ref(),
                &purpose,
                intent_kind,
                kind,
                &proposal,
                false,
                "visionActFailed",
                Some(format!("vision act failed: {}", error.message)),
                None,
            );
            let mut evidence = base_evidence;
            evidence.append(&mut screenshot_evidence);
            evidence.push(intent_evidence(execution_record_with_path(
                intent_kind,
                purpose,
                plan_summary,
                candidates,
                None,
                format!("visionActFailed:{verification}"),
                ResolutionDetails {
                    path: IntentResolutionPath::VisionFallback,
                    vision_proposal_sha256: Some(proposal_hash),
                    artifact_ids: artifact_ids_from(&screenshot_evidence),
                },
            )));
            return IntentOutcome::Failed {
                error: CommandError {
                    code: ErrorCode::VisionAssistFailed,
                    message: format!("vision act failed: {}", error.message),
                    layer: ErrorLayer::Page,
                    retryable: false,
                },
                evidence,
            };
        }
    };

    let mut evidence = prior_evidence;
    evidence.append(&mut screenshot_evidence);
    evidence.append(&mut act_evidence);
    let artifact_ids = artifact_ids_from(&evidence);
    // Ground the executed action back onto the candidate list before the
    // record is written; only clicks resolve (point-based).
    let resolved = match &proposal.action {
        VisionAction::Click { x, y } => browser
            .element_at_point(page_id, *x, *y)
            .await
            .ok()
            .flatten(),
        _ => None,
    };
    record_escalation(
        &corpus,
        corpus_inputs.as_ref(),
        &purpose,
        intent_kind,
        kind,
        &proposal,
        true,
        "visionFallback",
        None,
        resolved,
    );
    record_context_ranked_vision_metric(
        metric_context.as_ref(),
        context_ranked,
        provider_latency_ms,
        Some(proposal.confidence),
        VisionProposalOutcome::Accepted,
        Some(VerificationMetricResult::Accepted),
    );
    evidence.push(intent_evidence(execution_record_with_path(
        intent_kind,
        purpose,
        plan_summary,
        candidates,
        None,
        "visionFallback",
        ResolutionDetails {
            path: IntentResolutionPath::VisionFallback,
            vision_proposal_sha256: Some(proposal_hash),
            artifact_ids,
        },
    )));
    IntentOutcome::Completed { evidence }
}

async fn capture_sanitized_corpus_screenshot(
    browser: &dyn IntentBrowser,
    page_id: &PageId,
) -> Option<Vec<u8>> {
    match browser.capture_sanitized_screenshot(page_id).await {
        Ok(bytes) if !bytes.is_empty() => Some(bytes),
        Ok(_) => {
            tracing::warn!("vision.corpus_skipped_empty_sanitized_screenshot");
            None
        }
        Err(error) => {
            tracing::warn!(code = ?error.code, "vision.corpus_skipped_unsanitized_screenshot");
            None
        }
    }
}

fn record_context_ranked_vision_metric(
    metric_context: Option<&(OperationalMetrics, ProviderMode)>,
    context_ranked: bool,
    latency_ms: u64,
    confidence: Option<f32>,
    outcome: VisionProposalOutcome,
    verification: Option<VerificationMetricResult>,
) {
    record_vision_metric(
        metric_context,
        latency_ms,
        confidence,
        outcome,
        verification,
    );
    let (true, Some((metrics, provider_mode))) = (context_ranked, metric_context) else {
        return;
    };
    metrics.record_context_ranked_vision(ContextRankedVisionMetric {
        provider_mode: *provider_mode,
        confidence: confidence.map(f64::from),
        verification,
    });
}

fn record_vision_metric(
    metric_context: Option<&(OperationalMetrics, ProviderMode)>,
    latency_ms: u64,
    confidence: Option<f32>,
    outcome: VisionProposalOutcome,
    verification: Option<VerificationMetricResult>,
) {
    let Some((metrics, provider_mode)) = metric_context else {
        return;
    };
    metrics.record_vision_proposal(VisionProposalMetric {
        provider_mode: *provider_mode,
        latency_ms,
        confidence: confidence.map(f64::from),
        outcome,
    });
    if let Some(verification) = verification {
        metrics.record_verification(verification);
    }
}

/// Write one corpus record for a terminal escalation branch. No-ops unless
/// `[vision].corpus_dir` threaded a sink through the `VisionContext`.
#[allow(clippy::too_many_arguments)]
fn record_escalation(
    corpus: &Option<crate::VisionCorpus>,
    inputs: Option<&(String, Option<String>, Vec<crate::CorpusCandidate>)>,
    purpose: &Option<String>,
    intent_kind: &str,
    kind: StuckKind,
    proposal: &crate::VisionProposal,
    success: bool,
    stage: &'static str,
    error_message: Option<String>,
    resolved: Option<(String, String)>,
) {
    let (Some(corpus), Some((image_b64, context_url, candidates))) = (corpus, inputs) else {
        return;
    };
    // An empty candidate window is not selection signal: the model was asked
    // to pick from nothing, so any outcome — especially a floor rejection —
    // says nothing about its judgment. Recording it would mislabel backend
    // or gather failure as an ambiguous-negative and poison the corpus.
    if candidates.is_empty() {
        tracing::info!(
            intent = intent_kind,
            stage,
            "vision.corpus_skipped_empty_candidates"
        );
        return;
    }
    let resolved_element = resolved.map(|(role, name)| crate::ResolvedElement { role, name });
    let target_index = match proposal.action {
        VisionAction::ClickCandidate { index }
        | VisionAction::TypeIntoCandidate { index }
        | VisionAction::ExtractFromCandidate { index }
            if success =>
        {
            usize::try_from(index)
                .ok()
                .filter(|index| *index < candidates.len())
        }
        _ => resolved_element.as_ref().and_then(|element| {
            crate::corpus::match_resolved(candidates, &(element.role.clone(), element.name.clone()))
        }),
    };
    corpus.record(&crate::CorpusRecord {
        image_b64: image_b64.clone(),
        purpose: purpose.clone().unwrap_or_default(),
        intent_kind: intent_kind.to_owned(),
        stuck: stuck_label(kind).to_owned(),
        context_url: context_url.clone(),
        context_candidates: candidates.clone(),
        target_index,
        resolved_element,
        model_response: crate::corpus::CorpusModelResponse {
            confidence: proposal.confidence,
            action: crate::corpus::raw_action(&proposal.action, target_index),
        },
        success,
        journey: "production".into(),
        step: intent_kind.to_owned(),
        outcome_stage: stage.to_owned(),
        error_message,
    });
}

fn stuck_label(kind: StuckKind) -> &'static str {
    match kind {
        StuckKind::TargetMissing => "targetMissing",
        StuckKind::TargetAmbiguous => "targetAmbiguous",
        StuckKind::ObstructionSuspected => "obstructionSuspected",
        StuckKind::VerifyNoDomSignal => "verifyNoDomSignal",
        StuckKind::ChallengePresent => "challengePresent",
    }
}

async fn execute_vision_action(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    action: &VisionAction,
    prompt_candidates: &[types::CandidateEvidence],
    fill_payload: Option<&VisionFillPayload>,
) -> Result<Vec<Evidence>, CommandError> {
    match action {
        VisionAction::Click { x, y } => browser.click_xy(page_id, *x, *y).await,
        VisionAction::ClickCandidate { index } => {
            // The runtime owns spatial grounding: resolve the index against
            // the exact candidate list the model saw, then click the element
            // through the DOM path rather than by pixel.
            browser
                .click(
                    page_id,
                    &ClickCommand {
                        selector: String::new(),
                        target: Some(prompt_candidate_target(
                            "clickCandidate",
                            *index,
                            prompt_candidates,
                        )?),
                        boundary: false,
                        expected_url: None,
                        modifiers: Vec::new(),
                    },
                )
                .await
        }
        VisionAction::TypeIntoCandidate { index } => {
            let payload = fill_payload.ok_or_else(|| CommandError {
                code: ErrorCode::VisionAssistFailed,
                message: "typeIntoCandidate requires a runtime fill action".into(),
                layer: ErrorLayer::Page,
                retryable: false,
            })?;
            let target = prompt_candidate_target("typeIntoCandidate", *index, prompt_candidates)?;
            let role = target.role.as_deref().unwrap_or_default();
            let compatible = match &payload.action {
                ControlAction::SetFiles { .. } => role == "button",
                _ => compatible_role(&payload.action, role, false),
            };
            if !compatible {
                return Err(CommandError {
                    code: ErrorCode::IntentActionMismatch,
                    message: format!(
                        "typeIntoCandidate fill {} is incompatible with candidate role={role:?}",
                        fill_kind(&payload.action)
                    ),
                    layer: ErrorLayer::Page,
                    retryable: false,
                });
            }
            let evidence = match &payload.action {
                ControlAction::SetText { value, clear_first } => {
                    browser
                        .type_text(
                            page_id,
                            &TypeTextCommand {
                                selector: String::new(),
                                target: Some(target),
                                value: value.clone(),
                                clear_first: *clear_first,
                                expected_url: None,
                            },
                        )
                        .await?
                }
                ControlAction::SelectOne { .. }
                | ControlAction::SelectMany { .. }
                | ControlAction::SetChecked { .. }
                | ControlAction::Clear => {
                    let control_target = FormControlTarget {
                        role: target.role.clone().unwrap_or_default(),
                        accessible_name: target.accessible_name.clone().unwrap_or_default(),
                        ordinal: target.ordinal,
                        frame_path: Vec::new(),
                        shadow_path: Vec::new(),
                    };
                    browser
                        .control_action(
                            page_id,
                            &ControlActionCommand {
                                target: control_target,
                                action: payload.action.clone(),
                            },
                        )
                        .await?
                }
                ControlAction::SetFiles { paths } => browser
                    .upload_files(
                        page_id,
                        &UploadFilesCommand {
                            selector: String::new(),
                            target: Some(target),
                            paths: paths.clone(),
                        },
                    )
                    .await
                    .map_err(|error| CommandError {
                        message: "typeIntoCandidate file upload failed".into(),
                        ..error
                    })?,
                ControlAction::Activate => {
                    return Err(CommandError {
                        code: ErrorCode::IntentActionMismatch,
                        message: format!(
                            "typeIntoCandidate does not support fill {}",
                            fill_kind(&payload.action)
                        ),
                        layer: ErrorLayer::Page,
                        retryable: false,
                    });
                }
            };
            verify_fill(&payload.action, &evidence).map_err(|_| CommandError {
                code: ErrorCode::VerificationFailed,
                message: "typeIntoCandidate verification failed".into(),
                layer: ErrorLayer::Page,
                retryable: false,
            })?;
            Ok(evidence)
        }
        VisionAction::TypeText { text } => {
            browser
                .type_text(
                    page_id,
                    &TypeTextCommand {
                        selector: String::new(),
                        target: None,
                        value: text.clone(),
                        clear_first: false,
                        expected_url: None,
                    },
                )
                .await
        }
        // `ExtractValue` is read-only: `resolve_extract_field` consumes it directly, this
        // act-on-the-page dispatcher never does.
        VisionAction::ExtractValue { .. } => Err(CommandError {
            code: ErrorCode::VisionAssistFailed,
            message: "extractValue vision action is not an actionable page operation".into(),
            layer: ErrorLayer::Page,
            retryable: false,
        }),
        VisionAction::ExtractFromCandidate { .. } => Err(CommandError {
            code: ErrorCode::VisionAssistFailed,
            message: "extractFromCandidate vision action is not an actionable page operation"
                .into(),
            layer: ErrorLayer::Page,
            retryable: false,
        }),
        // Terminal signal, consumed by `execute_solve_challenge` before
        // dispatch; never an act-on-the-page operation.
        VisionAction::ChallengeSolved => Err(CommandError {
            code: ErrorCode::VisionAssistFailed,
            message: "challengeSolved vision action is not an actionable page operation".into(),
            layer: ErrorLayer::Page,
            retryable: false,
        }),
        // Classification answers, consumed by `execute_detect_challenge`
        // before dispatch; detection never acts on the page.
        VisionAction::ChallengeDetected { .. } | VisionAction::NoChallengeDetected => {
            Err(CommandError {
                code: ErrorCode::VisionAssistFailed,
                message: "detection vision action is not an actionable page operation".into(),
                layer: ErrorLayer::Page,
                retryable: false,
            })
        }
    }
}

fn prompt_candidate_target(
    action: &str,
    index: u32,
    prompt_candidates: &[types::CandidateEvidence],
) -> Result<TargetSpec, CommandError> {
    let candidate = prompt_candidates
        .get(index as usize)
        .ok_or_else(|| CommandError {
            code: ErrorCode::VisionAssistFailed,
            message: format!(
                "{action} index {index} out of range ({} candidates)",
                prompt_candidates.len()
            ),
            layer: ErrorLayer::Page,
            retryable: false,
        })?;
    let role = candidate.role.clone().ok_or_else(|| CommandError {
        code: ErrorCode::VisionAssistFailed,
        message: format!("{action} index {index} has no role"),
        layer: ErrorLayer::Page,
        retryable: false,
    })?;
    let name = candidate.name.clone().ok_or_else(|| CommandError {
        code: ErrorCode::VisionAssistFailed,
        message: format!("{action} index {index} has no name"),
        layer: ErrorLayer::Page,
        retryable: false,
    })?;
    let ordinal = prompt_candidates
        .iter()
        .take(index as usize)
        .filter(|prior| prior.role.as_deref() == Some(role.as_str()))
        .filter(|prior| prior.name.as_deref() == Some(name.as_str()))
        .count();
    Ok(TargetSpec {
        role: Some(role),
        accessible_name: Some(name),
        ordinal: Some(ordinal),
        ..TargetSpec::default()
    })
}

fn artifact_ids_from(evidence: &[Evidence]) -> Vec<String> {
    evidence
        .iter()
        .filter_map(|item| match item {
            Evidence::Screenshot { artifact_id, .. } => Some(artifact_id.clone()),
            _ => None,
        })
        .collect()
}

fn intent_evidence(record: ExecutionRecord) -> Evidence {
    Evidence::IntentExecution { record }
}
