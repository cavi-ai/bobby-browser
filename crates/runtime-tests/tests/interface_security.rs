use std::{convert::Infallible, future::IntoFuture, sync::Arc};

use artifact_store::ArtifactStore;
use axum::{
    body::{to_bytes, Body, Bytes},
    http::{Request, StatusCode},
};
use broker::{router, AppState, ArtifactCatalog};
use cdp_gateway::{
    parse_frame, CdpConnection, CdpErrorCode, CdpEvent, CdpGateway, CdpRequest, MethodRegistry,
    MAX_FRAME_BYTES as MAX_CDP_FRAME_BYTES, MAX_QUEUED_EVENTS,
};
use chrono::{Duration, SecondsFormat, Utc};
use config::InterfaceConfig;
use futures_util::stream;
use futures_util::{SinkExt, StreamExt};
use interface_conformance::live::{all_capabilities, ChromeRuntimeHarness};
use interface_core::{
    canonical_sha256, ArtifactOwnershipLimits, ArtifactReader, Authority, AuthorityStore, Event,
    EventGapReason, EventStore, IdempotencyReservation, IdempotencyStore, RuntimeInterface,
    SessionOwnershipAuthority, SessionOwnershipRecorder, SessionOwnershipRegistry,
};
use mcp_gateway::{
    protocol::{MAX_FRAME_BYTES as MAX_MCP_FRAME_BYTES, MCP_PROTOCOL_VERSION},
    ArtifactResources, Server as McpServer,
};
use sdk_core::AuthenticatedRuntime;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio_tungstenite::{
    connect_async,
    tungstenite::{client::IntoClientRequest, Message},
};
use tower::ServiceExt;
use types::{
    AttemptId, Capability, CaptureScreenshotCommand, CommandEnvelope, CommandId, CommandOutcome,
    CorrelationId, CreateSessionRequest, Evidence, IdempotencyKey, InspectCommand,
    InterfaceErrorCode, InterfaceOperation, InterfaceVersion, NavigateCommand, PageId,
    PrimitiveCommand, PrincipalId, ScreenshotMode, SessionId, WaitUntil, WorkflowId,
    CURRENT_INTERFACE_VERSION,
};

const SECRET: &str = "release-gate-secret-that-must-never-escape";
type SecurityResult = Result<(), String>;

macro_rules! require {
    ($condition:expr, $($message:tt)+) => {
        if !$condition { return Err(format!($($message)+)); }
    };
}

#[derive(Debug, Clone, Copy)]
enum SecurityCase {
    CanaryLeakage,
    HttpLifetime,
    McpLifetime,
    CdpLifetime,
    PrincipalIsolation,
    FilesystemConfinement,
    DuplicateHeaders,
    OversizedHttp,
    OversizedMcp,
    OversizedCdp,
    UnsupportedProtocols,
    IdempotencyMismatch,
    QueueOverflow,
    EventGap,
}

const SECURITY_MATRIX: [SecurityCase; 14] = [
    SecurityCase::CanaryLeakage,
    SecurityCase::HttpLifetime,
    SecurityCase::McpLifetime,
    SecurityCase::CdpLifetime,
    SecurityCase::PrincipalIsolation,
    SecurityCase::FilesystemConfinement,
    SecurityCase::DuplicateHeaders,
    SecurityCase::OversizedHttp,
    SecurityCase::OversizedMcp,
    SecurityCase::OversizedCdp,
    SecurityCase::UnsupportedProtocols,
    SecurityCase::IdempotencyMismatch,
    SecurityCase::QueueOverflow,
    SecurityCase::EventGap,
];

const REQUIRED_SECURITY_CASES: [&str; 14] = [
    "canary leakage surfaces",
    "mid-connection HTTP expiry and revocation",
    "mid-connection MCP expiry and revocation",
    "mid-connection CDP expiry and revocation",
    "cross-principal sessions and artifacts",
    "path traversal and symlink swap",
    "duplicate HTTP headers",
    "oversized HTTP input",
    "oversized MCP input",
    "oversized CDP input",
    "unsupported versions and methods",
    "idempotency mismatch",
    "queue overflow",
    "event gaps",
];

impl SecurityCase {
    const fn name(self) -> &'static str {
        match self {
            Self::CanaryLeakage => "canary leakage surfaces",
            Self::HttpLifetime => "mid-connection HTTP expiry and revocation",
            Self::McpLifetime => "mid-connection MCP expiry and revocation",
            Self::CdpLifetime => "mid-connection CDP expiry and revocation",
            Self::PrincipalIsolation => "cross-principal sessions and artifacts",
            Self::FilesystemConfinement => "path traversal and symlink swap",
            Self::DuplicateHeaders => "duplicate HTTP headers",
            Self::OversizedHttp => "oversized HTTP input",
            Self::OversizedMcp => "oversized MCP input",
            Self::OversizedCdp => "oversized CDP input",
            Self::UnsupportedProtocols => "unsupported versions and methods",
            Self::IdempotencyMismatch => "idempotency mismatch",
            Self::QueueOverflow => "queue overflow",
            Self::EventGap => "event gaps",
        }
    }

    async fn run(self, harness: &ChromeRuntimeHarness) -> SecurityResult {
        match self {
            Self::CanaryLeakage => canary_leakage(harness).await,
            Self::HttpLifetime => http_lifetime(harness).await,
            Self::McpLifetime => mcp_lifetime(harness).await,
            Self::CdpLifetime => cdp_lifetime(harness).await,
            Self::PrincipalIsolation => principal_isolation(harness).await,
            Self::FilesystemConfinement => filesystem_confinement().await,
            Self::DuplicateHeaders => duplicate_headers(harness).await,
            Self::OversizedHttp => oversized_http(harness).await,
            Self::OversizedMcp => oversized_mcp(harness).await,
            Self::OversizedCdp => oversized_cdp(),
            Self::UnsupportedProtocols => unsupported_protocols(harness).await,
            Self::IdempotencyMismatch => idempotency_mismatch().await,
            Self::QueueOverflow => queue_overflow(harness).await,
            Self::EventGap => event_gap(harness).await,
        }
    }
}

#[test]
fn release_security_aggregation_declares_every_required_case() {
    assert_eq!(
        SECURITY_MATRIX.map(SecurityCase::name),
        REQUIRED_SECURITY_CASES
    );
}

#[tokio::test]
#[ignore = "requires installed Chromium and loopback production fixtures"]
async fn real_security_release_matrix_executes_every_production_boundary() {
    let harness = ChromeRuntimeHarness::start().await;
    let mut executed = Vec::new();
    let mut failures = Vec::new();
    for case in SECURITY_MATRIX {
        executed.push(case.name());
        if let Err(error) = case.run(&harness).await {
            failures.push(format!("{}: {error}", case.name()));
        }
    }
    assert_eq!(executed, REQUIRED_SECURITY_CASES);
    assert!(failures.is_empty(), "{}", failures.join("\n"));
    println!("AUTOMATION_RUNTIME_SECURITY_PROOF:v1:interface-boundaries");
}

fn runtime_app(harness: &ChromeRuntimeHarness, interface: InterfaceConfig) -> axum::Router {
    let service = harness.service.clone();
    router(AppState::new(
        harness.authority.clone(),
        move |handle| {
            Arc::new(AuthenticatedRuntime::new(service.clone(), handle))
                as Arc<dyn RuntimeInterface>
        },
        interface,
    ))
}

fn authorized(method: &str, uri: &str, token: &str, body: Body) -> Request<Body> {
    Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {token}"))
        .header("x-interface-version", CURRENT_INTERFACE_VERSION)
        .header("x-correlation-id", uuid::Uuid::new_v4().to_string())
        .header(
            "x-deadline",
            (Utc::now() + Duration::minutes(2)).to_rfc3339_opts(SecondsFormat::Millis, true),
        )
        .header("content-type", "application/json")
        .body(body)
        .unwrap()
}

async fn identity(
    authority: &AuthorityStore,
    capabilities: impl IntoIterator<Item = Capability>,
    expires_at: chrono::DateTime<Utc>,
) -> Result<(PrincipalId, String, interface_core::CapabilityHandle), String> {
    let principal = PrincipalId::from_uuid(uuid::Uuid::new_v4());
    let token = authority
        .issue(principal.clone(), capabilities, expires_at)
        .await
        .map_err(|error| format!("{error:?}"))?
        .expose_once();
    let handle = authority
        .verify(&token)
        .await
        .map_err(|error| format!("{error:?}"))?;
    Ok((principal, token, handle))
}

fn envelope(session: &SessionId, page: &PageId, command: PrimitiveCommand) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: CommandId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id: session.clone(),
        page_id: Some(page.clone()),
        deadline: Utc::now() + Duration::seconds(20),
        command,
    }
}

fn completed(outcome: CommandOutcome) -> Result<Vec<Evidence>, String> {
    match outcome {
        CommandOutcome::Completed { evidence, .. } => Ok(evidence),
        other => Err(format!("expected completed command, got {other:?}")),
    }
}

fn require_canary_absent<const N: usize>(surfaces: [(&str, Vec<u8>); N]) -> SecurityResult {
    for (surface, bytes) in surfaces {
        require!(
            !bytes
                .windows(SECRET.len())
                .any(|window| window == SECRET.as_bytes()),
            "credential leaked through {surface}"
        );
    }
    Ok(())
}

#[test]
fn canary_detector_rejects_a_surface_that_contains_the_credential() {
    let error = require_canary_absent([("adapter", format!("error: {SECRET}").into_bytes())])
        .expect_err("credential-bearing adapter output must fail the release gate");
    assert!(error.contains("adapter"));
}

async fn initialize_mcp(server: &McpServer) {
    server
        .handle_message(
            json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{
                "protocolVersion":MCP_PROTOCOL_VERSION,"capabilities":{},
                "clientInfo":{"name":"release-security","version":"1"}
            }}),
        )
        .await;
    server
        .handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;
}

async fn mcp_transport_call<R: AsyncBufRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    writer: &mut W,
    message: Value,
) -> Result<Value, String> {
    let id = message.get("id").cloned();
    writer
        .write_all(format!("{message}\n").as_bytes())
        .await
        .map_err(|e| e.to_string())?;
    writer.flush().await.map_err(|e| e.to_string())?;
    if id.is_none() {
        return Ok(Value::Null);
    }
    loop {
        let mut line = String::new();
        require!(
            reader
                .read_line(&mut line)
                .await
                .map_err(|e| e.to_string())?
                > 0,
            "MCP transport closed"
        );
        let response: Value = serde_json::from_str(line.trim()).map_err(|e| e.to_string())?;
        if response.get("id") == id.as_ref() {
            return Ok(response);
        }
    }
}

async fn mcp_foreign_session_checks(
    server: McpServer,
    session: SessionId,
    page: PageId,
) -> Result<(Value, Value, Value), String> {
    tokio::task::LocalSet::new().run_until(async move {
        let (client_io, server_io) = tokio::io::duplex(256 * 1024);
        let (server_read, server_write) = tokio::io::split(server_io);
        let task = tokio::task::spawn_local(async move { server.serve(server_read, server_write).await });
        let (client_read, mut writer) = tokio::io::split(client_io);
        let mut reader = BufReader::new(client_read);
        mcp_transport_call(&mut reader, &mut writer, json!({"jsonrpc":"2.0","id":50,"method":"initialize","params":{"protocolVersion":MCP_PROTOCOL_VERSION,"capabilities":{},"clientInfo":{"name":"isolation","version":"1"}}})).await?;
        mcp_transport_call(&mut reader, &mut writer, json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}})).await?;
        let list = mcp_transport_call(&mut reader, &mut writer, json!({"jsonrpc":"2.0","id":51,"method":"tools/call","params":{"name":"session_list","arguments":{}}})).await?;
        let open = mcp_transport_call(&mut reader, &mut writer, json!({"jsonrpc":"2.0","id":52,"method":"tools/call","params":{"name":"page_open","arguments":{"sessionId":session}}})).await?;
        let submit = mcp_transport_call(&mut reader, &mut writer, json!({"jsonrpc":"2.0","id":53,"method":"tools/call","params":{"name":"command_execute","arguments":{"envelope":envelope(&session, &page, PrimitiveCommand::Inspect(InspectCommand::default()))}}})).await?;
        task.abort();
        let _ = task.await;
        Ok((list, open, submit))
    }).await
}

async fn broker_request(
    client: &reqwest::Client,
    method: reqwest::Method,
    url: String,
    token: &str,
    body: Option<Value>,
) -> Result<reqwest::Response, String> {
    let mut request = client
        .request(method, url)
        .bearer_auth(token)
        .header("x-interface-version", CURRENT_INTERFACE_VERSION)
        .header("x-correlation-id", uuid::Uuid::new_v4().to_string())
        .header(
            "x-deadline",
            (Utc::now() + Duration::minutes(2)).to_rfc3339(),
        );
    if let Some(body) = body {
        request = request
            .header("content-type", "application/json")
            .body(body.to_string());
    }
    request.send().await.map_err(|error| error.to_string())
}

async fn cdp_wire_request(
    socket: &mut tokio_tungstenite::WebSocketStream<
        tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>,
    >,
    id: u64,
    method: &str,
    params: Value,
) -> Result<(Value, Vec<Value>), String> {
    socket
        .send(Message::Text(
            json!({"id":id,"method":method,"params":params})
                .to_string()
                .into(),
        ))
        .await
        .map_err(|error| error.to_string())?;
    let mut events = Vec::new();
    loop {
        let text = tokio::time::timeout(std::time::Duration::from_secs(2), socket.next())
            .await
            .map_err(|_| format!("CDP {method} response timed out"))?
            .ok_or_else(|| format!("CDP {method} transport closed"))?
            .map_err(|error| error.to_string())?
            .into_text()
            .map_err(|error| error.to_string())?;
        let value: Value = serde_json::from_str(&text).map_err(|error| error.to_string())?;
        if value.get("id") == Some(&json!(id)) {
            return Ok((value, events));
        }
        events.push(value);
    }
}

async fn canary_leakage(harness: &ChromeRuntimeHarness) -> SecurityResult {
    let canary_principal = PrincipalId::from_uuid(uuid::Uuid::new_v4());
    harness
        .authority
        .enroll_hash(
            Sha256::digest(SECRET.as_bytes()).into(),
            canary_principal,
            all_capabilities(),
            Utc::now() + Duration::minutes(5),
        )
        .await
        .map_err(|error| format!("{error:?}"))?;
    let canary_handle = harness
        .authority
        .verify(SECRET)
        .await
        .map_err(|error| format!("{error:?}"))?;
    let runtime = Arc::new(AuthenticatedRuntime::new(
        harness.service.clone(),
        canary_handle.clone(),
    ));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    let service = harness.service.clone();
    let broker_app = router(AppState::new(
        harness.authority.clone(),
        move |handle| {
            Arc::new(AuthenticatedRuntime::new(service.clone(), handle))
                as Arc<dyn RuntimeInterface>
        },
        InterfaceConfig::default(),
    ));
    let broker = tokio::spawn(axum::serve(listener, broker_app).into_future());
    let client = reqwest::Client::new();
    let mut http_surfaces = Vec::new();
    let response = broker_request(
        &client,
        reqwest::Method::POST,
        format!("http://{address}/v1/sessions"),
        SECRET,
        Some(json!({"profile":"security-canary","proxy":null})),
    )
    .await?;
    http_surfaces.extend_from_slice(format!("{:?}", response.headers()).as_bytes());
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    http_surfaces.extend_from_slice(&bytes);
    let session: types::SessionState = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let response = broker_request(
        &client,
        reqwest::Method::POST,
        format!("http://{address}/v1/pages"),
        SECRET,
        Some(json!({"session_id":session.id})),
    )
    .await?;
    http_surfaces.extend_from_slice(format!("{:?}", response.headers()).as_bytes());
    let bytes = response.bytes().await.map_err(|e| e.to_string())?;
    http_surfaces.extend_from_slice(&bytes);
    let page: types::PageState = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    let mut dom = Vec::new();
    let mut shot = Vec::new();
    for (index, command) in [
        PrimitiveCommand::Navigate(NavigateCommand {
            url: harness.site_url(),
            wait_until: WaitUntil::DomContentLoaded,
            timeout_ms: 15_000,
        }),
        PrimitiveCommand::Inspect(InspectCommand::default()),
        PrimitiveCommand::CaptureScreenshot(CaptureScreenshotCommand {
            mode: ScreenshotMode::Viewport,
        }),
    ]
    .into_iter()
    .enumerate()
    {
        let response = broker_request(
            &client,
            reqwest::Method::POST,
            format!("http://{address}/v1/commands"),
            SECRET,
            Some(serde_json::to_value(envelope(&session.id, &page.id, command)).unwrap()),
        )
        .await?;
        http_surfaces.extend_from_slice(format!("{:?}", response.headers()).as_bytes());
        let bytes = response.bytes().await.map_err(|e| e.to_string())?;
        http_surfaces.extend_from_slice(&bytes);
        let evidence = completed(serde_json::from_slice(&bytes).map_err(|e| e.to_string())?)?;
        if index == 1 {
            dom = evidence;
        } else if index == 2 {
            shot = evidence;
        }
    }
    let response = broker_request(
        &client,
        reqwest::Method::GET,
        format!("http://{address}/v1/events?after=0&limit=16"),
        SECRET,
        None,
    )
    .await?;
    http_surfaces.extend_from_slice(format!("{:?}", response.headers()).as_bytes());
    http_surfaces.extend_from_slice(&response.bytes().await.map_err(|e| e.to_string())?);
    let artifact_id = shot
        .iter()
        .find_map(|item| match item {
            Evidence::Screenshot { artifact_id, .. } => Some(artifact_id),
            _ => None,
        })
        .ok_or_else(|| "missing screenshot evidence".to_owned())?;
    let screenshot = ArtifactStore::new(
        &harness.config.browser.artifacts_dir,
        harness.config.browser.max_artifact_bytes,
        harness.config.browser.max_screenshot_dimension,
    )
    .get(&session.id, artifact_id)
    .await
    .map_err(|error| error.to_string())?;

    broker.abort();

    let mcp_handle = harness
        .authority
        .verify(SECRET)
        .await
        .map_err(|e| format!("{e:?}"))?;
    let mcp = McpServer::new(Arc::new(AuthenticatedRuntime::new(
        harness.service.clone(),
        mcp_handle,
    )));
    let mcp_site_url = harness.site_url();
    let stdout: Vec<u8> = tokio::task::LocalSet::new().run_until(async move {
      let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
      let (server_read, server_write) = tokio::io::split(server_io);
      let mcp_task = tokio::task::spawn_local(async move { mcp.serve(server_read, server_write).await });
      let (client_read, mut client_write) = tokio::io::split(client_io);
      let mut client_read = BufReader::new(client_read);
      let mut stdout = Vec::new();
    let initialized = mcp_transport_call(&mut client_read, &mut client_write,
        json!({"jsonrpc":"2.0","id":30,"method":"initialize","params":{"protocolVersion":MCP_PROTOCOL_VERSION,"capabilities":{},"clientInfo":{"name":"canary","version":"1"}}})).await?;
    stdout.extend_from_slice(format!("{initialized}\n").as_bytes());
    mcp_transport_call(
        &mut client_read,
        &mut client_write,
        json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}),
    )
    .await?;
    let mcp_create = mcp_transport_call(&mut client_read, &mut client_write,
        json!({"jsonrpc":"2.0","id":31,"method":"tools/call","params":{"name":"session_create","arguments":{"profile":"mcp-canary","proxy":null}}})).await?;
    stdout.extend_from_slice(format!("{mcp_create}\n").as_bytes());
    let mcp_session = mcp_create["result"]["structuredContent"]["id"].clone();
    require!(
        mcp_session.is_string(),
        "MCP session creation failed: {mcp_create}"
    );
    let open = mcp_transport_call(&mut client_read, &mut client_write, json!({"jsonrpc":"2.0","id":32,"method":"tools/call","params":{"name":"page_open","arguments":{"sessionId":mcp_session}}})).await?;
    stdout.extend_from_slice(format!("{open}\n").as_bytes());
    let mcp_page = open["result"]["structuredContent"]["id"].clone();
    require!(mcp_page.is_string(), "MCP page open failed: {open}");
    for (id, command) in [
        (
            33,
            PrimitiveCommand::Navigate(NavigateCommand {
                url: mcp_site_url,
                wait_until: WaitUntil::DomContentLoaded,
                timeout_ms: 15_000,
            }),
        ),
        (34, PrimitiveCommand::Inspect(InspectCommand::default())),
        (
            35,
            PrimitiveCommand::CaptureScreenshot(CaptureScreenshotCommand {
                mode: ScreenshotMode::Viewport,
            }),
        ),
    ] {
        let value = mcp_transport_call(&mut client_read, &mut client_write, json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"command_execute","arguments":{"envelope":envelope(&serde_json::from_value(mcp_session.clone()).unwrap(), &serde_json::from_value(mcp_page.clone()).unwrap(), command)}}})).await?;
        stdout.extend_from_slice(format!("{value}\n").as_bytes());
    }
    let events = mcp_transport_call(&mut client_read, &mut client_write, json!({"jsonrpc":"2.0","id":36,"method":"tools/call","params":{"name":"events_read","arguments":{"cursor":0,"limit":16}}})).await?;
        stdout.extend_from_slice(format!("{events}\n").as_bytes());
        drop(client_write);
        mcp_task.abort();
        let _ = mcp_task.await;
    Ok::<_, String>(stdout)
    }).await?;

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|error| error.to_string())?;
    let address = listener.local_addr().map_err(|error| error.to_string())?;
    let gateway = Arc::new(CdpGateway::new(
        harness.authority.clone(),
        runtime,
        MethodRegistry::compiled(),
        format!("ws://{address}"),
    ));
    let websocket_url = gateway
        .version(Some(SECRET))
        .await
        .map_err(|error| format!("{error:?}"))?
        .web_socket_debugger_url;
    let cdp_server = tokio::spawn(axum::serve(listener, gateway.router()).into_future());
    let mut request = websocket_url
        .into_client_request()
        .map_err(|error| error.to_string())?;
    request
        .headers_mut()
        .insert("authorization", format!("Bearer {SECRET}").parse().unwrap());
    let (mut socket, _) = connect_async(request)
        .await
        .map_err(|error| error.to_string())?;
    let mut cdp_diagnostics = Vec::new();
    for request in [
        json!({"id":40,"method":"Browser.getVersion","params":{}}),
        json!({"id":41,"method":"Target.setDiscoverTargets","params":{"discover":true}}),
        json!({"id":42,"method":"Target.getTargets","params":{}}),
        json!({"id":43,"method":"Target.createTarget","params":{"url":"about:blank"}}),
    ] {
        let id = request["id"].clone();
        socket
            .send(Message::Text(request.to_string().into()))
            .await
            .map_err(|e| e.to_string())?;
        loop {
            let text = socket
                .next()
                .await
                .ok_or_else(|| "CDP adapter closed without response".to_owned())?
                .map_err(|e| e.to_string())?
                .into_text()
                .map_err(|e| e.to_string())?
                .to_string();
            cdp_diagnostics.extend_from_slice(text.as_bytes());
            let value: Value = serde_json::from_str(&text).map_err(|e| e.to_string())?;
            if value.get("id") == Some(&id) {
                break;
            }
        }
    }
    socket
        .close(None)
        .await
        .map_err(|error| error.to_string())?;
    cdp_server.abort();
    let journal = tokio::fs::read(&harness.config.storage.journal_path)
        .await
        .map_err(|error| error.to_string())?;

    let surfaces = [
        ("HTTP headers/bodies/errors", http_surfaces),
        ("journal", journal),
        ("screenshot", screenshot),
        ("DOM evidence", serde_json::to_vec(&dom).unwrap()),
        ("MCP stdout", stdout),
        ("CDP responses/events/diagnostics", cdp_diagnostics),
    ];
    require_canary_absent(surfaces)
}

async fn http_lifetime(harness: &ChromeRuntimeHarness) -> SecurityResult {
    for revoke in [false, true] {
        let (principal, token, _) = identity(
            &harness.authority,
            [Capability::SessionWrite],
            if revoke {
                Utc::now() + Duration::minutes(5)
            } else {
                Utc::now() + Duration::milliseconds(100)
            },
        )
        .await?;
        let delayed = stream::once(async {
            tokio::time::sleep(std::time::Duration::from_millis(175)).await;
            Ok::<_, Infallible>(Bytes::from_static(br#"{"profile":"late","proxy":null}"#))
        });
        let task = tokio::spawn(runtime_app(harness, InterfaceConfig::default()).oneshot(
            authorized("POST", "/v1/sessions", &token, Body::from_stream(delayed)),
        ));
        if revoke {
            tokio::time::sleep(std::time::Duration::from_millis(40)).await;
            harness
                .authority
                .revoke(&principal)
                .await
                .map_err(|error| format!("{error:?}"))?;
        }
        let response = task
            .await
            .map_err(|error| error.to_string())?
            .map_err(|error| error.to_string())?;
        require!(
            response.status() == StatusCode::UNAUTHORIZED,
            "HTTP lifetime returned {}",
            response.status()
        );
    }
    Ok(())
}

async fn mcp_lifetime(harness: &ChromeRuntimeHarness) -> SecurityResult {
    for revoke in [false, true] {
        let (principal, _, handle) = identity(
            &harness.authority,
            [Capability::SessionRead],
            if revoke {
                Utc::now() + Duration::minutes(5)
            } else {
                Utc::now() + Duration::milliseconds(100)
            },
        )
        .await?;
        let server = McpServer::new(Arc::new(AuthenticatedRuntime::new(
            harness.service.clone(),
            handle,
        )));
        initialize_mcp(&server).await;
        if revoke {
            harness
                .authority
                .revoke(&principal)
                .await
                .map_err(|error| format!("{error:?}"))?;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
        let response = server
            .handle_message(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
            .await
            .unwrap();
        require!(
            response["error"]["data"]["interfaceError"]["code"] == "authenticationFailed",
            "MCP lifetime returned {response}"
        );
    }
    Ok(())
}

async fn cdp_lifetime(harness: &ChromeRuntimeHarness) -> SecurityResult {
    for revoke in [false, true] {
        let (principal, token, handle) = identity(
            &harness.authority,
            [Capability::SessionRead],
            if revoke {
                Utc::now() + Duration::minutes(5)
            } else {
                Utc::now() + Duration::milliseconds(100)
            },
        )
        .await?;
        let gateway = CdpGateway::new(
            harness.authority.clone(),
            Arc::new(AuthenticatedRuntime::new(harness.service.clone(), handle)),
            MethodRegistry::compiled(),
            "ws://localhost",
        );
        let version = gateway
            .version(Some(&token))
            .await
            .map_err(|error| format!("{error:?}"))?;
        let path = version
            .web_socket_debugger_url
            .strip_prefix("ws://localhost")
            .ok_or_else(|| "unexpected CDP URL".to_owned())?;
        let connection = gateway
            .upgrade(path, Some(&token))
            .await
            .map_err(|error| format!("{error:?}"))?;
        if revoke {
            harness
                .authority
                .revoke(&principal)
                .await
                .map_err(|error| format!("{error:?}"))?;
        } else {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        }
        let response = connection
            .dispatch(CdpRequest::new(1, "Target.getTargets", json!({})))
            .await;
        require!(
            response
                .error()
                .is_some_and(|error| error.code == CdpErrorCode::RuntimeFailure as i32),
            "CDP lifetime did not fail closed"
        );
    }
    Ok(())
}

async fn principal_isolation(harness: &ChromeRuntimeHarness) -> SecurityResult {
    let (_, owner_token, owner_handle) = identity(
        &harness.authority,
        [
            Capability::SessionRead,
            Capability::SessionWrite,
            Capability::PageWrite,
            Capability::BrowserMutate,
            Capability::ArtifactRead,
            Capability::ArtifactCapture,
        ],
        Utc::now() + Duration::minutes(5),
    )
    .await?;
    let (_, other_token, other_handle) = identity(
        &harness.authority,
        [
            Capability::SessionRead,
            Capability::PageWrite,
            Capability::BrowserMutate,
            Capability::ArtifactRead,
        ],
        Utc::now() + Duration::minutes(5),
    )
    .await?;
    let owner_ctx = owner_handle.context(Utc::now() + Duration::minutes(1), None);
    let other_ctx = other_handle.context(Utc::now() + Duration::minutes(1), None);
    let (session_ownership, recorder) = SessionOwnershipRegistry::bounded(4);
    let owner = AuthenticatedRuntime::with_session_ownership(
        harness.service.clone(),
        owner_handle.clone(),
        recorder.clone(),
    );
    let session = owner
        .create_session(
            owner_ctx.clone(),
            CreateSessionRequest {
                profile: "principal-owner".into(),
                proxy: None,
            },
        )
        .await
        .map_err(|error| format!("{error:?}"))?;
    let owner_page = owner
        .open_page(
            owner_ctx.clone(),
            types::OpenPageRequest {
                session_id: session.id.clone(),
            },
        )
        .await
        .map_err(|error| format!("{error:?}"))?;
    require!(
        session_ownership.owns_session(&owner_ctx.principal_id, &session.id),
        "owner session absent from authenticated ownership boundary"
    );
    require!(
        !session_ownership.owns_session(&other_ctx.principal_id, &session.id),
        "session crossed principal boundary"
    );

    // Exercise the same foreign session through each production adapter. The
    // adapters must neither enumerate it nor dispatch an operation against it.
    let service = harness.service.clone();
    let adapter_recorder = recorder.clone();
    let app = router(AppState::new(
        harness.authority.clone(),
        move |handle| {
            Arc::new(AuthenticatedRuntime::with_session_ownership(
                service.clone(),
                handle,
                adapter_recorder.clone(),
            )) as Arc<dyn RuntimeInterface>
        },
        InterfaceConfig::default(),
    ));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    let broker_server = tokio::spawn(axum::serve(listener, app).into_future());
    let client = reqwest::Client::new();
    let response = broker_request(
        &client,
        reqwest::Method::GET,
        format!("http://{address}/v1/sessions"),
        &other_token,
        None,
    )
    .await?;
    let listed: Vec<types::SessionState> =
        serde_json::from_slice(&response.bytes().await.map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    require!(listed.is_empty(), "broker enumerated foreign session");
    let response = broker_request(
        &client,
        reqwest::Method::POST,
        format!("http://{address}/v1/pages"),
        &other_token,
        Some(json!({"session_id":session.id})),
    )
    .await?;
    require!(
        response.status() == StatusCode::NOT_FOUND,
        "broker admitted foreign session: {}",
        response.status()
    );
    let response = broker_request(
        &client,
        reqwest::Method::POST,
        format!("http://{address}/v1/commands"),
        &other_token,
        Some(
            serde_json::to_value(envelope(
                &session.id,
                &owner_page.id,
                PrimitiveCommand::Inspect(InspectCommand::default()),
            ))
            .unwrap(),
        ),
    )
    .await?;
    require!(
        response.status() == StatusCode::NOT_FOUND,
        "broker submitted against foreign session: {}",
        response.status()
    );
    broker_server.abort();

    let other_runtime = Arc::new(AuthenticatedRuntime::with_session_ownership(
        harness.service.clone(),
        other_handle.clone(),
        recorder.clone(),
    ));
    let mcp = McpServer::new(other_runtime.clone());
    let (mcp_list, mcp_denial, mcp_submit) =
        mcp_foreign_session_checks(mcp, session.id.clone(), owner_page.id.clone()).await?;
    require!(
        mcp_list["result"]["structuredContent"]
            .as_array()
            .is_some_and(Vec::is_empty),
        "MCP enumerated foreign session: {mcp_list}"
    );
    require!(
        mcp_denial["error"]["data"]["interfaceError"]["code"] == "notFound",
        "MCP admitted foreign session: {mcp_denial}"
    );
    require!(
        mcp_submit["error"]["data"]["interfaceError"]["code"] == "notFound",
        "MCP submitted against foreign session: {mcp_submit}"
    );

    let owner_runtime = Arc::new(AuthenticatedRuntime::with_session_ownership(
        harness.service.clone(),
        owner_handle.clone(),
        recorder.clone(),
    ));
    let owner_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let owner_address = owner_listener.local_addr().map_err(|e| e.to_string())?;
    let owner_gateway = Arc::new(CdpGateway::new(
        harness.authority.clone(),
        owner_runtime,
        MethodRegistry::compiled(),
        format!("ws://{owner_address}"),
    ));
    let owner_websocket = owner_gateway
        .version(Some(&owner_token))
        .await
        .map_err(|e| format!("{e:?}"))?
        .web_socket_debugger_url;
    let owner_server =
        tokio::spawn(axum::serve(owner_listener, owner_gateway.router()).into_future());
    let mut owner_request = owner_websocket
        .into_client_request()
        .map_err(|e| e.to_string())?;
    owner_request.headers_mut().insert(
        "authorization",
        format!("Bearer {owner_token}").parse().unwrap(),
    );
    let (mut owner_socket, _) = connect_async(owner_request)
        .await
        .map_err(|e| e.to_string())?;
    let (owner_targets, _) =
        cdp_wire_request(&mut owner_socket, 20, "Target.getTargets", json!({})).await?;
    let owner_target = owner_targets["result"]["targetInfos"]
        .as_array()
        .and_then(|targets| targets.first())
        .and_then(|target| target["targetId"].as_str())
        .ok_or_else(|| format!("owner target missing: {owner_targets}"))?
        .to_owned();
    let (_, mut attach_events) = cdp_wire_request(
        &mut owner_socket,
        21,
        "Target.setAutoAttach",
        json!({"autoAttach":true,"waitForDebuggerOnStart":false,"flatten":true}),
    )
    .await?;
    if !attach_events
        .iter()
        .any(|event| event["method"] == "Target.attachedToTarget")
    {
        let event = tokio::time::timeout(std::time::Duration::from_secs(2), owner_socket.next())
            .await
            .map_err(|_| "owner attached event timed out".to_owned())?
            .ok_or_else(|| "owner CDP transport closed".to_owned())?
            .map_err(|e| e.to_string())?
            .into_text()
            .map_err(|e| e.to_string())?;
        attach_events.push(serde_json::from_str(&event).map_err(|e| e.to_string())?);
    }
    let owner_cdp_session = attach_events
        .iter()
        .find(|event| event["method"] == "Target.attachedToTarget")
        .and_then(|event| event["params"]["sessionId"].as_str())
        .ok_or_else(|| format!("owner CDP session missing: {attach_events:?}"))?
        .to_owned();
    let (created, _) = cdp_wire_request(
        &mut owner_socket,
        22,
        "Target.createTarget",
        json!({"url":"about:blank"}),
    )
    .await?;
    require!(
        created["result"]["targetId"].is_string(),
        "owner target creation failed: {created}"
    );

    let other_listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let other_address = other_listener.local_addr().map_err(|e| e.to_string())?;
    let other_gateway = Arc::new(CdpGateway::new(
        harness.authority.clone(),
        other_runtime,
        MethodRegistry::compiled(),
        format!("ws://{other_address}"),
    ));
    let other_websocket = other_gateway
        .version(Some(&other_token))
        .await
        .map_err(|e| format!("{e:?}"))?
        .web_socket_debugger_url;
    let other_server =
        tokio::spawn(axum::serve(other_listener, other_gateway.router()).into_future());
    let discovery_response = reqwest::Client::new()
        .get(format!("http://{other_address}/json/list"))
        .bearer_auth(&other_token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let discovery: Value = serde_json::from_slice(
        &discovery_response
            .bytes()
            .await
            .map_err(|e| e.to_string())?,
    )
    .map_err(|e| e.to_string())?;
    require!(
        !discovery.to_string().contains(&owner_target),
        "CDP discovery disclosed owner target: {discovery}"
    );
    let mut other_request = other_websocket
        .into_client_request()
        .map_err(|e| e.to_string())?;
    other_request.headers_mut().insert(
        "authorization",
        format!("Bearer {other_token}").parse().unwrap(),
    );
    let (mut other_socket, _) = connect_async(other_request)
        .await
        .map_err(|e| e.to_string())?;
    let (other_targets, _) =
        cdp_wire_request(&mut other_socket, 23, "Target.getTargets", json!({})).await?;
    require!(
        !other_targets.to_string().contains(&owner_target),
        "Target.getTargets disclosed owner target: {other_targets}"
    );
    for (id, method, params) in [
        (
            24,
            "Target.attachToTarget",
            json!({"targetId":owner_target,"flatten":true}),
        ),
        (25, "Target.getTargetInfo", json!({"targetId":owner_target})),
        (
            26,
            "Page.getFrameTree",
            json!({"sessionId":owner_cdp_session}),
        ),
    ] {
        let (denial, _) = cdp_wire_request(&mut other_socket, id, method, params).await?;
        require!(
            denial["error"].is_object(),
            "CDP admitted owner identifier via {method}: {denial}"
        );
        require!(
            !denial.to_string().contains(&owner_target)
                && !denial.to_string().contains(&owner_cdp_session),
            "CDP denial disclosed owner identifiers via {method}: {denial}"
        );
    }
    owner_socket.close(None).await.map_err(|e| e.to_string())?;
    other_socket.close(None).await.map_err(|e| e.to_string())?;
    owner_server.abort();
    other_server.abort();

    let root = tempfile::tempdir().map_err(|error| error.to_string())?;
    let store = ArtifactStore::new(root.path(), 4096, 4096);
    let record = store
        .put(
            &session.id,
            &PageId::new(),
            "application/octet-stream",
            "bin",
            b"principal artifact",
            4096,
        )
        .await
        .map_err(|error| format!("{error:?}"))?;
    let (ownership, recorder): (_, SessionOwnershipRecorder) = SessionOwnershipRegistry::bounded(4);
    recorder
        .record_authenticated_session(owner_ctx.principal_id.clone(), session.id.clone())
        .map_err(|error| format!("{error:?}"))?;
    let reader = ArtifactReader::new(
        store,
        ownership,
        4096,
        ArtifactOwnershipLimits {
            max_records: 4,
            max_bytes: 16 * 1024,
        },
    )
    .map_err(|error| error.to_string())?;
    let reference = reader
        .register(&owner_handle, &owner_ctx, &session.id, &record)
        .await
        .map_err(|error| format!("{error:?}"))?;
    let denial = reader
        .read(&other_handle, &other_ctx, &session.id, &reference)
        .await
        .unwrap_err();
    require!(
        denial.code == InterfaceErrorCode::ArtifactDenied,
        "artifact crossed principal boundary"
    );

    let catalog = ArtifactCatalog::new(reader.clone(), 4);
    catalog
        .register_trusted(session.id.clone(), reference.clone())
        .await
        .map_err(|e| e.to_string())?;
    let service = harness.service.clone();
    let artifact_recorder = recorder.clone();
    let app = router(
        AppState::new(
            harness.authority.clone(),
            move |handle| {
                Arc::new(AuthenticatedRuntime::with_session_ownership(
                    service.clone(),
                    handle,
                    artifact_recorder.clone(),
                )) as Arc<dyn RuntimeInterface>
            },
            InterfaceConfig::default(),
        )
        .with_boundaries(EventStore::new(16), catalog),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .map_err(|e| e.to_string())?;
    let address = listener.local_addr().map_err(|e| e.to_string())?;
    let server = tokio::spawn(axum::serve(listener, app).into_future());
    let response = broker_request(
        &reqwest::Client::new(),
        reqwest::Method::GET,
        format!("http://{address}/v1/artifacts/{}", reference.artifact_id()),
        &other_token,
        None,
    )
    .await?;
    require!(
        response.status() == StatusCode::NOT_FOUND,
        "broker exposed foreign artifact: {}",
        response.status()
    );
    let denial_body: Value =
        serde_json::from_slice(&response.bytes().await.map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    require!(
        denial_body["error"]["code"] == "artifactDenied",
        "broker returned untyped artifact denial: {denial_body}"
    );
    server.abort();
    Ok(())
}

async fn filesystem_confinement() -> SecurityResult {
    let root = tempfile::tempdir().map_err(|error| error.to_string())?;
    let allowed = root.path().join("allowed");
    std::fs::create_dir(&allowed).map_err(|error| error.to_string())?;
    let outside = root.path().join("outside.txt");
    std::fs::write(&outside, b"outside").map_err(|error| error.to_string())?;
    require!(
        worker_pool::resolve_upload_paths(&[allowed], std::slice::from_ref(&outside)).is_err(),
        "upload traversal escaped root"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;
        let store = ArtifactStore::new(root.path().join("artifacts"), 4096, 4096);
        let session = SessionId::new();
        let record = store
            .put(
                &session,
                &PageId::new(),
                "application/octet-stream",
                "bin",
                b"trusted",
                4096,
            )
            .await
            .map_err(|error| error.to_string())?;
        let authority = AuthorityStore::in_memory();
        let (_, _, handle) = identity(
            &authority,
            [Capability::ArtifactRead, Capability::ArtifactCapture],
            Utc::now() + Duration::minutes(5),
        )
        .await?;
        let context = handle.context(Utc::now() + Duration::minutes(1), None);
        let (ownership, recorder) = SessionOwnershipRegistry::bounded(2);
        recorder
            .record_authenticated_session(context.principal_id.clone(), session.clone())
            .map_err(|error| format!("{error:?}"))?;
        let reader = ArtifactReader::new(
            store.clone(),
            ownership,
            4096,
            ArtifactOwnershipLimits {
                max_records: 2,
                max_bytes: 8192,
            },
        )
        .map_err(|error| error.to_string())?;
        let reference = reader
            .register(&handle, &context, &session, &record)
            .await
            .map_err(|error| format!("{error:?}"))?;
        let directory = store
            .configured_root()
            .join(session.0.to_string())
            .join(&record.artifact_id);
        let moved = directory.with_extension("moved");
        std::fs::rename(&directory, &moved).map_err(|error| error.to_string())?;
        symlink(&moved, &directory).map_err(|error| error.to_string())?;
        let denial = reader.read(&handle, &context, &session, &reference).await;
        require!(denial.is_err(), "artifact symlink swap was followed");
    }
    Ok(())
}

async fn duplicate_headers(harness: &ChromeRuntimeHarness) -> SecurityResult {
    let app = runtime_app(harness, InterfaceConfig::default());
    for (header, expected) in [
        ("authorization", StatusCode::UNAUTHORIZED),
        ("x-interface-version", StatusCode::UNPROCESSABLE_ENTITY),
        ("x-correlation-id", StatusCode::UNPROCESSABLE_ENTITY),
        ("x-deadline", StatusCode::UNPROCESSABLE_ENTITY),
        ("idempotency-key", StatusCode::UNPROCESSABLE_ENTITY),
    ] {
        let mut request = authorized("GET", "/v1/runtime", &harness.token, Body::empty());
        if header == "idempotency-key" {
            request
                .headers_mut()
                .append(header, "first".parse().unwrap());
        }
        request
            .headers_mut()
            .append(header, "duplicate".parse().unwrap());
        let response = app
            .clone()
            .oneshot(request)
            .await
            .map_err(|error| error.to_string())?;
        require!(
            response.status() == expected,
            "duplicate {header} returned {}",
            response.status()
        );
    }
    Ok(())
}

async fn oversized_http(harness: &ChromeRuntimeHarness) -> SecurityResult {
    let app = runtime_app(
        harness,
        InterfaceConfig {
            max_request_bytes: 64,
            ..InterfaceConfig::default()
        },
    );
    let body = json!({"profile":"x".repeat(256),"proxy":null}).to_string();
    let response = app
        .oneshot(authorized(
            "POST",
            "/v1/sessions",
            &harness.token,
            Body::from(body),
        ))
        .await
        .map_err(|error| error.to_string())?;
    require!(
        response.status() == StatusCode::PAYLOAD_TOO_LARGE,
        "oversized HTTP returned {}",
        response.status()
    );
    Ok(())
}

async fn oversized_mcp(harness: &ChromeRuntimeHarness) -> SecurityResult {
    let server = McpServer::new(harness.runtime.clone());
    initialize_mcp(&server).await;
    let input = format!("{{\"padding\":\"{}\"}}\n", "x".repeat(MAX_MCP_FRAME_BYTES));
    let mut stdout = Vec::new();
    server
        .serve(input.as_bytes(), &mut stdout)
        .await
        .map_err(|error| error.to_string())?;
    require!(!stdout.is_empty(), "oversized MCP produced no error");
    for line in stdout
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
    {
        require!(
            line.len() <= MAX_MCP_FRAME_BYTES,
            "MCP error exceeded frame bound"
        );
        serde_json::from_slice::<Value>(line).map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn oversized_cdp() -> SecurityResult {
    require!(
        parse_frame(&vec![b'x'; MAX_CDP_FRAME_BYTES + 1]).is_err(),
        "oversized CDP frame parsed"
    );
    Ok(())
}

async fn unsupported_protocols(harness: &ChromeRuntimeHarness) -> SecurityResult {
    require!(
        serde_json::from_str::<InterfaceVersion>("\"unsupported\"").is_err(),
        "unsupported interface version parsed"
    );
    let mut http = authorized("GET", "/v1/runtime", &harness.token, Body::empty());
    http.headers_mut()
        .insert("x-interface-version", "unsupported".parse().unwrap());
    let response = runtime_app(harness, InterfaceConfig::default())
        .oneshot(http)
        .await
        .map_err(|error| error.to_string())?;
    require!(
        response.status() == StatusCode::UNPROCESSABLE_ENTITY,
        "unsupported HTTP version returned {}",
        response.status()
    );
    let mcp = McpServer::new(harness.runtime.clone());
    let response = mcp.handle_message(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"1900-01-01","capabilities":{},"clientInfo":{"name":"old","version":"1"}}})).await.unwrap();
    require!(
        response["error"]["code"] == -32602,
        "unsupported MCP version returned {response}"
    );
    let cdp = CdpConnection::new(
        harness.handle.clone(),
        harness.runtime.clone(),
        MethodRegistry::compiled(),
    );
    let response = cdp
        .dispatch(CdpRequest::new(9, "SystemInfo.getProcessInfo", json!({})))
        .await;
    require!(
        response
            .error()
            .is_some_and(|error| error.code == CdpErrorCode::MethodNotFound as i32),
        "unsupported CDP method forwarded"
    );
    let mut malicious = authorized("GET", "/v1/runtime", &harness.token, Body::empty());
    malicious
        .headers_mut()
        .insert("x-correlation-id", SECRET.parse().unwrap());
    let response = runtime_app(harness, InterfaceConfig::default())
        .oneshot(malicious)
        .await
        .map_err(|error| error.to_string())?;
    let body = to_bytes(response.into_body(), 16 * 1024)
        .await
        .map_err(|error| error.to_string())?;
    require!(
        !body
            .windows(SECRET.len())
            .any(|window| window == SECRET.as_bytes()),
        "malicious correlation was reflected"
    );
    Ok(())
}

async fn idempotency_mismatch() -> SecurityResult {
    let store = IdempotencyStore::with_global_capacity(4, 8, Duration::minutes(5));
    let principal = PrincipalId::from_uuid(uuid::Uuid::new_v4());
    let key = IdempotencyKey::try_from("release-security-idempotency")
        .map_err(|error| error.to_string())?;
    let now = Utc::now();
    let first = store
        .reserve(
            principal.clone(),
            key.clone(),
            InterfaceOperation::SubmitCommand,
            canonical_sha256(&json!({"value":1})).map_err(|error| format!("{error:?}"))?,
            now,
            now + Duration::seconds(5),
            CorrelationId::new(),
        )
        .await
        .map_err(|error| format!("{error:?}"))?;
    require!(
        matches!(first, IdempotencyReservation::Acquired(_)),
        "first reservation not acquired"
    );
    let mismatch = store
        .reserve(
            principal,
            key,
            InterfaceOperation::SubmitCommand,
            canonical_sha256(&json!({"value":2})).map_err(|error| format!("{error:?}"))?,
            Utc::now(),
            Utc::now() + Duration::seconds(5),
            CorrelationId::new(),
        )
        .await
        .unwrap_err();
    require!(
        mismatch.code == InterfaceErrorCode::IdempotencyConflict,
        "idempotency mismatch returned {mismatch:?}"
    );
    Ok(())
}

async fn queue_overflow(harness: &ChromeRuntimeHarness) -> SecurityResult {
    let connection = CdpConnection::new(
        harness.handle.clone(),
        harness.runtime.clone(),
        MethodRegistry::compiled(),
    );
    for index in 0..MAX_QUEUED_EVENTS {
        connection
            .queue_event(CdpEvent {
                method: "Target.targetDestroyed".into(),
                params: json!({"targetId":format!("target-{index}")}),
                session_id: None,
            })
            .await
            .map_err(|error| error.message)?;
    }
    require!(
        connection
            .queue_event(CdpEvent {
                method: "Target.targetDestroyed".into(),
                params: json!({"targetId":"overflow"}),
                session_id: None
            })
            .await
            .is_err(),
        "CDP queue accepted overflow"
    );
    Ok(())
}

async fn event_gap(harness: &ChromeRuntimeHarness) -> SecurityResult {
    let events = EventStore::new(2);
    for index in 1..=3 {
        events
            .append(Event::new(format!("event-{index}"), json!({"index":index})))
            .await;
    }
    let gap = events.read_after(0.into(), 2).await.unwrap_err();
    require!(
        gap.reason == EventGapReason::HistoryLost && gap.earliest_available.0 == 2,
        "non-deterministic event gap {gap:?}"
    );
    let server = McpServer::production(
        harness.runtime.clone(),
        events,
        ArtifactResources::default(),
    );
    initialize_mcp(&server).await;
    let response = server.handle_message(json!({"jsonrpc":"2.0","id":8,"method":"tools/call","params":{"name":"events_read","arguments":{"cursor":0,"limit":2}}})).await.unwrap();
    require!(
        response["error"]["data"]["eventGap"]["reason"] == "historyLost"
            && response["error"]["data"]["eventGap"]["earliestAvailable"] == 2,
        "MCP gap metadata changed: {response}"
    );
    Ok(())
}

#[tokio::test]
async fn credentials_expire_revoke_and_never_reach_observable_payloads() {
    let authority = AuthorityStore::in_memory();
    let principal = PrincipalId::from_uuid(uuid::Uuid::new_v4());
    let token = authority
        .issue(
            principal.clone(),
            [Capability::SessionRead],
            Utc::now() + Duration::seconds(30),
        )
        .await
        .unwrap()
        .expose_once();
    let handle = authority.verify(&token).await.unwrap();
    authority.revoke(&principal).await.unwrap();
    assert!(!handle.is_valid_at(Utc::now()));
    assert!(authority.verify(&token).await.is_err());
    let events = EventStore::new(2);
    events.append(Event::new("diagnostic", json!({"authorization":format!("Bearer {SECRET}"),"nested":{"token":SECRET,"cookie":SECRET}}))).await;
    let encoded = serde_json::to_string(&events.read_after(0.into(), 1).await.unwrap()).unwrap();
    assert!(!encoded.contains(SECRET));
    assert!(encoded.contains("[REDACTED]"));
    assert!(!format!("{authority:?} {handle:?}").contains(&token));
}
