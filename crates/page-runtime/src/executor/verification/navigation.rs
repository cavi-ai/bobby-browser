//! Navigation command verification and its local predicates.
use super::*;
pub(super) struct Navigation;
#[async_trait::async_trait]
impl CommandVerifier for Navigation {
    async fn verify(
        &self,
        _context: VerificationContext<'_>,
        command: &PrimitiveCommand,
        evidence: Vec<Evidence>,
    ) -> Result<Vec<Evidence>, CommandError> {
        match command {
            PrimitiveCommand::Navigate(_) => match evidence.first() {
                Some(Evidence::Navigation { url, .. }) if !url.is_empty() => Ok(evidence),
                _ => Err(verification_error("navigation returned no final URL")),
            },
            PrimitiveCommand::WaitFor(_) => {
                if evidence
                    .iter()
                    .any(|item| matches!(item, Evidence::Wait { .. }))
                {
                    Ok(evidence)
                } else {
                    Err(verification_error("wait returned no condition evidence"))
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

    #[tokio::test]
    async fn navigation_requires_a_nonempty_final_url_in_the_first_evidence_item() {
        let fixture = Fixture::new().await;
        let command = command(
            "navigate",
            json!({"url":"https://example.test", "waitUntil":"commit", "timeoutMs":1000}),
        );
        for (evidence, accepted) in [
            (vec![], false),
            (
                vec![Evidence::Navigation {
                    url: "".into(),
                    title: "".into(),
                }],
                false,
            ),
            (
                vec![Evidence::Navigation {
                    url: "https://example.test/final".into(),
                    title: "Final".into(),
                }],
                true,
            ),
            (
                vec![
                    Evidence::Configuration {
                        name: "noise".into(),
                        value: "ignored".into(),
                    },
                    Evidence::Navigation {
                        url: "https://example.test/final".into(),
                        title: "Final".into(),
                    },
                ],
                false,
            ),
        ] {
            assert_eq!(
                fixture
                    .verify(&Navigation, &command, evidence)
                    .await
                    .is_ok(),
                accepted
            );
        }
    }

    #[tokio::test]
    async fn waits_require_condition_evidence_and_reject_other_verifier_families() {
        let fixture = Fixture::new().await;
        let condition = WaitCondition::Url {
            matcher: TextMatch::Exact("https://example.test".into()),
        };
        let wait = PrimitiveCommand::WaitFor(WaitForCommand {
            condition: condition.clone(),
            timeout_ms: 1000,
        });
        let evidence = vec![
            Evidence::Configuration {
                name: "noise".into(),
                value: "ignored".into(),
            },
            Evidence::Wait {
                condition,
                elapsed_ms: 1,
                observations: 1,
                excluded_classes: vec![],
                observed: None,
            },
        ];
        assert_eq!(
            fixture
                .verify(&Navigation, &wait, evidence.clone())
                .await
                .unwrap(),
            evidence
        );
        assert_eq!(
            fixture
                .verify(&Navigation, &wait, vec![])
                .await
                .unwrap_err()
                .code,
            ErrorCode::VerificationFailed
        );
        assert!(fixture
            .verify(
                &Navigation,
                &PrimitiveCommand::Inspect(InspectCommand::default()),
                evidence
            )
            .await
            .is_err());
    }
}
