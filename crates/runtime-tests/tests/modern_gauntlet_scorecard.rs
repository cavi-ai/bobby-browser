#[path = "modern_gauntlet/scorecard.rs"]
mod scorecard;

use std::io::Write;

use scorecard::Scorecard;

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
    assert_eq!(scorecard.wall_ms, 130);
    assert_eq!(scorecard.snapshots_taken, 1);
    assert_eq!(scorecard.vision_escalations_attempted, 1);
    assert_eq!(scorecard.vision_escalations_accepted, 1);
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
