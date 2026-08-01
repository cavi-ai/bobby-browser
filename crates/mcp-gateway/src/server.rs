use std::{
    collections::{BTreeMap, BTreeSet},
    future::Future,
    io,
    pin::Pin,
    sync::{
        atomic::{AtomicBool, Ordering},
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

use crate::protocol::{
    error, success, INTERFACE_ERROR, INTERNAL_ERROR, INVALID_PARAMS, INVALID_REQUEST,
    MAX_EVENT_LIMIT, MAX_FRAME_BYTES, MAX_INPUT_BYTES, MAX_REQUEST_ID_BYTES, MCP_PROTOCOL_VERSION,
    METHOD_NOT_FOUND, NOT_INITIALIZED, PARSE_ERROR, REQUEST_CANCELLED,
};
use crate::schema::{tool_schema, validate_tool_arguments};
use crate::ArtifactResources;

const MAX_RESOURCE_ENCODED_BYTES: usize = 768 * 1024;
const MAX_PENDING_CANCELLATIONS: usize = 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Lifecycle {
    AwaitingInitialize,
    AwaitingInitializedNotification,
    Ready,
}

pub struct Server {
    runtime: Arc<dyn RuntimeInterface>,
    handle: CapabilityHandle,
    authorization: AuthorizationGuard,
    events: EventStore,
    resources: ArtifactResources,
    lifecycle: Mutex<Lifecycle>,
    in_flight: Mutex<BTreeMap<String, Arc<Notify>>>,
    pending_cancellations: Mutex<BTreeSet<String>>,
    shutting_down: AtomicBool,
}

impl Server {
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
        Self {
            runtime,
            handle: handle.clone(),
            authorization: AuthorizationGuard::new(handle),
            events,
            resources,
            lifecycle: Mutex::new(Lifecycle::AwaitingInitialize),
            in_flight: Mutex::new(BTreeMap::new()),
            pending_cancellations: Mutex::new(BTreeSet::new()),
            shutting_down: AtomicBool::new(false),
        }
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
            // A re-`initialize` from any prior lifecycle state is a session
            // reset, not a protocol error: MCP clients over streamable HTTP
            // call `initialize` on every reconnect, and rejecting it strands
            // a principal behind its own once-per-process handshake. Reset
            // clears stale cancellation state from the previous session;
            // in-flight work from that session keeps running to completion.
            self.pending_cancellations.lock().await.clear();
            *lifecycle = Lifecycle::AwaitingInitializedNotification;
            return id.map(|id| {
                success(
                    id,
                    json!({
                        "protocolVersion": MCP_PROTOCOL_VERSION,
                        "capabilities": {
                            "tools": {"listChanged": false},
                            "resources": {"subscribe": false, "listChanged": false}
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

    pub async fn serve<R, W>(&self, input: R, output: W) -> io::Result<()>
    where
        R: AsyncRead + Unpin,
        W: AsyncWrite + Unpin,
    {
        let mut input = BufReader::new(input);
        let output = Arc::new(Mutex::new(output));
        let mut pending: FuturesUnordered<Pin<Box<dyn Future<Output = io::Result<()>> + '_>>> =
            FuturesUnordered::new();
        let mut frame = Vec::new();
        loop {
            tokio::select! {
                status = read_bounded_frame(&mut input, &mut frame) => {
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
            "type_text",
            "upload_files",
            "wait_for",
            "workflow_recover",
        ] {
            let required = required_capabilities(name).expect("registered tool");
            if required
                .iter()
                .all(|capability| capabilities.contains(*capability))
            {
                tools.push(json!({
                    "name": name,
                    "description": tool_description(name),
                    "inputSchema": tool_schema(name)
                }));
            }
        }
        success(id, json!({"tools":tools}))
    }

    async fn list_resources(&self, id: Value, params: Value) -> Value {
        let context = self.request_context();
        if let Err(interface_error) = self
            .authorization
            .authorize(&context, types::InterfaceOperation::ReadArtifact)
        {
            return interface_error_response(id, interface_error);
        }
        if !valid_initial_list_params(&params) {
            return error(id, INVALID_PARAMS, "Invalid params", None);
        }
        let resources = self
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
            .collect::<Vec<_>>();
        success(id, json!({"resources":resources}))
    }

    async fn read_resource(&self, id: Value, params: Value) -> Value {
        let context = self.request_context();
        if let Err(interface_error) = self
            .authorization
            .authorize(&context, types::InterfaceOperation::ReadArtifact)
        {
            return interface_error_response(id, interface_error);
        }
        let input: ResourceReadArgs = match bounded_parse(params) {
            Ok(input) => input,
            Err(()) => return error(id, INVALID_PARAMS, "Invalid params", None),
        };
        let Some(artifact_id) = parse_artifact_uri(&input.uri) else {
            return error(id, INVALID_PARAMS, "Invalid params", None);
        };
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
                // Principal-scoped, so it belongs here rather than on the
                // runtime-wide `RuntimeInfo` the inner runtime produces. Without
                // it a caller cannot see the credential lapse coming — the stdio
                // gateway simply refuses to start afterwards.
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
                self.runtime.list_sessions(context).await.and_then(to_json)
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
                let (context, envelope) = primitive_envelope(
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
                let (context, envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
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
                let (context, envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
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
                let (context, envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
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
                let (context, envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
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
                let (context, envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
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
                let (context, envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
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
                let (context, envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
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
                let (context, envelope) = intent_envelope(
                    context,
                    input.session_id,
                    input.page_id,
                    input.workflow_id,
                    intent,
                );
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
                    None,
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
                self.runtime
                    .checkpoint(context, input.checkpoint, input.evidence)
                    .await
                    .and_then(to_json)
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
        success(
            id,
            json!({
                "content":content,
                "structuredContent":value,
                "isError":false
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
                        // `CommandOutcome` carries only `commandId`, so without
                        // this an agent cannot name the workflow it just ran in
                        // and `checkpoint_save` / `workflow_recover` stay out of
                        // reach. Pass it back into any tool's `workflowId` to
                        // keep subsequent commands in the same workflow.
                        if let Some(object) = value.as_object_mut() {
                            object.insert(
                                "workflowId".to_owned(),
                                json!(envelope.workflow_id.clone()),
                            );
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
    evidence: Vec<types::Evidence>,
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
///
/// Intents were reachable only by hand-building a `CommandEnvelope` for
/// `command_execute`, which meant minting three UUIDs and an RFC3339 deadline
/// per call. These mirror the flat primitive tools instead: the server builds
/// the envelope.
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

page_scoped_args!(ClickArgs {
    selector: Option<String>,
    target: Option<types::TargetSpec>,
    boundary: Option<bool>,
    expected_url: Option<String>,
});

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
/// Every rejection used to be an indistinguishable `"Invalid params"` with no
/// `data`, so a caller could not tell a missing field from an out-of-range one,
/// let alone find which field. `pointer` and `constraint` describe the schema,
/// never the submitted value, so they disclose nothing `tools/list` does not.
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
    interface_error.message = "runtime interface request failed".to_owned();
    error(
        id,
        INTERFACE_ERROR,
        "Runtime interface error",
        Some(json!({"interfaceError":interface_error})),
    )
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
/// `workflow_id` is the caller's when supplied. Every call used to mint a fresh
/// one and the outcome never echoed it back, so an agent on the flat tools
/// could not name the workflow it had just run in — which made
/// `checkpoint_save` and `workflow_recover` reachable only by hand-building
/// envelopes through `command_execute`.
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
        "checkpoint_save" => "Persist a verified workflow checkpoint.",
        "cookie_delete" => "Delete cookies by origin and name.",
        "cookie_get" => "Read cookies for a page or origins.",
        "cookie_set" => "Store cookies on a page's jar.",
        "click" => "Click an element on a page.",
        "command_execute" => "Execute one bounded browser command envelope.",
        "control_action" => "Perform one typed native form-control action and return the reread control state.",
        "download_url" => "Download a URL into the session's downloads.",
        "evaluate_javascript" => "Evaluate JavaScript on a page (session policy gated).",
        "events_read" => "Read retained runtime events after a cursor.",
        "intent_complete_form" => "Fill an ordered list of named form fields as one intent, verifying each before the next; never submits (Reconciliable).",
        "intent_dismiss_obstruction" => "Dismiss a popup, overlay, or cookie banner (Reconciliable).",
        "intent_extract" => "Read named fields off the page without mutating it (Replayable).",
        "intent_fill" => "Fill one described form control and verify the value (Reconciliable).",
        "intent_follow" => "Activate a described link or control and verify the destination (Boundary when boundary is true, else Reconciliable).",
        "intent_locate" => "Locate an element by described purpose (Replayable).",
        "intent_submit_and_verify" => "Submit a form and verify the expected resulting state (Boundary; needs a matching checkpoint).",
        "intent_wait_for_state" => "Wait for a described page state (Replayable).",
        "inspect" => "Read page state, optionally element-scoped.",
        "navigate" => "Navigate a page to a URL.",
        "a11y_snapshot" => "Capture a compact accessibility tree of a page.",
        "extract_structured" => "Extract schema-shaped JSON from a page via the vision provider.",
        "form_snapshot" => "Read a bounded, engine-neutral form inventory without exposing selectors or sensitive values.",
        "page_activate" => "Bring a page to the front in an owned session.",
        "page_close" => "Close a page in an owned session.",
        "page_list" => "List pages in an owned session.",
        "page_open" => "Open a page in an owned session.",
        "dialog" => "Accept or dismiss the next JavaScript dialog on a page.",
        "emulate" => "Set viewport size and geolocation overrides for a page.",
        "network_log" => "Dump the page's recorded network log as a HAR artifact.",
        "pdf" => "Print a page to a PDF artifact.",
        "recovery_status" => "Read a workflow's checkpoint and recovery receipts.",
        "runtime_info" => "Read runtime capability and health information.",
        "screenshot" => "Capture a screenshot artifact of a page or element.",
        "session_close" => "Close a browser session and release its worker.",
        "session_create" => "Create a browser session.",
        "session_list" => "List browser sessions visible to the principal.",
        "type_text" => "Type text into an element.",
        "upload_files" => "Set files on a file input from upload roots.",
        "wait_for" => "Wait for a page condition with a bounded timeout.",
        "workflow_recover" => "Recover a workflow from its verified checkpoint.",
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
}
