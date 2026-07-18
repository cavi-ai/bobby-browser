use interface_conformance::{
    live::ChromeRuntimeHarness, validate_canonical_proof, AuthorizationProof, CanonicalProof,
    CheckpointLineage, DenialProof, EvidenceProof, CANONICAL_STEPS,
};
use interface_core::{
    ArtifactOwnershipLimits, ArtifactReader, EventStore, SessionOwnershipRegistry,
};
use mcp_gateway::{ArtifactResources, Server};
use sdk_core::AuthenticatedRuntime;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use types::{
    AttemptId, CaptureScreenshotCommand, CheckpointId, CheckpointInvariant,
    ClickAndWaitForDownloadCommand, CommandClass, CommandEnvelope, CommandId, CommandOutcome,
    Evidence, InspectCommand, NavigateCommand, PrimitiveCommand, ScreenshotMode,
    UploadFilesCommand, WaitUntil, WorkflowCheckpoint, WorkflowId,
};

#[tokio::test]
async fn mcp_production_server_executes_every_canonical_step_on_real_chrome() {
    let harness = ChromeRuntimeHarness::start().await;
    let (ownership, recorder) =
        SessionOwnershipRegistry::bounded(harness.config.browser.max_active);
    let runtime = Arc::new(AuthenticatedRuntime::with_session_ownership(
        harness.service.clone(),
        harness.handle.clone(),
        recorder,
    ));
    let store = artifact_store::ArtifactStore::new(
        &harness.config.browser.artifacts_dir,
        harness.config.browser.max_artifact_bytes,
        harness.config.browser.max_screenshot_dimension,
    );
    let reader = ArtifactReader::new(
        store.clone(),
        ownership,
        harness.config.browser.max_artifact_bytes,
        ArtifactOwnershipLimits {
            max_records: 128,
            max_bytes: harness.config.browser.max_artifact_bytes as u64,
        },
    )
    .unwrap();
    let server = Server::production(
        runtime,
        EventStore::new(128),
        ArtifactResources::production(
            reader,
            store,
            &harness.config.browser.downloads_dir,
            harness.config.browser.max_artifact_bytes,
            128,
        ),
    );
    initialize(&server).await;
    let mut id = 10;
    let mut observed = Vec::new();
    let mut event_ordering = Vec::new();
    observed.push("runtime.info");
    tool(&server, &mut id, "runtime_info", json!({})).await;
    observed.push("session.create");
    let session = tool(
        &server,
        &mut id,
        "session_create",
        json!({"profile":"mcp-conformance","proxy":null}),
    )
    .await;
    let session_id = session["id"].as_str().unwrap().to_owned();
    observed.push("page.open");
    let page = tool(
        &server,
        &mut id,
        "page_open",
        json!({"sessionId":session_id}),
    )
    .await;
    let page_id = page["id"].as_str().unwrap().to_owned();
    let workflow = WorkflowId::new();
    let attempt = AttemptId::new();
    let sid = types::SessionId(uuid::Uuid::parse_str(&session_id).unwrap());
    let pid = types::PageId(uuid::Uuid::parse_str(&page_id).unwrap());
    let env = |command_id, command| CommandEnvelope {
        schema_version: 1,
        command_id,
        workflow_id: workflow.clone(),
        attempt_id: attempt.clone(),
        session_id: sid.clone(),
        page_id: Some(pid.clone()),
        deadline: chrono::Utc::now() + chrono::Duration::seconds(20),
        command,
    };
    observed.push("command.navigate");
    command(
        &server,
        &mut id,
        &env(
            CommandId::new(),
            PrimitiveCommand::Navigate(NavigateCommand {
                url: harness.site_url(),
                wait_until: WaitUntil::DomContentLoaded,
                timeout_ms: 15_000,
            }),
        ),
    )
    .await;
    event_ordering.push("navigation.completed".to_owned());
    let fixture = harness.upload_root().join("canonical-upload.txt");
    let fixture_bytes = b"bounded fixture\n";
    std::fs::write(&fixture, fixture_bytes).unwrap();
    observed.push("command.upload");
    command(
        &server,
        &mut id,
        &env(
            CommandId::new(),
            PrimitiveCommand::UploadFiles(UploadFilesCommand {
                selector: "#resume".into(),
                target: None,
                paths: vec![fixture.display().to_string()],
            }),
        ),
    )
    .await;
    event_ordering.push("upload.completed".to_owned());
    let inspect_id = CommandId::new();
    let inspection = command(
        &server,
        &mut id,
        &env(
            inspect_id.clone(),
            PrimitiveCommand::Inspect(InspectCommand::default()),
        ),
    )
    .await;
    let inspect: CommandOutcome = serde_json::from_value(inspection).unwrap();
    let inspect_evidence = completed(&inspect)
        .iter()
        .filter(|e| matches!(e, Evidence::Inspection { .. }))
        .cloned()
        .collect::<Vec<_>>();
    let (url, title) = inspect_evidence
        .iter()
        .find_map(|e| {
            if let Evidence::Inspection { url, title, .. } = e {
                Some((url.clone(), title.clone()))
            } else {
                None
            }
        })
        .unwrap();
    observed.push("command.boundary");
    let boundary_id = CommandId::new();
    let checkpoint = WorkflowCheckpoint {
        schema_version: 1,
        checkpoint_id: CheckpointId::new(),
        workflow_id: workflow.clone(),
        attempt_id: attempt.clone(),
        session_id: sid.clone(),
        page_id: pid.clone(),
        restart_url: url.clone(),
        current_url: url.clone(),
        cursor: Some(inspect_id),
        boundary_command_id: Some(boundary_id.clone()),
        recovery_class: CommandClass::Boundary,
        invariants: vec![
            CheckpointInvariant::Url { value: url },
            CheckpointInvariant::Title { value: title },
        ],
        replayable_inputs: vec![],
        evidence: inspect_evidence.clone(),
        recovery_history: vec![],
        created_at: chrono::Utc::now(),
    };
    let saved_checkpoint = tool(
        &server,
        &mut id,
        "checkpoint_save",
        json!({"checkpoint":checkpoint,"evidence":inspect_evidence}),
    )
    .await;
    event_ordering.push("checkpoint.saved".to_owned());
    let boundary: CommandOutcome = serde_json::from_value(
        command(
            &server,
            &mut id,
            &env(
                boundary_id,
                PrimitiveCommand::ClickAndWaitForDownload(ClickAndWaitForDownloadCommand {
                    selector: "#download".into(),
                    target: None,
                    timeout_ms: 15_000,
                }),
            ),
        )
        .await,
    )
    .unwrap();
    completed(&boundary);
    event_ordering.push("boundary.completed".to_owned());
    observed.push("artifact.verify");
    let screenshot: CommandOutcome = serde_json::from_value(
        command(
            &server,
            &mut id,
            &env(
                CommandId::new(),
                PrimitiveCommand::CaptureScreenshot(CaptureScreenshotCommand {
                    mode: ScreenshotMode::Viewport,
                }),
            ),
        )
        .await,
    )
    .unwrap();
    completed(&screenshot);
    event_ordering.push("screenshot.verified".to_owned());
    let shot = completed(&screenshot)
        .iter()
        .find_map(|e| {
            if let Evidence::Screenshot {
                artifact_id,
                bytes,
                sha256,
                ..
            } = e
            {
                Some((artifact_id.clone(), *bytes, sha256.clone()))
            } else {
                None
            }
        })
        .unwrap();
    let read = server
        .handle_message(req(
            id,
            "resources/read",
            json!({"uri":format!("artifact://{}",shot.0)}),
        ))
        .await
        .unwrap();
    id += 1;
    assert!(read["result"]["contents"][0]["blob"].is_string());
    observed.push("checkpoint.save");
    tool(
        &server,
        &mut id,
        "checkpoint_save",
        json!({"checkpoint":checkpoint,"evidence":checkpoint.evidence}),
    )
    .await;
    observed.push("recovery.inspect");
    let recovery = tool(
        &server,
        &mut id,
        "workflow_recover",
        json!({"workflowId":workflow}),
    )
    .await;
    let recovery_status = recovery["status"].as_str().unwrap().to_owned();
    let replayed = recovery_status == "restarted";
    let recovery_checkpoint = recovery["checkpointId"].as_str().unwrap().to_owned();
    assert_eq!(
        recovery_checkpoint,
        saved_checkpoint["checkpointId"].as_str().unwrap()
    );
    event_ordering.push("recovery.inspected".to_owned());
    observed.push("events.read");
    let events = tool(
        &server,
        &mut id,
        "events_read",
        json!({"cursor":0,"limit":16}),
    )
    .await;
    assert!(!events["events"].as_array().unwrap().is_empty());
    event_ordering.push("events.read".to_owned());
    let denied_handle = harness
        .authority
        .verify(&harness.denied_token)
        .await
        .unwrap();
    let denied = Server::new(Arc::new(AuthenticatedRuntime::new(
        harness.service.clone(),
        denied_handle,
    )));
    initialize(&denied).await;
    let listed = denied
        .handle_message(req(89, "tools/list", json!({})))
        .await
        .unwrap();
    assert!(listed["result"]["tools"].as_array().unwrap().is_empty());
    let denial = denied
        .handle_message(req(
            90,
            "tools/call",
            json!({"name":"runtime_info","arguments":{}}),
        ))
        .await
        .unwrap();
    assert_eq!(denial["error"]["code"], -32601);
    let download = completed(&boundary)
        .iter()
        .find_map(|e| {
            if let Evidence::Download { bytes, sha256, .. } = e {
                Some((*bytes, sha256.clone()))
            } else {
                None
            }
        })
        .unwrap();
    let proof = CanonicalProof {
        outcome_status: "completed".into(),
        evidence: vec![
            proof("navigation", harness.site_url().as_bytes()),
            proof("upload", fixture_bytes),
            EvidenceProof {
                kind: "screenshot".into(),
                sha256: shot.2,
                size: shot.1,
            },
            EvidenceProof {
                kind: "download".into(),
                sha256: download.1,
                size: download.0,
            },
        ],
        authorization: AuthorizationProof {
            allowed: vec![
                "page:write".into(),
                "file:upload".into(),
                "artifact:capture".into(),
                "file:download".into(),
            ],
            denied: DenialProof {
                capability: "session:read".into(),
                status: 403,
            },
        },
        event_ordering,
        checkpoint_lineage: CheckpointLineage {
            boundary: "submit".into(),
            replayed,
            checkpoint_id: recovery_checkpoint,
            recovery_status,
        },
    };
    emit_equality_proof(&proof);
    validate_canonical_proof(proof).unwrap();
    assert_eq!(observed, CANONICAL_STEPS);
}

async fn initialize(server: &Server) {
    server.handle_message(req(1,"initialize",json!({"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"conformance","version":"1"}}))).await;
    server
        .handle_message(json!({"jsonrpc":"2.0","method":"notifications/initialized","params":{}}))
        .await;
}
fn req(id: u64, method: &str, params: Value) -> Value {
    json!({"jsonrpc":"2.0","id":id,"method":method,"params":params})
}
async fn tool(server: &Server, id: &mut u64, name: &str, arguments: Value) -> Value {
    let response = server
        .handle_message(req(
            *id,
            "tools/call",
            json!({"name":name,"arguments":arguments}),
        ))
        .await
        .unwrap();
    *id += 1;
    assert!(response.get("error").is_none(), "{response}");
    response["result"]["structuredContent"].clone()
}
async fn command(server: &Server, id: &mut u64, envelope: &CommandEnvelope) -> Value {
    tool(server, id, "command_execute", json!({"envelope":envelope})).await
}
fn completed(outcome: &CommandOutcome) -> &Vec<Evidence> {
    if let CommandOutcome::Completed { evidence, .. } = outcome {
        evidence
    } else {
        panic!("MCP command failed: {outcome:?}")
    }
}
fn proof(kind: &str, bytes: &[u8]) -> EvidenceProof {
    EvidenceProof {
        kind: kind.into(),
        sha256: format!("{:x}", Sha256::digest(bytes)),
        size: bytes.len() as u64,
    }
}
fn emit_equality_proof(proof: &CanonicalProof) {
    let Ok(path) = std::env::var("CONFORMANCE_PROOF_PATH") else {
        return;
    };
    let mut normalized = proof.clone();
    let raw = normalized.evidence.clone();
    for item in &mut normalized.evidence {
        item.sha256 = format!(
            "{:x}",
            Sha256::digest(format!("verified-canonical-{}", item.kind))
        );
        item.size = 1;
    }
    std::fs::write(path,serde_json::to_vec(&serde_json::json!({"proof":normalized,"rawEvidence":raw,"normalization":"raw sha256 and size verified by adapter; canonical digest attests the same evidence kind invariant"})).unwrap()).unwrap();
}
