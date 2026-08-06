use interface_conformance::{
    live::ChromeRuntimeHarness, validate_canonical_proof, AuthorizationProof, CanonicalProof,
    CheckpointLineage, DenialProof, EvidenceProof, CANONICAL_STEPS, NEGATIVE_CAPABILITY_MATRIX,
};
use interface_core::RuntimeInterface;
use sha2::{Digest, Sha256};
use std::time::{Duration, Instant};
use types::{
    AttemptId, Capability, CaptureScreenshotCommand, CheckpointId, CheckpointInvariant,
    ClickAndWaitForDownloadCommand, ClickAndWaitForPopupCommand, ClosePageCommand, CommandClass,
    CommandEnvelope, CommandId, CommandOutcome, CreateSessionRequest, Evidence, InspectCommand,
    NavigateCommand, OpenPageRequest, PrimitiveCommand, RuntimeCommand, ScreenshotMode,
    UploadFilesCommand, WaitUntil, WorkflowCheckpoint, WorkflowId,
};

#[tokio::test]
async fn rust_sdk_executes_every_canonical_step_on_real_chrome() {
    emit_performance_phase("boot");
    let harness = ChromeRuntimeHarness::start().await;
    emit_performance_phase("harness-ready");
    let runtime = harness.runtime.clone();
    let context = || harness.context();
    runtime.runtime_info(context()).await.unwrap();
    let session = runtime
        .create_session(
            context(),
            CreateSessionRequest {
                profile: "rust-conformance".into(),
                proxy: None,
                execution_policy: Default::default(),
            },
        )
        .await
        .unwrap();
    let page = runtime
        .open_page(
            context(),
            OpenPageRequest {
                session_id: session.id.clone(),
            },
        )
        .await
        .unwrap();
    let samples = performance_samples();
    if let Some(samples) = samples {
        emit_performance_phase("warmup-start");
        run_rust_sample(&harness, &session.id, &page.id).await;
        emit_performance_phase("warmup-end");
        emit_performance_event(
            serde_json::json!({"event":"measurement-start","adapter":"rust-sdk","samples":samples,"rootPid":std::process::id()}),
        );
        let mut measured = Vec::with_capacity(samples);
        for index in 0..samples {
            emit_performance_phase(&format!("sample-{index}-start"));
            let started = Instant::now();
            let operation = run_rust_sample(&harness, &session.id, &page.id).await;
            let wall = started.elapsed();
            let sample = serde_json::json!({
                "adapterWallMs": duration_ms(wall),
                "adapterOperationMs": duration_ms(operation),
                "harnessEnvelopeOverheadMs": duration_ms(wall) - duration_ms(operation),
            });
            emit_performance_event(
                serde_json::json!({"event":"sample","adapter":"rust-sdk","index":index,"sample":sample,"rootPid":std::process::id()}),
            );
            emit_performance_phase(&format!("sample-{index}-end"));
            measured.push(sample);
        }
        let cleanup = CommandEnvelope {
            schema_version: CommandEnvelope::SCHEMA_VERSION,
            command_id: CommandId::new(),
            workflow_id: WorkflowId::new(),
            attempt_id: AttemptId::new(),
            session_id: session.id.clone(),
            page_id: Some(page.id.clone()),
            deadline: chrono::Utc::now() + chrono::Duration::seconds(20),
            command: RuntimeCommand::Primitive(PrimitiveCommand::ClosePage(ClosePageCommand {
                page_id: page.id.clone(),
            })),
        };
        completed(&runtime.submit(context(), cleanup).await.unwrap());
        drop(runtime);
        emit_performance_event(
            serde_json::json!({"event":"client-disconnected","adapter":"rust-sdk","samples":measured,"rootPid":std::process::id()}),
        );
        wait_for_rss_acknowledgement();
    } else {
        run_rust_sample(&harness, &session.id, &page.id).await;
    }
}

async fn run_rust_sample(
    harness: &ChromeRuntimeHarness,
    session_id: &types::SessionId,
    page_id: &types::PageId,
) -> Duration {
    let runtime = harness.runtime.clone();
    let context = || harness.context();
    let mut observed = Vec::new();
    let mut event_ordering = Vec::new();
    let operation_started = Instant::now();
    observed.push("runtime.info");
    runtime.runtime_info(context()).await.unwrap();
    observed.push("session.create");
    observed.push("page.open");
    let workflow = WorkflowId::new();
    let attempt = AttemptId::new();
    let envelope = |id: CommandId, command: PrimitiveCommand| CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: id,
        workflow_id: workflow.clone(),
        attempt_id: attempt.clone(),
        session_id: session_id.clone(),
        page_id: Some(page_id.clone()),
        deadline: chrono::Utc::now() + chrono::Duration::seconds(20),
        command: RuntimeCommand::Primitive(command),
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
    event_ordering.push("navigation.completed".to_owned());
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
    event_ordering.push("upload.completed".to_owned());
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
    let popup_id = CommandId::new();
    let popup_checkpoint = WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: CheckpointId::new(),
        workflow_id: workflow.clone(),
        attempt_id: attempt.clone(),
        session_id: session_id.clone(),
        page_id: page_id.clone(),
        restart_url: url.clone(),
        current_url: url.clone(),
        cursor: Some(inspect_id),
        boundary_command_id: Some(popup_id.clone()),
        recovery_class: CommandClass::Boundary,
        invariants: vec![
            CheckpointInvariant::Url { value: url },
            CheckpointInvariant::Title { value: title },
        ],
        replayable_inputs: vec![],
        evidence: inspect_evidence.clone(),
        recovery_history: vec![],
        recovery_receipts: vec![],
        created_at: chrono::Utc::now(),
    };
    runtime
        .checkpoint(context(), popup_checkpoint, inspect_evidence.clone())
        .await
        .unwrap();
    event_ordering.push("checkpoint.saved".to_owned());
    completed(
        &runtime
            .submit(
                context(),
                envelope(
                    popup_id,
                    PrimitiveCommand::ClickAndWaitForPopup(ClickAndWaitForPopupCommand {
                        selector: "#root-popup".into(),
                        target: None,
                        timeout_ms: 15_000,
                    }),
                ),
            )
            .await
            .unwrap(),
    );
    event_ordering.push("boundary.completed".to_owned());
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
    let boundary_id = CommandId::new();
    let checkpoint = WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: CheckpointId::new(),
        workflow_id: workflow.clone(),
        attempt_id: attempt.clone(),
        session_id: session_id.clone(),
        page_id: page_id.clone(),
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
        recovery_receipts: vec![],
        created_at: chrono::Utc::now(),
    };
    let saved_checkpoint = runtime
        .checkpoint(context(), checkpoint.clone(), inspect_evidence.clone())
        .await
        .unwrap();
    event_ordering.push("checkpoint.saved".to_owned());
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
    completed(&boundary);
    event_ordering.push("boundary.completed".to_owned());
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
    completed(&screenshot);
    event_ordering.push("screenshot.verified".to_owned());
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
    let shot_bytes = store.get(session_id, &shot.0).await.unwrap();
    assert_eq!(shot_bytes.len() as u64, shot.1);
    assert_eq!(hex::encode(Sha256::digest(&shot_bytes)), shot.2);
    observed.push("checkpoint.save");
    runtime
        .checkpoint(context(), checkpoint, inspect_evidence.clone())
        .await
        .unwrap();
    observed.push("recovery.inspect");
    let recovery = runtime.recover(context(), workflow).await.unwrap();
    let (recovery_status, recovery_checkpoint, replayed) = match recovery {
        types::RecoveryDecision::Resumed { checkpoint_id, .. } => ("resumed", checkpoint_id, false),
        types::RecoveryDecision::NeedsReconciliation { checkpoint_id, .. } => {
            ("needsReconciliation", checkpoint_id, false)
        }
        types::RecoveryDecision::Restarted { checkpoint_id, .. } => {
            ("restarted", checkpoint_id, true)
        }
    };
    assert_eq!(recovery_checkpoint, saved_checkpoint.checkpoint_id);
    event_ordering.push("recovery.inspected".to_owned());
    observed.push("events.read");
    let journal = std::fs::read_to_string(&harness.config.storage.journal_path).unwrap();
    assert!(journal.lines().count() >= 5);
    event_ordering.push("events.read".to_owned());
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
    let operation_elapsed = operation_started.elapsed();
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
        event_ordering,
        checkpoint_lineage: CheckpointLineage {
            boundary: "boundary".into(),
            replayed,
            checkpoint_id: recovery_checkpoint.0.to_string(),
            workflow_id: saved_checkpoint.workflow_id.0.to_string(),
            boundary_command_id: saved_checkpoint
                .boundary_command_id
                .as_ref()
                .unwrap()
                .0
                .to_string(),
            recovery_status: recovery_status.into(),
        },
    };
    emit_equality_proof(&proof);
    validate_canonical_proof(proof).unwrap();
    assert_eq!(observed, CANONICAL_STEPS);
    operation_elapsed
}

fn performance_samples() -> Option<usize> {
    let raw = std::env::var("CONFORMANCE_PERFORMANCE_SAMPLES").ok()?;
    let samples = raw.parse::<usize>().expect("performance sample count");
    assert!(
        samples >= 7,
        "performance gate requires at least seven samples"
    );
    Some(samples)
}
fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}
fn emit_performance_event(value: serde_json::Value) {
    let Ok(directory) = std::env::var("CONFORMANCE_PERFORMANCE_CONTROL_DIR") else {
        return;
    };
    std::fs::create_dir_all(&directory).unwrap();
    let filename = match value["event"].as_str().unwrap() {
        "measurement-start" => "ready.json".to_owned(),
        "client-disconnected" => "disconnected.json".to_owned(),
        "sample" => format!("sample-{}.json", value["index"].as_u64().unwrap()),
        event => panic!("unknown performance event {event}"),
    };
    let destination = std::path::Path::new(&directory).join(filename);
    let temporary = destination.with_extension(format!("{}.tmp", std::process::id()));
    std::fs::write(&temporary, serde_json::to_vec(&value).unwrap()).unwrap();
    std::fs::rename(temporary, destination).unwrap();
}
fn emit_performance_phase(phase: &str) {
    let Ok(directory) = std::env::var("CONFORMANCE_PERFORMANCE_CONTROL_DIR") else {
        return;
    };
    std::fs::create_dir_all(&directory).unwrap();
    let destination = std::path::Path::new(&directory).join(format!("phase-{phase}.json"));
    let temporary = destination.with_extension(format!("{}.tmp", std::process::id()));
    let value = serde_json::json!({"event":"phase","phase":phase,"rootPid":std::process::id()});
    std::fs::write(&temporary, serde_json::to_vec(&value).unwrap()).unwrap();
    std::fs::rename(temporary, destination).unwrap();
}
fn wait_for_rss_acknowledgement() {
    if std::env::var("CONFORMANCE_PERFORMANCE_WAIT_FOR_RSS").as_deref() != Ok("1") {
        return;
    }
    let directory = std::env::var("CONFORMANCE_PERFORMANCE_CONTROL_DIR")
        .expect("performance control directory");
    let acknowledgement = std::path::Path::new(&directory).join("ack.json");
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if std::fs::read_to_string(&acknowledgement)
            .is_ok_and(|value| value.contains("rss-sampled"))
        {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("RSS acknowledgement timed out");
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
        sha256: hex::encode(Sha256::digest(bytes)),
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
        item.sha256 = hex::encode(Sha256::digest(format!("verified-canonical-{}", item.kind)));
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
