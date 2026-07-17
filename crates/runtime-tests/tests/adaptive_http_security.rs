use std::collections::BTreeMap;

use artifact_store::{ArtifactError, ArtifactStore};
use network_engine::state::HttpStateSnapshot;
use network_engine::{DirectHttpExecutor, NetworkPolicy};
use types::{
    CommandId, CommandOutcome, CommandPhase, DownloadUrlCommand, ErrorCode, InspectCommand, PageId,
    SessionId,
};
use workflow_journal::{CommandJournal, JournalRecord, JsonlJournal};

fn expect_error(
    result: Result<network_engine::HttpCandidate, types::CommandError>,
) -> types::CommandError {
    match result {
        Ok(_) => panic!("expected transfer failure"),
        Err(error) => error,
    }
}

fn snapshot(url: String, secret: &str) -> HttpStateSnapshot {
    HttpStateSnapshot {
        version: 1,
        current_url: url,
        cookies: Vec::new(),
        cache_validators: BTreeMap::new(),
        user_agent: format!("runtime-test/{secret}"),
        language: "en-US".into(),
    }
}

fn fixture_policy() -> NetworkPolicy {
    NetworkPolicy {
        allow_loopback: true,
        max_body_bytes: 16,
        max_download_bytes: 64,
        ..NetworkPolicy::default()
    }
}

#[tokio::test]
async fn production_denial_and_explicit_fixture_grant_are_enforced_per_hop() {
    let site = test_site::spawn().await;
    let production = DirectHttpExecutor::new(NetworkPolicy::default());
    let denied = expect_error(
        production
            .inspect(
                &snapshot(format!("{}/static", site.base_url()), "production-secret"),
                &InspectCommand::default(),
            )
            .await,
    );
    assert_eq!(denied.code, ErrorCode::NetworkPolicyDenied);

    let redirect = expect_error(
        DirectHttpExecutor::new(fixture_policy())
            .inspect(
                &snapshot(
                    format!("{}/redirect-private", site.base_url()),
                    "redirect-secret",
                ),
                &InspectCommand::default(),
            )
            .await,
    );
    assert_eq!(redirect.code, ErrorCode::NetworkPolicyDenied);
}

#[tokio::test]
async fn artifacts_and_http_state_remain_session_private() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(root.path(), 64, 16_384);
    let owner = SessionId::new();
    let stranger = SessionId::new();
    let record = store
        .put(
            &owner,
            &PageId::new(),
            "application/octet-stream",
            "bin",
            b"owner-only",
            64,
        )
        .await
        .unwrap();
    assert_eq!(
        store.get(&owner, &record.artifact_id).await.unwrap(),
        b"owner-only"
    );
    assert_eq!(
        store.get(&stranger, &record.artifact_id).await.unwrap_err(),
        ArtifactError::NotFound
    );

    let site = test_site::spawn().await;
    let executor = DirectHttpExecutor::new(NetworkPolicy {
        allow_loopback: true,
        ..NetworkPolicy::default()
    });
    let owner_state = snapshot(format!("{}/static", site.base_url()), "owner-agent");
    let stranger_state = snapshot(format!("{}/cookie-echo", site.base_url()), "stranger-agent");
    let owner_candidate = executor
        .inspect(&owner_state, &InspectCommand::default())
        .await
        .unwrap();
    let owner_delta = match owner_candidate {
        network_engine::HttpCandidate::Inspection { state, .. } => state,
        _ => panic!("expected inspection"),
    };
    assert!(!owner_delta.cookies.is_empty());
    let stranger_candidate = executor
        .inspect(&stranger_state, &InspectCommand::default())
        .await
        .unwrap();
    assert!(matches!(
        stranger_candidate,
        network_engine::HttpCandidate::Inspection {
            evidence: types::Evidence::Inspection { text, .. },
            ..
        } if text == "Cookies none"
    ));
}

#[tokio::test]
async fn failed_transfers_leave_no_artifacts_and_redact_request_secrets() {
    let site = test_site::spawn().await;
    let secret = "super-secret-capability";
    let executor = DirectHttpExecutor::new(fixture_policy());
    for route in ["oversized", "interrupted"] {
        let error = expect_error(
            executor
                .inspect(
                    &snapshot(format!("{}/{route}", site.base_url()), secret),
                    &InspectCommand::default(),
                )
                .await,
        );
        assert!(!format!("{error:?}").contains(secret));
    }

    let root = tempfile::tempdir().unwrap();
    let session = SessionId::new();
    let result = executor
        .download(
            &snapshot(format!("{}/interrupted", site.base_url()), secret),
            &DownloadUrlCommand {
                url: format!("{}/interrupted", site.base_url()),
                expected_content_type: None,
                max_bytes: 64,
            },
        )
        .await;
    let error = expect_error(result);
    let session_dir = root.path().join(session.0.to_string());
    assert!(!session_dir.exists());
    let outcome = CommandOutcome::Failed {
        command_id: CommandId::new(),
        error,
    };
    assert!(!format!("{outcome:?}").contains(secret));
    let journal_path = root.path().join("security-proof.jsonl");
    let journal = JsonlJournal::open(&journal_path).await.unwrap();
    let command_id = CommandId::new();
    journal
        .append(JournalRecord {
            sequence: 0,
            recorded_at: chrono::Utc::now(),
            command_id,
            phase: CommandPhase::Failed,
            envelope: None,
            outcome: Some(outcome),
        })
        .await
        .unwrap();
    let journal_bytes = tokio::fs::read(&journal_path).await.unwrap();
    assert!(!String::from_utf8(journal_bytes).unwrap().contains(secret));
}
