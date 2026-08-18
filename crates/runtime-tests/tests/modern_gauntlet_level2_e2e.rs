#[allow(dead_code)]
#[path = "modern_gauntlet/mod.rs"]
mod modern_gauntlet;

use modern_gauntlet::driver::{Journey, ModernRuntime};
use modern_gauntlet::evidence::{
    assert_effect_count, assert_journal_terminal_once, EvidenceBundle,
};
use modern_gauntlet::scenario::{ScenarioConfig, ScenarioServer};

type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

/// Level 2 is opt-in (`BOBBY_GAUNTLET_LEVEL=2` plus the reCAPTCHA keypair):
/// it drives a live reCAPTCHA widget and needs a vision assist endpoint, so
/// it can never join the mandatory release suite in
/// `modern_gauntlet_e2e.rs`.
#[tokio::test]
async fn level_two_onboarding_solves_recaptcha_and_completes() -> TestResult<()> {
    if std::env::var("BOBBY_GAUNTLET_LEVEL").as_deref() != Ok("2") {
        return Ok(());
    }
    let site_key = std::env::var("BOBBY_GAUNTLET_RECAPTCHA_SITE_KEY")
        .map_err(|_| "BOBBY_GAUNTLET_RECAPTCHA_SITE_KEY is required for Level 2")?;
    let secret = std::env::var("BOBBY_GAUNTLET_RECAPTCHA_SECRET")
        .map_err(|_| "BOBBY_GAUNTLET_RECAPTCHA_SECRET is required for Level 2")?;
    let config = ScenarioConfig::level_two("captcha-solve", site_key, secret)?;
    let server = ScenarioServer::start(config).await?;
    // `launch` already navigates to the journey's page ("/onboarding").
    let runtime = ModernRuntime::launch(&server, Journey::Onboarding).await?;

    // Level 2 trap: an interruption modal covers the form. "Open checkpoint"
    // spawns the checkpoint popup and lifts the backdrop; the popup itself is
    // informational and does not block the main page.
    runtime
        .click("section.interruption-dialog button[type='button']", false)
        .await?;

    // Fill all form fields
    for (selector, value) in [
        ("input[aria-label='Full name']", "Maya Chen"),
        ("input[aria-label='Work email']", "maya@atlas.example"),
        ("input[aria-label='Company name']", "Atlas Labs"),
        ("input[aria-label='Postal code']", "10001"),
    ] {
        runtime.type_text(selector, value).await?;
    }
    runtime.select_one("Plan", "growth").await?;
    runtime
        .wait_visible("select[aria-label='Billing cycle']")
        .await?;
    runtime.select_one("Billing cycle", "annual").await?;
    // Level 2's irregular form delays a confirmation field that must match.
    runtime
        .wait_visible("input[aria-label='Confirm work email']")
        .await?;
    runtime
        .type_text(
            "input[aria-label='Confirm work email']",
            "maya@atlas.example",
        )
        .await?;

    // Solve the reCAPTCHA challenge using vision-first intent
    let solve = runtime
        .solve_challenge("solve the reCAPTCHA challenge")
        .await?;
    println!("Solve challenge evidence: {:?}", solve.len());

    // Submit the form — backend verifies reCAPTCHA token from the widget callback
    runtime
        .click(
            "form[aria-label='Customer onboarding'] button[type='submit']",
            true,
        )
        .await?;
    runtime
        .wait_visible("form[aria-label='Customer onboarding'] [role='status']")
        .await?;

    // Verify the onboarding was accepted (reCAPTCHA token passed verification)
    let snapshot = server.snapshot().await;
    persist_evidence("onboarding-captcha", &server, &runtime).await?;
    assert_effect_count("onboarding record", snapshot.onboarding_records, 1)?;

    if let Some(ref onb) = snapshot.onboarding {
        assert_eq!(onb.full_name, "Maya Chen");
    } else {
        panic!("Onboarding record not saved despite successful submission");
    }

    assert_journal_terminal_once(runtime.journal_path())?;
    runtime.mark_completed("onboarding-captcha")?;
    Ok(())
}

async fn persist_evidence(
    journey: &str,
    server: &ScenarioServer,
    runtime: &ModernRuntime,
) -> TestResult<()> {
    let scorecard = runtime.emit_scorecard(true)?;
    runtime.capture_diagnostics(journey).await?;
    let bundle = EvidenceBundle::create(journey, server.run_id())?;
    bundle.write_json("server-state.json", &server.snapshot().await)?;
    bundle.write_json("request-log.json", &server.request_log().await)?;
    bundle.write_json("scorecard.json", &scorecard)?;
    bundle.write_json("run-manifest.json", &serde_json::json!({ "journey": journey, "runId": server.run_id(), "browser": "installed-chromium", "console": "unavailable", "network": "request-log.json" }))?;
    bundle.copy_if_present("commands.jsonl", runtime.journal_path())?;
    Ok(())
}
