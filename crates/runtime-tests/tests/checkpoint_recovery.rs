use std::path::PathBuf;
use std::sync::Arc;

use checkpoint_store::CheckpointStore;
use chrono::Utc;
use config::BrowserConfig;
use page_runtime::RecoveryCoordinator;
use types::{
    AttemptId, CheckpointId, CheckpointInvariant, CommandClass, Evidence, InspectCommand,
    NavigateCommand, PageId, RecoveryDecision, SessionId, WaitUntil, WorkflowCheckpoint,
    WorkflowId,
};
use worker_pool::{ChromiumWorkerFactory, WorkerPool};

fn checkpoint(
    session_id: SessionId,
    page_id: PageId,
    current_url: String,
    restart_url: String,
    title: &str,
) -> WorkflowCheckpoint {
    WorkflowCheckpoint {
        schema_version: WorkflowCheckpoint::SCHEMA_VERSION,
        checkpoint_id: CheckpointId::new(),
        workflow_id: WorkflowId::new(),
        attempt_id: AttemptId::new(),
        session_id,
        page_id,
        restart_url,
        current_url: current_url.clone(),
        cursor: None,
        boundary_command_id: None,
        recovery_class: CommandClass::Reconciliable,
        invariants: vec![
            CheckpointInvariant::Url { value: current_url },
            CheckpointInvariant::Title {
                value: title.into(),
            },
        ],
        replayable_inputs: Vec::new(),
        evidence: Vec::new(),
        recovery_history: Vec::new(),
        created_at: Utc::now(),
    }
}

async fn navigate(
    pool: &WorkerPool,
    session_id: &SessionId,
    page_id: &PageId,
    url: &str,
) -> Vec<Evidence> {
    let lease = pool.lease(session_id.clone()).await.unwrap();
    lease
        .worker()
        .navigate(
            page_id,
            &NavigateCommand {
                url: url.into(),
                wait_until: WaitUntil::Interactive,
                timeout_ms: 10_000,
            },
        )
        .await
        .unwrap()
}

#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn replaces_chrome_then_resumes_or_restarts_from_verified_state() {
    let fixture = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let profiles_dir = root.path().join("profiles");
    let store = CheckpointStore::open(root.path().join("checkpoints"))
        .await
        .unwrap();
    let pool = Arc::new(WorkerPool::new(
        1,
        Arc::new(ChromiumWorkerFactory::new(BrowserConfig {
            executable: Some(PathBuf::from(
                "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            )),
            profiles_dir,
            headless: true,
            max_active: 1,
            upload_roots: vec![root.path().join("uploads")],
            downloads_dir: root.path().join("downloads"),
            artifacts_dir: root.path().join("artifacts"),
            max_artifact_bytes: 8 * 1024 * 1024,
            max_screenshot_dimension: 16_384,
            max_js_result_bytes: 64 * 1024,
            max_js_timeout_ms: 30_000,
        })),
    ));
    let coordinator = RecoveryCoordinator::with_workers(store.clone(), pool.clone());
    let session_id = SessionId::new();
    let page_id = PageId::new();
    let first = pool.lease(session_id.clone()).await.unwrap();
    let first_worker_id = first.worker_id();
    first.worker().open_page(page_id.clone()).await.unwrap();

    let root_url = format!("{}/", fixture.base_url());
    let evidence = navigate(&pool, &session_id, &page_id, &root_url).await;
    let resume_checkpoint = checkpoint(
        session_id.clone(),
        page_id.clone(),
        root_url.clone(),
        root_url.clone(),
        "Runtime Fixture",
    );
    coordinator
        .save_verified(resume_checkpoint.clone(), evidence)
        .await
        .unwrap();
    let resumed = coordinator
        .recover(&resume_checkpoint.workflow_id)
        .await
        .unwrap();
    assert!(matches!(resumed, RecoveryDecision::Resumed { .. }));
    let second_worker_id = pool.lease(session_id.clone()).await.unwrap().worker_id();
    assert_ne!(first_worker_id, second_worker_id);

    let drift_url = format!("{}drift", root_url);
    let drift_evidence = navigate(&pool, &session_id, &page_id, &drift_url).await;
    let restart_checkpoint = checkpoint(
        session_id.clone(),
        page_id.clone(),
        drift_url,
        root_url.clone(),
        "Stable Checkpoint",
    );
    coordinator
        .save_verified(restart_checkpoint.clone(), drift_evidence)
        .await
        .unwrap();
    let restarted = coordinator
        .recover(&restart_checkpoint.workflow_id)
        .await
        .unwrap();
    let new_attempt_id = match restarted {
        RecoveryDecision::Restarted { lineage, .. } => {
            assert_eq!(lineage.abandoned_attempt_id, restart_checkpoint.attempt_id);
            lineage.attempt_id
        }
        decision => panic!("expected restart, got {decision:?}"),
    };
    let replacement = pool.lease(session_id.clone()).await.unwrap();
    assert_ne!(replacement.worker_id(), second_worker_id);
    let final_state = replacement
        .worker()
        .inspect(&page_id, &InspectCommand::default())
        .await
        .unwrap();
    assert!(final_state.iter().any(
        |item| matches!(item, Evidence::Inspection { url, title, .. }
            if url == &root_url && title == "Runtime Fixture")
    ));
    let persisted = store.load(&restart_checkpoint.workflow_id).await.unwrap();
    assert!(matches!(
        persisted.recovery_history.last().map(|item| &item.decision),
        Some(RecoveryDecision::Restarted { lineage, .. }) if lineage.attempt_id == new_attempt_id
    ));

    println!(
        "recovered workers=({first_worker_id:?},{second_worker_id:?},{:?}) attempts=({:?},{new_attempt_id:?}) final_url={root_url}",
        replacement.worker_id(),
        restart_checkpoint.attempt_id
    );
    pool.release_session(&session_id).await.unwrap();
}
