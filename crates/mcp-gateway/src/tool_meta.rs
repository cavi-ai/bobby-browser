//! Capability, operation, and description tables for MCP tools.
//!
//! Host annotations live in `annotations.rs`. These maps are the enforcement
//! and agent-facing description authority for `tools/list` / `tools/call`.

/// Shared authority contracts for the callable workflow surface.
pub(crate) const WORKFLOW_START_REQUIRED_CAPABILITIES: &[types::Capability] = &[
    types::Capability::SessionRead,
    types::Capability::SessionWrite,
    types::Capability::PageWrite,
];
pub(crate) const WORKFLOW_START_OPERATION: types::InterfaceOperation =
    types::InterfaceOperation::CreateSession;

pub(crate) const WORKFLOW_OBSERVE_REQUIRED_CAPABILITIES: &[types::Capability] =
    &[types::Capability::BrowserMutate];
pub(crate) const WORKFLOW_OBSERVE_OPERATION: types::InterfaceOperation =
    types::InterfaceOperation::SubmitCommand;

pub(crate) fn required_capabilities(name: &str) -> Option<&'static [types::Capability]> {
    crate::catalog::descriptor(name).map(|tool| tool.capabilities)
}

pub(crate) fn required_operation(name: &str) -> Option<types::InterfaceOperation> {
    crate::catalog::descriptor(name).and_then(|tool| tool.operation)
}

pub(crate) fn tool_description(name: &str) -> &'static str {
    crate::catalog::descriptor(name).map_or("Runtime operation.", |tool| tool.description)
}

#[cfg(test)]
mod tests {
    use super::tool_description;

    #[test]
    fn form_intents_describe_the_compact_verified_loop() {
        let complete = tool_description("intent_complete_form");
        assert!(
            complete.contains("evidenceDetail=compact"),
            "whole-form intent must advertise compact success evidence"
        );
        assert!(
            complete.contains("just-in-time") && complete.contains("revealedBy"),
            "whole-form intent must explain that fields resolve just-in-time and name revealedBy as the activation hook for a field that only appears once revealed"
        );

        let submit = tool_description("intent_submit_and_verify");
        assert!(
            submit.contains("Submit once")
                && submit.contains("submitSettlement=settled|validationRejected")
                && submit.contains("do not inspect or blindly resubmit"),
            "verified submit must identify the exactly-once settled stopping condition"
        );
    }

    #[test]
    fn iframe_and_download_descriptions_identify_terminal_evidence() {
        let snapshot = tool_description("a11y_snapshot");
        assert!(
            snapshot.contains("use the in-frame target directly"),
            "iframe targets must discourage a redundant second discovery pass"
        );

        let download = tool_description("download_url");
        assert!(
            download.contains("no shell check is needed"),
            "digest-verified saved downloads must identify terminal evidence"
        );
    }
}
