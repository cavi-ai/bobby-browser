//! Page registration and invalidate-before-record are one phase.
use super::*;

impl PageRuntime {
    pub(super) async fn record_execution_context(
        &self,
        envelope: &CommandEnvelope,
        evidence: &mut Vec<Evidence>,
    ) {
        match &envelope.command {
            RuntimeCommand::Primitive(PrimitiveCommand::OpenPage(_)) => {
                if let Some(Evidence::Page { page_id, url, .. }) = evidence.first() {
                    self.register_page_id(
                        envelope.session_id.clone(),
                        page_id.clone(),
                        url.clone(),
                    )
                    .await;
                }
            }
            RuntimeCommand::Primitive(PrimitiveCommand::ClickAndWaitForPopup(_)) => {
                if let Some(Evidence::Popup { page_id, url, .. }) = evidence.first() {
                    self.register_page_id(
                        envelope.session_id.clone(),
                        page_id.clone(),
                        url.clone(),
                    )
                    .await;
                }
            }
            RuntimeCommand::Primitive(PrimitiveCommand::ClosePage(command)) => {
                self.context().forget(&command.page_id);
                self.remove_page(&command.page_id).await
            }
            RuntimeCommand::Primitive(_) | RuntimeCommand::Intent(_) => {}
        }

        // Ordering matters: invalidate first, record second. A replayable
        // snapshot does not invalidate, and its result is what the graph should
        // hold; any other command may have changed the page. `finish_failure`
        // invalidates for the same reason.
        if let Some(page_id) = envelope.page_id.as_ref() {
            // Recorded before invalidation: which command produced evidence
            // does not go stale when the page changes, so it outlives the
            // generation bump below.
            if !evidence.is_empty() {
                self.context().record_command(
                    page_id,
                    envelope.command_id.clone(),
                    crate::context::command_kind_name(&envelope.command),
                );
            }
            self.context().invalidate_for(page_id, &envelope.command);
            for item in evidence.iter() {
                if let Evidence::AccessibilitySnapshot {
                    page_id: observed,
                    nodes,
                    truncated,
                    ..
                } = item
                {
                    // A truncated snapshot is not the page: recording it would
                    // let the graph answer "not found" for a control that
                    // exists past the truncation point.
                    if !*truncated {
                        self.context().record(observed, nodes.clone());
                    }
                }
            }
            if let Some(generation) = self.context().generation(page_id) {
                evidence.push(Evidence::PageGeneration {
                    page_id: page_id.clone(),
                    generation,
                });
            }
        }
    }
}
