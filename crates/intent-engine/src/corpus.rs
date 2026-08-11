//! Vision-escalation corpus collection.
//!
//! When `[vision].corpus_dir` is configured, every escalation through
//! `escalate_with_vision` appends one JSONL record to
//! `<corpus_dir>/vision-corpus.jsonl`: the screenshot, the exact candidate
//! list the model saw, the proposal, the terminal outcome stage, and — for
//! verified clicks — the target index resolved via `element_at_point`.
//!
//! Records are schema-agnostic (raw action kinds + `target_index`), matching
//! the gauntlet corpus contract; `build_completion` converts at training time.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::vision::VisionAction;

/// One escalation, serialized as one JSONL line in the training format.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CorpusRecord {
    pub image_b64: String,
    pub purpose: String,
    pub intent_kind: String,
    pub stuck: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_url: Option<String>,
    pub context_candidates: Vec<CorpusCandidate>,
    /// Index into `context_candidates` of the element the executed action
    /// actually hit. `None` when the action was not a verified click, the
    /// worker cannot resolve points, or the resolution matched no candidate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_index: Option<usize>,
    /// What `element_at_point` saw, for offline review when matching failed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolved_element: Option<ResolvedElement>,
    pub model_response: CorpusModelResponse,
    pub success: bool,
    pub journey: String,
    pub step: String,
    /// Terminal engine stage: `visionFallback`, `visionActFailed:<verify>`,
    /// `visionRejectionFloor`, `visionProposeFailed`, `visionScreenshotFailed`.
    pub outcome_stage: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CorpusCandidate {
    pub role: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResolvedElement {
    pub role: String,
    pub name: String,
}

#[derive(Debug, Serialize)]
pub struct CorpusModelResponse {
    pub confidence: f32,
    pub action: serde_json::Value,
}

/// Appends corpus records to `<dir>/vision-corpus.jsonl`.
#[derive(Debug, Clone)]
pub struct VisionCorpus {
    path: PathBuf,
}

impl VisionCorpus {
    pub fn new(dir: &Path) -> std::io::Result<Self> {
        std::fs::create_dir_all(dir)?;
        Ok(Self {
            path: dir.join("vision-corpus.jsonl"),
        })
    }

    pub fn record(&self, record: &CorpusRecord) {
        let line = match serde_json::to_string(record) {
            Ok(line) => line,
            Err(error) => {
                tracing::warn!(%error, "vision.corpus_serialize_failed");
                return;
            }
        };
        if let Err(error) = append_line(&self.path, &line) {
            tracing::warn!(%error, path = %self.path.display(), "vision.corpus_write_failed");
        }
    }
}

fn append_line(path: &Path, line: &str) -> std::io::Result<()> {
    use std::io::Write as _;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    file.write_all(line.as_bytes())?;
    file.write_all(b"\n")?;
    Ok(())
}

/// Match a resolved (role, name) onto the candidates the model saw. Exact
/// name match, then case-insensitive; duplicates resolve to the first, and
/// the mismatch is visible via `resolved_element` in the record.
pub fn match_resolved(
    candidates: &[CorpusCandidate],
    resolved: &(String, String),
) -> Option<usize> {
    let (role, name) = resolved;
    if name.is_empty() {
        return None;
    }
    let role_matches =
        |candidate: &&CorpusCandidate| role.is_empty() || candidate.role.eq_ignore_ascii_case(role);
    candidates
        .iter()
        .position(|c| c.name.as_str() == name.as_str() && role_matches(&c))
        .or_else(|| {
            let lowered = name.to_lowercase();
            candidates
                .iter()
                .position(|c| c.name.to_lowercase() == lowered && role_matches(&c))
        })
        .or_else(|| {
            candidates
                .iter()
                .position(|c| c.name.as_str() == name.as_str())
        })
}

/// Serialize a vision action into the raw action shape the training format
/// expects candidate-grounded and legacy action tags.
pub fn raw_action(action: &VisionAction) -> serde_json::Value {
    match action {
        VisionAction::Click { x, y } => serde_json::json!({"kind": "click", "x": x, "y": y}),
        VisionAction::TypeText { text } => {
            serde_json::json!({"kind": "typeText", "text": text})
        }
        VisionAction::ExtractValue { value } => {
            serde_json::json!({"kind": "extractValue", "value": value})
        }
        VisionAction::ClickCandidate { index } => {
            serde_json::json!({"kind": "clickCandidate", "index": index})
        }
        VisionAction::TypeIntoCandidate { index } => {
            serde_json::json!({"kind": "typeIntoCandidate", "index": index})
        }
        VisionAction::ExtractFromCandidate { index } => {
            serde_json::json!({"kind": "extractFromCandidate", "index": index})
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate(role: &str, name: &str) -> CorpusCandidate {
        CorpusCandidate {
            role: role.into(),
            name: name.into(),
        }
    }

    #[test]
    fn exact_name_match_with_role() {
        let candidates = vec![candidate("button", "Save"), candidate("link", "Save")];
        let resolved = ("button".into(), "Save".into());
        assert_eq!(match_resolved(&candidates, &resolved), Some(0));
    }

    #[test]
    fn case_insensitive_name_match() {
        let candidates = vec![candidate("button", "Create Customer")];
        let resolved = ("button".into(), "create customer".into());
        assert_eq!(match_resolved(&candidates, &resolved), Some(0));
    }

    #[test]
    fn empty_resolved_role_falls_back_to_name_only() {
        let candidates = vec![candidate("combobox", "Plan")];
        let resolved = (String::new(), "Plan".into());
        assert_eq!(match_resolved(&candidates, &resolved), Some(0));
    }

    #[test]
    fn empty_resolved_name_is_no_match() {
        let candidates = vec![candidate("button", "Save")];
        let resolved = ("button".into(), String::new());
        assert_eq!(match_resolved(&candidates, &resolved), None);
    }

    #[test]
    fn wrong_role_falls_back_to_name_only_match() {
        let candidates = vec![candidate("button", "Save")];
        let resolved = ("link".into(), "Save".into());
        assert_eq!(match_resolved(&candidates, &resolved), Some(0));
    }

    #[test]
    fn no_match_returns_none() {
        let candidates = vec![candidate("button", "Save")];
        let resolved = ("button".into(), "Delete".into());
        assert_eq!(match_resolved(&candidates, &resolved), None);
    }
}
