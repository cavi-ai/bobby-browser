#[path = "modern_gauntlet/mod.rs"]
mod modern_gauntlet;

use std::collections::BTreeSet;

use modern_gauntlet::driver::{Journey, ModernRuntime};
use modern_gauntlet::evidence::{
    assert_effect_count, assert_file_digest, assert_journal_terminal_once, EvidenceBundle,
};
use modern_gauntlet::scenario::{
    ScenarioConfig, ScenarioServer, MFA_CODE, OPERATOR_EMAIL, OPERATOR_PASSWORD, THREE_DS_CODE,
};
use sha2::{Digest, Sha256};
use types::{ControlAction, Evidence, RecoveryDecision, TextMatch, WaitCondition, WaitForCommand};

type TestResult<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

const REQUIRED_JOURNEYS: [&str; 7] = [
    "session_survives_cmp_login_and_mfa",
    "customer_discovery_and_update_is_durable",
    "validated_onboarding_preserves_accepted_values",
    "document_upload_preview_and_confirmation_are_durable",
    "popup_authorization_survives_obstruction",
    "checkout_address_calendar_and_3ds_charge_is_durable",
    "interrupted_report_recovers_once_and_downloads",
];

#[test]
fn release_suite_names_are_stable() {
    assert_eq!(REQUIRED_JOURNEYS.len(), 7);
    assert_eq!(
        REQUIRED_JOURNEYS
            .iter()
            .copied()
            .collect::<BTreeSet<_>>()
            .len(),
        7
    );
    let source = include_str!("modern_gauntlet_e2e.rs");
    assert_eq!(
        source.matches("#[tokio::test]\nasync fn ").count(),
        7,
        "release file must contain exactly seven Tokio browser tests"
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
async fn session_survives_cmp_login_and_mfa() -> TestResult<()> {
    let server = ScenarioServer::start(ScenarioConfig::seeded("session")).await?;
    let runtime = ModernRuntime::launch(&server, Journey::Session).await?;
    runtime
        .dismiss("Accept all cookies", "Accept all cookies")
        .await?;
    runtime
        .complete_form(
            "Operator sign in",
            vec![
                ModernRuntime::text_field("Work email", "Work email", OPERATOR_EMAIL),
                ModernRuntime::text_field("Password", "Password", OPERATOR_PASSWORD),
            ],
        )
        .await?;
    runtime
        .submit_and_verify(
            "Continue",
            "Continue",
            ModernRuntime::wait_named_cmd("textbox", "Authentication code"),
        )
        .await?;
    runtime
        .fill_named(
            "Authentication code",
            "textbox",
            "Authentication code",
            ControlAction::SetText {
                value: MFA_CODE.into(),
                clear_first: true,
            },
        )
        .await?;
    runtime
        .submit_and_verify(
            "Verify code",
            "Verify code",
            ModernRuntime::wait_named_cmd("navigation", "Primary navigation"),
        )
        .await?;
    let snapshot = server.snapshot().await;
    persist_evidence("session", &server, &runtime).await?;
    assert_eq!(snapshot.consent.as_deref(), Some("accept"));
    assert_eq!(snapshot.session_email.as_deref(), Some(OPERATOR_EMAIL));
    assert!(snapshot.mfa_completions >= 1);
    runtime.mark_completed("session")?;
    Ok(())
}

#[tokio::test]
async fn customer_discovery_and_update_is_durable() -> TestResult<()> {
    let server = ScenarioServer::start(ScenarioConfig::seeded("customer-update")).await?;
    let runtime = ModernRuntime::launch(&server, Journey::CustomerUpdate).await?;
    runtime
        .type_named("combobox", "Search customers", "Atlas")
        .await?;
    runtime
        .follow(
            "Search",
            "button",
            "Search",
            ModernRuntime::wait_named_cmd("option", "Atlas Labs"),
            false,
        )
        .await?;
    runtime
        .follow(
            "Atlas Labs",
            "option",
            "Atlas Labs",
            ModernRuntime::wait_named_cmd("link", "Atlas Labs"),
            false,
        )
        .await?;
    if let Err(error) = runtime.wait_named("link", "Atlas Labs").await {
        let diagnostic = runtime
            .accessibility_snapshot()
            .await
            .map(|value| format!("{value:?}"))
            .unwrap_or_else(|diagnostic_error| format!("unavailable: {diagnostic_error}"));
        return Err(format!("{error}; browser accessibility: {diagnostic}").into());
    }
    runtime
        .follow(
            "Atlas Labs",
            "link",
            "Atlas Labs",
            wait_url("/customers/cus_atlas"),
            false,
        )
        .await?;
    runtime.wait_named("combobox", "Customer priority").await?;
    runtime.choose_option("Customer priority", "High").await?;
    runtime
        .submit_and_verify(
            "Save priority",
            "Save priority",
            wait_status_named("Priority saved"),
        )
        .await?;
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
    runtime
        .complete_form(
            "Customer identity",
            vec![
                ModernRuntime::text_field("Full name", "Full name", "Maya Chen"),
                ModernRuntime::text_field("Work email", "Work email", "maya@atlas.example"),
            ],
        )
        .await?;
    runtime
        .follow(
            "Next",
            "button",
            "Next",
            ModernRuntime::wait_named_cmd("textbox", "Company name"),
            false,
        )
        .await?;
    runtime
        .complete_form(
            "Company details",
            vec![
                ModernRuntime::text_field("Company name", "Company name", "Atlas Labs"),
                ModernRuntime::text_field("Postal code", "Postal code", "02110"),
            ],
        )
        .await?;
    runtime
        .follow(
            "Back",
            "button",
            "Back",
            ModernRuntime::wait_named_cmd("textbox", "Full name"),
            false,
        )
        .await?;
    let identity = runtime
        .inspect(Some("input[aria-label='Full name']"))
        .await?;
    assert!(
        inspection_text(&identity).contains("Maya Chen")
            || inspection_html(&identity).contains("Maya Chen")
    );
    runtime
        .follow(
            "Next",
            "button",
            "Next",
            ModernRuntime::wait_named_cmd("textbox", "Company name"),
            false,
        )
        .await?;
    let company = runtime
        .inspect(Some("input[aria-label='Company name']"))
        .await?;
    assert!(
        inspection_text(&company).contains("Atlas Labs")
            || inspection_html(&company).contains("Atlas Labs")
    );
    runtime
        .follow(
            "Next",
            "button",
            "Next",
            ModernRuntime::wait_named_cmd("combobox", "Plan"),
            false,
        )
        .await?;
    runtime.select_one("Plan", "growth").await?;
    runtime.wait_named("combobox", "Billing cycle").await?;
    runtime.select_one("Billing cycle", "annual").await?;
    runtime
        .submit_and_verify(
            "Create customer",
            "Create customer",
            WaitForCommand {
                condition: WaitCondition::Element {
                    target: Box::new(types::TargetSpec {
                        css: Some("input[aria-label='Postal code'][aria-invalid='true']".into()),
                        ..types::TargetSpec::default()
                    }),
                    state: types::ElementState::Visible,
                },
                timeout_ms: 10_000,
            },
        )
        .await?;
    runtime
        .fill_named(
            "Postal code",
            "textbox",
            "Postal code",
            ControlAction::SetText {
                value: "10001".into(),
                clear_first: true,
            },
        )
        .await?;
    runtime
        .follow(
            "Next",
            "button",
            "Next",
            ModernRuntime::wait_named_cmd("combobox", "Plan"),
            false,
        )
        .await?;
    runtime
        .submit_and_verify(
            "Create customer",
            "Create customer",
            wait_status("Customer created"),
        )
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
    if let Err(error) = runtime.wait_named("button", "Upload document").await {
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
        .submit_and_verify(
            "Upload document",
            "Upload document",
            wait_status("Upload complete"),
        )
        .await?;
    runtime
        .wait_named("group", "Document preview widget")
        .await?;
    runtime
        .wait_shadow_named(
            "group",
            "Document preview widget",
            "button",
            "Confirm document preview",
        )
        .await?;
    runtime
        .click_shadow_named(
            "group",
            "Document preview widget",
            "button",
            "Confirm document preview",
            true,
        )
        .await?;
    runtime.wait_named("status", "Document confirmed").await?;
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
    runtime.wait_named("button", "Connect Ledger Cloud").await?;
    let popup = runtime
        .click_popup_named("button", "Connect Ledger Cloud")
        .await?;
    runtime.click_on(&popup, "#authorize").await?;
    runtime.wait_visible("[data-connected='true']").await?;
    assert_eq!(runtime.page_count().await?, 1, "authorization popup leaked");
    runtime
        .dismiss("Dismiss notification", "Dismiss notification")
        .await?;
    runtime
        .dismiss("Dismiss workspace assistant", "Dismiss workspace assistant")
        .await?;
    let snapshot = server.snapshot().await;
    persist_evidence("authorization", &server, &runtime).await?;
    assert_effect_count("authorization grant", snapshot.authorization_grants, 1)?;
    runtime.mark_completed("authorization")?;
    Ok(())
}

#[tokio::test]
async fn checkout_address_calendar_and_3ds_charge_is_durable() -> TestResult<()> {
    let server = ScenarioServer::start(ScenarioConfig::seeded("checkout")).await?;
    let runtime = ModernRuntime::launch(&server, Journey::Checkout).await?;
    runtime.select_one("Plan", "growth").await?;
    runtime
        .type_named("combobox", "Billing address", "Federal")
        .await?;
    runtime
        .follow(
            "Atlas Labs Boston",
            "option",
            "Atlas Labs Boston",
            wait_value("textbox", "Street", "Federal"),
            false,
        )
        .await?;
    runtime
        .follow(
            "2026-01-06",
            "gridcell",
            "2026-01-06",
            ModernRuntime::wait_named_cmd("gridcell", "2026-01-06"),
            false,
        )
        .await?;
    runtime
        .follow(
            "2026-01-20",
            "gridcell",
            "2026-01-20",
            ModernRuntime::wait_named_cmd("gridcell", "2026-01-20"),
            false,
        )
        .await?;
    runtime.wait_named("iframe", "Card details").await?;
    runtime
        .wait_in_named_frame("Card details", "textbox", "Card number")
        .await?;
    runtime
        .type_in_frame("Card details", "Card number", "4242424242424242")
        .await?;
    runtime
        .type_in_frame("Card details", "Expiry", "12/28")
        .await?;
    runtime.type_in_frame("Card details", "CVC", "123").await?;
    runtime
        .click_in_named_frame("Card details", "button", "Save card")
        .await?;
    runtime.wait_named("iframe", "3-D Secure challenge").await?;
    runtime
        .wait_in_named_frame("3-D Secure challenge", "textbox", "Challenge code")
        .await?;
    runtime
        .type_in_frame("3-D Secure challenge", "Challenge code", THREE_DS_CODE)
        .await?;
    runtime
        .click_in_named_frame("3-D Secure challenge", "button", "Verify payment")
        .await?;
    runtime
        .submit_and_verify(
            "Charge Atlas Labs",
            "Charge Atlas Labs",
            wait_status("Charged 8400 cents"),
        )
        .await?;
    let snapshot = server.snapshot().await;
    persist_evidence("checkout", &server, &runtime).await?;
    assert_eq!(snapshot.three_ds_completions, 1);
    assert_eq!(
        snapshot.charge.as_ref().map(|charge| charge.amount_cents),
        Some(8400)
    );
    assert_eq!(
        snapshot
            .billing_address
            .as_ref()
            .map(|address| address.postal_code.as_str()),
        Some("02110")
    );
    assert_eq!(
        snapshot
            .billing_period
            .as_ref()
            .map(|period| (period.start.as_str(), period.end.as_str())),
        Some(("2026-01-06", "2026-01-20"))
    );
    runtime.mark_completed("checkout")?;
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

fn wait_url(needle: &str) -> WaitForCommand {
    WaitForCommand {
        condition: WaitCondition::Url {
            matcher: TextMatch::Contains(needle.into()),
        },
        timeout_ms: 10_000,
    }
}

fn wait_status(contains: &str) -> WaitForCommand {
    WaitForCommand {
        condition: WaitCondition::Text {
            target: Box::new(types::TargetSpec {
                role: Some("status".into()),
                ..types::TargetSpec::default()
            }),
            matcher: TextMatch::Contains(contains.into()),
        },
        timeout_ms: 10_000,
    }
}

fn wait_status_named(name: &str) -> WaitForCommand {
    WaitForCommand {
        condition: WaitCondition::Text {
            target: Box::new(types::TargetSpec {
                role: Some("status".into()),
                accessible_name: Some(name.into()),
                ..types::TargetSpec::default()
            }),
            matcher: TextMatch::Exact(name.into()),
        },
        timeout_ms: 10_000,
    }
}

fn wait_value(role: &str, name: &str, contains: &str) -> WaitForCommand {
    WaitForCommand {
        condition: WaitCondition::Value {
            target: Box::new(ModernRuntime::named_target(role, name)),
            matcher: TextMatch::Contains(contains.into()),
        },
        timeout_ms: 10_000,
    }
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
    let scorecard = runtime.emit_scorecard(true)?;
    runtime.capture_diagnostics(journey).await?;
    let bundle = EvidenceBundle::create(journey, server.run_id())?;
    bundle.write_json("server-state.json", &server.snapshot().await)?;
    bundle.write_json("request-log.json", &server.request_log().await)?;
    bundle.write_json("scorecard.json", &scorecard)?;
    bundle.write_json("run-manifest.json", &serde_json::json!({ "journey": journey, "runId": server.run_id(), "browser": "installed-chromium", "console": "unavailable", "network": "request-log.json" }))?;
    bundle.copy_if_present("commands.jsonl", runtime.journal_path())?;
    scorecard.enforce_release_budget()?;
    Ok(())
}
