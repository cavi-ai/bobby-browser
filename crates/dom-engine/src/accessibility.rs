//! Accessibility names and candidate alignment shared by browser adapters.
use crate::{Candidate, CandidateState};

fn gathered_name_match(candidate: &Candidate, role: &str, name: &str) -> bool {
    candidate
        .role
        .as_deref()
        .is_some_and(|actual| actual.eq_ignore_ascii_case(role))
        && candidate.name.as_deref().map(str::trim) == Some(name.trim())
}

/// A single gathered role/name match needs no accessibility probe unless an
/// ordinal is requested.
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

/// Builds resolver candidates in accessibility-tree traversal order.
/// The adapter supplies the prefix for its candidate identities.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{resolve_candidates, ResolutionDecision, ResolutionPolicy};
    use types::TargetSpec;

    fn candidate(id: &str, role: &str, name: &str) -> Candidate {
        Candidate {
            id: id.into(),
            role: Some(role.into()),
            name: Some(name.into()),
            css: None,
            tag: None,
            test_id: None,
            label: None,
            text: String::new(),
            attributes: Default::default(),
            frame_path: Vec::new(),
            state: CandidateState {
                attached: true,
                visible: true,
                enabled: true,
            },
        }
    }

    #[test]
    fn alignment_rewrites_exposed_names_and_drops_covered_matches() {
        let mut candidates = vec![
            candidate("slotted", "generic", "label text"),
            candidate("covered", "BUTTON", " Save "),
            candidate("other", "link", "Docs"),
        ];
        apply_accessibility_alignment(&mut candidates, "button", "Save", &["slotted".into()]);
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.id.as_str())
                .collect::<Vec<_>>(),
            ["slotted", "other"]
        );
        let target = TargetSpec {
            role: Some("button".into()),
            accessible_name: Some("Save".into()),
            ..Default::default()
        };
        let ResolutionDecision::Resolved { candidate, .. } =
            resolve_candidates(&target, &candidates, &ResolutionPolicy::default()).unwrap()
        else {
            panic!("exposed candidate must resolve");
        };
        assert_eq!(candidate.id, "slotted");
    }

    #[test]
    fn one_gathered_match_skips_the_accessibility_probe_except_for_ordinals() {
        let candidates = [
            candidate("only", "button", "Save"),
            candidate("other", "link", "Docs"),
        ];
        assert!(accessibility_alignment_unnecessary(
            &candidates,
            "button",
            "Save",
            None
        ));
        assert!(!accessibility_alignment_unnecessary(
            &candidates,
            "button",
            "Save",
            Some(0)
        ));
        assert!(!accessibility_alignment_unnecessary(
            &[],
            "button",
            "Save",
            None
        ));
        let duplicates = [
            candidate("first", "button", "Save"),
            candidate("second", "button", "Save"),
        ];
        assert!(!accessibility_alignment_unnecessary(
            &duplicates,
            "button",
            "Save",
            None
        ));
    }

    #[test]
    fn empty_accessibility_exposure_keeps_gathered_candidates() {
        let mut candidates = vec![candidate("a", "button", "Save")];
        let original = candidates.clone();
        apply_accessibility_alignment(&mut candidates, "button", "Save", &[]);
        assert_eq!(candidates, original);
    }

    #[test]
    fn alignment_preserves_ordinal_order_and_candidate_metadata() {
        let first = candidate("first", "generic", "gathered label");
        let mut second = candidate("second", "generic", "gathered label");
        second.css = Some("#second".into());
        second.attributes.insert("type".into(), "file".into());
        second.frame_path.push(Box::new(TargetSpec {
            css: Some("#frame".into()),
            ..Default::default()
        }));
        let mut candidates = vec![
            first,
            candidate("covered", "button", "Save"),
            second.clone(),
        ];
        // Exposure order does not replace the gathered traversal order.
        apply_accessibility_alignment(
            &mut candidates,
            "button",
            "Save",
            &["second".into(), "first".into()],
        );
        let target = TargetSpec {
            role: Some("button".into()),
            accessible_name: Some("Save".into()),
            ordinal: Some(1),
            ..Default::default()
        };
        let ResolutionDecision::Resolved { candidate, .. } =
            resolve_candidates(&target, &candidates, &ResolutionPolicy::default()).unwrap()
        else {
            panic!("second ordinal must resolve");
        };
        second.role = Some("button".into());
        second.name = Some("Save".into());
        assert_eq!(*candidate, second);
    }

    #[test]
    fn accessibility_tree_preserves_order_names_and_control_metadata() {
        let nodes: Vec<types::AccessibilityNode> = serde_json::from_value(serde_json::json!([
            {"role":"main","children":[
                {"role":"button","name":"Resume","inputType":"file","disabled":true},
                {"role":"textbox","name":"Postal code","description":"Required","value":"12345","invalid":true}
            ]}
        ])).unwrap();
        let candidates = accessibility_candidates(&nodes, "firefox-a11y");
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.id.as_str())
                .collect::<Vec<_>>(),
            ["firefox-a11y-0", "firefox-a11y-1", "firefox-a11y-2"]
        );
        assert_eq!(
            candidates[1].attributes.get("type").map(String::as_str),
            Some("file")
        );
        assert!(!candidates[1].state.enabled);
        assert_eq!(
            candidates[2]
                .attributes
                .get("aria-invalid")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(candidates[2].text, "Postal code Required 12345");
        assert!(candidates[2].state.attached && candidates[2].state.visible);
    }
}
