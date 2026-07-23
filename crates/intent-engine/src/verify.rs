use types::{CandidateEvidence, ExecutionRecord, IntentResolutionPath};

pub fn execution_record(
    intent_kind: impl Into<String>,
    purpose: Option<String>,
    plan_summary: impl Into<String>,
    candidates: Vec<CandidateEvidence>,
    wait_elapsed_ms: Option<u64>,
    verification: impl Into<String>,
) -> ExecutionRecord {
    ExecutionRecord {
        intent_kind: intent_kind.into(),
        purpose,
        resolution_path: IntentResolutionPath::Deterministic,
        plan_summary: plan_summary.into(),
        candidates,
        wait_elapsed_ms,
        verification: verification.into(),
        artifact_ids: Vec::new(),
        vision_proposal_sha256: None,
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
