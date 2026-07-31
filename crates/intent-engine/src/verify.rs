use std::path::Path;

use dom_engine::Candidate;
use types::{CandidateEvidence, Evidence, ExecutionRecord, FillValue, IntentResolutionPath};

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
/// - native `<input type="file">` is emitted as `role=textbox` with `attributes["type"]=file`
/// - text-like controls use `textbox` / `searchbox` / `spinbutton` without `type=file`
/// - native `<select>` is emitted as `role=combobox` (also accept `listbox`)
/// - there is no dedicated select primitive; Select fills via `TypeTextCommand`
pub fn compatible(value: &FillValue, candidate: &Candidate) -> bool {
    let is_file_input = candidate
        .attributes
        .get("type")
        .is_some_and(|value| value.eq_ignore_ascii_case("file"));
    let role = candidate.role.as_deref().unwrap_or("");

    match value {
        FillValue::Files { .. } => is_file_input,
        FillValue::Text { .. } => {
            !is_file_input && matches!(role, "textbox" | "searchbox" | "spinbutton")
        }
        FillValue::Select { .. } => !is_file_input && matches!(role, "combobox" | "listbox"),
        FillValue::Checked { .. } => matches!(role, "checkbox" | "radio"),
    }
}

/// Verify fill postconditions from worker evidence when present.
/// Missing evidence is a verification failure: an action completing is not
/// proof that a reactive application accepted or retained the value.
pub fn verify_fill(value: &FillValue, evidence: &[Evidence]) -> Result<(), String> {
    match value {
        FillValue::Text { text, .. } => verify_typed_value(text, evidence),
        FillValue::Select { option } => verify_typed_value(option, evidence),
        FillValue::Checked { checked } => verify_typed_value(&checked.to_string(), evidence),
        FillValue::Files { paths } => verify_upload_paths(paths, evidence),
    }
}

fn verify_typed_value(expected: &str, evidence: &[Evidence]) -> Result<(), String> {
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
