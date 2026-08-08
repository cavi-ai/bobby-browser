//! Vision-corpus collection from scripted gauntlet journeys.
//!
//! Each journey step already knows its correct target; the collector captures
//! the page context (accessibility candidates, URL, screenshot) immediately
//! before the scripted action runs and records the known-correct action as
//! ground truth in the candidate-index training format.

use std::path::{Path, PathBuf};

use base64::Engine as _;
use serde_json::{json, Value};
use types::{
    AccessibilitySnapshotCommand, CaptureScreenshotCommand, Evidence, InspectCommand,
    PrimitiveCommand, ScreenshotMode,
};

use super::driver::{ModernRuntime, TestResult};

const INTERACTIVE_ROLES: [&str; 10] = [
    "button", "link", "textbox", "combobox", "checkbox", "radio", "tab", "menuitem",
    "searchbox", "switch",
];

#[derive(Debug, Clone)]
pub enum GroundTruth {
    Click { selector: &'static str, purpose: String },
    TypeText { selector: &'static str, text: &'static str, purpose: String },
}

impl GroundTruth {
    fn selector(&self) -> &'static str {
        match self {
            Self::Click { selector, .. } => selector,
            Self::TypeText { selector, .. } => selector,
        }
    }

    fn purpose(&self) -> &str {
        match self {
            Self::Click { purpose, .. } => purpose,
            Self::TypeText { purpose, .. } => purpose,
        }
    }
}

#[derive(Debug, Clone)]
struct CandidateRow {
    role: String,
    name: String,
}

#[derive(Default)]
pub struct CorpusCollector {
    records: Vec<Value>,
}

impl CorpusCollector {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    /// Capture one training example immediately before the scripted action
    /// described by `truth` executes.
    pub async fn capture(
        &mut self,
        runtime: &ModernRuntime,
        truth: &GroundTruth,
        journey: &str,
        step: &str,
    ) -> TestResult<()> {
        let snapshot = runtime
            .submit(PrimitiveCommand::AccessibilitySnapshot(
                AccessibilitySnapshotCommand {
                    max_nodes: Some(1024),
                },
            ))
            .await?;
        let candidates = extract_candidates(&snapshot)?;

        let inspection = runtime
            .submit(PrimitiveCommand::Inspect(InspectCommand {
                selector: Some(truth.selector().into()),
                target: None,
                include_html: true,
            }))
            .await?;
        let url = inspection.iter().find_map(|item| {
            if let Evidence::Inspection { url, .. } = item {
                Some(url.clone())
            } else {
                None
            }
        });
        let reference = inspection_reference(&inspection, truth.selector());
        let target_index = find_candidate(&candidates, &reference).ok_or_else(|| {
            format!(
                "collector could not map selector {:?} (reference {reference:?}) onto {} candidates at step {step}",
                truth.selector(),
                candidates.len(),
            )
        })?;

        let image_b64 = capture_screenshot_b64(runtime).await?;

        // Raw action kinds + target_index: the corpus is schema-agnostic.
        // `build_completion` converts to clickCandidate/typeIntoCandidate at
        // training time. There is no pixel ground truth (no bbox source in
        // the runtime evidence), so x,y are omitted by design.
        let action = match truth {
            GroundTruth::Click { .. } => json!({"kind": "click"}),
            GroundTruth::TypeText { text, .. } => json!({"kind": "typeText", "text": text}),
        };

        self.records.push(json!({
            "image_b64": image_b64,
            "purpose": truth.purpose(),
            "intent_kind": "locate",
            "stuck": "targetMissing",
            "context_url": url,
            "context_candidates": candidates
                .iter()
                .map(|c| json!({"role": c.role, "name": c.name}))
                .collect::<Vec<_>>(),
            "target_index": target_index,
            "model_response": {"confidence": 1.0, "action": action},
            "success": true,
            "journey": journey,
            "step": step,
        }));
        Ok(())
    }

    pub fn save(&self, path: &Path) -> TestResult<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = String::new();
        for record in &self.records {
            out.push_str(&serde_json::to_string(record)?);
            out.push('\n');
        }
        std::fs::write(path, out)?;
        Ok(())
    }
}

fn extract_candidates(evidence: &[Evidence]) -> TestResult<Vec<CandidateRow>> {
    for item in evidence {
        if let Evidence::AccessibilitySnapshot { nodes, .. } = item {
            let mut candidates = Vec::new();
            collect_interactive(nodes, &mut candidates);
            return Ok(candidates);
        }
    }
    Err("accessibility snapshot evidence missing".into())
}

fn collect_interactive(nodes: &[types::AccessibilityNode], out: &mut Vec<CandidateRow>) {
    for node in nodes {
        let interactive = node
            .role
            .as_deref()
            .is_some_and(|role| INTERACTIVE_ROLES.contains(&role))
            && node.name.as_deref().is_some_and(|name| !name.is_empty());
        if interactive {
            out.push(CandidateRow {
                role: node.role.clone().unwrap_or_default(),
                name: node.name.clone().unwrap_or_default(),
            });
        }
        collect_interactive(&node.children, out);
    }
}

/// What we know about the scripted target from its Inspect evidence. The
/// element's own aria-label wins, then its text; the selector's aria-label is
/// only a fallback because it may belong to an ancestor (e.g. the form's
/// label when the selector drills to a submit button).
fn inspection_reference(evidence: &[Evidence], selector: &str) -> String {
    for item in evidence {
        if let Evidence::Inspection { text, html, .. } = item {
            if let Some(html) = html {
                if let Some(label) = aria_label_from_html(html) {
                    return label;
                }
            }
            let trimmed = text.trim();
            if !trimmed.is_empty() {
                return trimmed.chars().take(120).collect();
            }
        }
    }
    aria_label_from_selector(selector).unwrap_or_default()
}

fn aria_label_from_selector(selector: &str) -> Option<String> {
    let marker = "aria-label='";
    let start = selector.find(marker)? + marker.len();
    let end = selector[start..].find('\'')? + start;
    Some(selector[start..end].to_string())
}

fn aria_label_from_html(html: &str) -> Option<String> {
    let marker = "aria-label=\"";
    let start = html.find(marker)? + marker.len();
    let end = html[start..].find('"')? + start;
    Some(html[start..end].to_string())
}

fn find_candidate(candidates: &[CandidateRow], reference: &str) -> Option<usize> {
    if reference.is_empty() {
        return None;
    }
    if let Some(index) = candidates.iter().position(|c| c.name == reference) {
        return Some(index);
    }
    let lowered = reference.to_lowercase();
    candidates
        .iter()
        .position(|c| c.name.to_lowercase() == lowered)
        .or_else(|| candidates.iter().position(|c| c.name.starts_with(reference)))
}

async fn capture_screenshot_b64(runtime: &ModernRuntime) -> TestResult<String> {
    let evidence = runtime
        .submit(PrimitiveCommand::CaptureScreenshot(CaptureScreenshotCommand {
            mode: ScreenshotMode::Viewport,
        }))
        .await?;
    for item in &evidence {
        if let Evidence::Screenshot { artifact_id, .. } = item {
            let path = find_artifact_file(runtime.artifacts_dir(), artifact_id, "png")
                .ok_or_else(|| format!("screenshot artifact {artifact_id} not on disk"))?;
            let bytes = std::fs::read(path)?;
            return Ok(base64::engine::general_purpose::STANDARD.encode(bytes));
        }
    }
    Err("screenshot evidence missing".into())
}

fn find_artifact_file(root: &Path, artifact_id: &str, extension: &str) -> Option<PathBuf> {
    let wanted = format!("{artifact_id}.{extension}");
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(dir).ok()? {
            let entry = entry.ok()?;
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if entry.file_name().to_string_lossy() == wanted {
                return Some(path);
            }
        }
    }
    None
}
