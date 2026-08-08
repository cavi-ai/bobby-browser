//! Collects a vision training corpus from the scripted onboarding journey.
//!
//! Every capture happens immediately before the scripted action it labels, so
//! each record pairs the real page context (candidates, URL, screenshot) with
//! the journey's known-correct target as a candidate-index ground truth.

#[path = "modern_gauntlet/mod.rs"]
mod modern_gauntlet;

use modern_gauntlet::collector::{CorpusCollector, GroundTruth};
use modern_gauntlet::driver::{Journey, ModernRuntime};
use modern_gauntlet::scenario::{ScenarioConfig, ScenarioServer};

type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn corpus_path(journey: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/vision-corpus")
        .join(format!("{journey}.jsonl"))
}

#[tokio::test]
async fn collect_onboarding_corpus() -> TestResult<()> {
    let server = ScenarioServer::start(ScenarioConfig::seeded("onboarding")).await?;
    let runtime = ModernRuntime::launch(&server, Journey::Onboarding).await?;
    let mut collector = CorpusCollector::new();

    for (selector, value, field) in [
        ("input[aria-label='Full name']", "Maya Chen", "full name"),
        ("input[aria-label='Work email']", "maya@atlas.example", "work email"),
        ("input[aria-label='Company name']", "Atlas Labs", "company name"),
        ("input[aria-label='Postal code']", "02110", "postal code"),
    ] {
        collector
            .capture(
                &runtime,
                &GroundTruth::TypeText {
                    selector,
                    text: value,
                    purpose: format!("Enter '{value}' into the {field} field"),
                },
                "onboarding",
                &format!("type_{field}"),
            )
            .await?;
        runtime.type_text(selector, value).await?;
    }

    collector
        .capture(
            &runtime,
            &GroundTruth::Click {
                selector: "select[aria-label='Plan']",
                purpose: "Choose the growth plan".into(),
            },
            "onboarding",
            "select_plan",
        )
        .await?;
    runtime.select_one("Plan", "growth").await?;

    runtime
        .wait_visible("select[aria-label='Billing cycle']")
        .await?;
    collector
        .capture(
            &runtime,
            &GroundTruth::Click {
                selector: "select[aria-label='Billing cycle']",
                purpose: "Choose annual billing".into(),
            },
            "onboarding",
            "select_billing",
        )
        .await?;
    runtime.select_one("Billing cycle", "annual").await?;

    collector
        .capture(
            &runtime,
            &GroundTruth::Click {
                selector: "form[aria-label='Customer onboarding'] button[type='submit']",
                purpose: "Submit the onboarding form".into(),
            },
            "onboarding",
            "submit_invalid_postal",
        )
        .await?;
    runtime
        .click(
            "form[aria-label='Customer onboarding'] button[type='submit']",
            true,
        )
        .await?;
    runtime
        .wait_visible("input[aria-label='Postal code'][aria-invalid='true']")
        .await?;

    collector
        .capture(
            &runtime,
            &GroundTruth::TypeText {
                selector: "input[aria-label='Postal code']",
                text: "10001",
                purpose: "Enter '10001' into the postal code field".into(),
            },
            "onboarding",
            "fix_postal_code",
        )
        .await?;
    runtime
        .type_text("input[aria-label='Postal code']", "10001")
        .await?;

    collector
        .capture(
            &runtime,
            &GroundTruth::Click {
                selector: "form[aria-label='Customer onboarding'] button[type='submit']",
                purpose: "Submit the onboarding form".into(),
            },
            "onboarding",
            "submit_valid",
        )
        .await?;
    runtime
        .click(
            "form[aria-label='Customer onboarding'] button[type='submit']",
            true,
        )
        .await?;
    runtime
        .wait_visible("form[aria-label='Customer onboarding'] [role='status']")
        .await?;

    let path = corpus_path("onboarding");
    collector.save(&path)?;
    assert_eq!(collector.len(), 9, "expected 9 onboarding examples");
    runtime.mark_completed("onboarding")?;
    println!("wrote {} examples to {}", collector.len(), path.display());
    Ok(())
}
