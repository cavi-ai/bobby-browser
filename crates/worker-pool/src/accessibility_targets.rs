use std::collections::BTreeMap;

pub fn accessibility_target_totals(
    nodes: &[types::AccessibilityNode],
) -> BTreeMap<(String, String), usize> {
    let mut totals = BTreeMap::new();
    count_accessibility_targets(nodes, &mut totals);
    totals
}

pub(crate) fn accessibility_role_is_actionable(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "checkbox"
            | "combobox"
            | "link"
            | "listbox"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "option"
            | "radio"
            | "searchbox"
            | "slider"
            | "spinbutton"
            | "switch"
            | "tab"
            | "textbox"
            | "treeitem"
    )
}

pub(crate) fn count_accessibility_targets(
    nodes: &[types::AccessibilityNode],
    totals: &mut BTreeMap<(String, String), usize>,
) {
    for node in nodes {
        if node
            .target
            .as_ref()
            .is_some_and(|target| !target.frame_path.is_empty())
        {
            continue;
        }
        if let (Some(role), Some(name)) = (&node.role, &node.name) {
            if accessibility_role_is_actionable(role) && !name.is_empty() && name != "[redacted]" {
                *totals.entry((role.clone(), name.clone())).or_default() += 1;
            }
        }
        count_accessibility_targets(&node.children, totals);
    }
}

/// `preceding` counts the targets of each role and name that come before
/// `nodes` in the page, so ordinals stay page-wide in a scoped tree.
pub fn annotate_accessibility_targets(
    nodes: &mut [types::AccessibilityNode],
    totals: &BTreeMap<(String, String), usize>,
    preceding: BTreeMap<(String, String), usize>,
) {
    fn annotate(
        nodes: &mut [types::AccessibilityNode],
        totals: &BTreeMap<(String, String), usize>,
        seen: &mut BTreeMap<(String, String), usize>,
    ) {
        for node in nodes {
            if node
                .target
                .as_ref()
                .is_some_and(|target| !target.frame_path.is_empty())
            {
                continue;
            }
            if let (Some(role), Some(name)) = (&node.role, &node.name) {
                let key = (role.clone(), name.clone());
                if accessibility_role_is_actionable(role)
                    && !name.is_empty()
                    && name != "[redacted]"
                {
                    let index = seen.entry(key.clone()).or_default();
                    if node.target.is_none() {
                        node.target = Some(types::AccessibilityTarget {
                            role: role.clone(),
                            accessible_name: name.clone(),
                            ordinal: (totals.get(&key).copied().unwrap_or_default() > 1)
                                .then_some(*index),
                            frame_path: Vec::new(),
                        });
                    }
                    *index += 1;
                }
            }
            annotate(&mut node.children, totals, seen);
        }
    }

    let mut seen = preceding;
    annotate(nodes, totals, &mut seen);
}

/// Page-wide counts for one role/name pair represented in a snapshot.
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AccessibilityTargetCount {
    pub role: String,
    pub name: String,
    pub total: u32,
    pub preceding: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_targets_do_not_shift_parent_document_ordinals() {
        let node = || types::AccessibilityNode {
            role: Some("button".into()),
            name: Some("Save".into()),
            ..Default::default()
        };
        let mut framed = node();
        framed.target = Some(types::AccessibilityTarget {
            role: "button".into(),
            accessible_name: "Save".into(),
            ordinal: Some(0),
            frame_path: vec![types::SemanticTargetSegment {
                role: "iframe".into(),
                accessible_name: "Frame".into(),
                ordinal: None,
            }],
        });
        let mut nodes = vec![node(), framed, node()];
        let totals = BTreeMap::from([(("button".into(), "Save".into()), 2)]);
        annotate_accessibility_targets(&mut nodes, &totals, BTreeMap::new());
        assert_eq!(nodes[2].target.as_ref().unwrap().ordinal, Some(1));
        assert_eq!(nodes[1].target.as_ref().unwrap().frame_path.len(), 1);
    }
}
