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
        frame_path: Vec::new(),
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
fn candidate_limit_error_reports_count_limit_matches_and_repair() {
    let candidates = (0..150)
        .map(|index| candidate(&format!("candidate-{index}"), "Same", true))
        .collect::<Vec<_>>();
    let error =
        resolve_candidates(&target("Same"), &candidates, &ResolutionPolicy::default()).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("150 candidates"), "{message}");
    assert!(message.contains("limit 100"), "{message}");
    assert!(
        message.contains("candidate-0") && message.contains("candidate-9"),
        "{message}"
    );
    assert!(!message.contains("candidate-10,"), "{message}");
    assert!(message.contains("Narrow the target"), "{message}");
}

#[test]
fn explicit_ordinal_is_exempt_from_the_candidate_limit() {
    let candidates = (0..150)
        .map(|index| candidate(&format!("row-{index}"), "Same", true))
        .collect::<Vec<_>>();
    let mut spec = target("Same");
    spec.ordinal = Some(120);
    let decision = resolve_candidates(&spec, &candidates, &ResolutionPolicy::default()).unwrap();
    assert!(
        matches!(decision, ResolutionDecision::Resolved { candidate, .. } if candidate.id == "row-120")
    );
}

#[test]
fn candidate_limit_applies_only_to_matching_candidates() {
    let mut candidates = (0..500)
        .map(|index| {
            let mut decoy = candidate(&format!("decoy-{index}"), &format!("Decoy {index}"), true);
            decoy.role = Some("generic".into());
            decoy
        })
        .collect::<Vec<_>>();
    candidates.push(candidate("go-now", "Go now", true));
    let decision =
        resolve_candidates(&target("Go now"), &candidates, &ResolutionPolicy::default()).unwrap();
    assert!(
        matches!(decision, ResolutionDecision::Resolved { candidate, .. } if candidate.id == "go-now")
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

/// The a11y snapshot emits the engine's role casing (Chrome's `Iframe`)
/// while DOM candidates carry the lowercase implicit role; a snapshot
/// target passed back verbatim must still resolve.
#[test]
fn role_matching_is_case_insensitive() {
    let mut frame = candidate("frame", "Preview", true);
    frame.role = Some("iframe".into());
    let target = TargetSpec {
        role: Some("Iframe".into()),
        accessible_name: Some("Preview".into()),
        ..TargetSpec::default()
    };
    assert!(
        matches!(resolve_candidates(&target, &[frame], &ResolutionPolicy::default()).unwrap(), ResolutionDecision::Resolved { candidate, .. } if candidate.id == "frame")
    );
}

/// `<label>Name <input></label>` produces an AX name with trailing
/// whitespace while the DOM collector trims label innerText; a snapshot
/// target passed back verbatim must still resolve, in either direction.
#[test]
fn accessible_name_matching_trims_surrounding_whitespace() {
    let trimmed = candidate("field", "Name", true);
    let target = TargetSpec {
        accessible_name: Some("Name ".into()),
        ..TargetSpec::default()
    };
    assert!(
        matches!(resolve_candidates(&target, &[trimmed], &ResolutionPolicy::default()).unwrap(), ResolutionDecision::Resolved { candidate, .. } if candidate.id == "field")
    );

    let untrimmed = candidate("field2", "Name ", true);
    let target = TargetSpec {
        accessible_name: Some("Name".into()),
        ..TargetSpec::default()
    };
    assert!(
        matches!(resolve_candidates(&target, &[untrimmed], &ResolutionPolicy::default()).unwrap(), ResolutionDecision::Resolved { candidate, .. } if candidate.id == "field2")
    );
}

/// Chrome's a11y tree moved from `img` to `image`; the DOM collector's
/// implicit-role mapping still emits `img` for an `<img>` element. A target
/// role of `image` (as returned by an a11y snapshot) must still resolve
/// against a candidate carrying the collector's `img` role.
#[test]
fn image_role_matches_img_candidate_role() {
    let mut logo = candidate("logo", "Logo", true);
    logo.role = Some("img".into());
    let target = TargetSpec {
        role: Some("image".into()),
        accessible_name: Some("Logo".into()),
        ..TargetSpec::default()
    };
    assert!(
        matches!(resolve_candidates(&target, &[logo], &ResolutionPolicy::default()).unwrap(), ResolutionDecision::Resolved { candidate, .. } if candidate.id == "logo")
    );
}
