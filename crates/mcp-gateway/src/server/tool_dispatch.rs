//! Name-matched `tools/call` dispatch (kept out of `call_tool` preamble).
//!
//! Routes to one of six per-domain dispatchers. Each owns a disjoint set of
//! tool names, declared in its own `TOOLS`, and returns the finished response.

use super::*;

impl Server {
    /// `handle` is the workflow handle `call_tool` resolved for this call
    /// (`None` for a raw-id call). The `WORKFLOW_SCOPE_TOOLS` dispatchers
    /// need it, to run the closed-page rule in `submit_envelope`;
    /// `workflow_start` takes no scope and `dispatch_workflow` tools mint
    /// their own. `defaulted_handle` is that same handle again, but only
    /// when the call carried no scope at all and
    /// `WorkflowHandles::normalize_arguments` resolved it against this
    /// connection's one live binding -- those same dispatchers thread
    /// it to `finish_tool` (or `workflow_observe_success`), which attaches
    /// `workflowHandleDefaulted` evidence to a successful outcome.
    pub(super) async fn dispatch_named_tool(
        &self,
        id: Value,
        call: ToolCall,
        context: types::RequestContext,
        handle: Option<String>,
        defaulted_handle: Option<String>,
    ) -> Value {
        let name = call.name.as_str();
        if dispatch_agent_workflow::TOOLS.contains(&name) {
            self.dispatch_agent_workflow(
                id,
                call,
                context,
                handle.as_deref(),
                defaulted_handle.as_deref(),
            )
            .await
        } else if dispatch_lifecycle::TOOLS.contains(&name) {
            self.dispatch_lifecycle(id, call, context).await
        } else if dispatch_primitives::TOOLS.contains(&name) {
            self.dispatch_primitives(
                id,
                call,
                context,
                handle.as_deref(),
                defaulted_handle.as_deref(),
            )
            .await
        } else if dispatch_intents::TOOLS.contains(&name) {
            self.dispatch_intents(
                id,
                call,
                context,
                handle.as_deref(),
                defaulted_handle.as_deref(),
            )
            .await
        } else if dispatch_page_ops::TOOLS.contains(&name) {
            self.dispatch_page_ops(
                id,
                call,
                context,
                handle.as_deref(),
                defaulted_handle.as_deref(),
            )
            .await
        } else if dispatch_workflow::TOOLS.contains(&name) {
            self.dispatch_workflow(id, call, context).await
        } else {
            unreachable!("availability checked above")
        }
    }

    /// Turns a dispatched result into the wire response.
    ///
    /// Shared by every domain dispatcher so the success and interface-error
    /// shaping stays in one place rather than being copied six times.
    /// `defaulted_handle` (`Some` only for a call `dispatch_named_tool`
    /// resolved by the single-live-binding default) earns the outcome a
    /// `workflowHandleDefaulted` evidence item -- attached here, after any
    /// per-tool evidence projection (`intent_complete_form`'s compact
    /// summary among them) has already run, so it is always appended to the
    /// final evidence array rather than a copy that projection discards.
    pub(super) async fn finish_tool(
        &self,
        id: Value,
        result: interface_core::InterfaceResult<Value>,
        defaulted_handle: Option<&str>,
    ) -> Value {
        match result {
            Ok(mut value) => {
                if let Some(handle) = defaulted_handle {
                    push_evidence(&mut value, workflow_handle_defaulted_evidence(handle));
                }
                self.tool_success(id, value).await
            }
            Err(interface_error) => interface_error_response(id, interface_error),
        }
    }
}
