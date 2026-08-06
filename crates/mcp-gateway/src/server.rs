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
    error, success, INTERFACE_ERROR, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST,
    MAX_EVENT_LIMIT, MAX_FRAME_BYTES, MAX_INPUT_BYTES, MAX_REQUEST_ID_BYTES, MCP_PROTOCOL_VERSION,
    METHOD_NOT_FOUND, NOT_INITIALIZED, PARSE_ERROR, REQUEST_CANCELLED,
};
use crate::resources::{static_resource_body, static_resources};
use crate::schema::{advertised_tool_schema, tool_output_schema, validate_tool_arguments};
use crate::ArtifactResources;

const MAX_RESOURCE_ENCODED_BYTES: usize = 768 * 1024;
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
    shutting_down: AtomicBool,
    /// The phase this connection's `tools/list` is scoped to. Per connection,
    /// not per principal: two agents on the same bearer can be in different
    /// phases, and the phase is a view over the surface, not authority.
    /// `std::sync::Mutex` because `list_tools` is sync and the critical section
    /// is a single enum copy.
    toolset: std::sync::Mutex<crate::toolset::Toolset>,
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
        Self::for_interface(runtime, handle, events, resources)
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
            shutting_down: AtomicBool::new(false),
            toolset: std::sync::Mutex::new(crate::toolset::Toolset::from_env().unwrap_or_default()),
        }
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
            if parsed.protocol_version != MCP_PROTOCOL_VERSION
                || !bounded_client_capabilities(&parsed.capabilities)
            {
                return id.map(|id| {
                    error(
                        id,
                        INVALID_PARAMS,
                        "Invalid params",
                        Some(json!({"supportedProtocolVersion": MCP_PROTOCOL_VERSION})),
                    )
                });
            }
            // A re-`initialize` is a session reset, not a protocol error: MCP
            // clients over streamable HTTP call `initialize` on every
            // reconnect. The reset clears stale cancellation state; in-flight
            // work from the previous session runs to completion.
            self.pending_cancellations.lock().await.clear();
            *lifecycle = Lifecycle::AwaitingInitializedNotification;
            return id.map(|id| {
                success(
                    id,
                    json!({
                        "protocolVersion": MCP_PROTOCOL_VERSION,
                        "capabilities": {
                            "tools": {"listChanged": true},
                            "resources": {"subscribe": false, "listChanged": false},
                            "prompts": {"listChanged": false}
                        },
                        "serverInfo": {
                            "name": "automation-runtime",
                            "version": env!("CARGO_PKG_VERSION")
                        }
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
            "workflow_recover",
        ] {
            let required = required_capabilities(name).expect("registered tool");
            if !self.current_toolset().advertises(name) {
                continue;
            }
            if required
                .iter()
                .all(|capability| capabilities.contains(*capability))
            {
                let mut input_schema = advertised_tool_schema(name);
                if name == "session_create" {
                    let policy = &mut input_schema["properties"]["executionPolicy"]["properties"];
                    if let Some(policy) = policy.as_object_mut() {
                        if !capabilities.contains(types::Capability::BrowserFingerprint) {
                            policy.remove("fingerprint");
                        }
                        if !capabilities.contains(types::Capability::BrowserHumanize) {
                            policy.remove("humanize");
                        }
                    }
                }
                tools.push(json!({
                    "name": name,
                    "title": tool_title(name),
                    "description": tool_description(name),
                    "inputSchema": input_schema,
                    "outputSchema": tool_output_schema(name),
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
        let call: ToolCall = match bounded_parse(params) {
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
        let mut context = self.handle.context(Utc::now() + Duration::minutes(1), None);
        let operation = required_operation(&call.name).expect("availability checked above");
        if let Err(interface_error) = self.authorization.authorize(&context, operation) {
            return interface_error_response(id, interface_error);
        }
        if let Err(violation) = validate_tool_arguments(&call.name, &call.arguments) {
            return invalid_params(id, Some(violation));
        }
        let result = match call.name.as_str() {
            "runtime_info" => {
                if bounded_parse::<EmptyArgs>(call.arguments).is_err() {
                    return invalid_params_reason(id, "malformedArguments");
                }
                // Principal-scoped, so it is added here rather than on the
                // runtime-wide `RuntimeInfo`. Without it a caller cannot see
                // the credential expiry coming.
                let credential_expires_at = self.handle.expires_at();
                self.runtime
                    .runtime_info(context)
                    .await
                    .and_then(to_json)
                    .map(|mut value| {
                        if let Some(object) = value.as_object_mut() {
                            object.insert(
                                "credentialExpiresAt".to_owned(),
                                json!(credential_expires_at.to_rfc3339()),
                            );
                        }
                        value
                    })
            }
            "session_list" => {
                if bounded_parse::<EmptyArgs>(call.arguments).is_err() {
                    return invalid_params_reason(id, "malformedArguments");
                }
                self.runtime
                    .list_sessions(context)
                    .await
                    .and_then(to_json)
                    .map(|sessions| json!({"sessions": sessions}))
            }
            "session_create" => {
                let input: SessionCreateArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                if input.profile.is_empty()
                    || input.profile.len() > 128
                    || input.proxy.as_ref().is_some_and(|proxy| proxy.len() > 2048)
                {
                    return invalid_params_reason(id, "malformedArguments");
                }
                self.runtime
                    .create_session(
                        context,
                        types::CreateSessionRequest {
                            profile: input.profile,
                            proxy: input.proxy,
                            execution_policy: input.execution_policy,
                        },
                    )
                    .await
                    .and_then(to_json)
            }
            "session_close" => {
                let input: SessionCloseArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                self.runtime
                    .delete_session(context, input.session_id)
                    .await
                    .and_then(|()| to_json(json!({"closed": true})))
            }
            "page_open" => {
                let input: PageOpenArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                async {
                    if input.url.is_some() {
                        self.authorization
                            .require_capability(&context, types::Capability::BrowserMutate)?;
                    }
                    let session_id = input.session_id;
                    let page = self
                        .runtime
                        .open_page(
                            context.clone(),
                            types::OpenPageRequest {
                                session_id: session_id.clone(),
                            },
                        )
                        .await?;
                    let Some(url) = input.url else {
                        return to_json(page);
                    };
                    let page_id = page.id.clone();
                    let (navigation_context, envelope) = primitive_envelope(
                        context.clone(),
                        session_id.clone(),
                        Some(page_id.clone()),
                        None,
                        types::PrimitiveCommand::Navigate(types::NavigateCommand {
                            url,
                            wait_until: types::WaitUntil::Interactive,
                            timeout_ms: DEFAULT_COMMAND_TIMEOUT_MS,
                        }),
                    );
                    let navigation_outcome =
                        self.runtime.submit(navigation_context, envelope).await?;
                    let navigation_completed =
                        matches!(navigation_outcome, types::CommandOutcome::Completed { .. });
                    let mut value = to_json(page)?;
                    let object = value
                        .as_object_mut()
                        .expect("page state serializes as an object");
                    object.insert("navigationOutcome".to_owned(), to_json(navigation_outcome)?);
                    if !navigation_completed {
                        let (cleanup_context, cleanup_envelope) = primitive_envelope(
                            context,
                            session_id,
                            Some(page_id.clone()),
                            None,
                            types::PrimitiveCommand::ClosePage(types::ClosePageCommand { page_id }),
                        );
                        let cleanup_outcome = self
                            .runtime
                            .submit(cleanup_context, cleanup_envelope)
                            .await?;
                        let page_closed =
                            matches!(cleanup_outcome, types::CommandOutcome::Completed { .. });
                        object.insert("cleanupOutcome".to_owned(), to_json(cleanup_outcome)?);
                        object.insert("pageClosed".to_owned(), json!(page_closed));
                    }
                    Ok(value)
                }
                .await
            }
            "command_execute" => {
                let input: CommandExecuteArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let now = Utc::now();
                if input.envelope.deadline <= now
                    || input.envelope.deadline
                        > now + Duration::milliseconds(MAX_COMMAND_DEADLINE_MS)
                {
                    return invalid_params_reason(id, "deadlineOutOfRange");
                }
                context.deadline = input.envelope.deadline;
                context.idempotency_key = match input.idempotency_key {
                    Some(key) => match types::IdempotencyKey::try_from(key) {
                        Ok(key) => Some(key),
                        Err(_) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                    },
                    None => None,
                };
                self.submit_envelope(context, input.envelope).await
            }
            "navigate" => {
                let input: NavigateArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::Navigate(types::NavigateCommand {
                        url: input.url,
                        wait_until: input.wait_until.unwrap_or(types::WaitUntil::Interactive),
                        timeout_ms: input.timeout_ms.unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS),
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "click" => {
                let input: ClickArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, mut envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::Click(types::ClickCommand {
                        selector: input.selector.unwrap_or_default(),
                        target: input.target,
                        boundary: input.boundary.unwrap_or(false),
                        expected_url: input.expected_url,
                    }),
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "type_text" => {
                let input: TypeTextArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::TypeText(types::TypeTextCommand {
                        selector: input.selector.unwrap_or_default(),
                        target: input.target,
                        value: input.value,
                        clear_first: input.clear_first.unwrap_or(false),
                        expected_url: input.expected_url,
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "inspect" => {
                let input: InspectArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::Inspect(types::InspectCommand {
                        selector: input.selector,
                        target: input.target,
                        include_html: input.include_html.unwrap_or(false),
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "screenshot" => {
                let input: ScreenshotArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::CaptureScreenshot(types::CaptureScreenshotCommand {
                        mode: input.mode.unwrap_or(types::ScreenshotMode::Viewport),
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "wait_for" => {
                let input: WaitForArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::WaitFor(types::WaitForCommand {
                        condition: input.condition,
                        timeout_ms: input.timeout_ms,
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "intent_locate" => {
                let input: IntentLocateArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::Locate(types::LocateIntent {
                    purpose: input.purpose,
                    hints: input.hints.unwrap_or_default(),
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "intent_fill" => {
                let input: IntentFillArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::Fill(types::FillIntent {
                    purpose: input.purpose,
                    hints: input.hints.unwrap_or_default(),
                    value: input.value,
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "intent_complete_form" => {
                let input: IntentCompleteFormArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::CompleteForm(types::CompleteFormIntent {
                    purpose: input.purpose,
                    fields: input.fields,
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "intent_submit_and_verify" => {
                let input: IntentSubmitAndVerifyArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::SubmitAndVerify(types::SubmitAndVerifyIntent {
                    purpose: input.purpose,
                    hints: input.hints.unwrap_or_default(),
                    expected_state: input.expected_state,
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "intent_wait_for_state" => {
                let input: IntentWaitForStateArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::WaitForState(types::WaitForStateIntent {
                    condition: input.condition,
                    timeout_ms: input.timeout_ms,
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "intent_follow" => {
                let input: IntentFollowArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::Follow(types::FollowIntent {
                    purpose: input.purpose,
                    hints: input.hints.unwrap_or_default(),
                    expected_destination: input.expected_destination,
                    boundary: input.boundary.unwrap_or(false),
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "intent_dismiss_obstruction" => {
                let input: IntentDismissObstructionArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent =
                    types::IntentCommand::DismissObstruction(types::DismissObstructionIntent {
                        purpose: input.purpose,
                        hints: input.hints.unwrap_or_default(),
                        timeout_ms: input
                            .timeout_ms
                            .unwrap_or(types::DEFAULT_DISMISS_OBSTRUCTION_TIMEOUT_MS),
                    });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "intent_extract" => {
                let input: IntentExtractArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let intent = types::IntentCommand::Extract(types::ExtractIntent {
                    purpose: input.purpose,
                    fields: input.fields,
                });
                match apply_idempotency_key(&mut context, input.idempotency_key) {
                    Ok(()) => {}
                    Err(()) => return invalid_params_reason(id, "invalidIdempotencyKey"),
                }
                let (context, mut envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
                pin_envelope_ids(&mut envelope, input.command_id, input.attempt_id);
                self.submit_envelope(context, envelope).await
            }
            "page_list" => {
                let input: PageListArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    None,
                    input.workflow_id,
                    types::PrimitiveCommand::ListPages(types::ListPagesCommand),
                );
                self.submit_envelope(context, envelope).await
            }
            "page_close" => {
                let input: FormSnapshotArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let page_id = input.page_id;
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(page_id.clone()),
                    input.workflow_id,
                    types::PrimitiveCommand::ClosePage(types::ClosePageCommand { page_id }),
                );
                self.submit_envelope(context, envelope).await
            }
            "page_activate" => {
                let input: PageCloseArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let page_id = input.page_id;
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(page_id.clone()),
                    input.workflow_id,
                    types::PrimitiveCommand::ActivatePage(types::ActivatePageCommand { page_id }),
                );
                self.submit_envelope(context, envelope).await
            }
            "a11y_snapshot" => {
                let input: A11ySnapshotArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::AccessibilitySnapshot(
                        types::AccessibilitySnapshotCommand {
                            max_nodes: input.max_nodes,
                        },
                    ),
                );
                self.submit_envelope(context, envelope).await
            }
            "context_ask" => {
                let input: ContextAskArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                self.runtime
                    .context_ask(context, input.session_id, input.page_id, input.description)
                    .await
                    // `None` is an answer, not a failure: the context does not
                    // know and the repair is to snapshot. An error here would
                    // be indistinguishable from a broken call.
                    .and_then(|answer| to_json(json!({"answer": answer})))
            }
            "context_neighbors" => {
                let input: ContextNeighborsArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                self.runtime
                    .context_neighbors(context, input.session_id, input.page_id, input.description)
                    .await
                    // Like context_ask: `None` is an answer, not a failure.
                    .and_then(|neighbors| to_json(json!({"neighbors": neighbors})))
            }
            "form_snapshot" => {
                let input: FormSnapshotArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                if input
                    .max_controls
                    .is_some_and(|limit| !(1..=512).contains(&limit))
                {
                    return invalid_params_reason(id, "malformedArguments");
                }
                self.runtime
                    .form_snapshot(context, input.session_id, input.page_id, input.max_controls)
                    .await
                    .and_then(to_json)
            }
            "control_action" => {
                let input: ControlActionArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                if input.action.validate().is_err() {
                    return invalid_params_reason(id, "malformedArguments");
                }
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::ControlAction(types::ControlActionCommand {
                        target: input.target,
                        action: input.action,
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "network_log" => {
                let input: NetworkLogArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    None,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::NetworkLog(
                        types::NetworkLogCommand {
                            clear: input.clear.unwrap_or(true),
                        },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "emulate" => {
                let input: EmulateArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    None,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::Emulate(
                        types::EmulateCommand {
                            viewport: input.viewport,
                            geolocation: input.geolocation,
                            mobile: input.mobile,
                        },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "dialog" => {
                let input: DialogArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    None,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::HandleDialog(
                        types::HandleDialogCommand {
                            action: input.action,
                            timeout_ms: input.timeout_ms,
                        },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "pdf" => {
                let input: PdfArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    None,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::PrintToPdf(
                        types::PrintToPdfCommand {
                            landscape: input.landscape,
                            print_background: input.print_background.unwrap_or(true),
                            scale: input.scale,
                            page_ranges: input.page_ranges,
                        },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "cookie_get" => {
                let input: CookieGetArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    None,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::GetCookies(
                        types::GetCookiesCommand { urls: input.urls },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "cookie_set" => {
                let input: CookieSetArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    None,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::SetCookies(
                        types::SetCookiesCommand {
                            cookies: input.cookies,
                        },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "cookie_delete" => {
                let input: CookieDeleteArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = command_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    None,
                    types::RuntimeCommand::Primitive(types::PrimitiveCommand::DeleteCookies(
                        types::DeleteCookiesCommand {
                            urls: input.urls,
                            names: input.names,
                        },
                    )),
                );
                self.submit_envelope(context, envelope).await
            }
            "extract_structured" => {
                let input: ExtractStructuredArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::ExtractStructured(types::ExtractStructuredCommand {
                        schema: input.schema,
                        purpose: input.purpose,
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "download_url" => {
                let input: DownloadUrlArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::DownloadUrl(types::DownloadUrlCommand {
                        url: input.url,
                        expected_content_type: input.expected_content_type,
                        max_bytes: input.max_bytes,
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "upload_files" => {
                let input: UploadFilesArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::UploadFiles(types::UploadFilesCommand {
                        selector: input.selector.unwrap_or_default(),
                        target: input.target,
                        paths: input.paths,
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "evaluate_javascript" => {
                let input: EvaluateJavaScriptArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                let (context, envelope) = primitive_envelope(
                    context,
                    input.session_id,
                    Some(input.page_id),
                    input.workflow_id,
                    types::PrimitiveCommand::EvaluateJavaScript(types::EvaluateJavaScriptCommand {
                        expression: input.expression,
                        timeout_ms: input.timeout_ms.unwrap_or(DEFAULT_COMMAND_TIMEOUT_MS),
                        await_promise: input.await_promise.unwrap_or(false),
                    }),
                );
                self.submit_envelope(context, envelope).await
            }
            "checkpoint_save" => {
                let input: CheckpointSaveArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                // The caller names commands, not evidence: each id is resolved
                // against the runtime's journal before the checkpoint is
                // persisted. An id with no journal record, no terminal outcome,
                // or a different owner is rejected rather than silently
                // contributing nothing.
                match self
                    .runtime
                    .resolve_command_evidence(context.clone(), input.evidence_refs)
                    .await
                {
                    Ok(evidence) => self
                        .runtime
                        .checkpoint(context, input.checkpoint, evidence)
                        .await
                        .and_then(to_json),
                    Err(interface_error) => Err(interface_error),
                }
            }
            "workflow_recover" => {
                let input: WorkflowRecoverArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                self.runtime
                    .recover(context, input.workflow_id)
                    .await
                    .and_then(to_json)
            }
            "recovery_status" => {
                let input: WorkflowRecoverArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                self.runtime
                    .recovery_status(context, input.workflow_id)
                    .await
                    .and_then(to_json)
            }
            "events_read" => {
                let input: EventsReadArgs = match bounded_parse(call.arguments) {
                    Ok(input) => input,
                    Err(()) => return invalid_params_reason(id, "malformedArguments"),
                };
                if input.limit == 0 || input.limit > MAX_EVENT_LIMIT {
                    return invalid_params_reason(id, "malformedArguments");
                }
                if let Err(interface_error) = self
                    .authorization
                    .authorize(&context, types::InterfaceOperation::SubscribeEvents)
                {
                    return interface_error_response(id, interface_error);
                }
                let remaining = match (context.deadline - Utc::now()).to_std() {
                    Ok(remaining) => remaining,
                    Err(_) => return invalid_params_reason(id, "malformedArguments"),
                };
                match tokio::time::timeout(
                    remaining,
                    self.events.read_after_for(
                        &context.principal_id,
                        input.cursor.into(),
                        input.limit,
                    ),
                )
                .await
                {
                    Ok(Ok(batch)) => to_json(batch),
                    Ok(Err(gap)) => {
                        return error(
                            id,
                            INTERFACE_ERROR,
                            "Runtime interface error",
                            Some(json!({"eventGap": gap})),
                        )
                    }
                    Err(_) => {
                        return error(
                            id,
                            INTERFACE_ERROR,
                            "Runtime interface error",
                            Some(json!({
                                "interfaceError": {
                                    "code":"deadlineExceeded",
                                    "layer":"interface",
                                    "message":"runtime interface request failed",
                                    "correlationId":context.correlation_id,
                                    "commandId":null,
                                    "retryable":false,
                                    "retryAfterMs":null,
                                    "reconciliationRequired":false,
                                    "requiredCapability":null
                                }
                            })),
                        )
                    }
                }
            }
            _ => unreachable!("availability checked above"),
        };
        match result {
            Ok(value) => self.tool_success(id, value).await,
            Err(interface_error) => interface_error_response(id, interface_error),
        }
    }

    async fn tool_success(&self, id: Value, mut value: Value) -> Value {
        crate::resources::redact_mcp_download_paths(&mut value);
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
                        if let Some(object) = value.as_object_mut() {
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
#[serde(deny_unknown_fields)]
struct ToolCall {
    name: String,
    #[serde(default = "empty_arguments")]
    arguments: Value,
    #[serde(default, rename = "_meta")]
    _meta: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EmptyArgs {}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionCreateArgs {
    profile: String,
    #[serde(default)]
    proxy: Option<String>,
    #[serde(default)]
    execution_policy: types::ExecutionPolicy,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PageOpenArgs {
    session_id: types::SessionId,
    #[serde(default)]
    url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CommandExecuteArgs {
    envelope: types::CommandEnvelope,
    #[serde(default)]
    idempotency_key: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CheckpointSaveArgs {
    checkpoint: types::WorkflowCheckpoint,
    #[serde(default)]
    evidence_refs: Vec<types::CommandId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ContextAskArgs {
    session_id: types::SessionId,
    page_id: types::PageId,
    description: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ContextNeighborsArgs {
    session_id: types::SessionId,
    page_id: types::PageId,
    description: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ToolsetSelectArgs {
    toolset: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WorkflowRecoverArgs {
    workflow_id: types::WorkflowId,
}

macro_rules! page_scoped_args {
    ($name:ident { $($field:ident : $ty:ty),* $(,)? }) => {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct $name {
            session_id: types::SessionId,
            page_id: types::PageId,
            #[serde(default)]
            workflow_id: Option<types::WorkflowId>,
            $($field : $ty,)*
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
        struct $name {
            session_id: types::SessionId,
            page_id: types::PageId,
            #[serde(default)]
            workflow_id: Option<types::WorkflowId>,
            #[serde(default)]
            command_id: Option<types::CommandId>,
            #[serde(default)]
            attempt_id: Option<types::AttemptId>,
            #[serde(default)]
            idempotency_key: Option<String>,
            $($field : $ty,)*
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
/// pre-action checkpoint gate (see `pin_envelope_ids`).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ClickArgs {
    session_id: types::SessionId,
    page_id: types::PageId,
    #[serde(default)]
    workflow_id: Option<types::WorkflowId>,
    #[serde(default)]
    command_id: Option<types::CommandId>,
    #[serde(default)]
    attempt_id: Option<types::AttemptId>,
    selector: Option<String>,
    target: Option<types::TargetSpec>,
    boundary: Option<bool>,
    expected_url: Option<String>,
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
struct PageListArgs {
    session_id: types::SessionId,
    #[serde(default)]
    workflow_id: Option<types::WorkflowId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct SessionCloseArgs {
    session_id: types::SessionId,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PageCloseArgs {
    session_id: types::SessionId,
    page_id: types::PageId,
    #[serde(default)]
    workflow_id: Option<types::WorkflowId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FormSnapshotArgs {
    session_id: types::SessionId,
    page_id: types::PageId,
    #[serde(default)]
    max_controls: Option<u32>,
    #[serde(default)]
    workflow_id: Option<types::WorkflowId>,
}

page_scoped_args!(ControlActionArgs {
    target: types::FormControlTarget,
    action: types::ControlAction,
});

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct A11ySnapshotArgs {
    session_id: types::SessionId,
    page_id: types::PageId,
    #[serde(default)]
    max_nodes: Option<u32>,
    #[serde(default)]
    workflow_id: Option<types::WorkflowId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct NetworkLogArgs {
    session_id: types::SessionId,
    page_id: types::PageId,
    #[serde(default)]
    clear: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EmulateArgs {
    session_id: types::SessionId,
    page_id: types::PageId,
    #[serde(default)]
    viewport: Option<types::ViewportSize>,
    #[serde(default)]
    geolocation: Option<types::GeolocationCoordinates>,
    #[serde(default)]
    mobile: Option<bool>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DialogArgs {
    session_id: types::SessionId,
    page_id: types::PageId,
    action: types::DialogAction,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PdfArgs {
    session_id: types::SessionId,
    page_id: types::PageId,
    #[serde(default)]
    landscape: bool,
    #[serde(default)]
    print_background: Option<bool>,
    #[serde(default)]
    scale: Option<f64>,
    #[serde(default)]
    page_ranges: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CookieGetArgs {
    session_id: types::SessionId,
    page_id: types::PageId,
    #[serde(default)]
    urls: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CookieSetArgs {
    session_id: types::SessionId,
    page_id: types::PageId,
    cookies: Vec<types::SetCookieParam>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CookieDeleteArgs {
    session_id: types::SessionId,
    page_id: types::PageId,
    #[serde(default)]
    urls: Vec<String>,
    #[serde(default)]
    names: Vec<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExtractStructuredArgs {
    session_id: types::SessionId,
    page_id: types::PageId,
    #[serde(default)]
    workflow_id: Option<types::WorkflowId>,
    schema: serde_json::Value,
    #[serde(default)]
    purpose: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DownloadUrlArgs {
    session_id: types::SessionId,
    page_id: types::PageId,
    url: String,
    #[serde(default)]
    expected_content_type: Option<String>,
    max_bytes: u64,
    #[serde(default)]
    workflow_id: Option<types::WorkflowId>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EventsReadArgs {
    #[serde(default)]
    cursor: u64,
    limit: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ResourceReadArgs {
    uri: String,
    #[serde(default, rename = "_meta")]
    _meta: Option<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PromptGetArgs {
    name: String,
    #[serde(default = "empty_arguments")]
    arguments: Value,
    #[serde(default, rename = "_meta")]
    _meta: Option<Value>,
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

fn empty_arguments() -> Value {
    json!({})
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
    serde_json::to_value(value).map_err(|_| types::InterfaceError {
        code: types::InterfaceErrorCode::Internal,
        layer: types::ErrorLayer::Interface,
        message: "runtime interface request failed".to_owned(),
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
        json!({
            "reason":"schemaViolation",
            "pointer":violation.pointer,
            "constraint":violation.constraint
        })
    });
    error(id, INVALID_PARAMS, "Invalid params", data)
}

/// `-32602` for a rejection with no single offending field: a body that passed
/// the schema but failed to deserialize, an expired deadline, a malformed
/// idempotency key.
fn invalid_params_reason(id: Value, reason: &'static str) -> Value {
    error(
        id,
        INVALID_PARAMS,
        "Invalid params",
        Some(json!({"reason":reason})),
    )
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
    interface_error.message = "runtime interface request failed".to_owned();
    let mut response = error(
        id,
        INTERFACE_ERROR,
        "Runtime interface error",
        Some(json!({"interfaceError":interface_error})),
    );
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

fn required_capabilities(name: &str) -> Option<&'static [types::Capability]> {
    match name {
        "checkpoint_save" | "workflow_recover" => Some(&[types::Capability::RecoveryWrite]),
        "recovery_status" => Some(&[types::Capability::RecoveryRead]),
        "command_execute" | "control_action" | "navigate" | "click" | "type_text" | "inspect"
        | "screenshot" | "wait_for" | "page_list" | "page_close" | "page_activate"
        | "a11y_snapshot" | "pdf" | "dialog" | "emulate" | "network_log" | "cookie_get"
        | "cookie_set" | "cookie_delete" => Some(&[types::Capability::BrowserMutate]),
        "extract_structured" => Some(&[
            types::Capability::BrowserMutate,
            types::Capability::VisionAssist,
        ]),
        "download_url" => Some(&[
            types::Capability::BrowserMutate,
            types::Capability::FileDownload,
        ]),
        "upload_files" => Some(&[
            types::Capability::BrowserMutate,
            types::Capability::FileUpload,
        ]),
        "evaluate_javascript" => Some(&[
            types::Capability::BrowserMutate,
            types::Capability::JavascriptEvaluate,
        ]),
        "intent_complete_form"
        | "intent_dismiss_obstruction"
        | "intent_extract"
        | "intent_fill"
        | "intent_follow"
        | "intent_locate"
        | "intent_submit_and_verify"
        | "intent_wait_for_state" => Some(&[
            types::Capability::BrowserMutate,
            types::Capability::IntentExecute,
        ]),
        "events_read" | "runtime_info" | "session_list" => Some(&[types::Capability::SessionRead]),
        "context_ask" => Some(&[types::Capability::PageRead]),
        "context_neighbors" => Some(&[types::Capability::ContextRead]),
        // Grants nothing, so it needs nothing beyond an authenticated
        // connection. A capability gate here could strand a principal in a
        // phase it lacked the capability to leave.
        "toolset_select" => Some(&[]),
        "form_snapshot" => Some(&[types::Capability::PageRead]),
        "page_open" => Some(&[types::Capability::PageWrite]),
        "session_close" | "session_create" => Some(&[types::Capability::SessionWrite]),
        _ => None,
    }
}

fn required_operation(name: &str) -> Option<types::InterfaceOperation> {
    match name {
        "checkpoint_save" => Some(types::InterfaceOperation::CreateCheckpoint),
        "recovery_status" => Some(types::InterfaceOperation::ReadCheckpoint),
        "command_execute"
        | "control_action"
        | "navigate"
        | "click"
        | "type_text"
        | "inspect"
        | "screenshot"
        | "wait_for"
        | "page_list"
        | "page_close"
        | "page_activate"
        | "a11y_snapshot"
        | "extract_structured"
        | "pdf"
        | "dialog"
        | "emulate"
        | "network_log"
        | "cookie_get"
        | "cookie_set"
        | "cookie_delete"
        | "download_url"
        | "upload_files"
        | "evaluate_javascript"
        | "intent_complete_form"
        | "intent_dismiss_obstruction"
        | "intent_extract"
        | "intent_fill"
        | "intent_follow"
        | "intent_locate"
        | "intent_submit_and_verify"
        | "intent_wait_for_state" => Some(types::InterfaceOperation::SubmitCommand),
        "events_read" => Some(types::InterfaceOperation::SubscribeEvents),
        "context_ask" => Some(types::InterfaceOperation::ReadPage),
        "context_neighbors" => Some(types::InterfaceOperation::ReadContext),
        "form_snapshot" => Some(types::InterfaceOperation::ReadPage),
        "page_open" => Some(types::InterfaceOperation::OpenPage),
        "runtime_info" => Some(types::InterfaceOperation::RuntimeInfo),
        "session_close" => Some(types::InterfaceOperation::DeleteSession),
        "session_create" => Some(types::InterfaceOperation::CreateSession),
        "session_list" => Some(types::InterfaceOperation::ReadSession),
        "workflow_recover" => Some(types::InterfaceOperation::RecoverWorkflow),
        _ => None,
    }
}

fn tool_description(name: &str) -> &'static str {
    match name {
        "context_ask" => "Ask the retained page context where a described control is, instead of pulling a whole accessibility tree into your context. Requires page:read. Returns a bound target and a confidence score, or nothing. On no answer, take an a11y_snapshot -- the context is invalidated by every command that may have changed the page.",
        "context_neighbors" => "Show the remembered form structure around a described control: its form, sibling controls, and per-intent success counters, marked as remembered rather than live-observed. Requires context:read. Returns nothing for an unknown site or control.",
        "toolset_select" => "Narrow tools/list to one phase: explore, act, intent, verify, or full. Requires no capability. Emits notifications/tools/list_changed, so re-read tools/list after calling it. Hidden tools stay callable; this changes what is advertised, not what is permitted.",
        "runtime_info" => "Runtime version, granted capabilities, active session count, uptime, and credential expiry. Requires session:read.",
        "session_list" => "List browser sessions visible to this principal, each with its profile and open-page count. Requires session:read.",
        "page_list" => "List open pages in an owned session, each with its id, URL, and title. Requires browser:mutate.",
        "inspect" => "Read a page's text, optionally scoped to one element by selector or target, with HTML on request. Requires browser:mutate.",
        "a11y_snapshot" => "Capture a compact accessibility tree for a page, capped at 2048 nodes, with command-ready targets on actionable nodes. Requires browser:mutate. Start here: pass a node's target into an intent_* tool rather than guessing a selector.",
        "form_snapshot" => "Read a bounded, engine-neutral inventory of a page's form controls without exposing selectors or sensitive values. Requires page:read.",
        "screenshot" => "Capture a screenshot artifact of a page's viewport, full page, or one element. Requires browser:mutate.",
        "events_read" => "Read retained runtime events for this principal after a cursor, bounded by a limit. Requires session:read. Long-polls: it blocks until an event past the cursor arrives or the request deadline expires (about 60s), so it is not a quick read. The notifications/bobby/event channel pushes the same frames without polling -- see bobby://failure-taxonomy.",
        "recovery_status" => "Read a workflow's checkpoint and recovery receipts without attempting recovery. Requires recovery:read.",
        "cookie_get" => "Read cookies visible to a page, optionally filtered by URL. Requires browser:mutate.",
        "checkpoint_save" => "Persist a verified workflow checkpoint. Requires recovery:write. Pass evidenceRefs -- command ids whose evidence the runtime resolves from its journal. For a Boundary command, save BEFORE it with recoveryClass boundary and boundaryCommandId/attemptId equal to the ids you pass that command. On failure with a missing command id, confirm the command completed first.",
        "workflow_recover" => "Recover a workflow from its last verified checkpoint, resuming, restarting, or flagging reconciliation. Requires recovery:write. Produces recovery evidence and the decision reached. On failure with notFound, this principal doesn't own the checkpoint's session (it may be closed) -- verify with session_list. A missing checkpoint itself surfaces as an opaque internal error, not notFound.",
        "session_create" => "Create a browser session with a profile, optional proxy, and execution policy. Requires session:write. Produces the session's id and initial state. On failure with resourceExhausted, this principal already holds its session limit -- close an idle one first.",
        "session_close" => "Close a session and release its pages, workers, and artifacts. Requires session:write. Destructive: in-flight commands on the session are cancelled. On failure, the session may already be closed -- confirm with session_list.",
        "page_open" => "Open a page in an owned session, optionally navigating it to a URL in the same call. Requires page:write, and browser:mutate too if a URL is given. Produces the page's id and, if navigated, navigation evidence. On failure with notFound, the session is not owned by this principal -- check session_list.",
        "page_close" => "Close a page in an owned session. Requires browser:mutate. Destructive: the page and its in-flight commands are gone immediately. On failure with notFound, the page id is stale -- call page_list for current ids.",
        "page_activate" => "Bring a page to the front in an owned session. Requires browser:mutate. Produces the activated page's URL and title. On failure with notFound, the page id is stale -- call page_list for current ids.",
        "navigate" => "Navigate a page to a URL and wait for the requested load state. Requires browser:mutate. Produces navigation evidence with the settled URL and title. On failure with invalidRequest, the URL scheme isn't http(s) or data -- use one of those; on deadlineExceeded, retry with a longer timeout_ms.",
        "click" => "Click an element identified by a selector or a resolved target. Requires browser:mutate. Produces execution-path evidence for the click. On failure with targetNotFound or targetAmbiguous, take a fresh a11y_snapshot and pass the new target.",
        "type_text" => "Type text into an element identified by a selector or a resolved target, optionally clearing it first. Requires browser:mutate. Produces execution-path evidence for the input. On failure with targetNotFound or targetAmbiguous, take a fresh a11y_snapshot and pass the new target.",
        "wait_for" => "Wait for a page condition with a bounded timeout. Requires browser:mutate. Produces wait evidence with elapsed time and observation count. On failure with waitConditionTimedOut, confirm the condition still matches page state via inspect, then retry with a longer timeout.",
        "control_action" => "Perform one typed native form-control action and return the reread control state. Requires browser:mutate, and file:upload too if the action is setFiles. Produces control-action evidence with the post-action value. On failure with targetNotFound, take a fresh form_snapshot and pass the new target.",
        "emulate" => "Set viewport size, mobile mode, and geolocation overrides for a page. Requires browser:mutate. Produces emulation evidence confirming the applied overrides. On failure with invalidRequest, viewport or coordinates are out of range -- keep width/height within 1-16384 and coordinates within valid bounds.",
        "dialog" => "Accept or dismiss the next JavaScript dialog on a page within a timeout. Requires browser:mutate. Produces dialog evidence with the dialog's message and the action taken. On failure with deadlineExceeded, no dialog opened in time -- confirm the triggering action actually opens one.",
        "pdf" => "Print a page to a PDF artifact with optional layout and scale. Requires browser:mutate. Produces a PDF artifact with its size and checksum. On failure with invalidRequest, scale is out of range -- pass a value between 0.1 and 2.0.",
        "cookie_set" => "Store cookies on a page's jar. Requires browser:mutate. Produces the updated cookie-jar state. On failure with invalidRequest, more than 128 cookies were passed in one call -- split into batches of 128 or fewer.",
        "cookie_delete" => "Delete cookies from a page's jar by origin and optionally by name. Requires browser:mutate. Destructive: matching cookies are removed immediately. On failure with notFound, the page id is stale -- call page_list for current ids.",
        "extract_structured" => "Extract schema-shaped JSON from a page via the configured vision provider. Requires browser:mutate and vision:assist. Produces structured-extraction evidence with the schema-shaped value. On failure with visionAssistDenied, the session's vision policy or provider isn't enabled -- read the page with inspect or a11y_snapshot instead.",
        "download_url" => "Download a URL into the session's downloads, bounded by a byte limit. Requires browser:mutate and file:download. Produces a download artifact with its size and checksum. On failure with networkPolicyDenied, use an http(s) URL without embedded credentials and a max_bytes within the configured range.",
        "upload_files" => "Set files on a file input from the runtime's configured upload roots. Requires browser:mutate and file:upload. Produces upload evidence naming the selector and resolved paths. On failure with policyDenied, the path is outside the configured upload roots -- pass a path under an allowed root.",
        "evaluate_javascript" => "Evaluate a JavaScript expression on a page, optionally awaiting its promise. Requires browser:mutate and javascript:evaluate. Produces the returned value, or notes truncation. On failure with policyDenied, the session's execution policy forbids evaluation -- use a11y_snapshot and the intent_* tools instead.",
        "command_execute" => "Execute one bounded browser command envelope naming its own capability and evidence. Requires browser:mutate, plus whatever the wrapped command needs. Produces the same evidence as the named command it wraps. On failure with deadlineOutOfRange, set the envelope's deadline within the allowed window and resubmit.",
        "intent_locate" => "Locate an element by described purpose and hints, without acting on it (Replayable). Requires browser:mutate and intent:execute. Produces resolution evidence with the matched target's fingerprint. On failure with targetNotFound or targetAmbiguous, narrow the purpose or hints and retry.",
        "intent_fill" => "Fill one described form control and verify the value (Reconciliable). Requires browser:mutate and intent:execute. Produces fill evidence carrying the browser's own validity state. On failure with verificationFailed, read the retained validation message and re-fill; on targetNotFound, take a fresh a11y_snapshot and pass the new target.",
        "intent_complete_form" => "Fill an ordered list of named form fields as one intent, verifying each before the next; never submits (Reconciliable). Requires browser:mutate and intent:execute. Produces per-field resolution and fill evidence in order. On failure with verificationFailed, targetNotFound, or intentActionMismatch on one field, the fields before it are already filled -- re-run with only the remaining fields.",
        "intent_submit_and_verify" => "Submit a form and verify the expected resulting state (Boundary; refused without a matching pre-saved checkpoint). Requires browser:mutate and intent:execute. Pin commandId/attemptId, checkpoint_save with those exact ids first, then call with the same ones. On failure with needsReconciliation, do not retry -- the submit may have already landed; call recovery_status and reconcile before continuing.",
        "intent_wait_for_state" => "Wait for a described page state to hold (Replayable). Requires browser:mutate and intent:execute. Produces wait evidence with elapsed time and observation count. On failure with waitConditionTimedOut, confirm the condition still matches page state via inspect, then retry with a longer timeout.",
        "intent_follow" => "Activate a described link or control and verify the destination (Boundary when boundary is true, else Reconciliable). Requires browser:mutate and intent:execute. Produces resolution and destination evidence. On failure with needsReconciliation, do not retry -- call recovery_status first; on targetNotFound, take a fresh a11y_snapshot.",
        "intent_dismiss_obstruction" => "Dismiss a popup, overlay, or cookie banner blocking the page (Reconciliable). Requires browser:mutate and intent:execute. Produces resolution and dismissal evidence. On failure with obstructionSuspected, the obstruction is still present after the attempt -- take a fresh a11y_snapshot to find another dismissal control.",
        "intent_extract" => "Read named fields off the page without mutating it (Replayable). Requires browser:mutate and intent:execute. Produces one extraction result per named field, with a resolution path and error code for any that failed. On failure with notFound, the session or page id is stale -- call page_list; a single unresolved field is reported per field, not as a call failure.",
        "network_log" => "Dump the page's recorded network log as a HAR artifact, then clear the buffer unless clear is false. Requires browser:mutate. Produces HAR-artifact evidence with entry count, byte size, and checksum. On failure: verificationFailed (no HAR captured), browserCommandFailed (engine could not persist it), or internal (write failed) -- none caller-fixable; retry, and report if it persists.",
        _ => "Runtime operation.",
    }
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
        message: "runtime interface request failed".to_owned(),
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
