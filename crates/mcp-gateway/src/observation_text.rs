//! Agent-facing shaping of accessibility snapshots: each piece of page text
//! is carried once. Targets stay verbatim; only repeated copies are dropped.

use serde_json::{Map, Value};

/// Roles an engine may name from their descendants' text. Such a name is
/// dropped when the descendants already carry all of it.
const NAME_FROM_CONTENT_ROLES: &[&str] = &[
    "cell",
    "columnheader",
    "gridcell",
    "heading",
    "listitem",
    "menuitem",
    "menuitemcheckbox",
    "menuitemradio",
    "option",
    "row",
    "rowheader",
    "tab",
    "tooltip",
    "treeitem",
];

/// Rewrites every accessibility snapshot evidence item in a tool result, at
/// any depth (a `postState` nests one), so each text appears once:
/// - a node's `name` is dropped when its `target` carries the same role and name;
/// - `StaticText` children are dropped when together they repeat their parent's text;
/// - a content-named node's `name` is dropped when its descendants carry that text;
/// - a bullet-glyph list marker is dropped.
pub(crate) fn dedupe_snapshot_text(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, child) in map.iter_mut() {
                match (key.as_str(), child) {
                    ("evidence", Value::Array(items)) => {
                        for item in items {
                            if item.get("kind").and_then(Value::as_str)
                                == Some("accessibilitySnapshot")
                            {
                                if let Some(Value::Array(nodes)) = item.get_mut("nodes") {
                                    for node in nodes {
                                        dedupe_node(node);
                                    }
                                }
                            } else {
                                dedupe_snapshot_text(item);
                            }
                        }
                    }
                    (_, child) => dedupe_snapshot_text(child),
                }
            }
        }
        Value::Array(items) => {
            for item in items {
                dedupe_snapshot_text(item);
            }
        }
        _ => {}
    }
}

/// Dedupes `node`'s subtree and returns the text it still carries, with
/// whitespace removed, for its ancestors' coverage checks.
fn dedupe_node(node: &mut Value) -> String {
    let Some(map) = node.as_object_mut() else {
        return String::new();
    };
    let mut descendants = String::new();
    let mut text_leaves_only = true;
    if let Some(Value::Array(children)) = map.get_mut("children") {
        children.retain(|child| !is_bullet_marker(child));
        for child in children.iter_mut() {
            text_leaves_only &= is_text_leaf(child);
            descendants.push_str(&dedupe_node(child));
        }
        if children.is_empty() {
            map.remove("children");
        }
    }
    let target_name = target_name(map).map(str::to_owned);
    if target_name.is_some() && map.get("name").and_then(Value::as_str) == target_name.as_deref() {
        map.remove("name");
    }
    let own = map
        .get("name")
        .and_then(Value::as_str)
        .or(target_name.as_deref())
        .map(without_whitespace)
        .unwrap_or_default();
    if own.is_empty() || descendants.is_empty() {
        return own + &descendants;
    }
    if text_leaves_only && descendants == own {
        map.remove("children");
        return own;
    }
    let content_named = map
        .get("role")
        .and_then(Value::as_str)
        .is_some_and(|role| NAME_FROM_CONTENT_ROLES.contains(&role));
    if target_name.is_none() && content_named && descendants.contains(&own) {
        map.remove("name");
        return descendants;
    }
    own + &descendants
}

/// The target's `accessibleName` when the target names this node's own role.
fn target_name(map: &Map<String, Value>) -> Option<&str> {
    let target = map.get("target")?;
    (target.get("role") == map.get("role"))
        .then(|| target.get("accessibleName").and_then(Value::as_str))
        .flatten()
}

/// A list marker that is only a bullet glyph repeats its item's role; a
/// numbered marker carries text and stays.
fn is_bullet_marker(node: &Value) -> bool {
    node.as_object().is_some_and(|map| {
        map.get("role").and_then(Value::as_str) == Some("ListMarker")
            && map.keys().all(|key| key == "role" || key == "name")
            && !map
                .get("name")
                .and_then(Value::as_str)
                .is_some_and(|name| name.chars().any(char::is_alphanumeric))
    })
}

fn is_text_leaf(node: &Value) -> bool {
    node.as_object().is_some_and(|map| {
        map.get("role").and_then(Value::as_str) == Some("StaticText")
            && map.keys().all(|key| key == "role" || key == "name")
    })
}

fn without_whitespace(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_whitespace())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::dedupe_snapshot_text;
    use serde_json::{json, Value};

    fn snapshot(nodes: Value) -> Value {
        json!({"evidence":[{"kind":"accessibilitySnapshot","pageId":"p","nodes":nodes,"truncated":false}]})
    }

    fn deduped(nodes: Value) -> Value {
        let mut value = snapshot(nodes);
        dedupe_snapshot_text(&mut value);
        value["evidence"][0]["nodes"].clone()
    }

    #[test]
    fn nested_list_items_keep_one_copy_of_their_link_text() {
        let link = "Entry 01: a long link text";
        let item = "Entry 01: a long link text Save";
        let nodes = json!([{"role":"listitem","name":item,"children":[
            {"role":"listitem","name":item,"children":[
                {"role":"link","name":link,"target":{"role":"link","accessibleName":link}},
                {"role":"button","name":"Save","target":{"role":"button","accessibleName":"Save","ordinal":1}}
            ]}
        ]}]);
        assert_eq!(
            deduped(nodes),
            json!([{"role":"listitem","children":[
                {"role":"listitem","children":[
                    {"role":"link","target":{"role":"link","accessibleName":link}},
                    {"role":"button","target":{"role":"button","accessibleName":"Save","ordinal":1}}
                ]}
            ]}])
        );
    }

    #[test]
    fn a_truncated_container_name_is_dropped_when_its_descendants_carry_it() {
        let nodes = json!([{"role":"listitem","name":"Entry 01: a long","children":[
            {"role":"link","name":"Entry 01: a long link text","target":{"role":"link","accessibleName":"Entry 01: a long link text"}}
        ]}]);
        assert!(deduped(nodes)[0].get("name").is_none());
    }

    #[test]
    fn text_leaves_repeating_their_parent_are_dropped() {
        let nodes = json!([
            {"role":"link","name":"Home page","target":{"role":"link","accessibleName":"Home page"},"children":[
                {"role":"StaticText","name":"Home"},{"role":"StaticText","name":" page"}
            ]},
            {"role":"heading","name":"Recent entries","children":[{"role":"StaticText","name":"Recent entries"}]}
        ]);
        assert_eq!(
            deduped(nodes),
            json!([
                {"role":"link","target":{"role":"link","accessibleName":"Home page"}},
                {"role":"heading","name":"Recent entries"}
            ])
        );
    }

    #[test]
    fn text_the_descendants_do_not_carry_stays() {
        let nodes = json!([
            {"role":"listitem","name":"Feed post","children":[{"role":"StaticText","name":"Post 0"}]},
            {"role":"listitem","name":"Posted by Ada: Entry","children":[
                {"role":"link","name":"Entry","target":{"role":"link","accessibleName":"Entry"}}
            ]},
            {"role":"navigation","name":"Home","children":[
                {"role":"link","name":"Home","target":{"role":"link","accessibleName":"Home"}}
            ]},
            {"role":"button","name":"Close dialog","target":{"role":"button","accessibleName":"Close dialog"},"children":[
                {"role":"StaticText","name":"X"}
            ]},
            {"role":"textbox","name":"[redacted]","value":"[redacted]"}
        ]);
        let kept = deduped(nodes);
        assert_eq!(kept[0]["name"], "Feed post");
        assert_eq!(kept[0]["children"][0]["name"], "Post 0");
        assert_eq!(kept[1]["name"], "Posted by Ada: Entry");
        assert_eq!(kept[2]["name"], "Home");
        assert_eq!(kept[3]["children"][0]["name"], "X");
        assert_eq!(kept[4]["name"], "[redacted]");
    }

    #[test]
    fn bullet_markers_are_dropped_and_numbered_markers_stay() {
        let nodes = json!([
            {"role":"listitem","children":[{"role":"ListMarker","name":"•"},{"role":"StaticText","name":"First"}]},
            {"role":"listitem","children":[{"role":"ListMarker","name":"◦"}]},
            {"role":"listitem","children":[{"role":"ListMarker","name":"2. "},{"role":"StaticText","name":"Second"}]}
        ]);
        assert_eq!(
            deduped(nodes),
            json!([
                {"role":"listitem","children":[{"role":"StaticText","name":"First"}]},
                {"role":"listitem"},
                {"role":"listitem","children":[{"role":"ListMarker","name":"2. "},{"role":"StaticText","name":"Second"}]}
            ])
        );
    }

    #[test]
    fn a_name_differing_from_its_target_stays() {
        let nodes = json!([{"role":"link","name":"Shown","target":{"role":"iframe","accessibleName":"Shown"}}]);
        assert_eq!(deduped(nodes.clone()), nodes);
    }

    #[test]
    fn a_post_state_snapshot_is_deduped() {
        let link =
            json!({"role":"link","name":"Entry","target":{"role":"link","accessibleName":"Entry"}});
        let mut value = json!({"status":"completed","postState":{"observationOutcome":snapshot(json!([link]))}});
        dedupe_snapshot_text(&mut value);
        assert!(
            value["postState"]["observationOutcome"]["evidence"][0]["nodes"][0]
                .get("name")
                .is_none()
        );
    }
}
