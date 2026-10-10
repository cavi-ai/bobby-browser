//! Select a verifier family; each family owns its evidence rules.
use super::*;
mod navigation;
mod pointer;
mod state;
mod text;

struct VerificationContext<'a> {
    envelope: &'a CommandEnvelope,
    lease: &'a worker_pool::WorkerLease,
    typed_on: Option<String>,
}

#[async_trait::async_trait]
trait CommandVerifier: Send + Sync {
    async fn verify(
        &self,
        context: VerificationContext<'_>,
        command: &PrimitiveCommand,
        evidence: Vec<Evidence>,
    ) -> Result<Vec<Evidence>, CommandError>;
}

fn verifier_for(command: &PrimitiveCommand) -> &dyn CommandVerifier {
    match command {
        PrimitiveCommand::TypeText(_)
        | PrimitiveCommand::ControlAction(_)
        | PrimitiveCommand::UploadFiles(_)
        | PrimitiveCommand::UploadAndConfirm(_) => &text::Text,
        PrimitiveCommand::Click(_)
        | PrimitiveCommand::ClickAndWaitForPopup(_)
        | PrimitiveCommand::ClickAndWaitForDownload(_) => &pointer::Pointer,
        PrimitiveCommand::Navigate(_) | PrimitiveCommand::WaitFor(_) => &navigation::Navigation,
        PrimitiveCommand::Inspect(_)
        | PrimitiveCommand::AccessibilitySnapshot(_)
        | PrimitiveCommand::NetworkLog(_)
        | PrimitiveCommand::Emulate(_)
        | PrimitiveCommand::HandleDialog(_)
        | PrimitiveCommand::PrintToPdf(_)
        | PrimitiveCommand::GetCookies(_)
        | PrimitiveCommand::SetCookies(_)
        | PrimitiveCommand::DeleteCookies(_)
        | PrimitiveCommand::ExtractStructured(_)
        | PrimitiveCommand::ActivatePage(_)
        | PrimitiveCommand::OpenPage(_)
        | PrimitiveCommand::ClosePage(_)
        | PrimitiveCommand::ListPages(_)
        | PrimitiveCommand::DownloadUrl(_)
        | PrimitiveCommand::CaptureScreenshot(_)
        | PrimitiveCommand::SetFocusEmulation(_)
        | PrimitiveCommand::SetEmulatedMedia(_)
        | PrimitiveCommand::EvaluateJavaScript(_) => &state::State,
    }
}

impl PageRuntime {
    pub(super) async fn verify(
        &self,
        envelope: &CommandEnvelope,
        lease: &worker_pool::WorkerLease,
        evidence: Vec<Evidence>,
        typed_on: Option<String>,
    ) -> Result<Vec<Evidence>, CommandError> {
        let RuntimeCommand::Primitive(command) = &envelope.command else {
            if evidence
                .iter()
                .any(|item| matches!(item, Evidence::IntentExecution { .. }))
            {
                return Ok(evidence);
            }
            return Err(verification_error(
                "intent command returned no execution record",
            ));
        };
        verifier_for(command)
            .verify(
                VerificationContext {
                    envelope,
                    lease,
                    typed_on,
                },
                command,
                evidence,
            )
            .await
    }
}
