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
