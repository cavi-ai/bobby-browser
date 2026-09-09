//! Name-matched `tools/call` dispatch (kept out of `call_tool` preamble).
//!
//! Routes to one of six per-domain dispatchers. Each owns a disjoint set of
//! tool names, declared in its own `TOOLS`, and returns the finished response.

use super::*;

impl Server {
    /// `handle` is the workflow handle `call_tool` resolved for this call
    /// (`None` for a raw-id call). Only the three dispatchers that own a
    /// `WORKFLOW_SCOPE_TOOLS` tool need it, to run the closed-page rule in
    /// `submit_envelope`; `workflow_observe` (in `dispatch_agent_workflow`)
    /// takes its handle from its own required argument instead and is
    /// unaffected by this parameter.
    pub(super) async fn dispatch_named_tool(
        &self,
        id: Value,
        call: ToolCall,
        context: types::RequestContext,
        handle: Option<String>,
    ) -> Value {
        let name = call.name.as_str();
        if dispatch_agent_workflow::TOOLS.contains(&name) {
            self.dispatch_agent_workflow(id, call, context).await
        } else if dispatch_lifecycle::TOOLS.contains(&name) {
            self.dispatch_lifecycle(id, call, context).await
        } else if dispatch_primitives::TOOLS.contains(&name) {
            self.dispatch_primitives(id, call, context, handle.as_deref())
                .await
        } else if dispatch_intents::TOOLS.contains(&name) {
            self.dispatch_intents(id, call, context, handle.as_deref())
                .await
        } else if dispatch_page_ops::TOOLS.contains(&name) {
            self.dispatch_page_ops(id, call, context, handle.as_deref())
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
    pub(super) async fn finish_tool(
        &self,
        id: Value,
        result: interface_core::InterfaceResult<Value>,
    ) -> Value {
        match result {
            Ok(value) => self.tool_success(id, value).await,
            Err(interface_error) => interface_error_response(id, interface_error),
        }
    }
}
