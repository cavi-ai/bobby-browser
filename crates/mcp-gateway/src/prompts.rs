use serde_json::{json, Value};

/// Server-authored working loops: each prompt names a real, ordered sequence
/// of real tools with `sessionId`/`pageId`/`workflowId` bound in, so an agent
/// does not have to infer the sequence from the tool list on its own.
///
/// Every tool and argument named below is real. Before touching this file,
/// re-check the named tool's dispatch arm in `crate::server` and its argument
/// schema in `crate::schema::tool_schema` -- a prompt that names a wrong tool
/// or a wrong argument is a defect an agent will actually execute.
pub(crate) fn list_prompts() -> Value {
    json!({
        "prompts": [
            {
                "name": "start_browsing",
                "title": "Start browsing",
                "description": "Create and bind a browser session, page, and retained workflow, then observe it. No existing IDs are required.",
                "arguments": [
                    {
                        "name": "url",
                        "description": "Optional URL to open in the new workflow.",
                        "required": false
                    }
                ]
            },
            {
                "name": "fill_and_submit_form",
                "title": "Fill and submit a form",
                "description": "Snapshot a page's form, fill it, submit and verify, then checkpoint the verified result. On needsReconciliation call recovery_status — do not blind-retry the Boundary submit.",
                "arguments": [
                    {
                        "name": "sessionId",
                        "description": "Owned session holding the page.",
                        "required": true
                    },
                    {
                        "name": "pageId",
                        "description": "Page carrying the form.",
                        "required": true
                    }
                ]
            },
            {
                "name": "extract_from_page",
                "title": "Extract data from a page",
                "description": "Snapshot a page and read named fields off it without mutating it. Per-field extract failures are not a tools/call failure — read structuredContent.",
                "arguments": [
                    {
                        "name": "sessionId",
                        "description": "Owned session holding the page.",
                        "required": true
                    },
                    {
                        "name": "pageId",
                        "description": "Page to read.",
                        "required": true
                    }
                ]
            },
            {
                "name": "recover_workflow",
                "title": "Recover an interrupted workflow",
                "description": "Read a workflow's checkpoint, attempt recovery, then resume, restart, or reconcile based on the decision. Never replay a Boundary command after needsReconciliation.",
                "arguments": [
                    {
                        "name": "sessionId",
                        "description": "Owned session the workflow ran in.",
                        "required": true
                    },
                    {
                        "name": "pageId",
                        "description": "Page the workflow was acting on.",
                        "required": true
                    },
                    {
                        "name": "workflowId",
                        "description": "Workflow to recover.",
                        "required": true
                    }
                ]
            }
        ]
    })
}

pub(crate) fn get_prompt(name: &str, arguments: &Value) -> Option<Value> {
    match name {
        "start_browsing" => start_browsing(arguments),
        "fill_and_submit_form" => fill_and_submit_form(arguments),
        "extract_from_page" => extract_from_page(arguments),
        "recover_workflow" => recover_workflow(arguments),
        _ => None,
    }
}

fn start_browsing(arguments: &Value) -> Option<Value> {
    let url = argument(arguments, "url");
    let start = match url {
        Some(url) => format!("Call workflow_start with profile=\"default\" and url=\"{url}\"."),
        None => "Call workflow_start with profile=\"default\".".to_string(),
    };
    Some(json!({
        "description": "Start a retained browser workflow and observe its first state.",
        "messages": [message(format!(
            "{start} It creates and binds the session, page, and workflow. Then call workflow_observe with the returned handle before taking the next action."
        ))]
    }))
}

/// A required prompt argument, per the MCP prompts contract, arrives as a
/// non-empty string under `arguments`.
fn argument(arguments: &Value, name: &str) -> Option<String> {
    let value = arguments.get(name)?.as_str()?;
    if value.is_empty() {
        return None;
    }
    Some(value.to_owned())
}

fn message(text: String) -> Value {
    json!({
        "role": "user",
        "content": {"type": "text", "text": text}
    })
}

fn fill_and_submit_form(arguments: &Value) -> Option<Value> {
    let session_id = argument(arguments, "sessionId")?;
    let page_id = argument(arguments, "pageId")?;
    let text = format!(
        "Fill and submit the form on page {page_id} in session {session_id}, checkpointing \
         before the boundary submit.\n\
         \n\
         1. Call a11y_snapshot with sessionId=\"{session_id}\", pageId=\"{page_id}\" to get \
         command-ready targets for the form's controls -- pass those targets into the intent_* \
         tools below rather than guessing a selector. Capture the workflowId the outcome \
         returns and thread that same workflowId into every following call, so the whole \
         sequence stays inside one workflow; that is what keeps checkpoint_save reachable.\n\
         \n\
         2. Call intent_complete_form with sessionId=\"{session_id}\", pageId=\"{page_id}\", the \
         workflowId from step 1, a purpose describing the form, and fields: one entry per \
         control to fill, each of shape {{\"name\": <control name>, \"purpose\": <what this \
         field is for>, \"value\": <FillValue>}} where FillValue is exactly one of \
         {{\"kind\":\"text\",\"text\":...}}, {{\"kind\":\"select\",\"option\":...}}, \
         {{\"kind\":\"checked\",\"checked\":...}}, {{\"kind\":\"files\",\"paths\":[...]}}. \
         All of name, purpose, and value are required -- omitting one fails validation. \
         The tool fills and verifies each field in order, never submits on its own, and is \
         Reconciliable: an interrupted fill is safe to inspect and redo. Capture the \
         commandId its outcome carries.\n\
         \n\
         3. The submit is Boundary class: the runtime refuses it unless a matching checkpoint \
         already exists. autoCheckpoint defaults to true, so the short way is one call: go \
         straight to step 4 and the runtime mints the checkpoint for you and returns its \
         checkpointId. Do the rest of this step by hand only when you need to author the \
         checkpoint's invariants or replayableInputs (pass autoCheckpoint=false on the \
         submit). Pick two fresh UUIDs -- one for the \
         submit's commandId, one for its attemptId -- then call checkpoint_save with \
         checkpoint set to a WorkflowCheckpoint for this workflowId, sessionId, and pageId \
         (schemaVersion, checkpointId, attemptId set to your chosen attemptId, restartUrl and \
         currentUrl from the page's current URL, recoveryClass set to \"boundary\", \
         boundaryCommandId set to your chosen submit commandId, invariants, replayableInputs, \
         createdAt -- its own evidence field is always saved empty), and evidenceRefs set to \
         the commandIds captured in step 2. The runtime resolves each referenced command's \
         evidence from its own journal; a reference with no journal record, one that never \
         reached a terminal outcome, or one belonging to a session this principal does not \
         own, is rejected.\n\
         \n\
         4. Call intent_submit_and_verify with sessionId=\"{session_id}\", pageId=\"{page_id}\", \
         the same workflowId (omit autoCheckpoint, or pass false with the SAME commandId and \
         attemptId you pinned in step 3 if you checkpointed by hand), a purpose \
         describing the submit control, and expectedState describing the page condition that \
         proves the submit landed. If the outcome is needsReconciliation, do NOT retry it -- \
         the submit may have already landed. Instead call recovery_status with the workflowId \
         and follow workflow_recover if a checkpoint exists (see bobby://failure-taxonomy for \
         the full repair). The call only counts as landed when the outcome is completed."
    );
    Some(json!({
        "description": "Fill and submit a form, checkpointing before the boundary submit.",
        "messages": [message(text)]
    }))
}

fn extract_from_page(arguments: &Value) -> Option<Value> {
    let session_id = argument(arguments, "sessionId")?;
    let page_id = argument(arguments, "pageId")?;
    let text = format!(
        "Read data off page {page_id} in session {session_id} without mutating it.\n\
         \n\
         1. Call a11y_snapshot with sessionId=\"{session_id}\", pageId=\"{page_id}\" to get \
         command-ready targets for the elements you need to read -- pass those targets into \
         the intent_extract call below rather than guessing a selector. Capture the workflowId \
         the outcome returns and thread it into the next call.\n\
         \n\
         2. Call intent_extract with sessionId=\"{session_id}\", pageId=\"{page_id}\", the \
         workflowId from step 1, a purpose describing what you're reading, and fields: one \
         entry per item to extract, each of shape {{\"name\": <field name>, \"purpose\": \
         <what this value is>, \"value\": <ExtractValueKind>}} where ExtractValueKind is \
         exactly one of {{\"kind\":\"text\"}}, {{\"kind\":\"attribute\",\"attribute\":...}}, \
         {{\"kind\":\"href\"}}. All of name, purpose, and value are required -- omitting one \
         fails validation. intent_extract is Replayable: it never mutates the page, so it is \
         safe to call again on its own if it fails, and a field that can't be resolved is \
         reported per-field rather than failing the whole call (see bobby://intents).\n\
         \n\
         There is no checkpoint step here: nothing mutated the page, so there is no side effect \
         a checkpoint would need to protect against replaying."
    );
    Some(json!({
        "description": "Read named fields off a page without mutating it.",
        "messages": [message(text)]
    }))
}

fn recover_workflow(arguments: &Value) -> Option<Value> {
    let session_id = argument(arguments, "sessionId")?;
    let page_id = argument(arguments, "pageId")?;
    let workflow_id = argument(arguments, "workflowId")?;
    let text = format!(
        "Recover workflow {workflow_id} (session {session_id}, page {page_id}) from its last \
         verified checkpoint.\n\
         \n\
         1. Call recovery_status with workflowId=\"{workflow_id}\" to read the checkpoint and \
         any recorded recovery receipts without attempting recovery yet.\n\
         \n\
         2. Call workflow_recover with workflowId=\"{workflow_id}\". It returns one of three \
         decisions:\n\
         - resumed: the checkpoint's attempt continues. Call a11y_snapshot with \
         sessionId=\"{session_id}\", pageId=\"{page_id}\", workflowId=\"{workflow_id}\" to see \
         current page state, then continue the interrupted working loop from there.\n\
         - restarted: the abandoned attempt was replaced by a new one (see the returned \
         lineage). Call navigate with sessionId=\"{session_id}\", pageId=\"{page_id}\", \
         workflowId=\"{workflow_id}\", url set to the checkpoint's restartUrl (from \
         recovery_status), then restart the working loop with a11y_snapshot.\n\
         - needsReconciliation: the runtime still cannot prove the boundary command's side \
         effect never happened. Do not retry the original action. Inspect the returned \
         evidence and the page's current state (a11y_snapshot or inspect) to establish ground \
         truth before deciding how to proceed.\n\
         \n\
         If you are reconnecting after the interruption, the server pushes runtime events as \
         notifications/bobby/event on this connection -- no polling needed. A pushed frame whose \
         kind is event.gap means events were dropped from the push channel before you \
         reconnected; call events_read with cursor set to its payload's earliestAvailable - 1 \
         to read them back. See bobby://failure-taxonomy.\n\
         \n\
         On failure with notFound, this principal does not own workflowId's checkpoint \
         session, or the session is closed -- verify with session_list before retrying."
    );
    Some(json!({
        "description": "Recover a workflow from its last verified checkpoint.",
        "messages": [message(text)]
    }))
}
