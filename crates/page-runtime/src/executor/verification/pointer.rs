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
                    if let Err(error) = worker_pool::PageBehavior::wait_for(
                        lease.worker().wait_provider(),
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
                    let verification =
                        worker_pool::observation_or_default(lease.worker().observation())
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
