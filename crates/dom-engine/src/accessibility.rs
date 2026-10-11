//! Accessibility names and candidate alignment shared by browser adapters.
use crate::{Candidate, CandidateState};

fn gathered_name_match(candidate: &Candidate, role: &str, name: &str) -> bool {
    candidate
        .role
        .as_deref()
        .is_some_and(|actual| actual.eq_ignore_ascii_case(role))
        && candidate.name.as_deref().map(str::trim) == Some(name.trim())
}

/// One gathered match and no ordinal already agrees with the accessibility
/// tree, so the tree probe can be skipped.
pub fn accessibility_alignment_unnecessary(
    raw: &[Candidate],
    role: &str,
    name: &str,
    ordinal: Option<usize>,
) -> bool {
    ordinal.is_none()
        && raw
            .iter()
            .filter(|candidate| gathered_name_match(candidate, role, name))
            .count()
            == 1
}

/// Keeps candidates the accessibility tree exposes for `role` and `name`,
/// and rewrites those candidates' names to the tree's. An empty exposure
/// leaves the gathered set alone. Gathered matches the tree does not expose
/// drop out; other gathered candidates stay.
pub fn apply_accessibility_alignment(
    raw: &mut Vec<Candidate>,
    role: &str,
    name: &str,
    exposed: &[String],
) {
    if exposed.is_empty() {
        return;
    }
    raw.retain(|candidate| {
        exposed.iter().any(|id| id == &candidate.id) || !gathered_name_match(candidate, role, name)
    });
    for candidate in raw
        .iter_mut()
        .filter(|candidate| exposed.iter().any(|id| id == &candidate.id))
    {
        candidate.role = Some(role.to_owned());
        candidate.name = Some(name.to_owned());
    }
}

pub fn accessibility_candidates(
    nodes: &[types::AccessibilityNode],
    id_prefix: &str,
) -> Vec<Candidate> {
    fn collect(
        nodes: &[types::AccessibilityNode],
        candidates: &mut Vec<Candidate>,
        id_prefix: &str,
    ) {
        for node in nodes {
            let mut text = Vec::new();
            if let Some(name) = node.name.as_deref().filter(|value| !value.is_empty()) {
                text.push(name);
            }
            if let Some(description) = node
                .description
                .as_deref()
                .filter(|value| !value.is_empty())
            {
                text.push(description);
            }
            if let Some(value) = node.value.as_deref().filter(|value| !value.is_empty()) {
                text.push(value);
            }
            let mut attributes = std::collections::BTreeMap::new();
            if let Some(input_type) = &node.input_type {
                attributes.insert("type".into(), input_type.clone());
            }
            if node.invalid == Some(true) {
                attributes.insert("aria-invalid".into(), "true".into());
            }
            candidates.push(Candidate {
                id: format!("{id_prefix}-{}", candidates.len()),
                css: None,
                tag: None,
                test_id: None,
                role: node.role.clone(),
                name: node.name.clone(),
                label: None,
                text: text.join(" "),
                attributes,
                state: CandidateState {
                    attached: true,
                    visible: true,
                    enabled: node.disabled != Some(true),
                },
                frame_path: Vec::new(),
            });
            collect(&node.children, candidates, id_prefix);
        }
    }

    let mut candidates = Vec::new();
    collect(nodes, &mut candidates, id_prefix);
    candidates
}
