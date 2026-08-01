use std::sync::Arc;

use async_trait::async_trait;
use dom_engine::{resolve_candidates, Candidate, ResolutionDecision, ResolutionPolicy};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, ErrorCode, ErrorLayer, Evidence,
    ExecutionRecord, ExtractValueKind, FillValue, IntentCommand, IntentResolutionPath, PageId,
    ScreenshotMode, TargetFingerprint, TargetSpec, TypeTextCommand, UploadFilesCommand,
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

    async fn wait_for(
        &self,
        page_id: &PageId,
        command: &WaitForCommand,
    ) -> Result<Vec<Evidence>, CommandError>;

    async fn capture_screenshot(
        &self,
        page_id: &PageId,
        command: &CaptureScreenshotCommand,
    ) -> Result<(Vec<u8>, Vec<Evidence>), CommandError>;
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
    for field in fields {
        evidence.push(Evidence::Configuration {
            name: "completeFormField".into(),
            value: field.name.clone(),
        });
        let intent = IntentCommand::Fill(types::FillIntent {
            purpose: field.purpose,
            hints: types::IntentHints::default(),
            value: field.value.clone(),
        });
        match execute_fill(&intent, page_id, browser, vision, field.target, field.value).await {
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

async fn execute_locate(
    intent: &IntentCommand,
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    target: TargetSpec,
) -> IntentOutcome {
    let purpose = match intent {
        IntentCommand::Locate(locate) => Some(locate.purpose.clone()),
        _ => None,
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
            stuck_outcome(
                StuckReport {
                    intent_kind: "locate",
                    kind: StuckKind::TargetMissing,
                    purpose,
                    plan_summary,
                    candidates: Vec::new(),
                    verification: "targetNotFound",
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
                },
                page_id,
                browser,
                vision,
            )
            .await
        }
    }
}

async fn execute_fill(
    intent: &IntentCommand,
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    target: TargetSpec,
    value: FillValue,
) -> IntentOutcome {
    let purpose = match intent {
        IntentCommand::Fill(fill) => Some(fill.purpose.clone()),
        _ => None,
    };
    let plan_summary = format!("{} value={}", summarize_target(&target), fill_kind(&value));
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
            return stuck_outcome(
                StuckReport {
                    intent_kind: "fill",
                    kind: StuckKind::TargetMissing,
                    purpose,
                    plan_summary,
                    candidates: Vec::new(),
                    verification: "targetNotFound",
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

    let mut act_evidence = match act_fill(page_id, browser, &candidate, &value).await {
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
    value: &FillValue,
) -> Result<Vec<Evidence>, CommandError> {
    let (selector, target) = action_target(candidate);
    match value {
        // Worker-pool has no select API; Select is typed via TypeTextCommand.
        FillValue::Text { text, clear_first } => {
            browser
                .type_text(
                    page_id,
                    &TypeTextCommand {
                        selector,
                        target: Some(target),
                        value: text.clone(),
                        clear_first: *clear_first,
                        expected_url: None,
                    },
                )
                .await
        }
        FillValue::Select { option } => {
            browser
                .type_text(
                    page_id,
                    &TypeTextCommand {
                        selector,
                        target: Some(target),
                        value: option.clone(),
                        clear_first: true,
                        expected_url: None,
                    },
                )
                .await
        }
        FillValue::Checked { checked } => {
            browser
                .type_text(
                    page_id,
                    &TypeTextCommand {
                        selector,
                        target: Some(target),
                        value: checked.to_string(),
                        clear_first: true,
                        expected_url: None,
                    },
                )
                .await
        }
        FillValue::Files { paths } => {
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
    }
}

fn action_target(candidate: &Candidate) -> (String, TargetSpec) {
    let selector = candidate.css.clone().unwrap_or_default();
    let target = TargetSpec {
        css: candidate.css.clone(),
        test_id: candidate.test_id.clone(),
        role: candidate.role.clone(),
        accessible_name: candidate.name.clone(),
        label: candidate.label.clone(),
        attributes: candidate.attributes.clone(),
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

fn fill_kind(value: &FillValue) -> &'static str {
    match value {
        FillValue::Text { .. } => "text",
        FillValue::Select { .. } => "select",
        FillValue::Checked { .. } => "checked",
        FillValue::Files { .. } => "files",
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
    let purpose = match intent {
        IntentCommand::SubmitAndVerify(submit) => Some(submit.purpose.clone()),
        _ => None,
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

    let (selector, action_target) = action_target(&candidate);
    let click = ClickCommand {
        selector,
        target: Some(action_target),
        boundary: true,
        expected_url: expected_url_from_wait(&expected_state),
    };
    let mut click_evidence = match browser.click(page_id, &click).await {
        Ok(evidence) => evidence,
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
        "submitted",
    )));
    IntentOutcome::Completed { evidence }
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

    let (selector, action_target) = action_target(&candidate);
    let click = ClickCommand {
        selector,
        target: Some(action_target),
        boundary,
        expected_url: expected_url_from_wait(&expected_destination),
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

/// Interval between re-resolution polls while waiting for a dismissed
/// obstruction to leave the DOM or become hidden. Mirrors the 25ms cadence
/// `worker-pool`'s `wait_for` primitive already uses.
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

    let (selector, action_target) = action_target(&candidate);
    let click = ClickCommand {
        selector,
        target: Some(action_target),
        // DismissObstructionIntent has no caller-supplied boundary flag: it is
        // always CommandClass::Reconciliable, so the underlying act never
        // needs a pre-established checkpoint.
        boundary: false,
        expected_url: None,
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

/// Polls the same target that was just acted on until it is gone — either
/// removed from the DOM entirely, or still present but no longer visible.
/// Unlike `WaitCondition::Element`, this checks both `Detached` and `Hidden`
/// semantics in one pass, since real dismiss affordances do either (remove
/// the node vs. toggle a hidden class) and callers supply no expectation.
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

/// Schema-bounded structured extraction. Each field is resolved and read
/// independently: a field that cannot be resolved (deterministically, or via
/// vision when permitted) is recorded as missing in its own `Extraction`
/// evidence rather than failing the whole command, so this always returns
/// `Completed` — the caller inspects per-field evidence to see what came
/// back.
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

/// Resolves and reads one `ExtractIntent` field. Always returns evidence
/// ending in exactly one `Evidence::Extraction` for `field.name`, preceded by
/// a `Evidence::Resolution` when the field was found (deterministically or
/// via vision).
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

/// Attempts a vision-proposed value for a field the deterministic resolver
/// could not place, under the same double-gate rule every other intent's
/// vision fallback uses. Unlike the click-oriented escalation path, success
/// here never touches the page — the proposal's text becomes the field's
/// value directly.
async fn escalate_extract_field_with_vision(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
    field: &ExtractFieldPlan,
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

    let proposal = match assist
        .propose(VisionProposeRequest {
            purpose: field.purpose.clone(),
            intent_kind: "extract".to_owned(),
            screenshot_png: png,
            stuck,
        })
        .await
    {
        Ok(proposal) => proposal,
        Err(_) => {
            screenshot_evidence.push(missing_extraction(
                &field.name,
                Some(ErrorCode::VisionAssistFailed),
            ));
            return screenshot_evidence;
        }
    };

    let value = match &proposal.action {
        VisionAction::ExtractValue { value } if proposal.confidence >= VISION_CONFIDENCE_FLOOR => {
            Some(value.clone())
        }
        _ => None,
    };
    let error_code = value.is_none().then_some(ErrorCode::VisionAssistFailed);

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

/// The deterministic-path facts a stuck report carries into failure evidence and
/// into any vision escalation.
struct StuckReport<'a> {
    intent_kind: &'a str,
    kind: StuckKind,
    purpose: Option<String>,
    plan_summary: String,
    candidates: Vec<types::CandidateEvidence>,
    verification: &'a str,
}

async fn stuck_outcome(
    report: StuckReport<'_>,
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
) -> IntentOutcome {
    stuck_outcome_with_prior_evidence(report, page_id, browser, vision, Vec::new()).await
}

/// Same as `stuck_outcome`, but preserves evidence already gathered before the
/// intent got stuck (e.g. resolution + a completed act that did not have its
/// intended effect, as with `DismissObstructionIntent`'s post-click check).
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

    let gates_open = vision.session_ok && vision.capability_ok;
    let Some(assist) = vision.assist.as_ref() else {
        return vision_denied_or_unavailable(
            gates_open,
            prior_evidence,
            stuck_evidence,
            verification,
        );
    };
    if !gates_open {
        let mut evidence = prior_evidence;
        evidence.push(stuck_evidence);
        tracing::warn!(intent = intent_kind, "policy.vision_denied");
        return IntentOutcome::Failed {
            error: CommandError {
                code: ErrorCode::VisionAssistDenied,
                message: format!("vision assist denied; underlying stuck reason: {verification}"),
                layer: ErrorLayer::Page,
                retryable: false,
            },
            evidence,
        };
    }

    escalate_with_vision(
        report,
        stuck_evidence,
        prior_evidence,
        page_id,
        browser,
        assist.as_ref(),
    )
    .await
}

fn vision_denied_or_unavailable(
    gates_open: bool,
    prior_evidence: Vec<Evidence>,
    stuck_evidence: Evidence,
    verification: &str,
) -> IntentOutcome {
    let mut evidence = prior_evidence;
    evidence.push(stuck_evidence);
    if gates_open {
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
                message: format!("vision assist denied; underlying stuck reason: {verification}"),
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
) -> IntentOutcome {
    let StuckReport {
        intent_kind,
        kind,
        purpose,
        plan_summary,
        candidates,
        verification,
    } = report;
    // `stuck_evidence` documents why the deterministic path failed; callers
    // only need that record when the intent still fails after vision, so it
    // is only prefixed onto failure evidence, not the final Completed one.
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

    let proposal = match assist
        .propose(VisionProposeRequest {
            purpose: purpose.clone().unwrap_or_default(),
            intent_kind: intent_kind.to_owned(),
            screenshot_png: png,
            stuck: kind,
        })
        .await
    {
        Ok(proposal) => proposal,
        Err(error) => {
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

    let proposal_hash = proposal_sha256(&proposal);
    if proposal.confidence < VISION_CONFIDENCE_FLOOR {
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

    let mut act_evidence = match execute_vision_action(page_id, browser, &proposal.action).await {
        Ok(evidence) => evidence,
        Err(error) => {
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

async fn execute_vision_action(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    action: &VisionAction,
) -> Result<Vec<Evidence>, CommandError> {
    match action {
        VisionAction::Click { x, y } => browser.click_xy(page_id, *x, *y).await,
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
        // `ExtractValue` is read-only and only ever consumed directly by
        // `resolve_extract_field`'s own vision path, never by this
        // act-on-the-page dispatcher.
        VisionAction::ExtractValue { .. } => Err(CommandError {
            code: ErrorCode::VisionAssistFailed,
            message: "extractValue vision action is not an actionable page operation".into(),
            layer: ErrorLayer::Page,
            retryable: false,
        }),
    }
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
