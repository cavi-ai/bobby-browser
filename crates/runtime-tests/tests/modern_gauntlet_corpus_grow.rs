//! Corpus volume growth: run each scripted journey N times into a single
//! per-journey collector and save once, accumulating records across runs.
//!
//! Each run is an isolated ScenarioServer (fresh run id, fresh state), so
//! captures pick up small pixel/timing variation and distinct run URLs
//! while sharing the journey's ground-truth targets. Complements
//! modern_gauntlet_collect.rs, which captures one canonical pass per journey.

#[allow(dead_code)]
#[path = "modern_gauntlet/mod.rs"]
mod modern_gauntlet;

use modern_gauntlet::collector::{CorpusCollector, GroundTruth};
use modern_gauntlet::driver::{Journey, ModernRuntime};
use modern_gauntlet::scenario::{GauntletLevel, LevelTwoTrapPlan, ScenarioConfig, ScenarioServer};

type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Runs per journey; override with BOBBY_CORPUS_RUNS_PER_JOURNEY for
/// bigger collection passes (e.g. 10 -> ~200 records).
fn runs_per_journey() -> usize {
    std::env::var("BOBBY_CORPUS_RUNS_PER_JOURNEY")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value: &usize| *value > 0)
        .unwrap_or(5)
}

/// Level 2 traps without the CAPTCHA: the server only verifies when a
/// verifier is configured, so `recaptcha: None` yields trap layouts
/// (reversed identity fields, delayed controls, interruption modal) with
/// no external keys. Each trap seed flips different combinations.
fn trap_config(seed: &str, run_idx: usize) -> ScenarioConfig {
    ScenarioConfig {
        seed: seed.to_string(),
        reject_postal_once: true,
        level: GauntletLevel::Two,
        traps: LevelTwoTrapPlan {
            extra_modal: run_idx.is_multiple_of(2),
            extra_popup: false,
            reversed_identity_fields: run_idx % 3 == 1,
            delayed_control_ms: 150 + (run_idx as u64) * 100,
        },
        recaptcha: None,
    }
}

fn corpus_path(journey: &str) -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../target/vision-corpus")
        .join(format!("{journey}.jsonl"))
}

async fn grow_journey<F>(journey: &str, run: F) -> TestResult<usize>
where
    F: AsyncFn(
        ModernRuntime,
        &mut CorpusCollector,
        &ScenarioServer,
        usize,
    ) -> TestResult<ModernRuntime>,
{
    let mut collector = CorpusCollector::new();
    let path = corpus_path(journey);
    for run_idx in 0..runs_per_journey() {
        let seed = format!("{journey}-grow-{run_idx}");
        let server = ScenarioServer::start(ScenarioConfig::seeded(&seed)).await?;
        let runtime = ModernRuntime::launch(
            &server,
            match journey {
                "customer-update" => Journey::CustomerUpdate,
                "onboarding" => Journey::Onboarding,
                "documents" => Journey::Documents,
                "authorization" => Journey::Authorization,
                _ => Journey::ReportRecovery,
            },
        )
        .await?;
        let mark = format!("{journey}-grow-{run_idx}");
        let runtime = run(runtime, &mut collector, &server, run_idx).await?;
        runtime.mark_completed(&mark)?;
        // Publish each completed run atomically. A later retryable browser
        // failure must not hide the usable corpus already collected.
        collector.save(&path)?;
    }
    println!(
        "wrote {} examples to {} ({} runs)",
        collector.len(),
        path.display(),
        runs_per_journey()
    );
    Ok(collector.len())
}

#[tokio::test]
async fn grow_documents_traps_corpus() -> TestResult<()> {
    // Trap-mode pass over the documents journey: file input + upload +
    // iframe preview, with modal and delayed-control variation per run.
    // This is the layout family the adapter failed OOS on before trap
    // diversity entered training (§4f/§4h of the whitepaper).
    let mut collector = CorpusCollector::new();
    for run_idx in 0..runs_per_journey() {
        let seed = format!("documents-traps-{run_idx}");
        let server = ScenarioServer::start(trap_config(&seed, run_idx)).await?;
        let runtime = ModernRuntime::launch(&server, Journey::Documents).await?;
        let step = |name: &str| format!("{name}_t{run_idx}");

        if run_idx.is_multiple_of(2)
            && runtime
                .wait_visible("section[aria-label='Workflow interruption'] button")
                .await
                .is_ok()
        {
            collector
                .capture(
                    &runtime,
                    &GroundTruth::Click {
                        selector: "section[aria-label='Workflow interruption'] button",
                        purpose: "Dismiss the workflow interruption".into(),
                        ordinal: None,
                    },
                    "documents-traps",
                    &step("dismiss_interruption"),
                )
                .await?;
            runtime
                .click("section[aria-label='Workflow interruption'] button", false)
                .await?;
        }

        let fixture = runtime.fixture_path("approved-upload.txt");
        runtime
            .wait_visible("input[aria-label='Customer document']")
            .await?;
        collector
            .capture(
                &runtime,
                &GroundTruth::Click {
                    selector: "input[aria-label='Customer document']",
                    purpose: "Choose the customer document file".into(),
                    ordinal: None,
                },
                "documents-traps",
                &step("choose_file"),
            )
            .await?;
        runtime
            .upload("input[aria-label='Customer document']", &fixture)
            .await?;

        collector
            .capture(
                &runtime,
                &GroundTruth::Click {
                    selector: "form[aria-label='Upload customer document'] button",
                    purpose: "Upload the chosen document".into(),
                    ordinal: None,
                },
                "documents-traps",
                &step("click_upload"),
            )
            .await?;
        runtime
            .click("form[aria-label='Upload customer document'] button", true)
            .await?;
        runtime.wait_visible("iframe[title^='Preview of']").await?;
        runtime
            .wait_in_frame_button("#document-preview", "#confirm-preview")
            .await?;
        runtime
            .click_in_frame("#document-preview", "#confirm-preview")
            .await?;
        runtime.mark_completed(&format!("documents-traps-{run_idx}"))?;
    }
    let path = corpus_path("documents-traps");
    collector.save(&path)?;
    println!(
        "wrote {} examples to {} ({} runs)",
        collector.len(),
        path.display(),
        runs_per_journey()
    );
    Ok(())
}

#[tokio::test]
async fn grow_customer_update_traps_corpus() -> TestResult<()> {
    // Layout diversity pass: Level 2 traps without CAPTCHA keys. Each run
    // flips a different trap combination (modal on/off, control delay), so
    // captures see genuinely different layouts for the same scripted steps.
    // Onboarding is excluded from trap mode: its level-2 submit hard-gates
    // on a CAPTCHA token, which this configuration intentionally lacks.
    let mut collector = CorpusCollector::new();
    for run_idx in 0..runs_per_journey() {
        let seed = format!("customer-update-traps-{run_idx}");
        let server = ScenarioServer::start(trap_config(&seed, run_idx)).await?;
        let runtime = ModernRuntime::launch(&server, Journey::CustomerUpdate).await?;
        let step = |name: &str| format!("{name}_t{run_idx}");

        // Dismiss the interruption modal when this trap combo shows one.
        if run_idx.is_multiple_of(2)
            && runtime
                .wait_visible("section[aria-label='Workflow interruption'] button")
                .await
                .is_ok()
        {
            collector
                .capture(
                    &runtime,
                    &GroundTruth::Click {
                        selector: "section[aria-label='Workflow interruption'] button",
                        purpose: "Dismiss the workflow interruption".into(),
                        ordinal: None,
                    },
                    "customer-update-traps",
                    &step("dismiss_interruption"),
                )
                .await?;
            runtime
                .click("section[aria-label='Workflow interruption'] button", false)
                .await?;
        }

        collector
            .capture(
                &runtime,
                &GroundTruth::TypeText {
                    selector: "input[aria-label='Search customers']",
                    text: "Atlas",
                    purpose: "Enter 'Atlas' into the search customers field".into(),
                    ordinal: None,
                },
                "customer-update-traps",
                &step("type_search"),
            )
            .await?;
        runtime
            .type_text("input[aria-label='Search customers']", "Atlas")
            .await?;

        collector
            .capture(
                &runtime,
                &GroundTruth::Click {
                    selector: "form[aria-label='Customer search'] button",
                    purpose: "Run the customer search".into(),
                    ordinal: None,
                },
                "customer-update-traps",
                &step("click_search"),
            )
            .await?;
        runtime
            .click("form[aria-label='Customer search'] button", false)
            .await?;
        runtime
            .wait_visible("a[href='/customers/cus_atlas']")
            .await?;

        collector
            .capture(
                &runtime,
                &GroundTruth::Click {
                    selector: "a[href='/customers/cus_atlas']",
                    purpose: "Open the Atlas Labs customer".into(),
                    ordinal: None,
                },
                "customer-update-traps",
                &step("open_customer"),
            )
            .await?;
        runtime
            .click("a[href='/customers/cus_atlas']", false)
            .await?;
        runtime
            .wait_visible("select[aria-label='Customer priority']")
            .await?;

        collector
            .capture(
                &runtime,
                &GroundTruth::Click {
                    selector: "select[aria-label='Customer priority']",
                    purpose: "Choose the high priority".into(),
                    ordinal: None,
                },
                "customer-update-traps",
                &step("select_priority"),
            )
            .await?;
        runtime.select_one("Customer priority", "high").await?;

        collector
            .capture(
                &runtime,
                &GroundTruth::Click {
                    selector: "form[aria-label='Update customer priority'] button",
                    purpose: "Save the priority change".into(),
                    ordinal: None,
                },
                "customer-update-traps",
                &step("save_priority"),
            )
            .await?;
        runtime
            .click("form[aria-label='Update customer priority'] button", true)
            .await?;
        runtime.wait_visible("[role='status']").await?;
        runtime.mark_completed(&format!("customer-update-traps-{run_idx}"))?;
    }
    let path = corpus_path("customer-update-traps");
    collector.save(&path)?;
    println!(
        "wrote {} examples to {} ({} runs)",
        collector.len(),
        path.display(),
        runs_per_journey()
    );
    Ok(())
}

#[tokio::test]
async fn grow_customer_update_corpus() -> TestResult<()> {
    let count = grow_journey(
        "customer-update",
        async |runtime, collector, _server, run_idx| {
            let step = |name: &str| format!("{name}_r{run_idx}");
            collector
                .capture(
                    &runtime,
                    &GroundTruth::TypeText {
                        selector: "input[aria-label='Search customers']",
                        text: "Atlas",
                        purpose: "Enter 'Atlas' into the search customers field".into(),
                        ordinal: None,
                    },
                    "customer-update",
                    &step("type_search"),
                )
                .await?;
            runtime
                .type_text("input[aria-label='Search customers']", "Atlas")
                .await?;

            collector
                .capture(
                    &runtime,
                    &GroundTruth::Click {
                        selector: "form[aria-label='Customer search'] button",
                        purpose: "Run the customer search".into(),
                        ordinal: None,
                    },
                    "customer-update",
                    &step("click_search"),
                )
                .await?;
            runtime
                .click("form[aria-label='Customer search'] button", false)
                .await?;
            runtime
                .wait_visible("a[href='/customers/cus_atlas']")
                .await?;

            collector
                .capture(
                    &runtime,
                    &GroundTruth::Click {
                        selector: "a[href='/customers/cus_atlas']",
                        purpose: "Open the Atlas Labs customer".into(),
                        ordinal: None,
                    },
                    "customer-update",
                    &step("open_customer"),
                )
                .await?;
            runtime
                .click("a[href='/customers/cus_atlas']", false)
                .await?;
            runtime
                .wait_visible("select[aria-label='Customer priority']")
                .await?;

            collector
                .capture(
                    &runtime,
                    &GroundTruth::Click {
                        selector: "select[aria-label='Customer priority']",
                        purpose: "Choose the high priority".into(),
                        ordinal: None,
                    },
                    "customer-update",
                    &step("select_priority"),
                )
                .await?;
            runtime.select_one("Customer priority", "high").await?;

            collector
                .capture(
                    &runtime,
                    &GroundTruth::Click {
                        selector: "form[aria-label='Update customer priority'] button",
                        purpose: "Save the priority change".into(),
                        ordinal: None,
                    },
                    "customer-update",
                    &step("save_priority"),
                )
                .await?;
            runtime
                .click("form[aria-label='Update customer priority'] button", true)
                .await?;
            runtime.wait_visible("[role='status']").await?;
            Ok(runtime)
        },
    )
    .await?;
    assert_eq!(count, 5 * runs_per_journey());
    Ok(())
}

#[tokio::test]
async fn grow_onboarding_corpus() -> TestResult<()> {
    let count = grow_journey(
        "onboarding",
        async |runtime, collector, _server, run_idx| {
            let step = |name: &str| format!("{name}_r{run_idx}");
            for (selector, value, field) in [
                ("input[aria-label='Full name']", "Maya Chen", "full name"),
                (
                    "input[aria-label='Work email']",
                    "maya@atlas.example",
                    "work email",
                ),
                (
                    "input[aria-label='Company name']",
                    "Atlas Labs",
                    "company name",
                ),
                ("input[aria-label='Postal code']", "02110", "postal code"),
            ] {
                collector
                    .capture(
                        &runtime,
                        &GroundTruth::TypeText {
                            selector,
                            text: value,
                            purpose: format!("Enter '{value}' into the {field} field"),
                            ordinal: None,
                        },
                        "onboarding",
                        &step(&format!("type_{field}")),
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
                        ordinal: None,
                    },
                    "onboarding",
                    &step("select_plan"),
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
                        ordinal: None,
                    },
                    "onboarding",
                    &step("select_billing"),
                )
                .await?;
            runtime.select_one("Billing cycle", "annual").await?;

            collector
                .capture(
                    &runtime,
                    &GroundTruth::Click {
                        selector: "form[aria-label='Customer onboarding'] button[type='submit']",
                        purpose: "Submit the onboarding form".into(),
                        ordinal: None,
                    },
                    "onboarding",
                    &step("submit_invalid_postal"),
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
                        ordinal: None,
                    },
                    "onboarding",
                    &step("fix_postal_code"),
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
                        ordinal: None,
                    },
                    "onboarding",
                    &step("submit_valid"),
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
            Ok(runtime)
        },
    )
    .await?;
    assert_eq!(count, 9 * runs_per_journey());
    Ok(())
}

#[tokio::test]
async fn grow_authorization_corpus() -> TestResult<()> {
    let count = grow_journey(
        "authorization",
        async |runtime, collector, _server, run_idx| {
            let step = |name: &str| format!("{name}_r{run_idx}");
            runtime
                .wait_visible("button[aria-label='Connect Ledger Cloud']")
                .await?;
            collector
                .capture(
                    &runtime,
                    &GroundTruth::Click {
                        selector: "button[aria-label='Connect Ledger Cloud']",
                        purpose: "Connect the Ledger Cloud integration".into(),
                        ordinal: None,
                    },
                    "authorization",
                    &step("connect_ledger"),
                )
                .await?;
            let popup = runtime
                .click_popup("button[aria-label='Connect Ledger Cloud']")
                .await?;
            runtime.click_on(&popup, "#authorize").await?;
            runtime.wait_visible("[data-connected='true']").await?;

            collector
                .capture(
                    &runtime,
                    &GroundTruth::Click {
                        selector: "button[aria-label='Dismiss notification preferences']",
                        purpose: "Dismiss the notification preferences prompt".into(),
                        ordinal: None,
                    },
                    "authorization",
                    &step("dismiss_obstruction"),
                )
                .await?;
            runtime
                .click(
                    "button[aria-label='Dismiss notification preferences']",
                    false,
                )
                .await?;
            Ok(runtime)
        },
    )
    .await?;
    assert_eq!(count, 2 * runs_per_journey());
    Ok(())
}

#[tokio::test]
async fn grow_documents_corpus() -> TestResult<()> {
    let count = grow_journey("documents", async |runtime, collector, _server, run_idx| {
        let step = |name: &str| format!("{name}_r{run_idx}");
        let fixture = runtime.fixture_path("approved-upload.txt");
        runtime
            .wait_visible("input[aria-label='Customer document']")
            .await?;
        collector
            .capture(
                &runtime,
                &GroundTruth::Click {
                    selector: "input[aria-label='Customer document']",
                    purpose: "Choose the customer document file".into(),
                    ordinal: None,
                },
                "documents",
                &step("choose_file"),
            )
            .await?;
        runtime
            .upload("input[aria-label='Customer document']", &fixture)
            .await?;

        collector
            .capture(
                &runtime,
                &GroundTruth::Click {
                    selector: "form[aria-label='Upload customer document'] button",
                    purpose: "Upload the chosen document".into(),
                    ordinal: None,
                },
                "documents",
                &step("click_upload"),
            )
            .await?;
        runtime
            .click("form[aria-label='Upload customer document'] button", true)
            .await?;
        runtime.wait_visible("iframe[title^='Preview of']").await?;
        runtime
            .wait_in_frame_button("#document-preview", "#confirm-preview")
            .await?;
        runtime
            .click_in_frame("#document-preview", "#confirm-preview")
            .await?;
        Ok(runtime)
    })
    .await?;
    assert_eq!(count, 2 * runs_per_journey());
    Ok(())
}

#[tokio::test]
async fn grow_report_recovery_corpus() -> TestResult<()> {
    let count = grow_journey(
        "report-recovery",
        async |runtime, collector, server, run_idx| {
            let step = |name: &str| format!("{name}_r{run_idx}");
            collector
                .capture(
                    &runtime,
                    &GroundTruth::Click {
                        selector: "form[aria-label='Generate report'] button",
                        purpose: "Generate the operations report".into(),
                        ordinal: None,
                    },
                    "report-recovery",
                    &step("generate_report"),
                )
                .await?;
            let workflow_id = runtime
                .click_boundary_with_workflow("form[aria-label='Generate report'] button")
                .await?;

            let server_url = server.application_url("/reports");
            let (runtime, _recovery) = runtime
                .restart_and_recover(&workflow_id, &server_url)
                .await?;
            runtime
                .wait_visible("a[download='atlas-operations.csv']")
                .await?;

            collector
                .capture(
                    &runtime,
                    &GroundTruth::Click {
                        selector: "a[download='atlas-operations.csv']",
                        purpose: "Download the generated report".into(),
                        ordinal: None,
                    },
                    "report-recovery",
                    &step("download_report"),
                )
                .await?;
            runtime
                .click_download("a[download='atlas-operations.csv']")
                .await?;
            Ok(runtime)
        },
    )
    .await?;
    assert_eq!(count, 2 * runs_per_journey());
    Ok(())
}
