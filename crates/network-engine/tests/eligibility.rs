use network_engine::{EligibilityDecision, EligibilityPolicy, NetworkPolicy};
use types::{
    CaptureScreenshotCommand, ClickAndWaitForDownloadCommand, DownloadUrlCommand, ExecutionReason,
    InspectCommand, PrimitiveCommand, ScreenshotMode, TargetSpec,
};

fn policy() -> EligibilityPolicy {
    EligibilityPolicy::new(NetworkPolicy {
        max_download_bytes: 2 * 1_048_576,
        ..NetworkPolicy::default()
    })
}

fn inspect(selector: Option<&str>, include_html: bool) -> PrimitiveCommand {
    PrimitiveCommand::Inspect(InspectCommand {
        selector: selector.map(str::to_owned),
        target: None,
        include_html,
    })
}

fn inspect_with_semantic_target() -> PrimitiveCommand {
    PrimitiveCommand::Inspect(InspectCommand {
        target: Some(TargetSpec {
            role: Some("heading".into()),
            ..TargetSpec::default()
        }),
        ..InspectCommand::default()
    })
}

fn assert_direct(command: PrimitiveCommand) {
    assert!(matches!(
        policy().classify(&command, "https://example.test/report"),
        EligibilityDecision::DirectHttp(_)
    ));
}

fn assert_chromium(command: PrimitiveCommand, expected: ExecutionReason) {
    assert!(matches!(
        policy().classify(&command, "https://example.test/report"),
        EligibilityDecision::Chromium(reason) if reason == expected
    ));
}

fn assert_denied(command: PrimitiveCommand, page_url: &str) {
    assert!(matches!(
        policy().classify(&command, page_url),
        EligibilityDecision::Denied(_)
    ));
}

#[test]
fn classifies_the_required_command_matrix() {
    assert_direct(inspect(None, false));
    assert_direct(inspect(Some("#report"), true));
    assert_chromium(
        inspect_with_semantic_target(),
        ExecutionReason::SemanticTargetRequired,
    );
    assert_chromium(
        PrimitiveCommand::CaptureScreenshot(CaptureScreenshotCommand {
            mode: ScreenshotMode::Viewport,
        }),
        ExecutionReason::IneligibleCommand,
    );
    assert_direct(PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
        url: "https://example.test/report.bin".into(),
        expected_content_type: Some("application/octet-stream".into()),
        max_bytes: 1_048_576,
    }));
    assert_chromium(
        PrimitiveCommand::ClickAndWaitForDownload(ClickAndWaitForDownloadCommand {
            selector: "#download".into(),
            target: None,
            timeout_ms: 5_000,
        }),
        ExecutionReason::IneligibleCommand,
    );
}

#[test]
fn rejects_unsafe_urls_and_download_limits() {
    assert_denied(inspect(None, false), "file:///etc/passwd");
    assert_denied(inspect(None, false), "data:text/plain,secret");
    assert_denied(inspect(None, false), "https://user:pass@example.test/");

    for max_bytes in [0, 2 * 1_048_576 + 1] {
        assert_denied(
            PrimitiveCommand::DownloadUrl(DownloadUrlCommand {
                url: "https://example.test/report.bin".into(),
                expected_content_type: None,
                max_bytes,
            }),
            "https://example.test/",
        );
    }
}
