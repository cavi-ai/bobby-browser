use std::collections::BTreeMap;
use std::sync::Arc;

use context_store::{ContextStore, RecordSource};
use page_runtime::ContextPromotion;
use types::{
    CandidateEvidence, Evidence, ExecutionRecord, IntentResolutionPath, PageId, TargetFingerprint,
    TargetSpec,
};

async fn promotion() -> (ContextPromotion, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let (store, report) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    assert!(report.skipped.is_empty());
    (ContextPromotion::new(store), temp)
}

fn record(kind: &str, path: IntentResolutionPath) -> ExecutionRecord {
    ExecutionRecord {
        intent_kind: kind.to_string(),
        purpose: Some("fill the email field".to_string()),
        resolution_path: path,
        plan_summary: "resolve then fill".to_string(),
        candidates: vec![CandidateEvidence {
            role: Some("textbox".to_string()),
            name: Some("Email address".to_string()),
            score: 92,
            reasons: vec!["accessible name match".to_string()],
        }],
        wait_elapsed_ms: None,
        verification: "resolved".to_string(),
        artifact_ids: Vec::new(),
        vision_proposal_sha256: None,
    }
}

fn resolution(name: &str, ordinal: Option<usize>) -> Evidence {
    Evidence::Resolution {
        target: Box::new(TargetSpec {
            role: Some("textbox".to_string()),
            accessible_name: Some(name.to_string()),
            ordinal,
            ..TargetSpec::default()
        }),
        fingerprint: Box::new(TargetFingerprint {
            page_id: PageId::new(),
            frame: None,
            role: Some("textbox".to_string()),
            name: Some(name.to_string()),
            stable_attributes: BTreeMap::new(),
        }),
        candidates: Vec::new(),
        best_match_authorized: false,
    }
}

const URL: &str = "https://app.example.test/login";

#[tokio::test]
async fn verified_success_promotes_the_resolved_control() {
    let (promotion, temp) = promotion().await;
    let evidence = vec![
        resolution("Email address", Some(1)),
        Evidence::IntentExecution {
            record: record("fill", IntentResolutionPath::Deterministic),
        },
    ];
    promotion.record_outcome(Some(URL), &evidence, true).await;
    promotion.flush().await;
    drop(promotion);

    let (store, report) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    assert_eq!(report.sites_loaded, 1);
    let site = store.site("https://example.test").await.unwrap();
    let page = site.pages.get("/login").unwrap();
    let control = &page.forms.get("page").unwrap().controls[0];
    assert_eq!(control.role, "textbox");
    assert_eq!(control.accessible_name, "Email address");
    assert_eq!(control.ordinal, Some(1));
    let stats = control.intents.get("fill").unwrap();
    assert_eq!(stats.success_count, 1);
    assert_eq!(stats.failure_count, 0);
    assert_eq!(stats.source, Some(RecordSource::Observed));
    assert!(stats.last_verified_day.is_some());
}

#[tokio::test]
async fn repeated_outcomes_accumulate_counters() {
    let (promotion, _temp) = promotion().await;
    let evidence = vec![
        resolution("Email address", Some(1)),
        Evidence::IntentExecution {
            record: record("fill", IntentResolutionPath::Deterministic),
        },
    ];
    for _ in 0..3 {
        promotion.record_outcome(Some(URL), &evidence, true).await;
    }
    let stats = promotion
        .store()
        .site("https://example.test")
        .await
        .unwrap()
        .pages["/login"]
        .forms["page"]
        .controls[0]
        .intents["fill"]
        .clone();
    assert_eq!(stats.success_count, 3);
}

#[tokio::test]
async fn failure_increments_against_the_best_candidate_without_verification() {
    let (promotion, _temp) = promotion().await;
    let evidence = vec![Evidence::IntentExecution {
        record: record("fill", IntentResolutionPath::Deterministic),
    }];
    promotion.record_outcome(Some(URL), &evidence, false).await;

    let site = promotion
        .store()
        .site("https://example.test")
        .await
        .unwrap();
    let control = &site.pages["/login"].forms["page"].controls[0];
    assert_eq!(control.accessible_name, "Email address");
    let stats = control.intents.get("fill").unwrap();
    assert_eq!(stats.failure_count, 1);
    assert_eq!(stats.success_count, 0);
    assert_eq!(stats.last_verified_day, None);
    assert_eq!(stats.source, None);
}

#[tokio::test]
async fn vision_fallback_resolution_is_marked_vision_promoted() {
    let (promotion, _temp) = promotion().await;
    let evidence = vec![
        resolution("Email address", None),
        Evidence::IntentExecution {
            record: record("fill", IntentResolutionPath::VisionFallback),
        },
    ];
    promotion.record_outcome(Some(URL), &evidence, true).await;

    let site = promotion
        .store()
        .site("https://example.test")
        .await
        .unwrap();
    let stats = site.pages["/login"].forms["page"].controls[0].intents["fill"].clone();
    assert_eq!(stats.source, Some(RecordSource::VisionPromoted));
}

#[tokio::test]
async fn commands_without_an_intent_record_promote_nothing() {
    let (promotion, _temp) = promotion().await;
    let evidence = vec![resolution("Email address", None)];
    promotion.record_outcome(Some(URL), &evidence, true).await;
    assert!(promotion
        .store()
        .site("https://example.test")
        .await
        .is_none());
}

/// A challenge solve acts on pixels inside a widget iframe, so no control
/// resolution exists to promote; the outcome lands on the site's challenge
/// priors instead.
#[tokio::test]
async fn solve_challenge_outcomes_promote_to_site_challenge_priors() {
    let (promotion, temp) = promotion().await;
    let evidence = vec![Evidence::IntentExecution {
        record: record("solveChallenge", IntentResolutionPath::VisionFallback),
    }];
    promotion.record_outcome(Some(URL), &evidence, true).await;
    promotion.record_outcome(Some(URL), &evidence, false).await;
    promotion.flush().await;
    drop(promotion);

    let (store, report) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    assert_eq!(report.sites_loaded, 1);
    let site = store.site("https://example.test").await.unwrap();
    assert!(site.pages.is_empty());
    let stats = site.challenges.get("solveChallenge").unwrap();
    assert_eq!(stats.success_count, 1);
    assert_eq!(stats.failure_count, 1);
    assert!(stats.last_verified_day.is_some());
    let (kind, prior) = store.challenge_prior("https://example.test").await.unwrap();
    assert_eq!(kind, "solveChallenge");
    assert_eq!(prior.success_count, 1);
    assert_eq!(prior.failure_count, 1);
}

#[tokio::test]
async fn non_http_pages_promote_nothing() {
    let (promotion, _temp) = promotion().await;
    let evidence = vec![
        resolution("Email address", None),
        Evidence::IntentExecution {
            record: record("fill", IntentResolutionPath::Deterministic),
        },
    ];
    promotion
        .record_outcome(Some("about:blank"), &evidence, true)
        .await;
    promotion.record_outcome(None, &evidence, true).await;
    assert!(promotion.store().list_sites().await.is_empty());
}

/// The privacy canary for the write path: a typed value present in command
/// evidence must never reach a persisted byte.
#[tokio::test]
async fn typed_values_never_reach_the_store() {
    let canary = "canary-4f8b-typed-secret-value";
    let (promotion, temp) = promotion().await;
    let evidence = vec![
        resolution("Password", None),
        Evidence::Inspection {
            selector: Some("input[type=password]".to_string()),
            url: URL.to_string(),
            title: "Sign in".to_string(),
            text: canary.to_string(),
            html: Some(format!("<input value='{canary}'>")),
        },
        Evidence::IntentExecution {
            record: record("fill", IntentResolutionPath::Deterministic),
        },
    ];
    promotion.record_outcome(Some(URL), &evidence, true).await;
    promotion.flush().await;
    let profile_dir = promotion.store().root().to_path_buf();
    drop(promotion);

    for entry in std::fs::read_dir(profile_dir).unwrap() {
        let bytes = std::fs::read(entry.unwrap().path()).unwrap();
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            !text.contains(canary),
            "typed value leaked into the context store: {text}"
        );
    }
    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    let site = store.site("https://example.test").await.unwrap();
    assert_eq!(
        site.pages["/login"].forms["page"].controls[0].accessible_name,
        "Password"
    );
}

#[tokio::test]
async fn per_entity_urls_share_one_page_pattern() {
    let (promotion, _temp) = promotion().await;
    for id in ["cus_1001", "cus_2002"] {
        let url = format!("https://app.example.test/customers/{id}?tab=overview");
        let evidence = vec![
            resolution("Customer priority", None),
            Evidence::IntentExecution {
                record: record("locate", IntentResolutionPath::Deterministic),
            },
        ];
        promotion.record_outcome(Some(&url), &evidence, true).await;
    }
    let site = promotion
        .store()
        .site("https://example.test")
        .await
        .unwrap();
    assert_eq!(site.pages.len(), 1);
    let page = site.pages.get("/customers/{}").unwrap();
    let stats = page.forms["page"].controls[0].intents["locate"].clone();
    assert_eq!(stats.success_count, 2);
}

#[tokio::test]
async fn a_runtime_without_durable_profile_has_no_promotion_sink() {
    // C0-5: Chromium sessions are built without a promotion handle, so the
    // executor's promote hook is a no-op for them by construction.
    let runtime = page_runtime::PageRuntime::default();
    assert!(runtime.context_promotion().is_none());
    let temp = tempfile::tempdir().unwrap();
    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    let runtime = runtime.with_context_promotion(Arc::new(ContextPromotion::new(store)));
    assert!(runtime.context_promotion().is_some());
}

async fn seeded(name: &str, role: &str, ordinal: Option<usize>) -> ContextPromotion {
    let (promotion, _temp) = promotion().await;
    let evidence = vec![
        Evidence::Resolution {
            target: Box::new(TargetSpec {
                role: Some(role.to_string()),
                accessible_name: Some(name.to_string()),
                ordinal,
                ..TargetSpec::default()
            }),
            fingerprint: Box::new(TargetFingerprint {
                page_id: PageId::new(),
                frame: None,
                role: Some(role.to_string()),
                name: Some(name.to_string()),
                stable_attributes: BTreeMap::new(),
            }),
            candidates: Vec::new(),
            best_match_authorized: false,
        },
        Evidence::IntentExecution {
            record: record("fill", IntentResolutionPath::Deterministic),
        },
    ];
    promotion.record_outcome(Some(URL), &evidence, true).await;
    promotion
}

#[tokio::test]
async fn persisted_ladder_scores_are_pinned() {
    let promotion = seeded("Email address", "textbox", Some(1)).await;
    let url = Some(URL);

    let exact = promotion.ask(url, "Email address").await.unwrap();
    assert_eq!(exact.confidence, 1.0);
    let role_name = promotion.ask(url, "textbox Email address").await.unwrap();
    assert_eq!(role_name.confidence, 0.9);
    let fuzzy_full = promotion.ask(url, "address email").await.unwrap();
    assert_eq!(fuzzy_full.confidence, 0.8);
    assert!(
        promotion
            .ask(url, "email address missing-token")
            .await
            .is_none(),
        "partial token coverage must score under the floor"
    );
    assert!(
        promotion.ask(url, "email").await.is_none(),
        "single-token needles never fuzzy-match"
    );
}

#[tokio::test]
async fn fuzzy_never_beats_exact() {
    // "Sign in" exact-matches one control and fuzzy-matches none; a needle
    // that exact-matches one control and only partially covers another must
    // return the exact control at 1.0.
    let promotion = seeded("Sign in", "button", None).await;
    let evidence = vec![
        Evidence::Resolution {
            target: Box::new(TargetSpec {
                role: Some("link".into()),
                accessible_name: Some("Sign in with SSO".into()),
                ordinal: None,
                ..TargetSpec::default()
            }),
            fingerprint: Box::new(TargetFingerprint {
                page_id: PageId::new(),
                frame: None,
                role: Some("link".into()),
                name: Some("Sign in with SSO".into()),
                stable_attributes: BTreeMap::new(),
            }),
            candidates: Vec::new(),
            best_match_authorized: false,
        },
        Evidence::IntentExecution {
            record: record("follow", IntentResolutionPath::Deterministic),
        },
    ];
    promotion.record_outcome(Some(URL), &evidence, true).await;

    let answer = promotion.ask(Some(URL), "sign in").await.unwrap();
    assert_eq!(answer.confidence, 1.0);
    assert_eq!(answer.target.accessible_name, "Sign in");
}

#[tokio::test]
async fn persisted_answers_are_marked_with_provenance() {
    let promotion = seeded("Email address", "textbox", Some(2)).await;
    let answer = promotion.ask(Some(URL), "Email address").await.unwrap();
    assert_eq!(
        answer.observed_at,
        types::ContextObservedAt::Persisted,
        "a remembered answer must not claim a live generation"
    );
    assert_eq!(answer.source, Some(types::ContextAnswerSource::Observed));
    assert_eq!(answer.target.ordinal, Some(2));
}

#[tokio::test]
async fn vision_seeded_answers_carry_their_source() {
    let (promotion, _temp) = promotion().await;
    let evidence = vec![
        Evidence::Resolution {
            target: Box::new(TargetSpec {
                role: Some("button".into()),
                accessible_name: Some("Sign in".into()),
                ordinal: None,
                ..TargetSpec::default()
            }),
            fingerprint: Box::new(TargetFingerprint {
                page_id: PageId::new(),
                frame: None,
                role: Some("button".into()),
                name: Some("Sign in".into()),
                stable_attributes: BTreeMap::new(),
            }),
            candidates: Vec::new(),
            best_match_authorized: false,
        },
        Evidence::IntentExecution {
            record: record("locate", IntentResolutionPath::VisionFallback),
        },
    ];
    promotion.record_outcome(Some(URL), &evidence, true).await;

    let answer = promotion.ask(Some(URL), "Sign in").await.unwrap();
    assert_eq!(
        answer.source,
        Some(types::ContextAnswerSource::VisionPromoted)
    );
}

#[tokio::test]
async fn unknown_sites_answer_nothing() {
    let (promotion, _temp) = promotion().await;
    assert!(promotion
        .ask(Some("https://never-seen.example/login"), "Email address")
        .await
        .is_none());
    assert!(promotion.ask(None, "Email address").await.is_none());
}

#[tokio::test]
async fn ambiguous_remembered_controls_answer_nothing() {
    let promotion = seeded("Address", "textbox", Some(1)).await;
    let evidence = vec![
        Evidence::Resolution {
            target: Box::new(TargetSpec {
                role: Some("textbox".into()),
                accessible_name: Some("Address".into()),
                ordinal: Some(2),
                ..TargetSpec::default()
            }),
            fingerprint: Box::new(TargetFingerprint {
                page_id: PageId::new(),
                frame: None,
                role: Some("textbox".into()),
                name: Some("Address".into()),
                stable_attributes: BTreeMap::new(),
            }),
            candidates: Vec::new(),
            best_match_authorized: false,
        },
        Evidence::IntentExecution {
            record: record("fill", IntentResolutionPath::Deterministic),
        },
    ];
    promotion.record_outcome(Some(URL), &evidence, true).await;

    assert!(
        promotion.ask(Some(URL), "Address").await.is_none(),
        "two same-name remembered controls must tie-refuse"
    );
}
