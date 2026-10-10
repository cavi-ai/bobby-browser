//! Pointer command verification and its local predicates.
use super::*;
pub(super) struct Pointer;
#[async_trait::async_trait]
impl CommandVerifier for Pointer {
    async fn verify(
        &self,
        context: VerificationContext<'_>,
        command: &PrimitiveCommand,
        evidence: Vec<Evidence>,
    ) -> Result<Vec<Evidence>, CommandError> {
        let envelope = context.envelope;
        let page_id = envelope.page_id.as_ref();
        let lease = context.lease;
        match command {
            PrimitiveCommand::Click(command) => {
                if let Some(expected_url) = &command.expected_url {
                    let page_id = page_id.expect("validated page id");
                    // Settle wait: its failure is tolerated because the
                    // inspect below is the real verification, but it must be
                    // logged -- a silently skipped wait reads as a fast,
                    // flaky expected-URL failure.
                    if let Err(error) = lease
                        .worker()
                        .wait_for(
                            page_id,
                            &WaitForCommand {
                                condition: WaitCondition::Url {
                                    matcher: TextMatch::Exact(expected_url.clone()),
                                },
                                timeout_ms: 5_000,
                            },
                        )
                        .await
                    {
                        tracing::warn!(
                            error = %error.message,
                            "expected-URL settle wait failed before click verification"
                        );
                    }
                    let verification = lease
                        .worker()
                        .inspect(page_id, &InspectCommand::default())
                        .await?;
                    let matches = verification.iter().any(|item| {
                        matches!(item, Evidence::Inspection { url, .. } if url == expected_url)
                    });
                    if !matches {
                        return Err(verification_error("click did not reach expected URL"));
                    }
                    let mut combined = evidence;
                    combined.extend(verification);
                    Ok(combined)
                } else if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Element { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("click returned no target evidence"))
                }
            }
            PrimitiveCommand::ClickAndWaitForPopup(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Popup { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "popup command returned no popup evidence",
                    ))
                }
            }
            PrimitiveCommand::ClickAndWaitForDownload(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Download { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "download command returned no download evidence",
                    ))
                }
            }
            _ => Err(verification_error(
                "command was routed to the wrong verifier family",
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{command, Fixture};
    use super::*;
    use serde_json::json;
    use std::sync::atomic::Ordering;

    #[tokio::test]
    async fn click_expected_url_uses_read_back_even_when_settle_wait_fails() {
        let fixture = Fixture::new().await;
        let click = command(
            "click",
            json!({"selector":"button", "boundary":true,"expectedUrl":"https://example.test/final"}),
        );
        let inspection = Evidence::Inspection {
            selector: None,
            url: "https://example.test/final".into(),
            title: "Final".into(),
            text: "".into(),
            html: None,
        };
        *fixture.worker.inspection.lock().unwrap() = vec![inspection.clone()];
        let original = Evidence::Element {
            selector: "button".into(),
            text: None,
        };
        assert_eq!(
            fixture
                .verify(&Pointer, &click, vec![original.clone()])
                .await
                .unwrap(),
            vec![original, inspection]
        );
        assert_eq!(fixture.worker.waits.load(Ordering::SeqCst), 1);
        fixture.worker.inspection.lock().unwrap().clear();
        assert_eq!(
            fixture
                .verify(&Pointer, &click, vec![])
                .await
                .unwrap_err()
                .code,
            ErrorCode::VerificationFailed
        );
    }

    #[tokio::test]
    async fn pointer_commands_require_their_own_evidence_kind() {
        let fixture = Fixture::new().await;
        let click = command("click", json!({"selector":"button","boundary":false}));
        let download = command(
            "clickAndWaitForDownload",
            json!({"selector":"button","timeoutMs":1000}),
        );
        let popup = command(
            "clickAndWaitForPopup",
            json!({"selector":"button","timeoutMs":1000}),
        );
        let element = Evidence::Element {
            selector: "button".into(),
            text: None,
        };
        assert!(fixture
            .verify(&Pointer, &click, vec![element.clone()])
            .await
            .is_ok());
        for command in [&click, &download, &popup] {
            assert_eq!(
                fixture
                    .verify(&Pointer, command, vec![])
                    .await
                    .unwrap_err()
                    .code,
                ErrorCode::VerificationFailed
            );
        }
        assert!(fixture
            .verify(&Pointer, &popup, vec![element])
            .await
            .is_err());
        assert!(fixture
            .verify(
                &Pointer,
                &popup,
                vec![Evidence::Popup {
                    opener_page_id: fixture.envelope.page_id.clone().unwrap(),
                    page_id: types::PageId::new(),
                    url: "https://example.test/popup".into(),
                    title: "Popup".into(),
                }]
            )
            .await
            .is_ok());
        assert!(fixture
            .verify(
                &Pointer,
                &download,
                vec![Evidence::Download {
                    filename: "file.bin".into(),
                    path: "artifact://file".into(),
                    bytes: 7,
                    sha256: "a".repeat(64),
                    saved_to: None,
                }]
            )
            .await
            .is_ok());
        assert!(fixture
            .verify(
                &Pointer,
                &PrimitiveCommand::Inspect(InspectCommand::default()),
                vec![]
            )
            .await
            .is_err());
    }
}
