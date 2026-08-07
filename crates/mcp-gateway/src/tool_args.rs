//! MCP tool argument structs shared by `tools/call` and prompt get.

use serde::Deserialize;
use serde_json::Value;

pub(crate) fn empty_arguments() -> Value {
    serde_json::json!({})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ToolCall {
    pub(crate) name: String,
    #[serde(default = "empty_arguments")]
    pub(crate) arguments: Value,
    #[serde(default, rename = "_meta")]
    pub(crate) _meta: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EmptyArgs {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SessionCreateArgs {
    pub(crate) profile: String,
    #[serde(default)]
    pub(crate) proxy: Option<String>,
    #[serde(default)]
    pub(crate) execution_policy: types::ExecutionPolicy,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WorkflowStartArgs {
    pub(crate) profile: String,
    #[serde(default)]
    pub(crate) proxy: Option<String>,
    #[serde(default)]
    pub(crate) execution_policy: types::ExecutionPolicy,
    #[serde(default)]
    pub(crate) url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WorkflowObserveArgs {
    pub(crate) workflow_handle: String,
    #[serde(default)]
    pub(crate) goal: Option<String>,
    #[serde(default)]
    pub(crate) max_nodes: Option<u32>,
    #[serde(default)]
    pub(crate) include_forms: bool,
    #[serde(default)]
    pub(crate) max_controls: Option<u32>,
}

impl WorkflowObserveArgs {
    /// JSON Schema string lengths are Unicode scalar counts, unlike the
    /// gateway's byte-oriented internal string validator.
    pub(crate) fn goal_within_scalar_bound(&self) -> bool {
        self.goal
            .as_ref()
            .is_none_or(|goal| goal.chars().count() <= crate::schema::MAX_WORKFLOW_GOAL_SCALARS)
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PageOpenArgs {
    pub(crate) session_id: types::SessionId,
    #[serde(default)]
    pub(crate) url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CommandExecuteArgs {
    pub(crate) envelope: types::CommandEnvelope,
    #[serde(default)]
    pub(crate) idempotency_key: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CheckpointSaveArgs {
    pub(crate) checkpoint: types::WorkflowCheckpoint,
    #[serde(default)]
    pub(crate) evidence_refs: Vec<types::CommandId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ContextAskArgs {
    pub(crate) session_id: types::SessionId,
    pub(crate) page_id: types::PageId,
    pub(crate) description: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ContextNeighborsArgs {
    pub(crate) session_id: types::SessionId,
    pub(crate) page_id: types::PageId,
    pub(crate) description: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ToolsetSelectArgs {
    pub(crate) toolset: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct JobSubmitArgs {
    pub(crate) name: String,
    #[serde(default)]
    pub(crate) payload: Option<Value>,
    #[serde(default)]
    pub(crate) priority: Option<String>,
    #[serde(default)]
    pub(crate) max_retries: Option<u32>,
    #[serde(default)]
    pub(crate) timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct JobIdArgs {
    pub(crate) job_id: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct WorkflowRecoverArgs {
    pub(crate) workflow_id: types::WorkflowId,
}

/// `recovery_status` answers by workflow, or discovers a session's workflows.
///
/// Both optional here, exactly one enforced in the handler: an agent that was
/// compacted has no `workflowId` left to ask with.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct RecoveryStatusArgs {
    #[serde(default)]
    pub(crate) workflow_id: Option<types::WorkflowId>,
    #[serde(default)]
    pub(crate) session_id: Option<types::SessionId>,
    #[serde(default)]
    pub(crate) limit: Option<usize>,
}

macro_rules! page_scoped_args {
    ($name:ident { $($field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        pub(crate) struct $name {
            pub(crate) session_id: types::SessionId,
            pub(crate) page_id: types::PageId,
            #[serde(default)]
            pub(crate) workflow_id: Option<types::WorkflowId>,
            $(pub(crate) $field : $ty,)*
        }
    };
}

/// Intent tools take the same page scope plus the intent's own payload.
/// The server builds the `CommandEnvelope`, so a caller mints no deadline.
/// `commandId`/`attemptId` are optional: a Boundary intent's pre-action
/// checkpoint must name the exact ids the submit will carry, so the caller
/// pins them up front and the server threads them through unchanged.
macro_rules! intent_args {
    ($name:ident { $($field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        pub(crate) struct $name {
            pub(crate) session_id: types::SessionId,
            pub(crate) page_id: types::PageId,
            #[serde(default)]
            pub(crate) workflow_id: Option<types::WorkflowId>,
            #[serde(default)]
            pub(crate) command_id: Option<types::CommandId>,
            #[serde(default)]
            pub(crate) attempt_id: Option<types::AttemptId>,
            #[serde(default)]
            pub(crate) idempotency_key: Option<String>,
            $(pub(crate) $field : $ty,)*
        }
    };
}

intent_args!(IntentLocateArgs {
    purpose: String,
    hints: Option<types::IntentHints>,
});

intent_args!(IntentFillArgs {
    purpose: String,
    hints: Option<types::IntentHints>,
    value: types::FillValue,
});

intent_args!(IntentCompleteFormArgs {
    purpose: String,
    fields: Vec<types::CompleteFormField>,
});

intent_args!(IntentSubmitAndVerifyArgs {
    purpose: String,
    hints: Option<types::IntentHints>,
    expected_state: types::WaitForCommand,
    auto_checkpoint: Option<bool>,
});

intent_args!(IntentWaitForStateArgs {
    condition: types::WaitCondition,
    timeout_ms: u64,
});

intent_args!(IntentFollowArgs {
    purpose: String,
    hints: Option<types::IntentHints>,
    expected_destination: types::WaitForCommand,
    boundary: Option<bool>,
    auto_checkpoint: Option<bool>,
});

intent_args!(IntentDismissObstructionArgs {
    purpose: String,
    hints: Option<types::IntentHints>,
    timeout_ms: Option<u64>,
});

intent_args!(IntentExtractArgs {
    purpose: String,
    fields: Vec<types::ExtractField>,
});

page_scoped_args!(NavigateArgs {
    url: String,
    wait_until: Option<types::WaitUntil>,
    timeout_ms: Option<u64>,
});

/// Click is the one flat primitive that can be Boundary class, so — like the
/// intent tools — it accepts caller-pinned `commandId`/`attemptId` for the
/// pre-action checkpoint gate (see `pin_envelope_ids`). When `boundary` is
/// true, `autoCheckpoint` defaults to true and mints that checkpoint in the
/// same call.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ClickArgs {
    pub(crate) session_id: types::SessionId,
    pub(crate) page_id: types::PageId,
    #[serde(default)]
    pub(crate) workflow_id: Option<types::WorkflowId>,
    #[serde(default)]
    pub(crate) command_id: Option<types::CommandId>,
    #[serde(default)]
    pub(crate) attempt_id: Option<types::AttemptId>,
    pub(crate) selector: Option<String>,
    pub(crate) target: Option<types::TargetSpec>,
    pub(crate) boundary: Option<bool>,
    #[serde(default)]
    pub(crate) auto_checkpoint: Option<bool>,
    pub(crate) expected_url: Option<String>,
}

page_scoped_args!(TypeTextArgs {
    selector: Option<String>,
    target: Option<types::TargetSpec>,
    value: String,
    clear_first: Option<bool>,
    expected_url: Option<String>,
});

page_scoped_args!(InspectArgs {
    selector: Option<String>,
    target: Option<types::TargetSpec>,
    include_html: Option<bool>,
});

page_scoped_args!(ScreenshotArgs {
    mode: Option<types::ScreenshotMode>,
});

page_scoped_args!(WaitForArgs {
    condition: types::WaitCondition,
    timeout_ms: u64,
});

page_scoped_args!(UploadFilesArgs {
    selector: Option<String>,
    target: Option<types::TargetSpec>,
    paths: Vec<String>,
});

page_scoped_args!(EvaluateJavaScriptArgs {
    expression: String,
    timeout_ms: Option<u64>,
    await_promise: Option<bool>,
});

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PageListArgs {
    pub(crate) session_id: types::SessionId,
    #[serde(default)]
    pub(crate) workflow_id: Option<types::WorkflowId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct SessionCloseArgs {
    pub(crate) session_id: types::SessionId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PageCloseArgs {
    pub(crate) session_id: types::SessionId,
    pub(crate) page_id: types::PageId,
    #[serde(default)]
    pub(crate) workflow_id: Option<types::WorkflowId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct FormSnapshotArgs {
    pub(crate) session_id: types::SessionId,
    pub(crate) page_id: types::PageId,
    #[serde(default)]
    pub(crate) max_controls: Option<u32>,
    /// Accepted and advertised, but not yet threaded anywhere:
    /// `RuntimeInterface::form_snapshot` takes no workflow, so the dispatcher
    /// has nowhere to pass it. Kept deserializable so a caller that sends it is
    /// not rejected by `deny_unknown_fields`, which is the only reason this is
    /// not simply removed.
    #[allow(dead_code)]
    #[serde(default)]
    pub(crate) workflow_id: Option<types::WorkflowId>,
}

page_scoped_args!(ControlActionArgs {
    target: types::FormControlTarget,
    action: types::ControlAction,
});

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct A11ySnapshotArgs {
    pub(crate) session_id: types::SessionId,
    pub(crate) page_id: types::PageId,
    #[serde(default)]
    pub(crate) max_nodes: Option<u32>,
    #[serde(default)]
    pub(crate) workflow_id: Option<types::WorkflowId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct NetworkLogArgs {
    pub(crate) session_id: types::SessionId,
    pub(crate) page_id: types::PageId,
    #[serde(default)]
    pub(crate) clear: Option<bool>,
    #[serde(default)]
    pub(crate) workflow_id: Option<types::WorkflowId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct EmulateArgs {
    pub(crate) session_id: types::SessionId,
    pub(crate) page_id: types::PageId,
    #[serde(default)]
    pub(crate) viewport: Option<types::ViewportSize>,
    #[serde(default)]
    pub(crate) geolocation: Option<types::GeolocationCoordinates>,
    #[serde(default)]
    pub(crate) mobile: Option<bool>,
    #[serde(default)]
    pub(crate) workflow_id: Option<types::WorkflowId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DialogArgs {
    pub(crate) session_id: types::SessionId,
    pub(crate) page_id: types::PageId,
    pub(crate) action: types::DialogAction,
    #[serde(default)]
    pub(crate) timeout_ms: Option<u64>,
    #[serde(default)]
    pub(crate) workflow_id: Option<types::WorkflowId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct PdfArgs {
    pub(crate) session_id: types::SessionId,
    pub(crate) page_id: types::PageId,
    #[serde(default)]
    pub(crate) landscape: bool,
    #[serde(default)]
    pub(crate) print_background: Option<bool>,
    #[serde(default)]
    pub(crate) scale: Option<f64>,
    #[serde(default)]
    pub(crate) page_ranges: Option<String>,
    #[serde(default)]
    pub(crate) workflow_id: Option<types::WorkflowId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CookieGetArgs {
    pub(crate) session_id: types::SessionId,
    pub(crate) page_id: types::PageId,
    #[serde(default)]
    pub(crate) urls: Vec<String>,
    #[serde(default)]
    pub(crate) workflow_id: Option<types::WorkflowId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CookieSetArgs {
    pub(crate) session_id: types::SessionId,
    pub(crate) page_id: types::PageId,
    pub(crate) cookies: Vec<types::SetCookieParam>,
    #[serde(default)]
    pub(crate) workflow_id: Option<types::WorkflowId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct CookieDeleteArgs {
    pub(crate) session_id: types::SessionId,
    pub(crate) page_id: types::PageId,
    #[serde(default)]
    pub(crate) urls: Vec<String>,
    #[serde(default)]
    pub(crate) names: Vec<String>,
    #[serde(default)]
    pub(crate) workflow_id: Option<types::WorkflowId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ExtractStructuredArgs {
    pub(crate) session_id: types::SessionId,
    pub(crate) page_id: types::PageId,
    #[serde(default)]
    pub(crate) workflow_id: Option<types::WorkflowId>,
    pub(crate) schema: serde_json::Value,
    #[serde(default)]
    pub(crate) purpose: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct DownloadUrlArgs {
    pub(crate) session_id: types::SessionId,
    pub(crate) page_id: types::PageId,
    pub(crate) url: String,
    #[serde(default)]
    pub(crate) expected_content_type: Option<String>,
    pub(crate) max_bytes: u64,
    #[serde(default)]
    pub(crate) workflow_id: Option<types::WorkflowId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct EventsReadArgs {
    #[serde(default)]
    pub(crate) cursor: u64,
    pub(crate) limit: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ResourceReadArgs {
    pub(crate) uri: String,
    #[serde(default, rename = "_meta")]
    pub(crate) _meta: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PromptGetArgs {
    pub(crate) name: String,
    #[serde(default = "empty_arguments")]
    pub(crate) arguments: Value,
    #[serde(default, rename = "_meta")]
    pub(crate) _meta: Option<Value>,
}

#[cfg(test)]
mod tests {
    use super::WorkflowObserveArgs;

    #[test]
    fn workflow_observe_goal_limit_counts_unicode_scalars_not_utf8_bytes() {
        let parse = |goal| {
            serde_json::from_value::<WorkflowObserveArgs>(serde_json::json!({
                "workflowHandle":"wf_0123456789abcdef0123456789abcdef",
                "goal":goal,
            }))
            .expect("observe arguments parse")
        };
        assert!(parse("é".repeat(256)).goal_within_scalar_bound());
        assert!(!parse("é".repeat(257)).goal_within_scalar_bound());
    }
}
