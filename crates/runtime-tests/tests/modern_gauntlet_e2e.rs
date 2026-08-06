#[path = "modern_gauntlet/mod.rs"]
mod modern_gauntlet;

use std::collections::BTreeSet;

use modern_gauntlet::driver::{Journey, ModernRuntime};
use modern_gauntlet::evidence::{
    assert_effect_count, assert_file_digest, assert_journal_terminal_once, EvidenceBundle,
};
use modern_gauntlet::scenario::{ScenarioConfig, ScenarioServer};
use sha2::{Digest, Sha256};
use types::{Evidence, RecoveryDecision};

type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const REQUIRED_JOURNEYS: [&str; 5] = [
    "customer_discovery_and_update_is_durable",
    "validated_onboarding_preserves_accepted_values",
    "document_upload_preview_and_confirmation_are_durable",
    "popup_authorization_survives_obstruction",
    "interrupted_report_recovers_once_and_downloads",
];

#[test]
fn release_suite_names_are_stable() {
    assert_eq!(REQUIRED_JOURNEYS.len(), 5);
    assert_eq!(
        REQUIRED_JOURNEYS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len(),
        5
    );
    let source = include_str!("modern_gauntlet_e2e.rs");
    assert_eq!(
        source.matches("#[tokio::test]\nasync fn ").count(),
        5,
        "release file must contain exactly five Tokio browser tests"
    );
    assert!(
        !source.contains(concat!("#[", "ignore")),
        "release tests may not be ignored"
    );
    for name in REQUIRED_JOURNEYS {
        assert!(
            source.contains(&format!("#[tokio::test]\nasync fn {name}")),
            "missing mandatory release test {name}"
        );
    }
}

#[test]
fn level_two_recaptcha_training_ground() -> TestResult<()> {
    if std::env::var("BOBBY_GAUNTLET_LEVEL").as_deref() != Ok("2") {
        return Ok(());
    }
    let site_key = std::env::var("BOBBY_GAUNTLET_RECAPTCHA_SITE_KEY")
        .map_err(|_| "BOBBY_GAUNTLET_RECAPTCHA_SITE_KEY is required for Level 2")?;
    let secret = std::env::var("BOBBY_GAUNTLET_RECAPTCHA_SECRET")
        .map_err(|_| "BOBBY_GAUNTLET_RECAPTCHA_SECRET is required for Level 2")?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let server = ScenarioServer::start(ScenarioConfig::level_two(
            "live-training-ground",
            site_key,
            secret,
        )?)
        .await?;
        println!(
            "Level 2 training ground: {}",
            server.application_url("onboarding")
        );
        println!("Press Ctrl-C to stop the scenario server.");
        tokio::signal::ctrl_c().await?;
        Ok(())
    })
}

#[tokio::test]
async fn customer_discovery_and_update_is_durable() -> TestResult<()> {
    let server = ScenarioServer::start(ScenarioConfig::seeded("customer-update")).await?;
    let runtime = ModernRuntime::launch(&server, Journey::CustomerUpdate).await?;
    runtime
        .type_text("input[aria-label='Search customers']", "Atlas")
        .await?;
    runtime
        .click("form[aria-label='Customer search'] button", false)
        .await?;
    if let Err(error) = runtime.wait_visible("a[href='/customers/cus_atlas']").await {
        let diagnostic = runtime
            .accessibility_snapshot()
            .await
            .map(|value| format!("{value:?}"))
            .unwrap_or_else(|diagnostic_error| format!("unavailable: {diagnostic_error}"));
        return Err(format!("{error}; browser accessibility: {diagnostic}").into());
    }
    runtime
        .click("a[href='/customers/cus_atlas']", false)
        .await?;
    runtime
        .wait_visible("select[aria-label='Customer priority']")
        .await?;
    runtime.select_one("Customer priority", "high").await?;
    runtime
        .click("form[aria-label='Update customer priority'] button", true)
        .await?;
    runtime.wait_visible("[role='status']").await?;
    let visible = runtime.inspect(Some("[role='status']")).await?;
    assert!(inspection_text(&visible).contains("Priority saved"));
    let snapshot = server.snapshot().await;
    persist_evidence("customer-update", &server, &runtime).await?;
    assert_eq!(snapshot.atlas_priority, "high");
    assert_effect_count("priority update", snapshot.priority_updates, 1)?;
    assert_journal_terminal_once(runtime.journal_path())?;
    runtime.mark_completed("customer-update")?;
    Ok(())
}

#[tokio::test]
async fn validated_onboarding_preserves_accepted_values() -> TestResult<()> {
    let server = ScenarioServer::start(ScenarioConfig::seeded("onboarding")).await?;
    let runtime = ModernRuntime::launch(&server, Journey::Onboarding).await?;
    for (selector, value) in [
        ("input[aria-label='Full name']", "Maya Chen"),
        ("input[aria-label='Work email']", "maya@atlas.example"),
        ("input[aria-label='Company name']", "Atlas Labs"),
        ("input[aria-label='Postal code']", "02110"),
    ] {
        runtime.type_text(selector, value).await?;
    }
    runtime.select_one("Plan", "growth").await?;
    runtime
        .wait_visible("select[aria-label='Billing cycle']")
        .await?;
    runtime.select_one("Billing cycle", "annual").await?;
    runtime
        .click(
            "form[aria-label='Customer onboarding'] button[type='submit']",
            true,
        )
        .await?;
    runtime
        .wait_visible("input[aria-label='Postal code'][aria-invalid='true']")
        .await?;
    let company = runtime
        .inspect(Some("input[aria-label='Company name']"))
        .await?;
    assert!(
        inspection_text(&company).contains("Atlas Labs")
            || inspection_html(&company).contains("Atlas Labs")
    );
    runtime
        .type_text("input[aria-label='Postal code']", "10001")
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
    let snapshot = server.snapshot().await;
    persist_evidence("onboarding", &server, &runtime).await?;
    assert_effect_count("onboarding record", snapshot.onboarding_records, 1)?;
    assert_eq!(
        snapshot.onboarding,
        Some(modern_gauntlet::scenario::OnboardingRecord {
            full_name: "Maya Chen".into(),
            email: "maya@atlas.example".into(),
            company_name: "Atlas Labs".into(),
            postal_code: "10001".into(),
            plan: "growth".into(),
            billing_cycle: "annual".into(),
        })
    );
    runtime.mark_completed("onboarding")?;
    Ok(())
}

#[tokio::test]
async fn document_upload_preview_and_confirmation_are_durable() -> TestResult<()> {
    let server = ScenarioServer::start(ScenarioConfig::seeded("documents")).await?;
    let runtime = ModernRuntime::launch(&server, Journey::Documents).await?;
    let fixture = runtime.fixture_path("approved-upload.txt");
    if let Err(error) = runtime
        .wait_visible("input[aria-label='Customer document']")
        .await
    {
        return Err(format!(
            "{error}; browser accessibility: {:?}",
            runtime.accessibility_snapshot().await?
        )
        .into());
    }
    runtime
        .upload("input[aria-label='Customer document']", &fixture)
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
    server.wait_for_preview_confirmation().await?;
    let snapshot = server.snapshot().await;
    let expected = hex::encode(Sha256::digest(std::fs::read(&fixture)?));
    persist_evidence("documents", &server, &runtime).await?;
    assert_eq!(snapshot.uploaded_sha256.as_deref(), Some(expected.as_str()));
    assert_eq!(snapshot.uploaded_customer_id.as_deref(), Some("cus_atlas"));
    assert_eq!(
        snapshot.uploaded_filename.as_deref(),
        Some("approved-upload.txt")
    );
    assert_eq!(snapshot.uploaded_media_type.as_deref(), Some("text/plain"));
    assert_effect_count("preview confirmation", snapshot.preview_confirmations, 1)?;
    runtime.mark_completed("documents")?;
    Ok(())
}

#[tokio::test]
async fn popup_authorization_survives_obstruction() -> TestResult<()> {
    let server = ScenarioServer::start(ScenarioConfig::seeded("authorization")).await?;
    let runtime = ModernRuntime::launch(&server, Journey::Authorization).await?;
    runtime
        .wait_visible("button[aria-label='Connect Ledger Cloud']")
        .await?;
    let popup = runtime
        .click_popup("button[aria-label='Connect Ledger Cloud']")
        .await?;
    runtime.click_on(&popup, "#authorize").await?;
    runtime.wait_visible("[data-connected='true']").await?;
    assert_eq!(runtime.page_count().await?, 1, "authorization popup leaked");
    runtime
        .click(
            "button[aria-label='Dismiss notification preferences']",
            false,
        )
        .await?;
    let snapshot = server.snapshot().await;
    persist_evidence("authorization", &server, &runtime).await?;
    assert_effect_count("authorization grant", snapshot.authorization_grants, 1)?;
    runtime.mark_completed("authorization")?;
    Ok(())
}

#[tokio::test]
async fn interrupted_report_recovers_once_and_downloads() -> TestResult<()> {
    let server = ScenarioServer::start(ScenarioConfig::seeded("report-recovery")).await?;
    let runtime = ModernRuntime::launch(&server, Journey::ReportRecovery).await?;
    let workflow_id = runtime
        .click_boundary_with_workflow("form[aria-label='Generate report'] button")
        .await?;
    server.wait_for_report_generation().await?;
    let (runtime, recovery) = runtime
        .restart_and_recover(&workflow_id, &server.application_url("/reports"))
        .await?;
    assert!(
        matches!(
            recovery,
            RecoveryDecision::Resumed { .. } | RecoveryDecision::Restarted { .. }
        ),
        "recovery did not resume or restart from the verified checkpoint: {recovery:?}"
    );
    if let Err(error) = runtime
        .wait_visible("a[download='atlas-operations.csv']")
        .await
    {
        return Err(format!(
            "{error}; recovered page: {:?}",
            runtime.inspect(None).await?
        )
        .into());
    }
    let evidence = runtime
        .click_download("a[download='atlas-operations.csv']")
        .await?;
    let (path, digest) = evidence
        .iter()
        .find_map(|item| match item {
            Evidence::Download { path, sha256, .. } => Some((path, sha256)),
            _ => None,
        })
        .ok_or("download command completed without download evidence")?;
    persist_evidence("report-recovery", &server, &runtime).await?;
    assert_file_digest(std::path::Path::new(path), digest)?;
    assert_eq!(
        std::fs::read_to_string(path)?,
        "customer,priority\nAtlas Labs,high\n"
    );
    let snapshot = server.snapshot().await;
    assert_effect_count("report generation", snapshot.report_generations, 1)?;
    assert_journal_terminal_once(runtime.journal_path())?;
    runtime.mark_completed("report-recovery")?;
    Ok(())
}

fn inspection_text(evidence: &[Evidence]) -> String {
    evidence
        .iter()
        .find_map(|item| match item {
            Evidence::Inspection { text, .. } => Some(text.clone()),
            _ => None,
        })
        .unwrap_or_default()
}

fn inspection_html(evidence: &[Evidence]) -> String {
    evidence
        .iter()
        .find_map(|item| match item {
            Evidence::Inspection { html, .. } => html.clone(),
            _ => None,
        })
        .unwrap_or_default()
}

async fn persist_evidence(
    journey: &str,
    server: &ScenarioServer,
    runtime: &ModernRuntime,
) -> TestResult<()> {
    runtime.capture_diagnostics(journey).await?;
    let bundle = EvidenceBundle::create(journey, server.run_id())?;
    bundle.write_json("server-state.json", &server.snapshot().await)?;
    bundle.write_json("request-log.json", &server.request_log().await)?;
    bundle.write_json("run-manifest.json", &serde_json::json!({ "journey": journey, "runId": server.run_id(), "browser": "installed-chromium", "console": "unavailable", "network": "request-log.json" }))?;
    bundle.copy_if_present("commands.jsonl", runtime.journal_path())?;
    Ok(())
}
