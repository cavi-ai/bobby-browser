use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicUsize, Ordering},
        Arc,
    },
    time::Duration as StdDuration,
};

use base64::{engine::general_purpose::STANDARD as BASE64, Engine as _};
use chrono::{Duration, Utc};
use futures_util::{stream::FuturesUnordered, FutureExt, StreamExt};
use interface_core::{AuthorizationGuard, CapabilityHandle, EventStore, RuntimeInterface};
use sdk_core::AuthenticatedRuntime;
use serde::{de::DeserializeOwned, Deserialize};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncWrite, AsyncWriteExt, BufReader},
    sync::{Mutex, Notify},
};

use crate::annotations::{tool_annotations, tool_title};
use crate::notify::{tools_list_changed_frame, NotificationSink};
use crate::protocol::{
    error, negotiate_protocol_version, success, INTERFACE_ERROR, INTERNAL_ERROR, INVALID_PARAMS,
    INVALID_REQUEST, MAX_EVENT_LIMIT, MAX_FRAME_BYTES, MAX_INPUT_BYTES, MAX_REQUEST_ID_BYTES,
    MCP_PROTOCOL_VERSION, METHOD_NOT_FOUND, NOT_INITIALIZED, PARSE_ERROR, REQUEST_CANCELLED,
};
use crate::resources::{static_resource_body, static_resources};
use crate::schema::{
    advertised_tool_output_schema, advertised_tool_schema_for_capabilities,
    validate_tool_arguments, MAX_RECOVERABLE_WORKFLOWS,
};
use crate::tool_args::*;
use crate::tool_meta::{required_capabilities, required_operation, tool_description};
use crate::workflow_handles::{WorkflowHandleError, WorkflowHandles};
use crate::ArtifactResources;

mod dispatch_agent_workflow;
mod dispatch_intents;
mod dispatch_lifecycle;
mod dispatch_page_ops;
mod dispatch_primitives;
mod dispatch_workflow;
mod tool_dispatch;

const MAX_RESOURCE_ENCODED_BYTES: usize = 768 * 1024;
/// Only error messages with this prefix may cross the MCP boundary verbatim:
/// they are written by the runtime as operator-actionable diagnostics and
/// contain no secrets. Everything else is redacted to the canonical code.
pub(crate) const BROWSER_LAUNCH_DIAGNOSTIC_PREFIX: &str = "browser launch failed:";
const MAX_PENDING_CANCELLATIONS: usize = 1024;
/// How many notification frames `serve` may have queued for the writer before it
/// stops pulling more off the subscription. See the comment at its use site.
const MAX_PENDING_NOTIFICATION_WRITES: usize = 64;
/// In-flight bound shared by request handlers and queued notification writes
/// (both live in `pending`). A client pipelining thousands of `tools/call`
/// frames without reading responses used to grow `pending` without limit —
/// memory and browser-process exhaustion from one misbehaving agent. Past
/// the bound the read branch backpressures: it stops pulling frames until a
/// pending handler completes, exactly like the notification branch.
const MAX_IN_FLIGHT_REQUESTS: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    AwaitingInitialize,
    AwaitingInitializedNotification,
    Ready,
}

fn workflow_call_class(name: &str) -> observability::WorkflowCallClass {
    use observability::WorkflowCallClass;

    if name.starts_with("job_") {
        WorkflowCallClass::Job
    } else if name.starts_with("artifact_") || name == "download_url" {
        WorkflowCallClass::Artifact
    } else if name.contains("recover") || name.contains("checkpoint") {
        WorkflowCallClass::Recovery
    } else if matches!(
        name,
        "fill_and_submit_form" | "extract_structured" | "workflow_observe"
    ) {
        WorkflowCallClass::CompositeWorkflow
    } else if matches!(name, "runtime_info" | "session_create" | "session_close") {
        WorkflowCallClass::Lifecycle
    } else if name.ends_with("_list") || name == "tools_search" {
        WorkflowCallClass::Discovery
    } else if name.starts_with("context_")
        || name.starts_with("events_")
        || name.contains("snapshot")
        || matches!(name, "page_get" | "network_log")
    {
        WorkflowCallClass::Read
    } else {
        WorkflowCallClass::Mutation
    }
}

/// MCP JSON-RPC server for one authenticated principal.
///
/// Holds the runtime interface, capability guard, event store, and tool catalog.
/// Construct with [`Self::new`] and drive I/O through [`Self::serve`] or
/// [`Self::handle_message`].
pub struct Server {
    runtime: Arc<dyn RuntimeInterface>,
    handle: CapabilityHandle,
    authorization: AuthorizationGuard,
    events: EventStore,
    resources: ArtifactResources,
    notifications: NotificationSink,
    lifecycle: Mutex<Lifecycle>,
    in_flight: Mutex<BTreeMap<String, Arc<Notify>>>,
    pending_cancellations: Mutex<BTreeSet<String>>,
    workflow_handles: Arc<WorkflowHandles>,
    shutting_down: AtomicBool,
    /// The phase this connection's `tools/list` is scoped to. Per connection,
    /// not per principal: two agents on the same bearer can be in different
    /// phases, and the phase is a view over the surface, not authority.
    /// `std::sync::Mutex` because `list_tools` is sync and the critical section
    /// is a single enum copy.
    toolset: std::sync::Mutex<crate::toolset::Toolset>,
    /// Optional job scheduler. When absent, job_* tools are not advertised.
    jobs: Option<Arc<dyn crate::jobs::JobPort>>,
    operational_metrics: Option<observability::OperationalMetrics>,
}

impl Server {
    /// Production wiring: default event store and artifact resources.
    pub fn new(runtime: Arc<AuthenticatedRuntime>) -> Self {
        Self::production(
            runtime,
            EventStore::new(16_384),
            ArtifactResources::default(),
        )
    }

    pub fn production(
        runtime: Arc<AuthenticatedRuntime>,
        events: EventStore,
        resources: ArtifactResources,
    ) -> Self {
        let handle = runtime.capability_handle();
        let operational_metrics = runtime.operational_metrics();
        let mut server = Self::for_interface(runtime, handle, events, resources);
        server.operational_metrics = Some(operational_metrics);
        server
    }

    pub fn for_interface(
        runtime: Arc<dyn RuntimeInterface>,
        handle: CapabilityHandle,
        events: EventStore,
        resources: ArtifactResources,
    ) -> Self {
        // Built in the one constructor every transport funnels through, so no
        // `Server` can exist with its notification fan-out unwired.
        let notifications = NotificationSink::new(events.clone(), handle.principal_id().clone());
        Self {
            runtime,
            handle: handle.clone(),
            authorization: AuthorizationGuard::new(handle),
            events,
            resources,
            notifications,
            lifecycle: Mutex::new(Lifecycle::AwaitingInitialize),
            in_flight: Mutex::new(BTreeMap::new()),
            pending_cancellations: Mutex::new(BTreeSet::new()),
            workflow_handles: Arc::new(WorkflowHandles::new()),
            shutting_down: AtomicBool::new(false),
            toolset: std::sync::Mutex::new(crate::toolset::Toolset::from_env().unwrap_or_default()),
            jobs: None,
            operational_metrics: None,
        }
    }

    /// Attach a job port so `job_submit` / `job_status` / `job_cancel` advertise.
    pub fn with_jobs(mut self, jobs: Arc<dyn crate::jobs::JobPort>) -> Self {
        self.jobs = Some(jobs);
        self
    }

    /// Start on `toolset` unless `BOBBY_MCP_TOOLSET` already chose one, so
    /// the environment stays the operator's last word over the config file.
    /// Narrowing only changes what `tools/list` advertises; every tool stays
    /// callable and every capability gate stays in force.
    pub fn with_startup_toolset(self, toolset: crate::toolset::Toolset) -> Self {
        if crate::toolset::Toolset::from_env().is_none() {
            *self
                .toolset
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner) = toolset;
        }
        self
    }

    /// This principal's outbound notification fan-out. A transport subscribes
    /// here to learn what to push to the client without being asked.
    pub fn notifications(&self) -> &NotificationSink {
        &self.notifications
    }

    /// Tells every subscribed client that this principal's capability set, and
    /// therefore the tool list `tools/list` returns, has changed.
    ///
    /// Call only from whoever observes the change: over streamable HTTP that is
    /// `McpServers` detecting a rotated `CapabilityHandle`. A stdio session's
    /// capabilities are frozen by its bootstrap credential.
    pub fn notify_tools_list_changed(&self) {
        self.notifications.publish(tools_list_changed_frame());
    }

    pub async fn handle_message(&self, message: Value) -> Option<Value> {
        let Some(object) = message.as_object() else {
            return Some(error(Value::Null, INVALID_REQUEST, "Invalid Request", None));
        };
        let id = object.get("id").cloned();
        let response_id = id.clone().unwrap_or(Value::Null);
        if id
            .as_ref()
            .is_some_and(|id| !id.is_string() && !id.is_number())
        {
            return Some(error(Value::Null, INVALID_REQUEST, "Invalid Request", None));
        }
        if id.as_ref().is_some_and(|id| {
            serde_json::to_vec(id).map_or(true, |bytes| bytes.len() > MAX_REQUEST_ID_BYTES)
        }) {
            return Some(error(Value::Null, INVALID_REQUEST, "Invalid Request", None));
        }
        if object.get("jsonrpc") != Some(&Value::String("2.0".to_owned())) {
            return Some(error(response_id, INVALID_REQUEST, "Invalid Request", None));
        }
        let Some(method) = object.get("method").and_then(Value::as_str) else {
            return Some(error(response_id, INVALID_REQUEST, "Invalid Request", None));
        };
        if object
            .keys()
            .any(|key| !matches!(key.as_str(), "jsonrpc" | "id" | "method" | "params"))
        {
            return id.map(|id| error(id, INVALID_REQUEST, "Invalid Request", None));
        }
        let params = object.get("params").cloned().unwrap_or_else(|| json!({}));
        let mut lifecycle = self.lifecycle.lock().await;

        if method == "initialize" {
            id.as_ref()?;
            if !params
                .get("capabilities")
                .is_some_and(bounded_client_capabilities_value)
            {
                return id.map(|id| error(id, INVALID_PARAMS, "Invalid params", None));
            }
            let parsed = serde_json::from_value::<InitializeParams>(params);
            let Ok(parsed) = parsed else {
                return id.map(|id| error(id, INVALID_PARAMS, "Invalid params", None));
            };
            if !bounded_client_capabilities(&parsed.capabilities) {
                return id.map(|id| {
                    error(
                        id,
                        INVALID_PARAMS,
                        "Invalid params",
                        Some(json!({"supportedProtocolVersion": MCP_PROTOCOL_VERSION})),
                    )
                });
            }
            let negotiated = negotiate_protocol_version(&parsed.protocol_version);
            // A re-`initialize` is a session reset, not a protocol error: MCP
            // clients over streamable HTTP call `initialize` on every
            // reconnect. The reset clears stale cancellation state; in-flight
            // work from the previous session runs to completion.
            self.pending_cancellations.lock().await.clear();
            self.workflow_handles.reset();
            *lifecycle = Lifecycle::AwaitingInitializedNotification;
            return id.map(|id| {
                success(
                    id,
                    json!({
                        "protocolVersion": negotiated,
                        "capabilities": {
                            "tools": {"listChanged": true},
                            "resources": {"subscribe": false, "listChanged": false},
                            "prompts": {"listChanged": false}
                        },
                        "serverInfo": {
                            "name": "automation-runtime",
                            "version": env!("CARGO_PKG_VERSION")
                        },
                        "instructions": crate::INITIALIZE_INSTRUCTIONS
                    }),
                )
            });
        }

        if *lifecycle == Lifecycle::AwaitingInitialize {
            return id.map(|id| error(id, NOT_INITIALIZED, "Server not initialized", None));
        }
        if method == "notifications/initialized" {
            if id.is_some()
                || *lifecycle != Lifecycle::AwaitingInitializedNotification
                || bounded_parse::<NotificationParams>(params).is_err()
            {
                return id.map(|id| error(id, INVALID_REQUEST, "Invalid Request", None));
            }
            *lifecycle = Lifecycle::Ready;
            return None;
        }
        if *lifecycle != Lifecycle::Ready {
            return id.map(|id| error(id, NOT_INITIALIZED, "Server not initialized", None));
        }

        drop(lifecycle);

        if method == "notifications/cancelled" && id.is_none() {
            let cancellation: Cancellation = match bounded_parse(params) {
                Ok(cancellation) => cancellation,
                Err(()) => return None,
            };
            let key = request_key(&cancellation.request_id);
            if let Some(notification) = self.in_flight.lock().await.get(&key).cloned() {
                notification.notify_one();
            } else {
                let mut pending = self.pending_cancellations.lock().await;
                if pending.len() < MAX_PENDING_CANCELLATIONS {
                    pending.insert(key);
                }
            }
            return None;
        }

        id.as_ref()?;
        let id = id.expect("request id checked above");
        let key = request_key(&id);
        let cancelled = Arc::new(Notify::new());
        if self.shutting_down.load(Ordering::Acquire)
            || self.pending_cancellations.lock().await.remove(&key)
        {
            return Some(error(id, REQUEST_CANCELLED, "Request cancelled", None));
        }
        {
            let mut in_flight = self.in_flight.lock().await;
            if in_flight.contains_key(&key) {
                return Some(error(id, INVALID_REQUEST, "Invalid Request", None));
            }
            in_flight.insert(key.clone(), cancelled.clone());
        }
        // Close the race where cancellation observes no in-flight request after the
        // request's first pending check but immediately before registration.
        if self.pending_cancellations.lock().await.remove(&key) {
            cancelled.notify_one();
        }
        if self.shutting_down.load(Ordering::Acquire) {
            cancelled.notify_one();
        }
        let response = tokio::select! {
            biased;
            response = self.dispatch_request(id.clone(), method, params) => response,
            () = cancelled.notified() => error(
                id,
                REQUEST_CANCELLED,
                "Request cancelled",
                None,
            ),
        };
        self.in_flight.lock().await.remove(&key);
        Some(response)
    }

    async fn dispatch_request(&self, id: Value, method: &str, params: Value) -> Value {
        match method {
            "ping" if empty_object(&params) => self.authenticated_empty(id, json!({})),
            "ping" => error(id, INVALID_PARAMS, "Invalid params", None),
            "tools/list" => self.list_tools(id, params),
            "tools/call" => self.call_tool(id, params).await,
            "resources/list" => self.list_resources(id, params).await,
            "resources/read" => self.read_resource(id, params).await,
            "prompts/list" => self.list_prompts(id, params),
            "prompts/get" => self.get_prompt(id, params),
            "resources/templates/list" if valid_initial_list_params(&params) => {
                match self.authorize_response(id.clone(), types::InterfaceOperation::ReadArtifact) {
                    Ok(()) => success(id, json!({"resourceTemplates": []})),
                    Err(response) => response,
                }
            }
            "resources/templates/list" => error(id, INVALID_PARAMS, "Invalid params", None),
            _ => error(id, METHOD_NOT_FOUND, "Method not found", None),
        }
    }

    fn authenticated_empty(&self, id: Value, result: Value) -> Value {
        let context = self.request_context();
        match self.authorization.validate(&context) {
            Ok(()) => success(id, result),
            Err(error) => interface_error_response(id, error),
        }
    }

    fn authorize_response(
        &self,
        id: Value,
        operation: types::InterfaceOperation,
    ) -> Result<(), Value> {
        let context = self.request_context();
        self.authorization
            .authorize(&context, operation)
            .map_err(|error| interface_error_response(id, error))
    }

    fn request_context(&self) -> types::RequestContext {
        self.handle.context(Utc::now() + Duration::minutes(1), None)
    }

    /// Read newline-delimited JSON-RPC from `input` and write responses to
    /// `output` until the client disconnects or the server shuts down.
    pub async fn serve<R, W>(&self, input: R, output: W) -> io::Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut input = BufReader::new(input);
        // Every frame, response or notification, goes through `write_response`,
        // which writes the frame and its newline while holding this lock. That
        // is what stops a notification interleaving into a response and killing
        // the session with an unparseable line.
        let output = Arc::new(Mutex::new(output));
        let mut pending: FuturesUnordered<Pin<Box<dyn Future<Output = io::Result<()>> + '_>>> =
            FuturesUnordered::new();
        let mut notifications = self.notifications.subscribe().await;
        let mut notifications_open = true;
        // Notification writes are queued, not awaited inline, so a client that
        // stops draining stdout cannot stall the read loop and deadlock the
        // session. Notifications arrive unbidden, so they need the cap that the
        // client's own request volume already puts on responses: past
        // MAX_PENDING_NOTIFICATION_WRITES the branch is disabled and nothing is
        // pulled off the subscription until a write lands. Frames missed while
        // stalled are not lost: the subscription resumes from its cursor and
        // reports a gap if retention passed it.
        let outstanding = Arc::new(AtomicUsize::new(0));
        let mut frame = Vec::new();
        loop {
            tokio::select! {
                notification = notifications.recv(),
                    if notifications_open
                        && outstanding.load(Ordering::Acquire) < MAX_PENDING_NOTIFICATION_WRITES =>
                {
                    match notification {
                        // `write_response`'s oversized-frame fallback is
                        // unreachable here: an `EventStore` payload is
                        // sanitized to 16 KiB and a kind to 128 B, three orders
                        // of magnitude under `MAX_FRAME_BYTES`.
                        Some(notification) => {
                            let output = output.clone();
                            let outstanding = outstanding.clone();
                            outstanding.fetch_add(1, Ordering::Release);
                            pending.push(Box::pin(async move {
                                let result = write_response(&output, Some(notification)).await;
                                outstanding.fetch_sub(1, Ordering::Release);
                                result
                            }));
                        }
                        None => notifications_open = false,
                    }
                }
                status = read_bounded_frame(&mut input, &mut frame),
                    if pending.len() < MAX_IN_FLIGHT_REQUESTS =>
                {
                    let response = match status? {
                        FrameStatus::Eof => break,
                        FrameStatus::Oversized => Some(error(
                            Value::Null,
                            INVALID_REQUEST,
                            "Invalid Request",
                            Some(json!({"reason":"frameTooLarge","maxBytes":MAX_FRAME_BYTES})),
                        )),
                        FrameStatus::Complete => match serde_json::from_slice::<Value>(&frame) {
                            Ok(message) => {
                                // Handshake frames (and any traffic before Ready) must
                                // finish before the next frame is dispatched. Concurrent
                                // `pending` handlers otherwise let tools/call observe
                                // AwaitingInitializedNotification and return -32002 when
                                // the client wrote initialized + tools/call back-to-back.
                                let method = message
                                    .get("method")
                                    .and_then(Value::as_str)
                                    .unwrap_or("");
                                let serialize = matches!(
                                    method,
                                    "initialize" | "notifications/initialized"
                                ) || *self.lifecycle.lock().await != Lifecycle::Ready;
                                if serialize {
                                    let response = self.handle_message(message).await;
                                    write_response(&output, response).await?;
                                    None
                                } else {
                                    let output = output.clone();
                                    pending.push(Box::pin(async move {
                                        let response = self.handle_message(message).await;
                                        write_response(&output, response).await
                                    }));
                                    if let Some(Some(result)) = pending.next().now_or_never() {
                                        result?;
                                    }
                                    None
                                }
                            }
                            Err(_) => Some(error(Value::Null, PARSE_ERROR, "Parse error", None)),
                        },
                    };
                    if response.is_some() {
                        let output = output.clone();
                        pending.push(Box::pin(async move {
                            write_response(&output, response).await
                        }));
                    }
                }
                Some(result) = pending.next(), if !pending.is_empty() => result?,
            }
        }
        self.shutting_down.store(true, Ordering::Release);
        for notification in self.in_flight.lock().await.values() {
            notification.notify_one();
        }
        let drain = async {
            while let Some(result) = pending.next().await {
                result?;
            }
            Ok::<(), io::Error>(())
        };
        match tokio::time::timeout(StdDuration::from_millis(250), drain).await {
            Ok(result) => result?,
            Err(_) => {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "request drain timed out",
                ))
            }
        }
        let result = output.lock().await.flush().await;
        result
    }

    /// The phase this connection's `tools/list` is currently scoped to.
    fn current_toolset(&self) -> crate::toolset::Toolset {
        *self
            .toolset
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn set_toolset(&self, toolset: crate::toolset::Toolset) -> bool {
        let mut current = self
            .toolset
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let changed = *current != toolset;
        *current = toolset;
        changed
    }

    fn list_tools(&self, id: Value, params: Value) -> Value {
        let context = self.request_context();
        if let Err(interface_error) = self.authorization.validate(&context) {
            return interface_error_response(id, interface_error);
        }
        if !valid_initial_list_params(&params) {
            return error(id, INVALID_PARAMS, "Invalid params", None);
        }
        let capabilities = self
            .handle
            .context(chrono::Utc::now() + chrono::Duration::minutes(1), None)
            .capabilities;
        let mut tools = Vec::new();
        for name in [
            "checkpoint_save",
            "click",
            "click_and_wait_for_popup",
            "context_ask",
            "context_neighbors",
            "cookie_delete",
            "cookie_get",
            "cookie_set",
            "command_execute",
            "control_action",
            "intent_complete_form",
            "intent_dismiss_obstruction",
            "intent_extract",
            "intent_fill",
            "intent_follow",
            "intent_locate",
            "intent_submit_and_verify",
            "intent_wait_for_state",
            "dialog",
            "download_url",
            "emulate",
            "evaluate_javascript",
            "events_read",
            "inspect",
            "job_cancel",
            "job_status",
            "job_submit",
            "navigate",
            "network_log",
            "a11y_snapshot",
            "extract_structured",
            "form_snapshot",
            "page_activate",
            "page_close",
            "page_list",
            "page_open",
            "pdf",
            "recovery_status",
            "runtime_info",
            "screenshot",
            "session_close",
            "session_create",
            "session_list",
            "toolset_select",
            "type_text",
            "upload_files",
            "wait_for",
            "workflow_start",
            "workflow_observe",
            "workflow_recover",
        ] {
            if crate::jobs::is_job_tool(name) && self.jobs.is_none() {
                continue;
            }
            let required = required_capabilities(name).expect("registered tool");
            if !self.current_toolset().advertises(name) {
                continue;
            }
            if required
                .iter()
                .all(|capability| capabilities.contains(*capability))
            {
                let input_schema = advertised_tool_schema_for_capabilities(name, &capabilities);
                tools.push(json!({
                    "name": name,
                    "title": tool_title(name),
                    "description": tool_description(name),
                    "inputSchema": input_schema,
                    "outputSchema": advertised_tool_output_schema(name),
                    "annotations": tool_annotations(name)
                }));
            }
        }
        // `toolset_select` needs no capability, so alone it would be the one
        // tool a principal holding nothing still sees. A principal with no
        // capabilities must be shown no surface at all.
        if tools.len() == 1 && tools[0]["name"] == "toolset_select" {
            tools.clear();
        }
        success(id, json!({"tools":tools}))
    }

    fn list_prompts(&self, id: Value, params: Value) -> Value {
        let context = self.request_context();
        if let Err(interface_error) = self.authorization.validate(&context) {
            return interface_error_response(id, interface_error);
        }
        if !valid_initial_list_params(&params) {
            return error(id, INVALID_PARAMS, "Invalid params", None);
        }
        success(id, crate::prompts::list_prompts())
    }

    fn get_prompt(&self, id: Value, params: Value) -> Value {
        let context = self.request_context();
        if let Err(interface_error) = self.authorization.validate(&context) {
            return interface_error_response(id, interface_error);
        }
        let input: PromptGetArgs = match bounded_parse(params) {
            Ok(input) => input,
            Err(()) => return error(id, INVALID_PARAMS, "Invalid params", None),
        };
        match crate::prompts::get_prompt(&input.name, &input.arguments) {
            Some(result) => success(id, result),
            // An unknown name and a known name missing a required argument
            // both collapse here rather than falling through to a prompt
            // with placeholder text an agent would then execute verbatim.
            None => error(id, INVALID_PARAMS, "Invalid params", None),
        }
    }

    async fn list_resources(&self, id: Value, params: Value) -> Value {
        if !valid_initial_list_params(&params) {
            return error(id, INVALID_PARAMS, "Invalid params", None);
        }
        // The static bobby:// documents (capabilities, failure taxonomy,
        // intents, primitives) are repair documentation: a principal that
        // merely lacks artifact:read still reads them, because an agent that
        // just hit missingCapability is exactly the one that needs them. A
        // principal that fails to authenticate at all (revoked, expired) is
        // denied like anywhere else. Live artifact:// entries stay gated.
        let context = self.request_context();
        let artifact_entries = match self
            .authorization
            .authorize(&context, types::InterfaceOperation::ReadArtifact)
        {
            Ok(()) => self
                .resources
                .list()
                .await
                .into_iter()
                .map(|artifact_id| {
                    json!({
                        "uri":format!("artifact://{artifact_id}"),
                        "name":format!("artifact-{artifact_id}"),
                        "description":"Authenticated runtime artifact"
                    })
                })
                .collect::<Vec<_>>(),
            Err(error) if error.code == types::InterfaceErrorCode::MissingCapability => Vec::new(),
            Err(error) => return interface_error_response(id, error),
        };
        let resources = static_resources()
            .iter()
            .map(
                |(uri, name, description)| json!({"uri":uri,"name":name,"description":description}),
            )
            .chain(artifact_entries)
            .collect::<Vec<_>>();
        success(id, json!({"resources":resources}))
    }

    async fn read_resource(&self, id: Value, params: Value) -> Value {
        let input: ResourceReadArgs = match bounded_parse(params) {
            Ok(input) => input,
            Err(()) => return error(id, INVALID_PARAMS, "Invalid params", None),
        };
        if let Some(text) = static_resource_body(&input.uri) {
            let context = self.request_context();
            // Repair docs are readable by any authenticated principal, even
            // one missing artifact:read; a revoked/expired principal (any
            // other authorization failure) is denied like anywhere else.
            match self
                .authorization
                .authorize(&context, types::InterfaceOperation::ReadArtifact)
            {
                Ok(()) => {}
                Err(error) if error.code == types::InterfaceErrorCode::MissingCapability => {}
                Err(error) => return interface_error_response(id, error),
            }
            return success(
                id,
                json!({"contents":[{"uri":input.uri,"mimeType":"text/markdown","text":text}]}),
            );
        }
        let Some(artifact_id) = parse_artifact_uri(&input.uri) else {
            return error(id, INVALID_PARAMS, "Invalid params", None);
        };
        let context = self.request_context();
        if let Err(error) = self
            .authorization
            .authorize(&context, types::InterfaceOperation::ReadArtifact)
        {
            return interface_error_response(id, error);
        }
        let content = match self
            .resources
            .read(&self.handle, &context, artifact_id)
            .await
        {
            Ok(Some(content)) => content,
            Ok(None) => {
                return error(
                    id,
                    INTERFACE_ERROR,
                    "Runtime interface error",
                    Some(json!({
                        "interfaceError":not_found_error(&context),
                        "resource":{"uri":input.uri}
                    })),
                )
            }
            Err(interface_error) => return interface_error_response(id, interface_error),
        };
        if content.bytes.len() > MAX_RESOURCE_ENCODED_BYTES {
            return error(
                id,
                INTERFACE_ERROR,
                "Runtime interface error",
                Some(json!({
                    "resource":{"uri":input.uri},
                    "reason":"resourceTooLarge",
                    "maxEncodedBytes":MAX_RESOURCE_ENCODED_BYTES
                })),
            );
        }
        let body = if textual_media_type(&content.media_type) {
            match String::from_utf8(content.bytes) {
                Ok(text) => json!({
                    "uri":input.uri,"mimeType":content.media_type,"text":text
                }),
                Err(error) => {
                    let bytes = error.into_bytes();
                    let blob = BASE64.encode(bytes);
                    if blob.len() > MAX_RESOURCE_ENCODED_BYTES {
                        return resource_too_large(id, input.uri);
                    }
                    json!({"uri":input.uri,"mimeType":content.media_type,"blob":blob})
                }
            }
        } else {
            let blob = BASE64.encode(content.bytes);
            if blob.len() > MAX_RESOURCE_ENCODED_BYTES {
                return resource_too_large(id, input.uri);
            }
            json!({"uri":input.uri,"mimeType":content.media_type,"blob":blob})
        };
        let response = success(id.clone(), json!({"contents":[body]}));
        if serde_json::to_vec(&response).map_or(true, |bytes| bytes.len() > MAX_FRAME_BYTES) {
            return resource_too_large(id, input.uri);
        }
        response
    }

    async fn call_tool(&self, id: Value, params: Value) -> Value {
        let identity_context = self.request_context();
        if let Err(interface_error) = self.authorization.validate(&identity_context) {
            return interface_error_response(id, interface_error);
        }
        let mut call: ToolCall = match bounded_parse(params) {
            Ok(call) => call,
            Err(()) => return invalid_params_reason(id, "malformedArguments"),
        };
        if call.name.len() > 64 || !self.tool_available(&call.name) {
            return error(id, METHOD_NOT_FOUND, "Method not found", None);
        }
        // `toolset_select` is the one tool with no `InterfaceOperation`: it
        // authorizes nothing, only changing which tools this connection sees.
        // Handle it before the operation lookup; a `required_operation` that
        // tolerated `None` would un-gate any tool someone forgets to map.
        if call.name == "toolset_select" {
            if let Err(violation) = validate_tool_arguments(&call.name, &call.arguments) {
                return invalid_params(id, Some(violation));
            }
            let input: ToolsetSelectArgs = match bounded_parse(call.arguments) {
                Ok(input) => input,
                Err(()) => return invalid_params_reason(id, "malformedArguments"),
            };
            let Some(toolset) = crate::toolset::Toolset::parse(&input.toolset) else {
                return invalid_params_reason(id, "malformedArguments");
            };
            // Only notify on a real change: a client re-selecting the phase it
            // is already in should not be told to re-read an unchanged list.
            if self.set_toolset(toolset) {
                self.notify_tools_list_changed();
            }
            return match to_json(json!({"toolset": toolset.as_str()})) {
                Ok(value) => self.tool_success(id, value).await,
                Err(interface_error) => interface_error_response(id, interface_error),
            };
        }
        let context = self.handle.context(Utc::now() + Duration::minutes(1), None);
        let operation = required_operation(&call.name).expect("availability checked above");
        if let Err(interface_error) = self.authorization.authorize(&context, operation) {
            return interface_error_response(id, interface_error);
        }
        call.arguments = match self
            .workflow_handles
            .normalize_arguments(&call.name, &call.arguments)
        {
            Ok(arguments) => arguments,
            Err(WorkflowHandleError::BindingConflict) => {
                return invalid_params_reason(id, "workflowBindingConflict")
            }
            Err(
                WorkflowHandleError::Unknown
                | WorkflowHandleError::Malformed
                | WorkflowHandleError::GenerationChanged,
            ) => return invalid_params_reason(id, "unknownWorkflowHandle"),
            Err(WorkflowHandleError::CapacityExhausted | WorkflowHandleError::SupervisorLost) => {
                return invalid_params_reason(id, "unknownWorkflowHandle")
            }
        };
        if let Err(violation) = validate_tool_arguments(&call.name, &call.arguments) {
            return invalid_params(id, Some(violation));
        }
        if let Some(metrics) = &self.operational_metrics {
            metrics.record_workflow_call(workflow_call_class(&call.name));
        }
        self.dispatch_named_tool(id, call, context).await
    }

    async fn tool_success(&self, id: Value, mut value: Value) -> Value {
        crate::resources::redact_mcp_download_paths(&mut value);
        // Attach a machine-readable repair hint to failures before the value
        // is frozen into `content` text, so both representations carry it.
        // `needsReconciliation` always overrides the code's general repair:
        // retrying that outcome can double-apply a side effect.
        let status = value.get("status").and_then(Value::as_str);
        let candidate_limit = value
            .get("error")
            .and_then(|error| error.get("message"))
            .and_then(Value::as_str)
            .is_some_and(|message| message.starts_with("candidate limit exceeded:"));
        let repair = if status == Some("needsReconciliation") {
            Some(crate::repair::reconciliation_repair())
        } else if candidate_limit {
            Some(crate::repair::candidate_limit_repair())
        } else {
            value
                .get("error")
                .and_then(|error| error.get("code"))
                .and_then(Value::as_str)
                .and_then(crate::repair::repair_for_code)
        };
        if let (Some(repair), Some(error)) = (repair, value.get_mut("error")) {
            if error.is_object() {
                error["repair"] = repair;
            }
        }
        let text = serde_json::to_string(&value).unwrap_or_else(|_| "null".to_owned());
        let trusted = self
            .resources
            .list()
            .await
            .into_iter()
            .collect::<BTreeSet<_>>();
        let mut referenced = BTreeSet::new();
        collect_artifact_ids(&value, &mut referenced);
        let mut content = vec![json!({"type":"text","text":text})];
        for artifact_id in referenced.intersection(&trusted) {
            content.push(json!({
                "type":"resource_link",
                "uri":format!("artifact://{artifact_id}"),
                "name":format!("artifact-{artifact_id}"),
                "description":"Authenticated runtime artifact"
            }));
        }
        // MCP `isError` mirrors the command status: a failed command is a
        // failed tool call, not a successful one carrying a failure payload.
        // `restarted`/`resumed` are recovery decisions, not failures, and
        // tools whose result has no `status` never fail this way.
        let is_error = matches!(
            value.get("status").and_then(Value::as_str),
            Some(
                "retryableFailure"
                    | "needsReconciliation"
                    | "policyDenied"
                    | "resourceExhausted"
                    | "failed"
            )
        );
        success(
            id,
            json!({
                "content":content,
                "structuredContent":value,
                "isError":is_error
            }),
        )
    }

    /// One call for a Boundary command: mint the checkpoint, then run it.
    ///
    /// The gate in `Executor::validate` is unchanged and still matches on all
    /// five fields. A checkpoint that fails to save fails the call, so this is
    /// never a way to reach a Boundary action without one.
    async fn submit_envelope_with_auto_checkpoint(
        &self,
        context: types::RequestContext,
        envelope: types::CommandEnvelope,
    ) -> interface_core::InterfaceResult<Value> {
        let registration_context = context.clone();
        let (outcome, checkpoint_id) = self
            .runtime
            .submit_with_auto_checkpoint(context, envelope.clone())
            .await?;
        let admission = self
            .resources
            .register_outcome(&self.handle, &registration_context, &envelope, &outcome)
            .await;
        let mut value = to_json(outcome)?;
        admission.apply_to_mcp_value(&mut value, &envelope.command_id);
        if let Some(object) = value.as_object_mut() {
            object.insert("workflowId".to_owned(), json!(envelope.workflow_id.clone()));
            object.insert("attemptId".to_owned(), json!(envelope.attempt_id.clone()));
            // So the caller can still name this checkpoint to `workflow_recover`.
            object.insert("checkpointId".to_owned(), json!(checkpoint_id));
        }
        self.events
            .append_for(
                registration_context.principal_id.clone(),
                interface_core::Event::new("command.outcome", value.clone()),
            )
            .await;
        Ok(value)
    }

    async fn submit_envelope(
        &self,
        context: types::RequestContext,
        envelope: types::CommandEnvelope,
    ) -> interface_core::InterfaceResult<Value> {
        let registration_context = context.clone();
        match self.runtime.submit(context, envelope.clone()).await {
            Ok(outcome) => {
                let admission = self
                    .resources
                    .register_outcome(&self.handle, &registration_context, &envelope, &outcome)
                    .await;
                match to_json(outcome) {
                    Ok(mut value) => {
                        admission.apply_to_mcp_value(&mut value, &envelope.command_id);
                        // `CommandOutcome` carries only `commandId`, so the
                        // workflow and attempt ids are echoed here. Callers
                        // pass workflowId back to stay in the same workflow,
                        // and the pair (commandId, attemptId) is what a
                        // Boundary command's pre-action checkpoint must name.
                        //
                        // Only when the outcome belongs to this envelope. An
                        // idempotent replay returns the *original* attempt's
                        // outcome, whose `commandId` is not this envelope's;
                        // echoing this envelope's workflow and attempt ids
                        // there would hand back a pair that never ran, and a
                        // checkpoint naming it would fail the boundary gate.
                        // The returned `commandId` stays the caller's handle.
                        let is_this_attempt = value
                            .get("commandId")
                            .is_some_and(|id| *id == json!(envelope.command_id.clone()));
                        if let Some(object) = value.as_object_mut().filter(|_| is_this_attempt) {
                            object.insert(
                                "workflowId".to_owned(),
                                json!(envelope.workflow_id.clone()),
                            );
                            object
                                .insert("attemptId".to_owned(), json!(envelope.attempt_id.clone()));
                        }
                        self.events
                            .append_for(
                                registration_context.principal_id.clone(),
                                interface_core::Event::new("command.outcome", value.clone()),
                            )
                            .await;
                        Ok(value)
                    }
                    Err(error) => Err(error),
                }
            }
            Err(error) => Err(error),
        }
    }

    fn tool_available(&self, name: &str) -> bool {
        if crate::jobs::is_job_tool(name) && self.jobs.is_none() {
            return false;
        }
        let capabilities = self
            .handle
            .context(Utc::now() + Duration::minutes(1), None)
            .capabilities;
        required_capabilities(name).is_some_and(|required| {
            required
                .iter()
                .all(|capability| capabilities.contains(*capability))
        })
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct InitializeParams {
    protocol_version: String,
    capabilities: ClientCapabilities,
    #[serde(rename = "clientInfo")]
    _client_info: Implementation,
    #[serde(default, rename = "_meta")]
    _meta: Option<Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClientCapabilities {
    #[serde(default)]
    experimental: Option<BTreeMap<String, serde_json::Map<String, Value>>>,
    #[serde(default)]
    roots: Option<RootsCapability>,
    #[serde(default)]
    sampling: Option<SamplingCapability>,
    #[serde(default)]
    elicitation: Option<ElicitationCapability>,
    #[serde(default)]
    tasks: Option<TasksCapability>,
    #[serde(flatten)]
    extensions: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct RootsCapability {
    #[serde(default)]
    list_changed: bool,
    #[serde(flatten)]
    extensions: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct CapabilityMarker {
    #[serde(flatten)]
    extensions: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct SamplingCapability {
    #[serde(default)]
    context: Option<CapabilityMarker>,
    #[serde(default)]
    tools: Option<CapabilityMarker>,
    #[serde(flatten)]
    extensions: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct ElicitationCapability {
    #[serde(default)]
    form: Option<CapabilityMarker>,
    #[serde(default)]
    url: Option<CapabilityMarker>,
    #[serde(flatten)]
    extensions: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct TasksCapability {
    #[serde(default)]
    list: Option<CapabilityMarker>,
    #[serde(default)]
    cancel: Option<CapabilityMarker>,
    #[serde(default)]
    requests: Option<TaskRequestsCapability>,
    #[serde(flatten)]
    extensions: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct TaskRequestsCapability {
    #[serde(default)]
    sampling: Option<SamplingTaskCapability>,
    #[serde(default)]
    elicitation: Option<ElicitationTaskCapability>,
    #[serde(flatten)]
    extensions: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SamplingTaskCapability {
    #[serde(default)]
    create_message: Option<CapabilityMarker>,
    #[serde(flatten)]
    extensions: BTreeMap<String, Value>,
}

#[derive(Deserialize)]
struct ElicitationTaskCapability {
    #[serde(default)]
    create: Option<CapabilityMarker>,
    #[serde(flatten)]
    extensions: BTreeMap<String, Value>,
}

fn bounded_client_capabilities(capabilities: &ClientCapabilities) -> bool {
    let mut observed =
        capabilities.experimental.as_ref().map_or(0, BTreeMap::len) + capabilities.extensions.len();
    if let Some(roots) = &capabilities.roots {
        let _ = roots.list_changed;
        observed += roots.extensions.len();
    }
    if let Some(sampling) = &capabilities.sampling {
        observed += marker_size(sampling.context.as_ref())
            + marker_size(sampling.tools.as_ref())
            + sampling.extensions.len();
    }
    if let Some(elicitation) = &capabilities.elicitation {
        observed += marker_size(elicitation.form.as_ref())
            + marker_size(elicitation.url.as_ref())
            + elicitation.extensions.len();
    }
    if let Some(tasks) = &capabilities.tasks {
        observed += marker_size(tasks.list.as_ref())
            + marker_size(tasks.cancel.as_ref())
            + tasks.extensions.len();
        if let Some(requests) = &tasks.requests {
            observed += requests.extensions.len();
            if let Some(sampling) = &requests.sampling {
                observed +=
                    marker_size(sampling.create_message.as_ref()) + sampling.extensions.len();
            }
            if let Some(elicitation) = &requests.elicitation {
                observed += marker_size(elicitation.create.as_ref()) + elicitation.extensions.len();
            }
        }
    }
    observed <= 256
}

fn marker_size(marker: Option<&CapabilityMarker>) -> usize {
    marker.map_or(0, |marker| marker.extensions.len())
}

fn bounded_client_capabilities_value(value: &Value) -> bool {
    let Some(capabilities) = value.as_object() else {
        return false;
    };
    if capabilities.len() > 32
        || serde_json::to_vec(value).map_or(true, |bytes| bytes.len() > 16 * 1024)
        || capabilities.values().any(|value| !value.is_object())
    {
        return false;
    }
    if capabilities
        .get("experimental")
        .and_then(Value::as_object)
        .is_some_and(|experimental| experimental.values().any(|value| !value.is_object()))
    {
        return false;
    }
    if !valid_marker_fields(capabilities.get("sampling"), &["context", "tools"])
        || !valid_marker_fields(capabilities.get("elicitation"), &["form", "url"])
        || !valid_tasks_capability(capabilities.get("tasks"))
    {
        return false;
    }
    let mut nodes = 0usize;
    bounded_json_value(value, 0, &mut nodes)
}

fn valid_marker_fields(value: Option<&Value>, fields: &[&str]) -> bool {
    let Some(value) = value else {
        return true;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    fields
        .iter()
        .all(|field| object.get(*field).is_none_or(Value::is_object))
}

fn valid_tasks_capability(value: Option<&Value>) -> bool {
    let Some(value) = value else {
        return true;
    };
    let Some(tasks) = value.as_object() else {
        return false;
    };
    if ["list", "cancel"]
        .iter()
        .any(|field| tasks.get(*field).is_some_and(|value| !value.is_object()))
    {
        return false;
    }
    let Some(requests) = tasks.get("requests") else {
        return true;
    };
    let Some(requests) = requests.as_object() else {
        return false;
    };
    valid_marker_fields(requests.get("sampling"), &["createMessage"])
        && valid_marker_fields(requests.get("elicitation"), &["create"])
}

fn bounded_json_value(value: &Value, depth: usize, nodes: &mut usize) -> bool {
    *nodes = nodes.saturating_add(1);
    if depth > 8 || *nodes > 256 {
        return false;
    }
    match value {
        Value::String(value) => value.len() <= 4096,
        Value::Array(values) => {
            values.len() <= 64
                && values
                    .iter()
                    .all(|value| bounded_json_value(value, depth + 1, nodes))
        }
        Value::Object(values) => {
            values.len() <= 64
                && values.iter().all(|(key, value)| {
                    key.len() <= 128 && bounded_json_value(value, depth + 1, nodes)
                })
        }
        _ => true,
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Implementation {
    #[serde(rename = "name")]
    _name: String,
    #[serde(rename = "version")]
    _version: String,
    #[serde(default)]
    #[serde(rename = "title")]
    _title: Option<String>,
    #[serde(default, rename = "description")]
    _description: Option<String>,
    #[serde(default, rename = "websiteUrl")]
    _website_url: Option<String>,
    #[serde(default, rename = "icons")]
    _icons: Vec<Icon>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Icon {
    #[serde(rename = "src")]
    _src: String,
    #[serde(default, rename = "mimeType")]
    _mime_type: Option<String>,
    #[serde(default, rename = "sizes")]
    _sizes: Vec<String>,
    #[serde(default, rename = "theme")]
    _theme: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Cancellation {
    request_id: Value,
    #[serde(default, rename = "reason")]
    _reason: Option<String>,
    #[serde(default, rename = "_meta")]
    _meta: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NotificationParams {
    #[serde(default, rename = "_meta")]
    _meta: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListParams {
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default, rename = "_meta")]
    _meta: Option<Value>,
}

fn empty_object(value: &Value) -> bool {
    value.as_object().is_some_and(serde_json::Map::is_empty)
}

fn valid_initial_list_params(value: &Value) -> bool {
    bounded_parse::<ListParams>(value.clone()).is_ok_and(|params| params.cursor.is_none())
}

fn request_key(id: &Value) -> String {
    serde_json::to_string(id).unwrap_or_else(|_| "null".to_owned())
}

fn bounded_parse<T: DeserializeOwned>(value: Value) -> Result<T, ()> {
    if serde_json::to_vec(&value).map_or(true, |bytes| bytes.len() > MAX_INPUT_BYTES) {
        return Err(());
    }
    serde_json::from_value(value).map_err(|_| ())
}

fn to_json<T: serde::Serialize>(value: T) -> interface_core::InterfaceResult<Value> {
    serde_json::to_value(value).map_err(|error| types::InterfaceError {
        code: types::InterfaceErrorCode::Internal,
        layer: types::ErrorLayer::Interface,
        message: format!("failed to serialize runtime response: {error}"),
        correlation_id: types::CorrelationId::new(),
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: false,
        required_capability: None,
    })
}

/// `-32602` with the schema keyword and JSON Pointer that rejected the call.
///
/// `pointer` and `constraint` must describe the schema, never the submitted
/// value, so they disclose nothing `tools/list` does not.
fn invalid_params(id: Value, violation: Option<crate::schema::SchemaViolation>) -> Value {
    let data = violation.map(|violation| {
        let mut data = json!({
            "reason":"schemaViolation",
            "pointer":violation.pointer,
            "constraint":violation.constraint
        });
        if let Some(repair) = crate::repair::repair_for_protocol_reason("schemaViolation") {
            data["repair"] = repair;
        }
        data
    });
    error(id, INVALID_PARAMS, "Invalid params", data)
}

/// `-32602` for a rejection with no single offending field: a body that passed
/// the schema but failed to deserialize, an expired deadline, a malformed
/// idempotency key.
fn invalid_params_reason(id: Value, reason: &'static str) -> Value {
    let mut data = json!({"reason":reason});
    if let Some(repair) = crate::repair::repair_for_protocol_reason(reason) {
        data["repair"] = repair;
    }
    error(id, INVALID_PARAMS, "Invalid params", Some(data))
}

fn job_port_error_response(id: Value, port_error: crate::jobs::JobPortError) -> Value {
    match port_error {
        crate::jobs::JobPortError::InvalidName | crate::jobs::JobPortError::InvalidPriority => {
            invalid_params_reason(id, "malformedArguments")
        }
        crate::jobs::JobPortError::NotFound => error(
            id,
            INTERFACE_ERROR,
            "Runtime interface error",
            Some(json!({
                "code":"notFound",
                "message": port_error.message(),
            })),
        ),
        crate::jobs::JobPortError::Unavailable(detail) => error(
            id,
            INTERFACE_ERROR,
            "Runtime interface error",
            Some(json!({
                "code":"internal",
                "message": detail,
            })),
        ),
    }
}

fn interface_error_response(id: Value, mut interface_error: types::InterfaceError) -> Value {
    let code = serde_json::to_value(interface_error.code)
        .ok()
        .and_then(|value| value.as_str().map(ToOwned::to_owned))
        .unwrap_or_else(|| "internal".to_owned());
    let diagnostic = match interface_error.required_capability {
        Some(capability) => format!(
            "Runtime interface error: {code} (requires {})",
            capability.as_str()
        ),
        None => format!("Runtime interface error: {code}"),
    };
    // MCP clients are external agents: the message may carry secrets, so only
    // an allowlisted, operator-actionable diagnostic crosses this boundary.
    let safe_diagnostic = interface_error
        .message
        .starts_with(BROWSER_LAUNCH_DIAGNOSTIC_PREFIX)
        .then(|| interface_error.message.clone());
    interface_error.message = "runtime interface request failed".to_owned();
    let repair = if safe_diagnostic.is_some() {
        Some(crate::repair::browser_launch_repair())
    } else {
        crate::repair::repair_for_code(&code)
    };
    let mut data = json!({"interfaceError":interface_error});
    if let Some(diagnostic) = safe_diagnostic {
        data["diagnostic"] = json!(diagnostic);
    }
    if let Some(repair) = repair {
        data["repair"] = repair;
    }
    let mut response = error(id, INTERFACE_ERROR, "Runtime interface error", Some(data));
    response["error"]["message"] = json!(diagnostic);
    response
}

fn collect_artifact_ids(value: &Value, found: &mut BTreeSet<String>) {
    match value {
        Value::Array(values) => {
            for value in values {
                collect_artifact_ids(value, found);
            }
        }
        Value::Object(values) => {
            for (key, value) in values {
                if matches!(key.as_str(), "artifactId" | "artifact_id") {
                    if let Some(artifact_id) = value.as_str() {
                        found.insert(artifact_id.to_owned());
                    }
                }
                collect_artifact_ids(value, found);
            }
        }
        Value::String(value) => {
            if let Some(artifact_id) = value.strip_prefix("artifact://") {
                found.insert(artifact_id.to_owned());
            }
        }
        _ => {}
    }
}

const DEFAULT_COMMAND_TIMEOUT_MS: u64 = 30_000;
const MAX_COMMAND_DEADLINE_MS: i64 = 300_000;

/// Builds the envelope a flat tool submits.
///
/// `workflow_id` is the caller's when supplied, otherwise freshly minted.
/// Keeping the caller's is what makes `checkpoint_save` and `workflow_recover`
/// reachable from the flat tools.
fn command_envelope(
    context: types::RequestContext,
    session_id: types::SessionId,
    page_id: Option<types::PageId>,
    workflow_id: Option<types::WorkflowId>,
    command: types::RuntimeCommand,
) -> (types::RequestContext, types::CommandEnvelope) {
    let deadline = context.deadline;
    (
        context,
        types::CommandEnvelope {
            schema_version: types::CommandEnvelope::SCHEMA_VERSION,
            command_id: types::CommandId::new(),
            workflow_id: workflow_id.unwrap_or_default(),
            attempt_id: types::AttemptId::new(),
            session_id,
            page_id,
            deadline,
            command,
        },
    )
}

/// Caller-pinned ids for Boundary flows: a Boundary command's pre-action
/// checkpoint must name the exact `commandId`/`attemptId` the submit will
/// carry, so the flat tools accept them optionally and they replace the
/// minted ones here. Unset ids stay server-minted, as before.
fn pin_envelope_ids(
    envelope: &mut types::CommandEnvelope,
    command_id: Option<types::CommandId>,
    attempt_id: Option<types::AttemptId>,
) {
    if let Some(command_id) = command_id {
        envelope.command_id = command_id;
    }
    if let Some(attempt_id) = attempt_id {
        envelope.attempt_id = attempt_id;
    }
}

fn apply_idempotency_key(
    context: &mut types::RequestContext,
    key: Option<String>,
) -> Result<(), ()> {
    context.idempotency_key = match key {
        Some(key) => Some(types::IdempotencyKey::try_from(key).map_err(|_| ())?),
        None => None,
    };
    Ok(())
}

fn primitive_envelope(
    context: types::RequestContext,
    session_id: types::SessionId,
    page_id: Option<types::PageId>,
    workflow_id: Option<types::WorkflowId>,
    command: types::PrimitiveCommand,
) -> (types::RequestContext, types::CommandEnvelope) {
    command_envelope(
        context,
        session_id,
        page_id,
        workflow_id,
        types::RuntimeCommand::Primitive(command),
    )
}

fn intent_envelope(
    context: types::RequestContext,
    session_id: types::SessionId,
    page_id: types::PageId,
    workflow_id: Option<types::WorkflowId>,
    command: types::IntentCommand,
) -> (types::RequestContext, types::CommandEnvelope) {
    command_envelope(
        context,
        session_id,
        Some(page_id),
        workflow_id,
        types::RuntimeCommand::Intent(command),
    )
}

fn parse_artifact_uri(uri: &str) -> Option<&str> {
    let artifact_id = uri.strip_prefix("artifact://")?;
    if artifact_id.is_empty()
        || artifact_id.len() > 128
        || !artifact_id
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() || byte == b'-')
    {
        return None;
    }
    Some(artifact_id)
}

fn textual_media_type(media_type: &str) -> bool {
    media_type.starts_with("text/")
        || matches!(
            media_type,
            "application/json" | "application/xml" | "application/javascript"
        )
}

fn resource_too_large(id: Value, uri: String) -> Value {
    error(
        id,
        INTERFACE_ERROR,
        "Runtime interface error",
        Some(json!({
            "resource":{"uri":uri},
            "reason":"resourceTooLarge",
            "maxEncodedBytes":MAX_RESOURCE_ENCODED_BYTES
        })),
    )
}

fn not_found_error(context: &types::RequestContext) -> types::InterfaceError {
    types::InterfaceError {
        code: types::InterfaceErrorCode::NotFound,
        layer: types::ErrorLayer::Interface,
        message: "requested resource was not found".to_owned(),
        correlation_id: context.correlation_id.clone(),
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: false,
        required_capability: None,
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FrameStatus {
    Eof,
    Complete,
    Oversized,
}

async fn read_bounded_frame<R>(input: &mut R, frame: &mut Vec<u8>) -> io::Result<FrameStatus>
where
    R: AsyncBufRead + Unpin,
{
    frame.clear();
    let mut oversized = false;
    loop {
        let available = input.fill_buf().await?;
        if available.is_empty() {
            return if oversized {
                Ok(FrameStatus::Oversized)
            } else if frame.is_empty() {
                Ok(FrameStatus::Eof)
            } else {
                Ok(FrameStatus::Complete)
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let payload_len = newline.unwrap_or(available.len());
        if !oversized {
            if frame.len().saturating_add(payload_len) > MAX_FRAME_BYTES {
                oversized = true;
                frame.clear();
            } else {
                frame.extend_from_slice(&available[..payload_len]);
            }
        }
        let consumed = newline.map_or(available.len(), |index| index + 1);
        input.consume(consumed);
        if newline.is_some() {
            if !oversized && frame.last() == Some(&b'\r') {
                frame.pop();
            }
            return Ok(if oversized {
                FrameStatus::Oversized
            } else {
                FrameStatus::Complete
            });
        }
    }
}

async fn write_response<W>(output: &Mutex<W>, response: Option<Value>) -> io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let Some(response) = response else {
        return Ok(());
    };
    let serialized = serde_json::to_vec(&response).map_err(io::Error::other)?;
    let serialized = if serialized.len() <= MAX_FRAME_BYTES {
        serialized
    } else {
        let fallback_id = response
            .get("id")
            .filter(|id| {
                serde_json::to_vec(id).is_ok_and(|bytes| bytes.len() <= MAX_REQUEST_ID_BYTES)
            })
            .cloned()
            .unwrap_or(Value::Null);
        let fallback = serde_json::to_vec(&error(
            fallback_id,
            INTERNAL_ERROR,
            "Internal error",
            Some(json!({"reason":"resultTooLarge","maxBytes":MAX_FRAME_BYTES})),
        ))
        .map_err(io::Error::other)?;
        if fallback.len() <= MAX_FRAME_BYTES {
            fallback
        } else {
            serde_json::to_vec(&error(Value::Null, INTERNAL_ERROR, "Internal error", None))
                .map_err(io::Error::other)?
        }
    };
    let mut output = output.lock().await;
    output.write_all(&serialized).await?;
    output.write_all(b"\n").await?;
    output.flush().await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_error_metadata_is_preserved_while_diagnostic_is_redacted() {
        let secret = "planted-secret-diagnostic";
        let response = interface_error_response(
            json!(7),
            types::InterfaceError {
                code: types::InterfaceErrorCode::ResourceExhausted,
                layer: types::ErrorLayer::Interface,
                message: secret.to_owned(),
                correlation_id: types::CorrelationId::new(),
                command_id: None,
                retryable: true,
                retry_after_ms: Some(1_234),
                reconciliation_required: true,
                required_capability: Some(types::Capability::SessionRead),
            },
        );
        assert!(!response.to_string().contains(secret));
        let error = &response["error"]["data"]["interfaceError"];
        assert_eq!(error["code"], "resourceExhausted");
        assert_eq!(error["retryable"], true);
        assert_eq!(error["retryAfterMs"], 1_234);
        assert_eq!(error["reconciliationRequired"], true);
        assert_eq!(error["requiredCapability"], "session:read");
    }

    #[test]
    fn interface_error_message_surfaces_safe_recovery_fields() {
        let secret = "planted-secret-diagnostic";
        let response = interface_error_response(
            json!(8),
            types::InterfaceError {
                code: types::InterfaceErrorCode::MissingCapability,
                layer: types::ErrorLayer::Interface,
                message: secret.to_owned(),
                correlation_id: types::CorrelationId::new(),
                command_id: None,
                retryable: false,
                retry_after_ms: None,
                reconciliation_required: false,
                required_capability: Some(types::Capability::BrowserFingerprint),
            },
        );
        let message = response["error"]["message"].as_str().unwrap();
        assert!(message.contains("missingCapability"), "{message}");
        assert!(message.contains("browser:fingerprint"), "{message}");
        assert!(!response.to_string().contains(secret));
    }
}
