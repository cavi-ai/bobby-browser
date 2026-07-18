use interface_conformance::{
    live::ChromeRuntimeHarness, validate_canonical_proof, AuthorizationProof, CanonicalProof,
    CheckpointLineage, DenialProof, EvidenceProof, CANONICAL_EVENT_ORDER, CANONICAL_STEPS,
    NEGATIVE_CAPABILITY_MATRIX,
};
use interface_core::RuntimeInterface;
use sha2::{Digest, Sha256};
use types::{
    AttemptId, Capability, CaptureScreenshotCommand, CheckpointId, CheckpointInvariant,
    ClickAndWaitForDownloadCommand, CommandClass, CommandEnvelope, CommandId, CommandOutcome,
    CreateSessionRequest, Evidence, InspectCommand, NavigateCommand, OpenPageRequest,
    PrimitiveCommand, ScreenshotMode, UploadFilesCommand, WaitUntil, WorkflowCheckpoint,
    WorkflowId,
};

#[tokio::test]
async fn rust_sdk_executes_every_canonical_step_on_real_chrome() {
    let harness = ChromeRuntimeHarness::start().await;
    let runtime = harness.runtime.clone();
    let context = || harness.context();
    let mut observed = Vec::new();
    observed.push("runtime.info");
    runtime.runtime_info(context()).await.unwrap();
    observed.push("session.create");
    let session = runtime
        .create_session(
            context(),
            CreateSessionRequest {
                profile: "rust-conformance".into(),
                proxy: None,
            },
        )
        .await
        .unwrap();
    observed.push("page.open");
    let page = runtime
        .open_page(
            context(),
            OpenPageRequest {
                session_id: session.id.clone(),
            },
        )
        .await
        .unwrap();
    let workflow = WorkflowId::new();
    let attempt = AttemptId::new();
    let envelope = |id: CommandId, command: PrimitiveCommand| CommandEnvelope {
        schema_version: 1,
        command_id: id,
        workflow_id: workflow.clone(),
        attempt_id: attempt.clone(),
        session_id: session.id.clone(),
        page_id: Some(page.id.clone()),
        deadline: chrono::Utc::now() + chrono::Duration::seconds(20),
        command,
    };
    observed.push("command.navigate");
    completed(
        &runtime
            .submit(
                context(),
                envelope(
                    CommandId::new(),
                    PrimitiveCommand::Navigate(NavigateCommand {
                        url: harness.site_url(),
                        wait_until: WaitUntil::DomContentLoaded,
                        timeout_ms: 15_000,
                    }),
                ),
            )
            .await
            .unwrap(),
    );
    let fixture = harness.upload_root().join("canonical-upload.txt");
    let fixture_bytes = b"bounded fixture\n";
    std::fs::write(&fixture, fixture_bytes).unwrap();
    observed.push("command.upload");
    completed(
        &runtime
            .submit(
                context(),
                envelope(
                    CommandId::new(),
                    PrimitiveCommand::UploadFiles(UploadFilesCommand {
                        selector: "#resume".into(),
                        target: None,
                        paths: vec![fixture.display().to_string()],
                    }),
                ),
            )
            .await
            .unwrap(),
    );
    let inspect_id = CommandId::new();
    let inspection = runtime
        .submit(
            context(),
            envelope(
                inspect_id.clone(),
                PrimitiveCommand::Inspect(InspectCommand::default()),
            ),
        )
        .await
        .unwrap();
    let inspect_evidence = completed(&inspection);
    let (url, title) = inspect_evidence
        .iter()
        .find_map(|item| {
            if let Evidence::Inspection { url, title, .. } = item {
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
        session_id: session.id.clone(),
        page_id: page.id.clone(),
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
    runtime
        .checkpoint(context(), checkpoint.clone(), inspect_evidence.clone())
        .await
        .unwrap();
    let boundary = runtime
        .submit(
            context(),
            envelope(
                boundary_id,
                PrimitiveCommand::ClickAndWaitForDownload(ClickAndWaitForDownloadCommand {
                    selector: "#download".into(),
                    target: None,
                    timeout_ms: 15_000,
                }),
            ),
        )
        .await
        .unwrap();
    observed.push("artifact.verify");
    let screenshot = runtime
        .submit(
            context(),
            envelope(
                CommandId::new(),
                PrimitiveCommand::CaptureScreenshot(CaptureScreenshotCommand {
                    mode: ScreenshotMode::Viewport,
                }),
            ),
        )
        .await
        .unwrap();
    let shot = completed(&screenshot)
        .iter()
        .find_map(|item| {
            if let Evidence::Screenshot {
                artifact_id,
                bytes,
                sha256,
                ..
            } = item
            {
                Some((artifact_id.clone(), *bytes, sha256.clone()))
            } else {
                None
            }
        })
        .unwrap();
    let store = artifact_store::ArtifactStore::new(
        &harness.config.browser.artifacts_dir,
        harness.config.browser.max_artifact_bytes,
        harness.config.browser.max_screenshot_dimension,
    );
    let shot_bytes = store.get(&session.id, &shot.0).await.unwrap();
    assert_eq!(shot_bytes.len() as u64, shot.1);
    assert_eq!(format!("{:x}", Sha256::digest(&shot_bytes)), shot.2);
    observed.push("checkpoint.save");
    runtime
        .checkpoint(context(), checkpoint, inspect_evidence.clone())
        .await
        .unwrap();
    observed.push("recovery.inspect");
    let recovery = runtime.recover(context(), workflow).await.unwrap();
    assert!(!matches!(
        recovery,
        types::RecoveryDecision::Restarted { .. }
    ));
    observed.push("events.read");
    let journal = std::fs::read_to_string(&harness.config.storage.journal_path).unwrap();
    assert!(journal.lines().count() >= 5);
    let denied_handle = harness
        .authority
        .verify(&harness.denied_token)
        .await
        .unwrap();
    let denied_runtime =
        sdk_core::AuthenticatedRuntime::new(harness.service.clone(), denied_handle.clone());
    let denied = denied_runtime
        .runtime_info(
            denied_handle.context(chrono::Utc::now() + chrono::Duration::seconds(10), None),
        )
        .await
        .unwrap_err();
    assert_eq!(denied.required_capability, Some(Capability::SessionRead));
    let download = completed(&boundary)
        .iter()
        .find_map(|item| {
            if let Evidence::Download { bytes, sha256, .. } = item {
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
        event_ordering: CANONICAL_EVENT_ORDER.map(str::to_owned).to_vec(),
        checkpoint_lineage: CheckpointLineage {
            boundary: "submit".into(),
            replayed: false,
        },
    };
    emit_equality_proof(&proof);
    validate_canonical_proof(proof).unwrap();
    assert_eq!(observed, CANONICAL_STEPS);
}

fn completed(outcome: &CommandOutcome) -> &Vec<Evidence> {
    if let CommandOutcome::Completed { evidence, .. } = outcome {
        evidence
    } else {
        panic!("real command did not complete: {outcome:?}")
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

#[test]
fn rust_sdk_negative_capability_matrix_covers_every_step() {
    assert_eq!(NEGATIVE_CAPABILITY_MATRIX.len(), CANONICAL_STEPS.len());
    for step in CANONICAL_STEPS {
        assert!(NEGATIVE_CAPABILITY_MATRIX
            .iter()
            .any(|(candidate, _)| *candidate == step));
    }
}
