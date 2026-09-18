#[path = "modern_gauntlet/scorecard.rs"]
mod scorecard;

use std::io::Write;

use scorecard::{ContextSource, FailureTaxonomy, ModelTier, ProviderMode, Scorecard, VisionSource};

fn release_scorecard(station: &str) -> Scorecard {
    let budget = scorecard::release_budget_for(station, "chromium").expect("release budget");
    Scorecard {
        station: station.to_string(),
        engine: "chromium".to_string(),
        provider_mode: ProviderMode::Unknown,
        model_tier: ModelTier::Deterministic,
        context_source: ContextSource::None,
        vision_source: VisionSource::None,
        passed: true,
        tool_calls: budget.max_tool_calls,
        action_count: budget.max_action_count,
        wall_ms: 1,
        snapshots_taken: budget.max_snapshots,
        vision_escalations_attempted: 0,
        vision_escalations_accepted: 0,
        failed_commands: 0,
        failure_taxonomy: FailureTaxonomy::default(),
    }
}

#[test]
fn scorecard_counts_commands_snapshots_and_vision_outcomes() {
    let mut journal = tempfile::NamedTempFile::new().unwrap();
    for record in [
        r#"{"sequence":0,"recordedAt":"2026-08-09T12:00:00Z","commandId":"navigate","phase":"accepted","envelope":{"command":{"kind":"primitive","input":{"kind":"navigate"}}}}"#,
        r#"{"sequence":1,"recordedAt":"2026-08-09T12:00:00.040Z","commandId":"navigate","phase":"completed","outcome":{"status":"completed","evidence":[]}}"#,
        r#"{"sequence":2,"recordedAt":"2026-08-09T12:00:00.050Z","commandId":"snapshot","phase":"accepted","envelope":{"command":{"kind":"primitive","input":{"kind":"captureScreenshot"}}}}"#,
        r#"{"sequence":3,"recordedAt":"2026-08-09T12:00:00.080Z","commandId":"snapshot","phase":"completed","outcome":{"status":"completed","evidence":[{"kind":"screenshot"}]}}"#,
        r#"{"sequence":4,"recordedAt":"2026-08-09T12:00:00.100Z","commandId":"vision","phase":"accepted","envelope":{"command":{"kind":"intent"}}}"#,
        r#"{"sequence":5,"recordedAt":"2026-08-09T12:00:00.130Z","commandId":"vision","phase":"completed","outcome":{"status":"completed","evidence":[{"kind":"intentExecution","record":{"resolutionPath":"visionFallback","verification":"filled"}}]}}"#,
    ] {
        writeln!(journal, "{record}").unwrap();
    }

    let scorecard =
        Scorecard::from_journal("onboarding", "chromium", journal.path(), true).unwrap();

    assert_eq!(scorecard.station, "onboarding");
    assert_eq!(scorecard.engine, "chromium");
    assert!(scorecard.passed);
    assert_eq!(scorecard.tool_calls, 3);
    assert_eq!(scorecard.action_count, 2);
    assert_eq!(scorecard.wall_ms, 130);
    assert_eq!(scorecard.snapshots_taken, 1);
    assert_eq!(scorecard.vision_escalations_attempted, 1);
    assert_eq!(scorecard.vision_escalations_accepted, 1);
    assert_eq!(scorecard.vision_source, scorecard::VisionSource::Fallback);
    assert_eq!(scorecard.context_source, scorecard::ContextSource::None);
}

#[test]
fn scorecard_rejects_malformed_journal_lines() {
    let mut journal = tempfile::NamedTempFile::new().unwrap();
    writeln!(journal, "not json").unwrap();

    let error = Scorecard::from_journal("broken", "chromium", journal.path(), false)
        .unwrap_err()
        .to_string();

    assert!(error.contains("journal line 1"), "{error}");
}

#[test]
fn release_budgets_cover_every_canonical_journey() {
    for station in [
        "session",
        "customer-update",
        "onboarding",
        "documents",
        "authorization",
        "checkout",
        "report-recovery",
    ] {
        assert!(
            scorecard::release_budget_for(station, "chromium").is_some(),
            "missing release budget for {station}"
        );
    }
    assert!(scorecard::release_budget_for("unknown", "chromium").is_none());
    assert!(scorecard::release_budget_for("session", "firefox").is_none());
}

#[test]
fn release_budget_accepts_the_limit_and_rejects_each_regression() {
    let scorecard = release_scorecard("onboarding");
    scorecard.enforce_release_budget().unwrap();

    for (label, regressed) in [
        (
            "toolCalls",
            Scorecard {
                tool_calls: scorecard.tool_calls + 1,
                ..scorecard.clone()
            },
        ),
        (
            "actionCount",
            Scorecard {
                action_count: scorecard.action_count + 1,
                ..scorecard.clone()
            },
        ),
        (
            "snapshotsTaken",
            Scorecard {
                snapshots_taken: scorecard.snapshots_taken + 1,
                ..scorecard.clone()
            },
        ),
        (
            "failedCommands",
            Scorecard {
                failed_commands: 1,
                ..scorecard.clone()
            },
        ),
    ] {
        let error = regressed.enforce_release_budget().unwrap_err().to_string();
        assert!(error.contains(label), "{error}");
        assert!(error.contains("onboarding"), "{error}");
    }
}

#[test]
fn assisted_runs_require_source_attribution() {
    let mut scorecard = release_scorecard("onboarding");
    scorecard.vision_escalations_attempted = 1;
    let error = scorecard.enforce_release_budget().unwrap_err().to_string();
    assert!(error.contains("visionSource"), "{error}");

    scorecard.vision_source = VisionSource::Prefill;
    scorecard.vision_escalations_accepted = 1;
    scorecard.enforce_release_budget().unwrap();
}

#[test]
fn remembered_site_gate_requires_strictly_fewer_calls() {
    scorecard::enforce_remembered_site_reduction("onboarding", 12, 11).unwrap();
    for remembered in [12, 13] {
        let error = scorecard::enforce_remembered_site_reduction("onboarding", 12, remembered)
            .unwrap_err()
            .to_string();
        assert!(error.contains("rememberedCalls"), "{error}");
        assert!(error.contains("coldCalls=12"), "{error}");
    }
}
