//! Production-positive harvest: drive gauntlet journeys through the REAL
//! vision escalation chain with paraphrased (answerable, non-exact-name)
//! purposes. A step that commits and verifies is a production positive; a
//! step that abstains below the floor falls back to the scripted primitive
//! so the journey proceeds. The engine-side corpus records every
//! escalation with its terminal outcome (verified picks carry the resolved
//! target index).
//!
//! Run with the proxy up (v1 provider + adapter):
//!
//!   BOBBY_GAUNTLET_VISION_ENDPOINT=http://127.0.0.1:9200/vision \
//!   BOBBY_VISION_TOKEN=<bearer> \
//!   BOBBY_GAUNTLET_VISION_CORPUS_DIR=/tmp/vision-harvest \
//!   BOBBY_HARVEST_RUNS=5 \
//!   cargo test -p runtime-tests --test intent_vision_gauntlet -- --ignored --test-threads=1

#[allow(dead_code)]
#[path = "modern_gauntlet/mod.rs"]
mod modern_gauntlet;

use modern_gauntlet::driver::{Journey, ModernRuntime};
use modern_gauntlet::scenario::{ScenarioConfig, ScenarioServer};
use types::{ControlAction, FillIntent, IntentCommand, IntentHints, LocateIntent};

type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn harvest_runs() -> usize {
    std::env::var("BOBBY_HARVEST_RUNS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|value: &usize| *value > 0)
        .unwrap_or(3)
}

fn locate(purpose: &str) -> IntentCommand {
    IntentCommand::Locate(LocateIntent {
        purpose: purpose.into(),
        hints: IntentHints::default(),
    })
}

fn fill_text(purpose: &str, value: &str) -> IntentCommand {
    IntentCommand::Fill(FillIntent {
        purpose: purpose.into(),
        hints: IntentHints::default(),
        value: ControlAction::SetText {
            value: value.into(),
            clear_first: true,
        },
    })
}

fn select_one_intent(purpose: &str, value: &str) -> IntentCommand {
    IntentCommand::Fill(FillIntent {
        purpose: purpose.into(),
        hints: IntentHints::default(),
        value: ControlAction::SelectOne {
            value: value.into(),
        },
    })
}

/// Escalate the step through the vision chain; on any failure (abstain
/// below the floor is the common one) drive the scripted primitive so the
/// journey proceeds. Returns true when the intent committed+verified.
async fn escalate_or<T>(
    runtime: &ModernRuntime,
    command: IntentCommand,
    fallback: impl std::future::Future<Output = TestResult<T>>,
) -> TestResult<bool> {
    if runtime.submit_intent(command).await.is_ok() {
        return Ok(true);
    }
    fallback.await?;
    Ok(false)
}

fn require_harvest_env() {
    if std::env::var("BOBBY_GAUNTLET_VISION_ENDPOINT")
        .unwrap_or_default()
        .is_empty()
    {
        panic!("BOBBY_GAUNTLET_VISION_ENDPOINT unset; a harvest run collects nothing without it");
    }
    if std::env::var("BOBBY_GAUNTLET_VISION_CORPUS_DIR")
        .unwrap_or_default()
        .is_empty()
    {
        panic!("BOBBY_GAUNTLET_VISION_CORPUS_DIR unset; a harvest run records nothing without it");
    }
}

fn corpus_record_count() -> usize {
    let dir = std::env::var("BOBBY_GAUNTLET_VISION_CORPUS_DIR").unwrap_or_default();
    let path = std::path::Path::new(&dir).join("vision-corpus.jsonl");
    std::fs::read_to_string(path)
        .map(|contents| {
            contents
                .lines()
                .filter(|line| !line.trim().is_empty())
                .count()
        })
        .unwrap_or(0)
}

const UPLOAD_SUBMIT: [&str; 4] = [
    "Upload the staged document to the server",
    "Send the staged document off",
    "Push the staged document upload through",
    "Submit the staged customer document",
];

const TYPE_SEARCH: [&str; 4] = [
    "Put 'Atlas' in the lookup box",
    "Type 'Atlas' into the finder field",
    "Enter 'Atlas' in the client search input",
    "Key 'Atlas' into the customer finder",
];

const RUN_SEARCH: [&str; 4] = [
    "Push the button to search",
    "Fire off the customer search",
    "Execute the lookup with the button",
    "Trigger the search now",
];

const OPEN_CUSTOMER: [&str; 4] = [
    "Bring up the Atlas Labs record",
    "Open the Atlas Labs page",
    "Go to the Atlas Labs customer",
    "Pull up the Atlas Labs account",
];

const SELECT_PRIORITY: [&str; 4] = [
    "Pick the high priority for this customer",
    "Set the priority dropdown to high",
    "Choose high in the priority selector",
    "Mark the customer priority as high",
];

const SAVE_PRIORITY: [&str; 4] = [
    "Store the priority change with the save button",
    "Commit the new priority setting",
    "Save the updated customer priority",
    "Apply the priority change with the save control",
];

const FULL_NAME: [&str; 4] = [
    "Put 'Maya Chen' in the name field",
    "Type 'Maya Chen' where the name goes",
    "Enter 'Maya Chen' for the contact name",
    "Fill in 'Maya Chen' as the name",
];

const WORK_EMAIL: [&str; 4] = [
    "Type 'maya@atlas.example' into the email field",
    "Put 'maya@atlas.example' in the email box",
    "Enter the email 'maya@atlas.example'",
    "Fill in 'maya@atlas.example' for email",
];

const COMPANY: [&str; 4] = [
    "Enter 'Atlas Labs' as the company",
    "Type 'Atlas Labs' in the company field",
    "Put 'Atlas Labs' where the company goes",
    "Fill in 'Atlas Labs' as the organization",
];

const POSTAL: [&str; 4] = [
    "Put '02110' in the postal box",
    "Type '02110' into the postal field",
    "Enter '02110' for the postal code area",
    "Fill in '02110' in the postal slot",
];

const POSTAL_FIX: [&str; 4] = [
    "Enter '10001' in the postal code box",
    "Put '10001' in the postal field",
    "Type '10001' into the postal box",
    "Correct the postal code to '10001'",
];

const PLAN: [&str; 4] = [
    "Pick the growth plan",
    "Choose growth for the plan",
    "Select the growth tier",
    "Go with the growth plan",
];

const BILLING: [&str; 4] = [
    "Choose annual billing",
    "Pick the annual billing cycle",
    "Set billing to annual",
    "Select annual for the billing cycle",
];

const SUBMIT: [&str; 4] = [
    "Create the customer account",
    "Create the new customer",
    "Register the customer now",
    "Create this customer record",
];

#[tokio::test]
#[ignore = "requires installed Chrome and a running vision-proxy"]
async fn probe_fill_and_select_outcomes() -> TestResult<()> {
    require_harvest_env();
    let server = ScenarioServer::start(ScenarioConfig::seeded("fill-probe")).await?;
    let runtime = ModernRuntime::launch(&server, Journey::CustomerUpdate).await?;
    runtime
        .wait_visible("input[aria-label='Search customers']")
        .await?;

    let fill_outcome = runtime
        .submit_intent(fill_text("Put 'Atlas' in the lookup box", "Atlas"))
        .await;
    println!("FILL outcome ok={}", fill_outcome.is_ok());
    if let Err(error) = &fill_outcome {
        println!("FILL error: {error}");
    }

    let select_outcome = runtime
        .submit_intent(select_one_intent(
            "Pick the high priority for this customer",
            "high",
        ))
        .await;
    println!("SELECT outcome ok={}", select_outcome.is_ok());
    if let Err(error) = &select_outcome {
        println!("SELECT error: {error}");
    }
    Ok(())
}

async fn documents_run(run_idx: usize) -> TestResult<bool> {
    let seed = format!("documents-harvest-{run_idx}");
    let server = ScenarioServer::start(ScenarioConfig::seeded(&seed)).await?;
    let runtime = ModernRuntime::launch(&server, Journey::Documents).await?;

    // File input stays scripted: clicking it opens a native dialog.
    let fixture = runtime.fixture_path("approved-upload.txt");
    runtime
        .wait_visible("input[aria-label='Customer document']")
        .await?;
    runtime
        .upload("input[aria-label='Customer document']", &fixture)
        .await?;

    // The upload submit is the escalation target.
    let committed = escalate_or(
        &runtime,
        locate(UPLOAD_SUBMIT[run_idx % UPLOAD_SUBMIT.len()]),
        runtime.click("form[aria-label='Upload customer document'] button", true),
    )
    .await?;

    runtime.wait_visible("iframe[title^='Preview of']").await?;
    runtime
        .wait_in_frame_button("#document-preview", "#confirm-preview")
        .await?;
    runtime
        .click_in_frame("#document-preview", "#confirm-preview")
        .await?;
    runtime.mark_completed(&format!("documents-harvest-{run_idx}"))?;
    Ok(committed)
}

#[tokio::test]
#[ignore = "requires installed Chrome and a running vision-proxy"]
async fn harvest_documents_positives() -> TestResult<()> {
    require_harvest_env();
    let before = corpus_record_count();
    let mut committed = 0usize;
    let mut fell_back = 0usize;
    let mut derailed = 0usize;

    for run_idx in 0..harvest_runs() {
        match documents_run(run_idx).await {
            Ok(true) => committed += 1,
            Ok(false) => fell_back += 1,
            Err(error) => {
                // A committed wrong pick can derail a journey; that is real
                // production behavior, not a harness failure. Log and move on.
                derailed += 1;
                eprintln!("documents-harvest-{run_idx} derailed: {error}");
            }
        }
    }

    let collected = corpus_record_count() - before;
    println!(
        "documents harvest: {committed} committed, {fell_back} fell back, {derailed} derailed, {collected} corpus records"
    );
    assert!(
        collected > 0,
        "harvest produced no corpus records; the engine-side corpus path is broken"
    );
    Ok(())
}

async fn customer_update_run(run_idx: usize) -> TestResult<usize> {
    let seed = format!("customer-update-harvest-{run_idx}");
    let server = ScenarioServer::start(ScenarioConfig::seeded(&seed)).await?;
    let runtime = ModernRuntime::launch(&server, Journey::CustomerUpdate).await?;
    let mut committed = 0usize;

    runtime
        .wait_visible("input[aria-label='Search customers']")
        .await?;
    if escalate_or(
        &runtime,
        fill_text(TYPE_SEARCH[run_idx % TYPE_SEARCH.len()], "Atlas"),
        runtime.type_text("input[aria-label='Search customers']", "Atlas"),
    )
    .await?
    {
        committed += 1;
    }

    if escalate_or(
        &runtime,
        locate(RUN_SEARCH[run_idx % RUN_SEARCH.len()]),
        runtime.click("form[aria-label='Customer search'] button", false),
    )
    .await?
    {
        committed += 1;
    }
    runtime
        .wait_visible("a[href='/customers/cus_atlas']")
        .await?;

    if escalate_or(
        &runtime,
        locate(OPEN_CUSTOMER[run_idx % OPEN_CUSTOMER.len()]),
        runtime.click("a[href='/customers/cus_atlas']", false),
    )
    .await?
    {
        committed += 1;
    }
    runtime
        .wait_visible("select[aria-label='Customer priority']")
        .await?;

    if escalate_or(
        &runtime,
        select_one_intent(SELECT_PRIORITY[run_idx % SELECT_PRIORITY.len()], "high"),
        runtime.select_one("Customer priority", "high"),
    )
    .await?
    {
        committed += 1;
    }

    if escalate_or(
        &runtime,
        locate(SAVE_PRIORITY[run_idx % SAVE_PRIORITY.len()]),
        runtime.click("form[aria-label='Update customer priority'] button", true),
    )
    .await?
    {
        committed += 1;
    }
    runtime.wait_visible("[role='status']").await?;
    runtime.mark_completed(&format!("customer-update-harvest-{run_idx}"))?;
    Ok(committed)
}

// The nine-step run is split in two: one giant async fn composes one giant
// future type (every inline fallback future lands in the same state
// machine), which overflows the tokio worker stack. Split fns keep each
// composed future small.
async fn onboarding_fields(runtime: &ModernRuntime, run_idx: usize) -> TestResult<usize> {
    let mut committed = 0usize;

    runtime
        .wait_visible("input[aria-label='Full name']")
        .await?;
    for (phrasings, selector, value) in [
        (&FULL_NAME[..], "input[aria-label='Full name']", "Maya Chen"),
        (
            &WORK_EMAIL[..],
            "input[aria-label='Work email']",
            "maya@atlas.example",
        ),
        (
            &COMPANY[..],
            "input[aria-label='Company name']",
            "Atlas Labs",
        ),
        (&POSTAL[..], "input[aria-label='Postal code']", "02110"),
    ] {
        if escalate_or(
            runtime,
            fill_text(phrasings[run_idx % phrasings.len()], value),
            runtime.type_text(selector, value),
        )
        .await?
        {
            committed += 1;
        }
    }

    if escalate_or(
        runtime,
        select_one_intent(PLAN[run_idx % PLAN.len()], "growth"),
        runtime.select_one("Plan", "growth"),
    )
    .await?
    {
        committed += 1;
    }

    runtime
        .wait_visible("select[aria-label='Billing cycle']")
        .await?;
    if escalate_or(
        runtime,
        select_one_intent(BILLING[run_idx % BILLING.len()], "annual"),
        runtime.select_one("Billing cycle", "annual"),
    )
    .await?
    {
        committed += 1;
    }
    Ok(committed)
}

async fn onboarding_submit(
    runtime: &ModernRuntime,
    run_idx: usize,
    mut committed: usize,
) -> TestResult<usize> {
    // First submit: the scripted scenario rejects the first postal code
    // once, so the rejection screen is the expected outcome either way.
    if escalate_or(
        runtime,
        locate(SUBMIT[run_idx % SUBMIT.len()]),
        runtime.click(
            "form[aria-label='Customer onboarding'] button[type='submit']",
            true,
        ),
    )
    .await?
    {
        committed += 1;
    }
    runtime
        .wait_visible("input[aria-label='Postal code'][aria-invalid='true']")
        .await?;

    if escalate_or(
        runtime,
        fill_text(POSTAL_FIX[run_idx % POSTAL_FIX.len()], "10001"),
        runtime.type_text("input[aria-label='Postal code']", "10001"),
    )
    .await?
    {
        committed += 1;
    }

    if escalate_or(
        runtime,
        locate(SUBMIT[run_idx % SUBMIT.len()]),
        runtime.click(
            "form[aria-label='Customer onboarding'] button[type='submit']",
            true,
        ),
    )
    .await?
    {
        committed += 1;
    }
    runtime
        .wait_visible("form[aria-label='Customer onboarding'] [role='status']")
        .await?;
    runtime.mark_completed(&format!("onboarding-harvest-{run_idx}"))?;
    Ok(committed)
}

async fn onboarding_run(run_idx: usize) -> TestResult<usize> {
    let seed = format!("onboarding-harvest-{run_idx}");
    let server = ScenarioServer::start(ScenarioConfig::seeded(&seed)).await?;
    let runtime = ModernRuntime::launch(&server, Journey::Onboarding).await?;
    let committed = onboarding_fields(&runtime, run_idx).await?;
    onboarding_submit(&runtime, run_idx, committed).await
}

#[tokio::test]
#[ignore = "requires installed Chrome and a running vision-proxy"]
async fn harvest_onboarding_positives() -> TestResult<()> {
    require_harvest_env();
    let before = corpus_record_count();
    let mut committed = 0usize;
    let mut derailed = 0usize;

    for run_idx in 0..harvest_runs() {
        match onboarding_run(run_idx).await {
            Ok(count) => committed += count,
            Err(error) => {
                derailed += 1;
                eprintln!("onboarding-harvest-{run_idx} derailed: {error}");
            }
        }
    }

    let collected = corpus_record_count() - before;
    println!(
        "onboarding harvest: {committed} committed, {derailed} derailed, {collected} corpus records"
    );
    assert!(
        collected > 0,
        "harvest produced no corpus records; the engine-side corpus path is broken"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires installed Chrome and a running vision-proxy"]
async fn harvest_customer_update_positives() -> TestResult<()> {
    require_harvest_env();
    let before = corpus_record_count();
    let mut committed = 0usize;
    let mut derailed = 0usize;

    for run_idx in 0..harvest_runs() {
        match customer_update_run(run_idx).await {
            Ok(count) => committed += count,
            Err(error) => {
                derailed += 1;
                eprintln!("customer-update-harvest-{run_idx} derailed: {error}");
            }
        }
    }

    let collected = corpus_record_count() - before;
    println!(
        "customer-update harvest: {committed} committed, {derailed} derailed, {collected} corpus records"
    );
    assert!(
        collected > 0,
        "harvest produced no corpus records; the engine-side corpus path is broken"
    );
    Ok(())
}
