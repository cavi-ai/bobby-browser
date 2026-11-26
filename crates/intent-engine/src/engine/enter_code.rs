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
    dom_engine::control_target_spec(target)
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

#[derive(Clone, Copy, PartialEq, Eq)]
enum CodeInput {
    Single,
    SplitField,
    SplitFromFirst,
}

struct Run<'a> {
    page_id: &'a PageId,
    browser: &'a dyn IntentBrowser,
    code: String,
    summary: String,
    resolutions: Vec<Evidence>,
    /// A code field left the page as the code went in: the page took the
    /// step itself, and the page that follows decides.
    moved_on: bool,
}

impl Run<'_> {
    /// Whether `field` is still on the page.
    async fn on_page(&self, field: &CodeField) -> bool {
        let Ok(Some(snapshot)) = self.browser.form_snapshot(self.page_id).await else {
            return true;
        };
        code_entry::find_control(&snapshot, &field.id)
            .filter(|control| control.target == field.target)
            .is_some()
    }

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
        input: CodeInput,
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
            Err(error)
                if (error.code == ErrorCode::TargetDetached
                    || post_navigation_context_loss(&error))
                    && !self.on_page(field).await =>
            {
                self.moved_on = true;
                return Ok(());
            }
            // The worker compares the entire input with the first box.
            // The split step verifies the individual boxes after typing.
            Err(error)
                if input == CodeInput::SplitFromFirst
                    && error.code == ErrorCode::VerificationFailed =>
            {
                return Ok(());
            }
            Err(error) => {
                // A worker message can quote what was typed; with one
                // character per box there is nothing safe to keep of it.
                let message = if input == CodeInput::Single {
                    format!("typing the code failed: {}", error.message)
                } else {
                    "typing into a code box failed".to_owned()
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
        if input == CodeInput::Single
            && verify_fill(&value, &evidence).is_err()
            && !self.reads_back(field, chunk).await
        {
            if !self.on_page(field).await {
                self.moved_on = true;
                return Ok(());
            }
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
        field_readback(&snapshot, field, expected) == Some(true)
    }

    /// Reads every field back once all are filled and types again into any
    /// that lost its character: typing into one box can spill into the next
    /// when the page moves focus on input.
    async fn verify_filled(
        &mut self,
        fields: &[(&CodeField, String)],
        input: CodeInput,
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
                self.fill(field, chunk, input).await?;
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
        can_verify_exit: bool,
    ) -> Result<(), IntentOutcome> {
        for pass in 0..=REFILL_PASSES {
            if pass > 0 {
                tokio::time::sleep(Duration::from_millis(CODE_STEP_POLL_MS)).await;
            }
            let Ok(Some(snapshot)) = self.browser.form_snapshot(self.page_id).await else {
                continue;
            };
            if fields
                .iter()
                .all(|(field, expected)| field_readback(&snapshot, field, expected) == Some(true))
            {
                return Ok(());
            }
            // An early exit can swallow keystrokes. Require the caller's
            // expected state when the boxes disappeared before readback.
            if can_verify_exit
                && !snapshot.truncated
                && fields.iter().all(|(field, _)| {
                    code_entry::find_control(&snapshot, &field.id)
                        .is_none_or(|control| control.target != field.target)
                })
            {
                self.moved_on = true;
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
        input: CodeInput,
    ) -> Result<Vec<Evidence>, IntentOutcome> {
        if self.moved_on {
            return Ok(Vec::new());
        }
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
                // A typed-through step needs a named target for Enter.
                let Some(last) = step
                    .fields
                    .iter()
                    .rev()
                    .filter_map(|field| field.target.as_ref())
                    .find(|target| {
                        input != CodeInput::SplitFromFirst || !target.accessible_name.is_empty()
                    })
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
        .filter(|(field, expected)| field_readback(snapshot, field, expected) == Some(false))
        .collect()
}

fn named_target(field: &CodeField) -> Option<&FormControlTarget> {
    field
        .target
        .as_ref()
        .filter(|target| !target.accessible_name.is_empty())
}

fn field_readback(
    snapshot: &types::FormSnapshot,
    field: &CodeField,
    expected: &str,
) -> Option<bool> {
    code_entry::find_control(snapshot, &field.id)
        .filter(|control| control.target == field.target)
        .and_then(|control| code_entry::holds(control, expected))
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
        moved_on: false,
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
    // A snapshot can stamp unnamed boxes without giving them a name.
    // Start in the named first box and let the page move focus on input.
    let input = if !split {
        CodeInput::Single
    } else if named_target(&step.fields[0]).is_some()
        && step.fields[1..]
            .iter()
            .any(|field| named_target(field).is_none())
    {
        CodeInput::SplitFromFirst
    } else {
        CodeInput::SplitField
    };
    if input == CodeInput::SplitFromFirst {
        if let Err(outcome) = run.fill(&step.fields[0], &code, input).await {
            return outcome;
        }
        let can_verify_exit = request.submit && request.expected_state.is_some();
        if let Err(outcome) = run.verify_typed_through(&chunks, can_verify_exit).await {
            return outcome;
        }
    } else {
        for (index, (field, chunk)) in chunks.iter().enumerate() {
            if let Err(outcome) = run.fill(field, chunk, input).await {
                return outcome;
            }
            if run.moved_on && index + 1 < chunks.len() {
                return run.failed(
                    Run::error(
                        ErrorCode::VerificationFailed,
                        "the page left the code step before every character was entered",
                    ),
                    Vec::new(),
                    "verifyFailed",
                );
            }
        }
        if let Err(outcome) = run.verify_filled(&chunks, input).await {
            return outcome;
        }
    }

    // A submit can be verified by the page that follows. A fill-only run
    // needs the current page to confirm every field kept its value.
    if !request.submit {
        let confirmed = browser
            .form_snapshot(page_id)
            .await
            .ok()
            .flatten()
            .is_some_and(|snapshot| {
                chunks.iter().all(|(field, expected)| {
                    field_readback(&snapshot, field, expected) == Some(true)
                })
            });
        if !confirmed {
            return run.failed(
                Run::error(
                    ErrorCode::VerificationFailed,
                    "the page did not keep the entered code in every code field",
                ),
                Vec::new(),
                "verifyFailed",
            );
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
            .press_submit(&step, request.expected_state.as_ref(), input)
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
