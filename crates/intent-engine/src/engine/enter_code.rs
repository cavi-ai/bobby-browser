//! `enterCode`: fills the one-time-code step a page shows (one field, or one
//! character per box), submits it, and verifies the page took it. The code is
//! a credential: it never appears in evidence, errors or logs.

use std::time::Duration;

use super::*;
use crate::code_entry::{self, CodeField, CodeStep};

/// How long a page may take to leave the code step when the caller names no
/// expected state.
const CODE_STEP_EXIT_MS: u64 = 6_000;
const CODE_STEP_POLL_MS: u64 = 250;
/// Times a code box that lost its character is typed into again.
const REFILL_PASSES: usize = 3;
const KIND: &str = "enterCode";

pub(super) struct EnterCodeRequest<'a> {
    pub code: &'a types::OneTimeCode,
    pub hints: &'a types::IntentHints,
    pub submit: bool,
    pub expected_state: Option<WaitForCommand>,
}

/// The code step the page shows now, read from its form controls and text.
/// `hints` pins the field when the caller named it.
pub(super) async fn read_code_step(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    hints: Option<&types::IntentHints>,
) -> Result<Option<CodeStep>, CommandError> {
    let Some(snapshot) = browser.form_snapshot(page_id).await? else {
        return Ok(None);
    };
    let text = browser
        .inspect_settled_page(page_id)
        .await
        .unwrap_or_default()
        .into_iter()
        .find_map(|item| match item {
            Evidence::Inspection { text, .. } => Some(text),
            _ => None,
        })
        .unwrap_or_default();
    Ok(code_entry::detect(&snapshot, &text, hints))
}

pub(super) fn target_spec(target: &FormControlTarget) -> TargetSpec {
    let segment = |segment: &SemanticTargetSegment| {
        Box::new(TargetSpec {
            role: Some(segment.role.clone()),
            accessible_name: (!segment.accessible_name.is_empty())
                .then(|| segment.accessible_name.clone()),
            ordinal: segment.ordinal,
            ..TargetSpec::default()
        })
    };
    TargetSpec {
        role: Some(target.role.clone()),
        accessible_name: Some(target.accessible_name.clone()),
        ordinal: target.ordinal,
        frame_path: target.frame_path.iter().map(segment).collect(),
        shadow_path: target.shadow_path.iter().map(segment).collect(),
        ..TargetSpec::default()
    }
}

/// `text` with every occurrence of `code` masked. Codes shorter than four
/// characters are too short to mask without mangling the text.
fn mask(text: &str, code: &str) -> String {
    if code.chars().count() < 4 {
        return text.to_owned();
    }
    text.replace(code, "[redacted]")
}

/// `evidence` with `code` masked out of every string it carries. An item that
/// cannot be re-read after masking is dropped rather than risk the code.
fn mask_evidence(evidence: Vec<Evidence>, code: &str) -> Vec<Evidence> {
    fn walk(value: &mut serde_json::Value, code: &str) {
        match value {
            serde_json::Value::String(text) => *text = mask(text, code),
            serde_json::Value::Array(items) => items.iter_mut().for_each(|item| walk(item, code)),
            serde_json::Value::Object(fields) => {
                fields.values_mut().for_each(|item| walk(item, code))
            }
            _ => {}
        }
    }
    evidence
        .into_iter()
        .filter_map(|item| {
            let mut value = serde_json::to_value(&item).ok()?;
            walk(&mut value, code);
            serde_json::from_value(value).ok()
        })
        .collect()
}

struct Run<'a> {
    page_id: &'a PageId,
    browser: &'a dyn IntentBrowser,
    code: String,
    summary: String,
    resolutions: Vec<Evidence>,
}

impl Run<'_> {
    fn failed(
        &self,
        error: CommandError,
        mut evidence: Vec<Evidence>,
        verification: &str,
    ) -> IntentOutcome {
        let mut all = self.resolutions.clone();
        all.append(&mut evidence);
        all.push(intent_evidence(execution_record(
            KIND,
            Some("enter the one-time code".into()),
            self.summary.clone(),
            Vec::new(),
            None,
            verification,
        )));
        IntentOutcome::Failed {
            error: CommandError {
                message: mask(&error.message, &self.code),
                ..error
            },
            evidence: mask_evidence(all, &self.code),
        }
    }

    fn error(code: ErrorCode, message: impl Into<String>) -> CommandError {
        CommandError {
            code,
            message: message.into(),
            layer: ErrorLayer::Page,
            retryable: false,
        }
    }

    async fn locate(&mut self, target: &FormControlTarget) -> Result<LocatedTarget, IntentOutcome> {
        let spec = target_spec(target);
        let located = locate(
            self.page_id,
            self.browser,
            // The code never leaves for a vision provider.
            &VisionContext::default(),
            LocateRequest {
                intent_kind: KIND,
                purpose: Some("enter the one-time code"),
                plan_summary: &self.summary,
                target: &spec,
                mode: LocateMode::Standard,
            },
        )
        .await?;
        self.resolutions.push(Evidence::Resolution {
            target: Box::new(spec),
            fingerprint: Box::new(fingerprint(self.page_id, &located.candidate)),
            candidates: vec![located.evidence.clone()],
            best_match_authorized: located.best_match_authorized,
        });
        Ok(located)
    }

    /// Types `chunk` into `field` and checks the field kept it.
    async fn fill(
        &mut self,
        field: &CodeField,
        chunk: &str,
        split: bool,
    ) -> Result<(), IntentOutcome> {
        let Some(target) = &field.target else {
            return Err(self.failed(
                Self::error(
                    ErrorCode::IntentActionMismatch,
                    "a code field has no accessible name, so it cannot be targeted; pass hints \
                     naming it",
                ),
                Vec::new(),
                "unnamedField",
            ));
        };
        let located = self.locate(target).await?;
        let spec = target_spec(target);
        let value = ControlAction::SetText {
            value: chunk.to_owned(),
            clear_first: true,
        };
        let evidence = match act_fill(
            self.page_id,
            self.browser,
            &located.candidate,
            &spec,
            &value,
        )
        .await
        {
            Ok(evidence) => evidence,
            Err(error) => {
                // A worker message can quote what was typed; with one
                // character per box there is nothing safe to keep of it.
                let message = if split {
                    "typing into a code box failed".to_owned()
                } else {
                    format!("typing the code failed: {}", error.message)
                };
                return Err(self.failed(
                    CommandError { message, ..error },
                    Vec::new(),
                    "actFailed",
                ));
            }
        };
        // With one character per box a typing worker's own read-back can race
        // the page moving focus on, so the boxes are checked together below.
        if !split && verify_fill(&value, &evidence).is_err() && !self.reads_back(field, chunk).await
        {
            return Err(self.failed(
                Self::error(
                    ErrorCode::VerificationFailed,
                    "the code field did not keep the entered code",
                ),
                Vec::new(),
                "verifyFailed",
            ));
        }
        Ok(())
    }

    /// Whether the page itself shows `expected` in `field` now, for when the
    /// typing worker's own read-back raced a page that moved focus on input.
    async fn reads_back(&self, field: &CodeField, expected: &str) -> bool {
        let Ok(Some(snapshot)) = self.browser.form_snapshot(self.page_id).await else {
            return false;
        };
        code_entry::find_control(&snapshot, &field.id)
            .filter(|control| control.target == field.target)
            .and_then(|control| code_entry::holds(control, expected))
            == Some(true)
    }

    /// Reads every field back once all are filled and types again into any
    /// that lost its character: typing into one box can spill into the next
    /// when the page moves focus on input.
    async fn verify_filled(
        &mut self,
        fields: &[(&CodeField, String)],
        split: bool,
    ) -> Result<(), IntentOutcome> {
        for pass in 0..=REFILL_PASSES {
            let Ok(Some(snapshot)) = self.browser.form_snapshot(self.page_id).await else {
                return Ok(());
            };
            let wrong = wrong_fields(&snapshot, fields);
            if wrong.is_empty() {
                return Ok(());
            }
            if pass == REFILL_PASSES {
                break;
            }
            for (field, chunk) in wrong {
                self.fill(field, chunk, split).await?;
            }
        }
        Err(self.failed(
            Self::error(
                ErrorCode::VerificationFailed,
                "the page did not keep the entered code in every code field",
            ),
            Vec::new(),
            "verifyFailed",
        ))
    }

    /// Reads back boxes filled by typing the whole code from the first box,
    /// giving the page a few polls to move each character into its box.
    async fn verify_typed_through(
        &mut self,
        fields: &[(&CodeField, String)],
    ) -> Result<(), IntentOutcome> {
        for pass in 0..=REFILL_PASSES {
            if pass > 0 {
                tokio::time::sleep(Duration::from_millis(CODE_STEP_POLL_MS)).await;
            }
            let Ok(Some(snapshot)) = self.browser.form_snapshot(self.page_id).await else {
                return Ok(());
            };
            if wrong_fields(&snapshot, fields).is_empty() {
                return Ok(());
            }
        }
        Err(self.failed(
            Self::error(
                ErrorCode::VerificationFailed,
                "the code was typed from the first code box, but the page did not move each \
                 character into its own box",
            ),
            Vec::new(),
            "verifyFailed",
        ))
    }

    /// Presses the step's submit control, or Enter in the last field.
    async fn press_submit(
        &mut self,
        step: &CodeStep,
        expected_state: Option<&WaitForCommand>,
    ) -> Result<Vec<Evidence>, IntentOutcome> {
        let acted = match &step.submit {
            Some(submit) => {
                let located = self.locate(submit).await?;
                let spec = target_spec(submit);
                let (selector, target) = action_target(&located.candidate, &spec);
                self.browser
                    .click(
                        self.page_id,
                        &ClickCommand {
                            selector,
                            target: Some(target),
                            boundary: true,
                            expected_url: expected_state.and_then(expected_url_from_wait),
                            modifiers: Vec::new(),
                        },
                    )
                    .await
            }
            None => {
                // Enter goes to the last box the page names.
                let Some(last) = step
                    .fields
                    .iter()
                    .rev()
                    .find_map(|field| field.target.as_ref())
                else {
                    return Ok(Vec::new());
                };
                let located = self.locate(last).await?;
                let spec = target_spec(last);
                let (selector, target) = action_target(&located.candidate, &spec);
                self.browser
                    .type_text(
                        self.page_id,
                        &TypeTextCommand {
                            selector,
                            target: Some(target),
                            value: "\n".into(),
                            clear_first: false,
                            expected_url: None,
                        },
                    )
                    .await
                    .map(|_| Vec::new())
            }
        };
        match acted {
            Ok(evidence) => Ok(evidence),
            // The page navigated while the press returned: whether the code
            // took is for the verification below to say.
            Err(error) if post_navigation_context_loss(&error) => Ok(Vec::new()),
            Err(error) => Err(self.failed(error, Vec::new(), "actFailed")),
        }
    }
}

/// The fields whose control in `snapshot` shows something other than its
/// expected character. A field whose control is gone or unreadable is not
/// counted.
fn wrong_fields<'a, 'b>(
    snapshot: &types::FormSnapshot,
    fields: &'a [(&'b CodeField, String)],
) -> Vec<&'a (&'b CodeField, String)> {
    fields
        .iter()
        .filter(|(field, expected)| {
            code_entry::find_control(snapshot, &field.id)
                .filter(|control| control.target == field.target)
                .and_then(|control| code_entry::holds(control, expected))
                == Some(false)
        })
        .collect()
}

pub(super) async fn execute_enter_code(
    page_id: &PageId,
    browser: &dyn IntentBrowser,
    request: EnterCodeRequest<'_>,
) -> IntentOutcome {
    let code = request.code.normalized();
    let mut run = Run {
        page_id,
        browser,
        code: code.clone(),
        summary: "enterCode".into(),
        resolutions: Vec::new(),
    };
    let step = match read_code_step(page_id, browser, Some(request.hints)).await {
        Ok(Some(step)) => step,
        Ok(None) => {
            return run.failed(
                Run::error(
                    ErrorCode::TargetNotFound,
                    "no one-time-code field is on the page; take a form_snapshot and pass the \
                     field as hints",
                ),
                Vec::new(),
                "codeFieldNotFound",
            )
        }
        Err(error) => return run.failed(error, Vec::new(), "gatherFailed"),
    };
    run.summary = format!(
        "enterCode fields={} submit={}",
        step.fields.len(),
        request.submit
    );

    let characters: Vec<char> = code.chars().collect();
    let split = step.fields.len() > 1;
    if split && characters.len() != step.fields.len() {
        return run.failed(
            Run::error(
                ErrorCode::IntentActionMismatch,
                format!(
                    "the page shows {} code boxes but the code has {} characters",
                    step.fields.len(),
                    characters.len()
                ),
            ),
            vec![step.evidence()],
            "lengthMismatch",
        );
    }
    if let Some(limit) = step.fields.first().and_then(|field| field.max_length) {
        if !split && characters.len() > limit as usize {
            return run.failed(
                Run::error(
                    ErrorCode::IntentActionMismatch,
                    format!(
                        "the code field takes at most {limit} characters but the code has {}",
                        characters.len()
                    ),
                ),
                vec![step.evidence()],
                "lengthMismatch",
            );
        }
    }

    let chunks: Vec<(&CodeField, String)> = if split {
        step.fields
            .iter()
            .zip(&characters)
            .map(|(field, ch)| (field, ch.to_string()))
            .collect()
    } else {
        vec![(&step.fields[0], code.clone())]
    };
    // Boxes with no name after a named first box cannot be targeted: the
    // code is typed from the first box, and the page moves focus on input.
    let typed_through = split
        && step.fields[0].target.is_some()
        && step.fields[1..].iter().any(|field| field.target.is_none());
    if typed_through {
        if let Err(outcome) = run.fill(&step.fields[0], &code, true).await {
            return outcome;
        }
        if let Err(outcome) = run.verify_typed_through(&chunks).await {
            return outcome;
        }
    } else {
        for (field, chunk) in &chunks {
            if let Err(outcome) = run.fill(field, chunk, split).await {
                return outcome;
            }
        }
        if let Err(outcome) = run.verify_filled(&chunks, split).await {
            return outcome;
        }
    }

    let mut evidence = std::mem::take(&mut run.resolutions);
    evidence.push(Evidence::Configuration {
        name: KIND.into(),
        value: format!(
            "entered {} characters in {} field(s)",
            characters.len(),
            step.fields.len()
        ),
    });
    run.resolutions = evidence.clone();

    let mut verification = "filled";
    let mut wait_elapsed_ms = None;
    if request.submit {
        let mut acted = match run
            .press_submit(&step, request.expected_state.as_ref())
            .await
        {
            Ok(acted) => acted,
            Err(outcome) => return outcome,
        };
        evidence.append(&mut acted);
        match &request.expected_state {
            Some(expected) => match browser.wait_for(page_id, expected).await {
                Ok(mut waited) => {
                    wait_elapsed_ms = waited.iter().find_map(|item| match item {
                        Evidence::Wait { elapsed_ms, .. } => Some(*elapsed_ms),
                        _ => None,
                    });
                    evidence.append(&mut waited);
                    verification = "codeAccepted";
                }
                Err(error) => {
                    return run.rejected(request.hints, Some(error), &evidence).await;
                }
            },
            None => {
                if let Some(outcome) = run.await_exit(request.hints, &evidence).await {
                    return outcome;
                }
                verification = "codeAccepted";
            }
        }
    }

    evidence.push(intent_evidence(execution_record(
        KIND,
        Some("enter the one-time code".into()),
        run.summary.clone(),
        Vec::new(),
        wait_elapsed_ms,
        verification,
    )));
    IntentOutcome::Completed {
        evidence: mask_evidence(evidence, &code),
    }
}

impl Run<'_> {
    /// The submitted code did not reach the expected state. When the page
    /// still asks for a code it was not accepted; hand back where to enter a
    /// new one.
    async fn rejected(
        &self,
        hints: &types::IntentHints,
        waited: Option<CommandError>,
        evidence: &[Evidence],
    ) -> IntentOutcome {
        let still = read_code_step(self.page_id, self.browser, Some(hints))
            .await
            .ok()
            .flatten();
        let mut carried = evidence.to_vec();
        let message = match (&still, waited) {
            (Some(step), _) => {
                carried.push(step.evidence());
                "the page still asks for a code after it was submitted: the code may be wrong or \
                 expired. Get a fresh code, then call intent_enter_code again"
                    .to_owned()
            }
            (None, Some(error)) => format!(
                "the code was submitted but the expected post-state did not hold: {}",
                error.message
            ),
            (None, None) => "the code was submitted but the page did not reach a new state".into(),
        };
        self.failed(
            Self::error(ErrorCode::VerificationFailed, message),
            carried,
            if still.is_some() {
                "codeRejected"
            } else {
                "verifyFailed"
            },
        )
    }

    /// With no expected state the page proves the code took by leaving the
    /// code step. `Some` is the failure when it does not.
    async fn await_exit(
        &self,
        hints: &types::IntentHints,
        evidence: &[Evidence],
    ) -> Option<IntentOutcome> {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(CODE_STEP_EXIT_MS);
        loop {
            match read_code_step(self.page_id, self.browser, Some(hints)).await {
                Ok(None) => return None,
                Ok(Some(_)) | Err(_) if tokio::time::Instant::now() < deadline => {
                    tokio::time::sleep(Duration::from_millis(CODE_STEP_POLL_MS)).await;
                }
                Ok(Some(_)) => return Some(self.rejected(hints, None, evidence).await),
                Err(error) => {
                    return Some(self.failed(error, evidence.to_vec(), "verifyFailed"));
                }
            }
        }
    }
}
