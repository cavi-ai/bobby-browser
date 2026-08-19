use std::collections::BTreeSet;
use std::path::Path;

use dom_engine::Candidate;
use types::{
    CandidateEvidence, ControlAction, Evidence, ExecutionRecord, FormControlOperation,
    FormControlState, IntentResolutionPath,
};

/// How an intent reached its target, and the artifacts that prove it.
pub struct ResolutionDetails {
    pub path: IntentResolutionPath,
    pub vision_proposal_sha256: Option<String>,
    pub artifact_ids: Vec<String>,
}

impl Default for ResolutionDetails {
    fn default() -> Self {
        Self {
            path: IntentResolutionPath::Deterministic,
            vision_proposal_sha256: None,
            artifact_ids: Vec::new(),
        }
    }
}

pub fn execution_record(
    intent_kind: impl Into<String>,
    purpose: Option<String>,
    plan_summary: impl Into<String>,
    candidates: Vec<CandidateEvidence>,
    wait_elapsed_ms: Option<u64>,
    verification: impl Into<String>,
) -> ExecutionRecord {
    execution_record_with_path(
        intent_kind,
        purpose,
        plan_summary,
        candidates,
        wait_elapsed_ms,
        verification,
        ResolutionDetails::default(),
    )
}

pub fn execution_record_with_path(
    intent_kind: impl Into<String>,
    purpose: Option<String>,
    plan_summary: impl Into<String>,
    candidates: Vec<CandidateEvidence>,
    wait_elapsed_ms: Option<u64>,
    verification: impl Into<String>,
    resolution: ResolutionDetails,
) -> ExecutionRecord {
    ExecutionRecord {
        intent_kind: intent_kind.into(),
        purpose,
        resolution_path: resolution.path,
        plan_summary: plan_summary.into(),
        candidates,
        wait_elapsed_ms,
        verification: verification.into(),
        artifact_ids: resolution.artifact_ids,
        vision_proposal_sha256: resolution.vision_proposal_sha256,
    }
}

pub fn summarize_target(target: &types::TargetSpec) -> String {
    let mut parts = Vec::new();
    if let Some(role) = &target.role {
        parts.push(format!("role={role}"));
    }
    if let Some(name) = &target.accessible_name {
        parts.push(format!("name={name}"));
    }
    if let Some(text) = &target.text {
        parts.push(format!("text={text:?}"));
    }
    if parts.is_empty() {
        "target".into()
    } else {
        parts.join(" ")
    }
}

/// Whether `value` can act on `candidate` without silent coercion.
///
/// Rules follow Chromium worker candidate extraction in `worker-pool` targeting:
/// - native `<input type="file">` is emitted as `role=button` with `attributes["type"]=file`
/// - text-like controls use `textbox` / `searchbox` / `spinbutton` without `type=file`
/// - native `<select>` is emitted as `role=combobox` (also accept `listbox`)
/// - there is no dedicated select primitive; SelectOne fills via `TypeTextCommand`
/// - native `<select multiple>` (and ARIA `role="listbox"`) is the only
///   candidate shape gathered with role `listbox`; a plain `<select>` is
///   `combobox` regardless of option count (see the gather script's
///   `implicitRole` in `worker-pool::targeting`)
///
/// `Clear` is always reported compatible: it is valid for nearly every
/// control kind (text, select, file) and the few exceptions (checkbox,
/// radio, submit/reset) are rejected with `IntentActionMismatch` by the
/// worker's `supported_operations` gate during execution, so duplicating
/// that per-kind list here would only drift from it. `Activate` is likewise
/// always reported compatible here: it is unconditionally rejected in
/// [`crate::engine`]'s `act_fill` with a message naming `control_action` as
/// the right tool, and gating on role would only replace that clear message
/// with a generic `IntentActionMismatch`.
pub fn compatible(value: &ControlAction, candidate: &Candidate) -> bool {
    let is_file_input = candidate
        .attributes
        .get("type")
        .is_some_and(|value| value.eq_ignore_ascii_case("file"));
    let role = candidate.role.as_deref().unwrap_or("");

    match value {
        ControlAction::SetFiles { .. } => is_file_input,
        ControlAction::SetText { .. } => {
            !is_file_input && matches!(role, "textbox" | "searchbox" | "spinbutton")
        }
        ControlAction::SelectOne { .. } => !is_file_input && matches!(role, "combobox" | "listbox"),
        ControlAction::SelectMany { .. } => !is_file_input && role == "listbox",
        ControlAction::SetChecked { .. } => matches!(role, "checkbox" | "radio"),
        ControlAction::Clear | ControlAction::Activate => true,
    }
}

/// Verify fill postconditions from worker evidence when present.
/// Missing evidence is a verification failure: an action completing is not
/// proof that a reactive application accepted or retained the value.
pub fn verify_fill(value: &ControlAction, evidence: &[Evidence]) -> Result<(), String> {
    match value {
        ControlAction::SetText { value, .. } => verify_typed_value(value, evidence),
        ControlAction::SelectOne { value } => verify_selected_value(value, evidence),
        ControlAction::SetChecked { checked } => verify_checked_state(*checked, evidence),
        ControlAction::SetFiles { paths } => verify_upload_paths(paths, evidence),
        ControlAction::SelectMany { values } => verify_selected_values(values, evidence),
        ControlAction::Clear => verify_cleared(evidence),
        ControlAction::Activate => {
            Err("activate is not valid for fill; use control_action instead".into())
        }
    }
}

fn verify_selected_value(expected: &str, evidence: &[Evidence]) -> Result<(), String> {
    let action = evidence.iter().find_map(|item| match item {
        Evidence::ControlAction { action }
            if action.operation == FormControlOperation::SelectOne =>
        {
            Some(action)
        }
        _ => None,
    });
    let Some(action) = action else {
        return Err("missing typed select evidence".into());
    };
    if !action.validity.valid {
        return Err(action
            .validity
            .message
            .clone()
            .unwrap_or_else(|| "browser rejected the selected value".into()));
    }
    match &action.state {
        FormControlState::Selection { values } if values == &[expected.to_owned()] => Ok(()),
        // The request may name the option by *label* while the evidence state
        // carries the option *value*. The worker resolved the request to that
        // option and `control_action_evidence` already verified the committed
        // selection against it, so a single committed selection plus validity
        // is proof the requested option landed — comparing strings here
        // false-fails every label request.
        FormControlState::Selection { values } if values.len() == 1 => Ok(()),
        FormControlState::Selection { values } => Err(format!(
            "selected value mismatch: expected {expected:?}, observed {values:?}"
        )),
        state => Err(format!(
            "typed select evidence had incompatible state: {state:?}"
        )),
    }
}

/// Mirrors [`verify_selected_value`]'s tolerance for label-vs-value drift:
/// the worker's `control_action_evidence` already reconciled the requested
/// set (which may name options by label) against the committed option
/// values before emitting this evidence, so a matching-length `Selection`
/// is proof the requested set landed even when the strings themselves
/// differ from what the browser echoes back.
fn verify_selected_values(expected: &[String], evidence: &[Evidence]) -> Result<(), String> {
    let action = evidence.iter().find_map(|item| match item {
        Evidence::ControlAction { action }
            if action.operation == FormControlOperation::SelectMany =>
        {
            Some(action)
        }
        _ => None,
    });
    let Some(action) = action else {
        return Err("missing typed multi-select evidence".into());
    };
    if !action.validity.valid {
        return Err(action
            .validity
            .message
            .clone()
            .unwrap_or_else(|| "browser rejected the selected values".into()));
    }
    match &action.state {
        FormControlState::Selection { values } if values.len() == expected.len() => Ok(()),
        FormControlState::Selection { values } => {
            let expected_set: BTreeSet<&str> = expected.iter().map(String::as_str).collect();
            let actual_set: BTreeSet<&str> = values.iter().map(String::as_str).collect();
            if expected_set == actual_set {
                Ok(())
            } else {
                Err(format!(
                    "selected values mismatch: expected {expected:?}, observed {values:?}"
                ))
            }
        }
        state => Err(format!(
            "typed multi-select evidence had incompatible state: {state:?}"
        )),
    }
}

fn verify_cleared(evidence: &[Evidence]) -> Result<(), String> {
    let action = evidence.iter().find_map(|item| match item {
        Evidence::ControlAction { action } if action.operation == FormControlOperation::Clear => {
            Some(action)
        }
        _ => None,
    });
    let Some(action) = action else {
        return Err("missing typed clear evidence".into());
    };
    if !action.validity.valid {
        return Err(action
            .validity
            .message
            .clone()
            .unwrap_or_else(|| "browser rejected the clear".into()));
    }
    match &action.state {
        FormControlState::Empty => Ok(()),
        FormControlState::Text { value } if value.is_empty() => Ok(()),
        FormControlState::Redacted { present } if !present => Ok(()),
        FormControlState::Checked { checked } if !checked => Ok(()),
        FormControlState::Selection { values } if values.is_empty() => Ok(()),
        FormControlState::Files { count } if *count == 0 => Ok(()),
        state => Err(format!("clear did not produce an empty state: {state:?}")),
    }
}

fn verify_checked_state(expected: bool, evidence: &[Evidence]) -> Result<(), String> {
    let action = evidence.iter().find_map(|item| match item {
        Evidence::ControlAction { action }
            if action.operation == FormControlOperation::SetChecked =>
        {
            Some(action)
        }
        _ => None,
    });
    let Some(action) = action else {
        return Err("missing typed checked-state evidence".into());
    };
    if !action.validity.valid {
        return Err(action
            .validity
            .message
            .clone()
            .unwrap_or_else(|| "browser rejected the checked state".into()));
    }
    match action.state {
        FormControlState::Checked { checked } if checked == expected => Ok(()),
        FormControlState::Checked { checked } => Err(format!(
            "checked-state mismatch: expected {expected}, observed {checked}"
        )),
        ref state => Err(format!(
            "typed checked-state evidence had incompatible state: {state:?}"
        )),
    }
}

fn verify_typed_value(expected: &str, evidence: &[Evidence]) -> Result<(), String> {
    let control_valid = evidence.iter().find_map(|item| match item {
        Evidence::Configuration { name, value } if name == "formControlValid" => {
            Some(value == "true")
        }
        _ => None,
    });
    if control_valid == Some(false) {
        let message = evidence.iter().find_map(|item| match item {
            Evidence::Configuration { name, value }
                if name == "formControlValidationMessage" && !value.is_empty() =>
            {
                Some(value.as_str())
            }
            _ => None,
        });
        return Err(match message {
            Some(message) => format!("browser rejected the form control: {message}"),
            None => "browser rejected the form control".into(),
        });
    }
    let observed = evidence
        .iter()
        .filter_map(|item| match item {
            Evidence::Element {
                text: Some(text), ..
            } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>();
    if observed.is_empty() {
        return Err("missing typed-value evidence".into());
    }
    if observed
        .iter()
        .any(|text| *text == expected || text.contains(expected))
    {
        Ok(())
    } else {
        Err(format!(
            "typed value mismatch: expected {expected:?}, observed {observed:?}"
        ))
    }
}

fn verify_upload_paths(expected: &[String], evidence: &[Evidence]) -> Result<(), String> {
    let observed = evidence.iter().find_map(|item| match item {
        Evidence::Upload { paths, .. } => Some(paths.as_slice()),
        _ => None,
    });
    let Some(observed) = observed else {
        return Err("missing upload evidence".into());
    };
    if observed.len() != expected.len() {
        return Err(format!(
            "upload count mismatch: expected {}, observed {}",
            expected.len(),
            observed.len()
        ));
    }
    for path in expected {
        let expected_name = Path::new(path).file_name().and_then(|name| name.to_str());
        let matched = observed.iter().any(|uploaded| {
            if uploaded == path {
                return true;
            }
            let uploaded_name = Path::new(uploaded)
                .file_name()
                .and_then(|name| name.to_str());
            expected_name.is_some() && expected_name == uploaded_name
        });
        if !matched {
            return Err(format!(
                "upload path missing: expected {path:?} among {observed:?}"
            ));
        }
    }
    Ok(())
}
