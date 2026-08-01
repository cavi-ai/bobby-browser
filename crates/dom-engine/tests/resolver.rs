use std::collections::BTreeMap;

use dom_engine::{
    resolve_candidates, Candidate, CandidateState, ResolutionDecision, ResolutionPolicy,
};
use types::{TargetSpec, TextMatch};

fn candidate(id: &str, name: &str, visible: bool) -> Candidate {
    Candidate {
        id: id.into(),
        css: None,
        test_id: None,
        role: Some("button".into()),
        name: Some(name.into()),
        label: None,
        text: name.into(),
        attributes: BTreeMap::new(),
        state: CandidateState {
            attached: true,
            visible,
            enabled: true,
        },
    }
}

fn target(name: &str) -> TargetSpec {
    TargetSpec {
        role: Some("button".into()),
        accessible_name: Some(name.into()),
        ..TargetSpec::default()
    }
}

#[test]
fn exact_semantic_match_wins_over_hidden_decoy() {
    let candidates = vec![
        candidate("hidden", "Continue", false),
        candidate("real", "Continue", true),
    ];
    let decision = resolve_candidates(
        &target("Continue"),
        &candidates,
        &ResolutionPolicy::default(),
    )
    .unwrap();
    assert!(
        matches!(decision, ResolutionDecision::Resolved { candidate, .. } if candidate.id == "real")
    );
}

#[test]
fn equivalent_candidates_fail_closed_with_ranked_evidence() {
    let candidates = vec![
        candidate("first", "Continue", true),
        candidate("second", "Continue", true),
    ];
    let decision = resolve_candidates(
        &target("Continue"),
        &candidates,
        &ResolutionPolicy::default(),
    )
    .unwrap();
    assert!(
        matches!(decision, ResolutionDecision::Ambiguous { candidates } if candidates.len() == 2 && candidates[0].score == candidates[1].score)
    );
}

#[test]
fn explicit_ordinal_and_best_match_are_auditable() {
    let candidates = vec![
        candidate("first", "Continue", true),
        candidate("second", "Continue", true),
    ];
    let ordinal = TargetSpec {
        ordinal: Some(1),
        ..target("Continue")
    };
    assert!(
        matches!(resolve_candidates(&ordinal, &candidates, &ResolutionPolicy::default()).unwrap(), ResolutionDecision::Resolved { candidate, best_match_authorized: false, .. } if candidate.id == "second")
    );

    let best = TargetSpec {
        allow_best_match: true,
        ..target("Continue")
    };
    assert!(matches!(
        resolve_candidates(&best, &candidates, &ResolutionPolicy::default()).unwrap(),
        ResolutionDecision::Resolved {
            best_match_authorized: true,
            ..
        }
    ));
}

#[test]
fn ordinal_uses_candidate_collection_order_instead_of_lexical_internal_ids() {
    let candidates = vec![
        candidate("control-2", "Phone", true),
        candidate("control-10", "Phone", true),
    ];
    let second = TargetSpec {
        ordinal: Some(1),
        ..target("Phone")
    };

    assert!(
        matches!(resolve_candidates(&second, &candidates, &ResolutionPolicy::default()).unwrap(), ResolutionDecision::Resolved { candidate, .. } if candidate.id == "control-10")
    );
}

#[test]
fn text_and_attribute_constraints_filter_candidates() {
    let mut matching = candidate("match", "Save", true);
    matching.text = "Save changes".into();
    matching
        .attributes
        .insert("data-scope".into(), "profile".into());
    let other = candidate("other", "Save", true);
    let target = TargetSpec {
        text: Some(TextMatch::Contains("changes".into())),
        attributes: BTreeMap::from([("data-scope".into(), "profile".into())]),
        ..TargetSpec::default()
    };
    assert!(
        matches!(resolve_candidates(&target, &[other, matching], &ResolutionPolicy::default()).unwrap(), ResolutionDecision::Resolved { candidate, .. } if candidate.id == "match")
    );
}
