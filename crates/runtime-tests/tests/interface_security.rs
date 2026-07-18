use std::{convert::Infallible, sync::Arc};

use artifact_store::ArtifactStore;
use axum::{
    body::{to_bytes, Body, Bytes},
    http::{Request, StatusCode},
};
use broker::{router, AppState};
use cdp_gateway::{
    parse_frame, CdpConnection, CdpErrorCode, CdpEvent, CdpGateway, CdpRequest, MethodRegistry,
    MAX_FRAME_BYTES as MAX_CDP_FRAME_BYTES, MAX_QUEUED_EVENTS,
};
use chrono::{Duration, SecondsFormat, Utc};
use config::InterfaceConfig;
use futures_util::stream;
use interface_conformance::live::ChromeRuntimeHarness;
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
use tower::ServiceExt;
use types::{
    AttemptId, Capability, CaptureScreenshotCommand, CommandEnvelope, CommandId, CommandOutcome,
    CorrelationId, CreateSessionRequest, Evidence, IdempotencyKey, InspectCommand,
    InterfaceErrorCode, InterfaceOperation, InterfaceVersion, NavigateCommand, OpenPageRequest,
    PageId, PrimitiveCommand, PrincipalId, ScreenshotMode, SessionId, WaitUntil, WorkflowId,
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

async fn canary_leakage(harness: &ChromeRuntimeHarness) -> SecurityResult {
    let runtime = harness.runtime.clone();
    let session = runtime
        .create_session(
            harness.context(),
            CreateSessionRequest {
                profile: "security-canary".into(),
                proxy: None,
            },
        )
        .await
        .map_err(|error| format!("{error:?}"))?;
    let page = runtime
        .open_page(
            harness.context(),
            OpenPageRequest {
                session_id: session.id.clone(),
            },
        )
        .await
        .map_err(|error| format!("{error:?}"))?;
    completed(
        runtime
            .submit(
                harness.context(),
                envelope(
                    &session.id,
                    &page.id,
                    PrimitiveCommand::Navigate(NavigateCommand {
                        url: harness.site_url(),
                        wait_until: WaitUntil::DomContentLoaded,
                        timeout_ms: 15_000,
                    }),
                ),
            )
            .await
            .map_err(|error| format!("{error:?}"))?,
    )?;
    let dom = completed(
        runtime
            .submit(
                harness.context(),
                envelope(
                    &session.id,
                    &page.id,
                    PrimitiveCommand::Inspect(InspectCommand::default()),
                ),
            )
            .await
            .map_err(|error| format!("{error:?}"))?,
    )?;
    let shot = completed(
        runtime
            .submit(
                harness.context(),
                envelope(
                    &session.id,
                    &page.id,
                    PrimitiveCommand::CaptureScreenshot(CaptureScreenshotCommand {
                        mode: ScreenshotMode::Viewport,
                    }),
                ),
            )
            .await
            .map_err(|error| format!("{error:?}"))?,
    )?;
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

    let response = runtime_app(harness, InterfaceConfig::default())
        .oneshot(authorized(
            "GET",
            &format!("/v1/runtime?canary={}", harness.token),
            &harness.token,
            Body::empty(),
        ))
        .await
        .map_err(|error| error.to_string())?;
    let headers = format!("{:?}", response.headers()).into_bytes();
    let body = to_bytes(response.into_body(), 16 * 1024)
        .await
        .map_err(|error| error.to_string())?
        .to_vec();

    let mcp = McpServer::new(runtime.clone());
    initialize_mcp(&mcp).await;
    let input = format!(
        "{{bad {}}}\n{{\"jsonrpc\":\"2.0\",\"id\":9,\"method\":\"unknown-{}\",\"params\":{{}}}}\n",
        harness.token, harness.token
    );
    let mut stdout = Vec::new();
    mcp.serve(input.as_bytes(), &mut stdout)
        .await
        .map_err(|error| error.to_string())?;

    let cdp = CdpConnection::new(harness.handle.clone(), runtime, MethodRegistry::compiled());
    let cdp_error = cdp
        .dispatch(CdpRequest::new(
            7,
            format!("unknown.{}", harness.token),
            json!({}),
        ))
        .await;
    let cdp_events = cdp.drain_events().await;
    let journal = tokio::fs::read(&harness.config.storage.journal_path)
        .await
        .map_err(|error| error.to_string())?;

    let surfaces = [
        ("URL/body/error", body),
        ("headers", headers),
        ("journal", journal),
        ("screenshot", screenshot),
        ("DOM evidence", serde_json::to_vec(&dom).unwrap()),
        ("MCP stdout", stdout),
        ("CDP diagnostics", serde_json::to_vec(&cdp_error).unwrap()),
        ("CDP events", serde_json::to_vec(&cdp_events).unwrap()),
    ];
    for (surface, bytes) in surfaces {
        require!(
            !bytes
                .windows(harness.token.len())
                .any(|window| window == harness.token.as_bytes()),
            "credential leaked through {surface}"
        );
    }
    Ok(())
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
    let (_, _, owner_handle) = identity(
        &harness.authority,
        [
            Capability::SessionRead,
            Capability::SessionWrite,
            Capability::ArtifactRead,
            Capability::ArtifactCapture,
        ],
        Utc::now() + Duration::minutes(5),
    )
    .await?;
    let (_, _, other_handle) = identity(
        &harness.authority,
        [Capability::SessionRead, Capability::ArtifactRead],
        Utc::now() + Duration::minutes(5),
    )
    .await?;
    let owner_ctx = owner_handle.context(Utc::now() + Duration::minutes(1), None);
    let other_ctx = other_handle.context(Utc::now() + Duration::minutes(1), None);
    let (session_ownership, recorder) = SessionOwnershipRegistry::bounded(4);
    let owner = AuthenticatedRuntime::with_session_ownership(
        harness.service.clone(),
        owner_handle.clone(),
        recorder,
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
    require!(
        session_ownership.owns_session(&owner_ctx.principal_id, &session.id),
        "owner session absent from authenticated ownership boundary"
    );
    require!(
        !session_ownership.owns_session(&other_ctx.principal_id, &session.id),
        "session crossed principal boundary"
    );

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
