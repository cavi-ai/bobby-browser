use std::sync::Arc;

use async_trait::async_trait;
use dom_engine::{resolve_candidates, Candidate, ResolutionDecision, ResolutionPolicy};
use types::{
    CaptureScreenshotCommand, ClickCommand, CommandError, ErrorCode, ErrorLayer, Evidence,
    ExecutionRecord, FillValue, IntentCommand, IntentResolutionPath, PageId, ScreenshotMode,
    TargetFingerprint, TargetSpec, TypeTextCommand, UploadFilesCommand, WaitForCommand,
};

use crate::compiler::{compile_intent, IntentPlan};
use crate::stuck::{never_escalates, StuckKind};
use crate::verify::{
    compatible, execution_record, execution_record_with_path, summarize_target, verify_fill,
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
            IntentPlan::SubmitAndVerify {
                target,
                expected_state,
            } => {
                execute_submit_and_verify(intent, page_id, browser, vision, target, expected_state)
                    .await
            }
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
                "locate",
                StuckKind::TargetMissing,
                purpose,
                plan_summary,
                Vec::new(),
                "targetNotFound",
                page_id,
                browser,
                vision,
            )
            .await
        }
        ResolutionDecision::Ambiguous { candidates } => {
            stuck_outcome(
                "locate",
                StuckKind::TargetAmbiguous,
                purpose,
                plan_summary,
                candidates,
                "targetAmbiguous",
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
                "fill",
                StuckKind::TargetMissing,
                purpose,
                plan_summary,
                Vec::new(),
                "targetNotFound",
                page_id,
                browser,
                vision,
            )
            .await;
        }
        ResolutionDecision::Ambiguous { candidates } => {
            return stuck_outcome(
                "fill",
                StuckKind::TargetAmbiguous,
                purpose,
                plan_summary,
                candidates,
                "targetAmbiguous",
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
                "submitAndVerify",
                StuckKind::TargetMissing,
                purpose,
                plan_summary,
                Vec::new(),
                "targetNotFound",
                page_id,
                browser,
                vision,
            )
            .await;
        }
        ResolutionDecision::Ambiguous { candidates } => {
            return stuck_outcome(
                "submitAndVerify",
                StuckKind::TargetAmbiguous,
                purpose,
                plan_summary,
                candidates,
                "targetAmbiguous",
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

async fn stuck_outcome(
    intent_kind: &str,
    kind: StuckKind,
    purpose: Option<String>,
    plan_summary: String,
    candidates: Vec<types::CandidateEvidence>,
    verification: &str,
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    vision: &VisionContext,
) -> IntentOutcome {
    let stuck_code = kind.error_code();
    let stuck_evidence = intent_evidence(execution_record(
        intent_kind,
        purpose.clone(),
        plan_summary.clone(),
        candidates.clone(),
        None,
        verification,
    ));

    if never_escalates(stuck_code) || !kind.may_escalate_to_vision() {
        return IntentOutcome::Failed {
            error: CommandError {
                code: stuck_code,
                message: verification.to_owned(),
                layer: ErrorLayer::Page,
                retryable: false,
            },
            evidence: vec![stuck_evidence],
        };
    }

    let gates_open = vision.session_ok && vision.capability_ok;
    let Some(assist) = vision.assist.as_ref() else {
        return vision_denied_or_unavailable(gates_open, stuck_evidence, verification);
    };
    if !gates_open {
        return IntentOutcome::Failed {
            error: CommandError {
                code: ErrorCode::VisionAssistDenied,
                message: format!("vision assist denied; underlying stuck reason: {verification}"),
                layer: ErrorLayer::Page,
                retryable: false,
            },
            evidence: vec![stuck_evidence],
        };
    }

    escalate_with_vision(
        intent_kind,
        kind,
        purpose,
        plan_summary,
        candidates,
        verification,
        stuck_evidence,
        page_id,
        browser,
        assist.as_ref(),
    )
    .await
}

fn vision_denied_or_unavailable(
    gates_open: bool,
    stuck_evidence: Evidence,
    verification: &str,
) -> IntentOutcome {
    if gates_open {
        IntentOutcome::Failed {
            error: CommandError {
                code: ErrorCode::VisionAssistFailed,
                message: "vision assist provider is not configured".into(),
                layer: ErrorLayer::Page,
                retryable: false,
            },
            evidence: vec![stuck_evidence],
        }
    } else {
        IntentOutcome::Failed {
            error: CommandError {
                code: ErrorCode::VisionAssistDenied,
                message: format!("vision assist denied; underlying stuck reason: {verification}"),
                layer: ErrorLayer::Page,
                retryable: false,
            },
            evidence: vec![stuck_evidence],
        }
    }
}

async fn escalate_with_vision(
    intent_kind: &str,
    kind: StuckKind,
    purpose: Option<String>,
    plan_summary: String,
    candidates: Vec<types::CandidateEvidence>,
    verification: &str,
    stuck_evidence: Evidence,
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    assist: &dyn VisionAssist,
) -> IntentOutcome {
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
                evidence: vec![stuck_evidence],
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
            let mut evidence = vec![stuck_evidence];
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
        let mut evidence = vec![stuck_evidence];
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
            IntentResolutionPath::VisionFallback,
            Some(proposal_hash),
            artifact_ids_from(&screenshot_evidence),
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
            let mut evidence = vec![stuck_evidence];
            evidence.append(&mut screenshot_evidence);
            evidence.push(intent_evidence(execution_record_with_path(
                intent_kind,
                purpose,
                plan_summary,
                candidates,
                None,
                format!("visionActFailed:{verification}"),
                IntentResolutionPath::VisionFallback,
                Some(proposal_hash),
                artifact_ids_from(&screenshot_evidence),
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

    let mut evidence = screenshot_evidence;
    evidence.append(&mut act_evidence);
    let artifact_ids = artifact_ids_from(&evidence);
    evidence.push(intent_evidence(execution_record_with_path(
        intent_kind,
        purpose,
        plan_summary,
        candidates,
        None,
        "visionFallback",
        IntentResolutionPath::VisionFallback,
        Some(proposal_hash),
        artifact_ids,
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
                    },
                )
                .await
        }
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
