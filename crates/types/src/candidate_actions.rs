use std::sync::OnceLock;

use serde::Deserialize;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CandidateActionSpec {
    kind: String,
    acp_kind: String,
    #[serde(rename = "legacyKind")]
    _legacy_kind: String,
    legacy_acp_kind: String,
    intents: Vec<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CandidateActionContract {
    actions: Vec<CandidateActionSpec>,
}

fn candidate_action_specs() -> &'static [CandidateActionSpec] {
    static CONTRACT: OnceLock<CandidateActionContract> = OnceLock::new();
    &CONTRACT
        .get_or_init(|| {
            serde_json::from_str(include_str!(
                "../../../scripts/vision-mlx/candidate_action_contract.json"
            ))
            .expect("candidate action contract must be valid")
        })
        .actions
}

pub fn candidate_action_acp_kinds_for_intent(
    intent_kind: &str,
) -> Option<(&'static str, &'static str)> {
    candidate_action_specs()
        .iter()
        .find(|spec| spec.intents.iter().any(|intent| intent == intent_kind))
        .map(|spec| (spec.acp_kind.as_str(), spec.legacy_acp_kind.as_str()))
}

pub fn candidate_action_is_compatible(action_kind: &str, intent_kind: &str) -> bool {
    candidate_action_specs().iter().any(|spec| {
        spec.kind == action_kind && spec.intents.iter().any(|intent| intent == intent_kind)
    })
}

pub fn candidate_action_prompt_rules() -> String {
    candidate_action_specs()
        .iter()
        .map(|spec| format!("{} for {}", spec.kind, spec.intents.join("/")))
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_intent_maps_to_one_candidate_action() {
        for spec in candidate_action_specs() {
            for intent in &spec.intents {
                assert_eq!(
                    candidate_action_specs()
                        .iter()
                        .filter(|candidate| candidate.intents.contains(intent))
                        .count(),
                    1
                );
                assert!(candidate_action_is_compatible(&spec.kind, intent));
            }
        }
        assert!(candidate_action_acp_kinds_for_intent("unknown").is_none());
    }
}
