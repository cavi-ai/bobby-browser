//! State command verification and its local predicates.
use super::*;
pub(super) struct State;
#[async_trait::async_trait]
impl CommandVerifier for State {
    async fn verify(
        &self,
        _context: VerificationContext<'_>,
        command: &PrimitiveCommand,
        evidence: Vec<Evidence>,
    ) -> Result<Vec<Evidence>, CommandError> {
        match command {
            PrimitiveCommand::Inspect(_) => {
                if evidence.is_empty() {
                    Err(verification_error("inspection returned no evidence"))
                } else {
                    Ok(evidence)
                }
            }
            PrimitiveCommand::AccessibilitySnapshot(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::AccessibilitySnapshot { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "accessibility snapshot returned no snapshot evidence",
                    ))
                }
            }
            PrimitiveCommand::NetworkLog(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::HarArtifact { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("network log returned no HAR artifact"))
                }
            }
            PrimitiveCommand::Emulate(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Emulation { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("emulate command returned no emulation evidence"))
                }
            }
            PrimitiveCommand::HandleDialog(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Dialog { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("dialog command returned no dialog evidence"))
                }
            }
            PrimitiveCommand::PrintToPdf(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::PdfArtifact { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("PDF command returned no PDF artifact"))
                }
            }
            PrimitiveCommand::GetCookies(_)
            | PrimitiveCommand::SetCookies(_)
            | PrimitiveCommand::DeleteCookies(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::CookieState { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("cookie command returned no cookie state"))
                }
            }
            PrimitiveCommand::ExtractStructured(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::StructuredExtraction { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "structured extraction returned no extraction evidence",
                    ))
                }
            }
            PrimitiveCommand::ActivatePage(_) => {
                if evidence.iter().any(|item| {
                    matches!(
                        item,
                        Evidence::Page { .. } | Evidence::BrowserExecution { .. }
                    )
                }) {
                    Ok(evidence)
                } else {
                    Err(verification_error("page activation returned no page evidence"))
                }
            }
            PrimitiveCommand::OpenPage(_) | PrimitiveCommand::ClosePage(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Page { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("page command returned no page evidence"))
                }
            }
            PrimitiveCommand::ListPages(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Pages { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("page listing returned no evidence"))
                }
            }
            PrimitiveCommand::DownloadUrl(_) => {
                let download = evidence.iter().find_map(|item| match item {
                    Evidence::Download { bytes, sha256, .. } => Some((*bytes, sha256)),
                    _ => None,
                });
                let execution = evidence.iter().find_map(|item| match item {
                    Evidence::ExecutionPath { bytes, sha256, .. } => Some((*bytes, sha256)),
                    _ => None,
                });
                match (download, execution) {
                    (
                        Some((download_bytes, download_sha)),
                        Some((Some(exec_bytes), Some(exec_sha))),
                    ) if download_bytes == exec_bytes && download_sha == exec_sha => Ok(evidence),
                    _ => Err(verification_error(
                        "download lacks matching durable execution evidence",
                    )),
                }
            }
            PrimitiveCommand::CaptureScreenshot(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Screenshot { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "screenshot returned no artifact evidence",
                    ))
                }
            }
            PrimitiveCommand::SetFocusEmulation(command) => {
                if evidence.iter().any(|item| matches!(item, Evidence::Configuration { name, value } if name == "focusEmulation" && value == &command.enabled.to_string())) {
                    Ok(evidence)
                } else {
                    Err(verification_error("focus emulation returned no matching configuration evidence"))
                }
            }
            PrimitiveCommand::SetEmulatedMedia(command) => {
                let expected = serde_json::to_string(command).map_err(|_| verification_error("media configuration serialization failed"))?;
                if evidence.iter().any(|item| matches!(item, Evidence::Configuration { name, value } if name == "emulatedMedia" && value == &expected)) {
                    Ok(evidence)
                } else {
                    Err(verification_error("media emulation returned no matching configuration evidence"))
                }
            }
            PrimitiveCommand::EvaluateJavaScript(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::JavaScriptResult { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error(
                        "javascript evaluation returned no result evidence",
                    ))
                }
            }
            _ => Err(verification_error("command was routed to the wrong verifier family")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::{command, evidence, Fixture};
    use super::*;
    use serde_json::json;

    #[tokio::test]
    async fn download_requires_matching_durable_byte_count_and_digest() {
        let fixture = Fixture::new().await;
        let command = command(
            "downloadUrl",
            json!({"url":"https://example.test/file", "maxBytes":1024}),
        );
        let download = Evidence::Download {
            filename: "file.bin".into(),
            path: "artifact://file".into(),
            bytes: 7,
            sha256: "a".repeat(64),
            saved_to: None,
        };
        for (bytes, sha256, accepted) in [
            (Some(7), Some("a".repeat(64)), true),
            (Some(8), Some("a".repeat(64)), false),
            (Some(7), Some("b".repeat(64)), false),
            (None, None, false),
        ] {
            let execution = evidence(
                json!({"kind":"executionPath", "path":"directHttp", "reason":"eligibleExplicitDownload",
                "stateVersion":0, "elapsedMs":1, "bytes":bytes, "sha256":sha256}),
            );
            assert_eq!(
                fixture
                    .verify(&State, &command, vec![download.clone(), execution])
                    .await
                    .is_ok(),
                accepted
            );
        }
        assert!(fixture
            .verify(&State, &command, vec![download])
            .await
            .is_err());
    }

    #[tokio::test]
    async fn focus_and_media_require_exact_applied_configuration() {
        let fixture = Fixture::new().await;
        let focus = command("setFocusEmulation", json!({"enabled":false}));
        for (value, accepted) in [("false", true), ("true", false), ("", false)] {
            let evidence = vec![Evidence::Configuration {
                name: "focusEmulation".into(),
                value: value.into(),
            }];
            assert_eq!(
                fixture.verify(&State, &focus, evidence).await.is_ok(),
                accepted
            );
        }
        let media = command("setEmulatedMedia", json!({"media":"screen","features":{}}));
        let PrimitiveCommand::SetEmulatedMedia(input) = &media else {
            unreachable!()
        };
        let expected = serde_json::to_string(input).unwrap();
        assert!(fixture
            .verify(
                &State,
                &media,
                vec![Evidence::Configuration {
                    name: "emulatedMedia".into(),
                    value: expected
                }]
            )
            .await
            .is_ok());
        assert!(fixture
            .verify(
                &State,
                &media,
                vec![Evidence::Configuration {
                    name: "emulatedMedia".into(),
                    value: "{}".into()
                }]
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn accessibility_and_page_state_refuse_unrelated_evidence() {
        let fixture = Fixture::new().await;
        for command in [
            command("accessibilitySnapshot", json!({"maxNodes":32})),
            command("listPages", json!(null)),
        ] {
            for evidence in [
                vec![],
                vec![Evidence::Element {
                    selector: "button".into(),
                    text: None,
                }],
            ] {
                assert_eq!(
                    fixture
                        .verify(&State, &command, evidence)
                        .await
                        .unwrap_err()
                        .code,
                    ErrorCode::VerificationFailed
                );
            }
        }
        let command = command("accessibilitySnapshot", json!({"maxNodes":32}));
        let snapshot = Evidence::AccessibilitySnapshot {
            page_id: fixture.envelope.page_id.clone().unwrap(),
            nodes: vec![],
            truncated: false,
        };
        assert_eq!(
            fixture
                .verify(&State, &command, vec![snapshot.clone()])
                .await
                .unwrap(),
            vec![snapshot]
        );
        assert!(fixture
            .verify(
                &State,
                &PrimitiveCommand::WaitFor(WaitForCommand {
                    condition: WaitCondition::Url {
                        matcher: TextMatch::Exact("https://example.test".into())
                    },
                    timeout_ms: 1,
                }),
                vec![]
            )
            .await
            .is_err());
    }
}
