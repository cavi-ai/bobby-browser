use std::{collections::BTreeSet, sync::Arc};

use async_trait::async_trait;
use dom_engine::{resolve_candidates, Candidate, ResolutionDecision, ResolutionPolicy};
use observability::{
    OperationalMetrics, ProviderMode, VerificationMetricResult, VisionProposalMetric,
    VisionProposalOutcome,
};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, ControlAction, ControlActionCommand,
    ErrorCode, ErrorLayer, Evidence, ExecutionRecord, ExtractValueKind, FormControlTarget,
    IntentCommand, IntentResolutionPath, PageId, ScreenshotMode, SemanticTargetSegment,
    TargetFingerprint, TargetSpec, TypeTextCommand, UploadFilesCommand, WaitCondition,
    WaitForCommand,
};

use crate::compiler::{compile_intent, CompleteFormFieldPlan, ExtractFieldPlan, IntentPlan};
use crate::stuck::{never_escalates, StuckKind};
use crate::verify::{
    compatible, execution_record, execution_record_with_path, summarize_target, verify_fill,
    ResolutionDetails,
};
use crate::vision::{
    proposal_sha256, VisionAction, VisionAssist, VisionProposeRequest, VISION_CONFIDENCE_FLOOR,
};

#[derive(Clone, Default)]
pub struct VisionContext {
    pub session_ok: bool,
    pub capability_ok: bool,
    pub assist: Option<Arc<dyn VisionAssist>>,
    /// Prefill proposal cache. `None` unless `[vision].prefill` is on and
    /// both gates are open, so the default path is byte-identical to before.
    pub proposals: Option<Arc<dyn crate::ProposalLookup>>,
    /// Set by `execute_complete_form` while driving fields: a stuck field
    /// returns its plain stuck failure instead of escalating, so the form
    /// can batch one screenshot for every remaining purpose.
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
    /// verified click back onto the candidate list. Default: unsupported.
    async fn element_at_point(
        &self,
        _page_id: &PageId,
        _x: f64,
        _y: f64,
    ) -> Result<Option<(String, String)>, CommandError> {
        Ok(None)
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

async fn execute_complete_form(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    fields: Vec<CompleteFormFieldPlan>,
) -> IntentOutcome {
    let mut evidence = Vec::new();
    // Lazy batch prefill: with a cache threaded through open gates, fields
    // defer their own escalation; the first stuck field triggers one
    // screenshot and one propose per remaining purpose.
    let batching = vision.proposals.is_some()
        && vision.session_ok
        && vision.capability_ok
        && vision.assist.is_some();
    let mut deferred = vision.clone();
    deferred.defer_escalation = batching;
    let mut batched = false;
    for (index, field) in fields.iter().enumerate() {
        evidence.push(Evidence::Configuration {
            name: "completeFormField".into(),
            value: field.name.clone(),
        });
        let field_vision = if batched { vision } else { &deferred };
        let intent = IntentCommand::Fill(types::FillIntent {
            purpose: field.purpose.clone(),
            hints: types::IntentHints::default(),
            value: field.value.clone(),
        });
        match execute_fill(
            &intent,
            page_id,
            browser,
            field_vision,
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
                let eligible = batching
                    && !batched
                    && !never_escalates(error.code)
                    && !matches!(
                        error.code,
                        ErrorCode::VisionAssistDenied | ErrorCode::VisionAssistFailed
                    );
                if !eligible {
                    return IntentOutcome::Failed { error, evidence };
                }
                let purposes: Vec<String> = fields[index..]
                    .iter()
                    .map(|field| field.purpose.clone())
                    .collect();
                batch_prefill(page_id, browser, vision, purposes).await;
                batched = true;
                // Retry the stuck field once: the cache consult in the
                // escalation path answers from the batch with no screenshot.
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
                        evidence: mut retry_evidence,
                    } => evidence.append(&mut retry_evidence),
                    IntentOutcome::Failed {
                        error,
                        evidence: mut retry_evidence,
                    } => {
                        evidence.append(&mut retry_evidence);
                        return IntentOutcome::Failed { error, evidence };
                    }
                }
            }
        }
    }
    IntentOutcome::Completed { evidence }
}

/// One screenshot, one propose per remaining purpose, all cached. Every
/// failure degrades silently: transport loss, an auth rejection, or a
/// low-confidence proposal simply leaves that purpose uncached, and the
/// deterministic path (or a live escalation) handles the field.
async fn batch_prefill(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    purposes: Vec<String>,
) {
    let (Some(proposals), Some(assist)) = (&vision.proposals, &vision.assist) else {
        return;
    };
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
    let mut batch = Vec::new();
    let metric_context = assist.operational_metrics();
    for purpose in purposes {
        let propose_started = std::time::Instant::now();
        let proposal = match assist
            .propose(VisionProposeRequest {
                purpose: purpose.clone(),
                intent_kind: "fill".to_owned(),
                screenshot_png: png.clone(),
                stuck: StuckKind::TargetMissing,
                context: vision.prompt_context.clone(),
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
                continue;
            }
        };
        let latency_ms = propose_started.elapsed().as_millis() as u64;
        if proposal.confidence < VISION_CONFIDENCE_FLOOR {
            record_vision_metric(
                metric_context.as_ref(),
                latency_ms,
                Some(proposal.confidence),
                VisionProposalOutcome::Rejected,
                Some(VerificationMetricResult::OtherRejected),
            );
            continue;
        }
        // Only coordinate actions are cached; a TypeText or ExtractValue
        // proposal carries what the user typed and is never stored.
        if let VisionAction::Click { x, y } = proposal.action {
            record_vision_metric(
                metric_context.as_ref(),
                latency_ms,
                Some(proposal.confidence),
                VisionProposalOutcome::Accepted,
                Some(VerificationMetricResult::Accepted),
            );
            batch.push((
                purpose,
                crate::CachedProposal {
                    x,
                    y,
                    confidence: proposal.confidence,
                },
            ));
        } else {
            record_vision_metric(
                metric_context.as_ref(),
                latency_ms,
                Some(proposal.confidence),
                VisionProposalOutcome::Rejected,
                Some(VerificationMetricResult::OtherRejected),
            );
        }
    }
    if !batch.is_empty() {
        tracing::info!(recorded = batch.len(), "vision.prefill_batch");
        proposals.record_proposals(page_id, batch);
    } else {
        tracing::info!("vision.prefill_batch_empty");
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
const VISION_WINDOW_ROLES: [&str; 10] = [
    "button",
    "link",
    "textbox",
    "combobox",
    "checkbox",
    "radio",
    "tab",
    "menuitem",
    "searchbox",
    "switch",
];

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
        .take(5)
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
    let fill_payload = match &value {
        ControlAction::SetText { value, clear_first } => Some(VisionFillPayload {
            text: value.clone(),
            clear_first: *clear_first,
        }),
        _ => None,
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

    match resolve_candidates(&field.target, &candidates, &ResolutionPolicy::default()) {
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

    let screenshot_b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png);
    let propose_started = std::time::Instant::now();
    let metric_context = assist.operational_metrics();
    let proposal = match assist
        .propose(VisionProposeRequest {
            purpose: field.purpose.clone(),
            intent_kind: "extract".to_owned(),
            screenshot_png: png,
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

    if let Some(corpus) = &vision.corpus {
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
                image_b64: screenshot_b64,
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
                    action: crate::corpus::raw_action(&proposal.action),
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
    text: String,
    clear_first: bool,
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
        return vision_denied_or_unavailable(vision, prior_evidence, stuck_evidence, verification);
    };
    if !gates_open {
        let mut evidence = prior_evidence;
        evidence.push(stuck_evidence);
        tracing::warn!(intent = intent_kind, "policy.vision_denied");
        return IntentOutcome::Failed {
            error: CommandError {
                code: ErrorCode::VisionAssistDenied,
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
            match execute_vision_action(
                page_id,
                browser,
                &VisionAction::Click {
                    x: cached.x,
                    y: cached.y,
                },
                &[],
                None,
            )
            .await
            {
                Ok(mut act_evidence) => {
                    tracing::info!(intent = intent_kind, "vision.prefill_hit");
                    if let Some((metrics, _)) = assist.operational_metrics() {
                        metrics.record_prefill(observability::PrefillOutcome::Hit);
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

/// The `visionAssistDenied` message leads with the deterministic stuck
/// reason (the actionable part: which target was missing or ambiguous) and
/// then names the closed gate, so an agent that never asked for vision can
/// repair the target instead of reading the code as a policy wall. The code
/// stays `visionAssistDenied` because the ACP gateway and one-shot consent
/// flows key their "ask the human for vision" escalation off it.
fn vision_denied_message(vision: &VisionContext, verification: &str) -> String {
    let gate = if !vision.session_ok {
        "vision assist is off for this session (executionPolicy.visionAssist)"
    } else {
        "the principal lacks the vision:assist capability"
    };
    format!("{verification}; no vision fallback ran because {gate}")
}

fn vision_denied_or_unavailable(
    vision: &VisionContext,
    prior_evidence: Vec<Evidence>,
    stuck_evidence: Evidence,
    verification: &str,
) -> IntentOutcome {
    let mut evidence = prior_evidence;
    evidence.push(stuck_evidence);
    if vision.session_ok && vision.capability_ok {
        IntentOutcome::Failed {
            error: CommandError {
                code: ErrorCode::VisionAssistFailed,
                message: "vision assist provider is not configured".into(),
                layer: ErrorLayer::Page,
                retryable: false,
            },
            evidence,
        }
    } else {
        tracing::warn!("policy.vision_denied");
        IntentOutcome::Failed {
            error: CommandError {
                code: ErrorCode::VisionAssistDenied,
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
    let corpus_inputs = corpus.as_ref().map(|_| {
        (
            base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &png),
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
            stuck: kind,
            context,
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
        record_vision_metric(
            metric_context.as_ref(),
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
            record_vision_metric(
                metric_context.as_ref(),
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
    record_vision_metric(
        metric_context.as_ref(),
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
            action: crate::corpus::raw_action(&proposal.action),
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
                message: "typeIntoCandidate requires a runtime text fill payload".into(),
                layer: ErrorLayer::Page,
                retryable: false,
            })?;
            let target = prompt_candidate_target("typeIntoCandidate", *index, prompt_candidates)?;
            let evidence = browser
                .type_text(
                    page_id,
                    &TypeTextCommand {
                        selector: String::new(),
                        target: Some(target),
                        value: payload.text.clone(),
                        clear_first: payload.clear_first,
                        expected_url: None,
                    },
                )
                .await?;
            let value = ControlAction::SetText {
                value: payload.text.clone(),
                clear_first: payload.clear_first,
            };
            verify_fill(&value, &evidence).map_err(|_| CommandError {
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
