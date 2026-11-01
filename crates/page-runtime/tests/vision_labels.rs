use std::collections::BTreeMap;

use context_store::{ContextStore, LabelContext, RecordSource};
use page_runtime::ContextPromotion;
use types::{
    Evidence, ExecutionRecord, IntentResolutionPath, PageId, TargetFingerprint, TargetSpec,
};

const URL: &str = "https://app.example.test/login";
const SITE: &str = "https://example.test";
const PURPOSE: &str = "Contact email";

async fn promotion() -> (ContextPromotion, tempfile::TempDir) {
    let temp = tempfile::tempdir().unwrap();
    let (store, _) = ContextStore::open(temp.path(), "profile-a").await.unwrap();
    (ContextPromotion::new(store), temp)
}

fn outcome(purpose: &str, path: IntentResolutionPath, resolved: Option<&str>) -> Vec<Evidence> {
    let mut evidence = Vec::new();
    if let Some(name) = resolved {
        evidence.push(Evidence::Resolution {
            target: Box::new(TargetSpec {
                role: Some("textbox".to_string()),
                accessible_name: Some(name.to_string()),
                ordinal: Some(0),
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
        });
    }
    evidence.push(Evidence::IntentExecution {
        record: ExecutionRecord {
            intent_kind: "fill".to_string(),
            purpose: Some(purpose.to_string()),
            resolution_path: path,
            plan_summary: "resolve then fill".to_string(),
            candidates: Vec::new(),
            wait_elapsed_ms: None,
            verification: "resolved".to_string(),
            artifact_ids: Vec::new(),
            vision_proposal_sha256: None,
        },
    });
    evidence
}

async fn labels(promotion: &ContextPromotion) -> BTreeMap<String, LabelContext> {
    promotion
        .store()
        .site(SITE)
        .await
        .and_then(|mut site| site.pages.remove("/login"))
        .map(|page| page.labels)
        .unwrap_or_default()
}

#[tokio::test]
async fn a_verified_vision_answer_binds_the_purpose_to_the_resolved_control() {
    let (promotion, _temp) = promotion().await;
    promotion
        .record_outcome(
            Some(URL),
            &outcome(PURPOSE, IntentResolutionPath::VisionFallback, Some("f17")),
            true,
        )
        .await;

    let label = labels(&promotion).await.remove("contact email").unwrap();
    assert_eq!(
        (
            label.role.as_str(),
            label.accessible_name.as_str(),
            label.ordinal
        ),
        ("textbox", "f17", Some(0))
    );
    assert_eq!(
        (label.stats.success_count, label.stats.failure_count),
        (1, 0)
    );
    assert_eq!(label.stats.source, Some(RecordSource::VisionPromoted));
    assert!(label.trusted());
}

#[tokio::test]
async fn deterministic_resolutions_store_no_label() {
    let (promotion, _temp) = promotion().await;
    promotion
        .record_outcome(
            Some(URL),
            &outcome(PURPOSE, IntentResolutionPath::Deterministic, Some("f17")),
            true,
        )
        .await;

    assert!(labels(&promotion).await.is_empty());
}

#[tokio::test]
async fn a_newer_verified_answer_moves_the_label() {
    let (promotion, _temp) = promotion().await;
    for name in ["f17", "f17", "f18"] {
        promotion
            .record_outcome(
                Some(URL),
                &outcome(PURPOSE, IntentResolutionPath::VisionFallback, Some(name)),
                true,
            )
            .await;
    }

    let labels = labels(&promotion).await;
    assert_eq!(labels.len(), 1);
    let label = &labels["contact email"];
    assert_eq!(label.accessible_name, "f18");
    assert_eq!(
        (label.stats.success_count, label.stats.failure_count),
        (1, 0)
    );
}

#[tokio::test]
async fn a_failure_counts_against_the_label_until_it_is_no_longer_trusted() {
    let (promotion, _temp) = promotion().await;
    promotion
        .record_outcome(
            Some(URL),
            &outcome(PURPOSE, IntentResolutionPath::VisionFallback, Some("f17")),
            true,
        )
        .await;
    promotion
        .record_outcome(
            Some(URL),
            &outcome(PURPOSE, IntentResolutionPath::Context, None),
            false,
        )
        .await;

    let label = labels(&promotion).await.remove("contact email").unwrap();
    assert_eq!(
        (label.stats.success_count, label.stats.failure_count),
        (1, 1)
    );
    assert!(!label.trusted());
}

#[tokio::test]
async fn a_context_hit_reinforces_its_label() {
    let (promotion, _temp) = promotion().await;
    for path in [
        IntentResolutionPath::VisionFallback,
        IntentResolutionPath::Context,
    ] {
        promotion
            .record_outcome(Some(URL), &outcome(PURPOSE, path, Some("f17")), true)
            .await;
    }

    let label = labels(&promotion).await.remove("contact email").unwrap();
    assert_eq!(label.stats.success_count, 2);
}

#[tokio::test]
async fn an_overlong_purpose_is_never_remembered() {
    let (promotion, _temp) = promotion().await;
    let purpose = "x".repeat(context_store::MAX_LABEL_BYTES + 1);
    promotion
        .record_outcome(
            Some(URL),
            &outcome(&purpose, IntentResolutionPath::VisionFallback, Some("f17")),
            true,
        )
        .await;

    assert!(labels(&promotion).await.is_empty());
}
