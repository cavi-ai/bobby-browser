use std::collections::BTreeMap;

use network_engine::state::HttpStateSnapshot;
use network_engine::{DirectHttpExecutor, HttpCandidate, NetworkPolicy};
use test_site::spawn;
use types::{DownloadUrlCommand, Evidence, ExecutionReason, InspectCommand};

fn snapshot(url: String) -> HttpStateSnapshot {
    HttpStateSnapshot {
        version: 7,
        current_url: url,
        cookies: Vec::new(),
        cache_validators: BTreeMap::new(),
        user_agent: "fixture-agent secret-token".into(),
        language: "en-US".into(),
    }
}

fn policy() -> NetworkPolicy {
    NetworkPolicy {
        allow_loopback: true,
        max_body_bytes: 1024,
        max_download_bytes: 64,
        ..NetworkPolicy::default()
    }
}

#[tokio::test]
async fn inspects_static_selector_and_reports_state() {
    let site = spawn().await;
    let candidate = DirectHttpExecutor::new(policy())
        .inspect(
            &snapshot(format!("{}/static", site.base_url())),
            &InspectCommand {
                selector: Some("#message".into()),
                target: None,
                include_html: true,
            },
        )
        .await
        .expect("inspect");
    match candidate {
        HttpCandidate::Inspection {
            evidence: Evidence::Inspection {
                text, html, title, ..
            },
            state,
            meta,
        } => {
            assert_eq!(text, "café fixture");
            assert!(html.expect("html").contains("café fixture"));
            assert_eq!(title, "Static Fixture");
            assert_eq!(meta.redirect_chain.len(), 1);
            assert!(!state.cookies.is_empty());
        }
        _ => panic!("unexpected candidate"),
    }
}

#[tokio::test]
async fn follows_redirects_and_decodes_compression_and_latin1() {
    let site = spawn().await;
    for route in ["redirect-static", "gzip", "brotli", "latin1"] {
        let candidate = DirectHttpExecutor::new(policy())
            .inspect(
                &snapshot(format!("{}/{route}", site.base_url())),
                &InspectCommand::default(),
            )
            .await
            .expect("inspect");
        assert!(
            matches!(candidate, HttpCandidate::Inspection { .. }),
            "{route}"
        );
    }
}

#[tokio::test]
async fn downloads_with_metadata_hash_and_exact_bound() {
    let site = spawn().await;
    let command = DownloadUrlCommand {
        url: format!("{}/download", site.base_url()),
        expected_content_type: Some("application/octet-stream".into()),
        max_bytes: 20,
    };
    let candidate = DirectHttpExecutor::new(policy())
        .download(&snapshot(site.base_url()), &command)
        .await
        .expect("download");
    match candidate {
        HttpCandidate::Download {
            bytes,
            filename,
            media_type,
            meta,
            ..
        } => {
            assert_eq!(bytes, b"workflow-download-v1");
            assert_eq!(filename, "workflow-fixture.bin");
            assert_eq!(media_type, "application/octet-stream");
            assert_eq!(meta.bytes, 20);
            assert_eq!(meta.sha256.len(), 64);
        }
        _ => panic!("unexpected candidate"),
    }
}

#[tokio::test]
async fn javascript_shell_requires_fallback() {
    let site = spawn().await;
    let candidate = DirectHttpExecutor::new(policy())
        .inspect(
            &snapshot(format!("{}/js-shell", site.base_url())),
            &InspectCommand::default(),
        )
        .await
        .expect("inspect");
    assert!(matches!(
        candidate,
        HttpCandidate::FallbackRequired(ExecutionReason::JavascriptRequired)
    ));
}

#[tokio::test]
async fn rejects_decoded_body_over_exact_limit() {
    let site = spawn().await;
    let error = match DirectHttpExecutor::new(NetworkPolicy {
        allow_loopback: true,
        max_body_bytes: 15,
        ..NetworkPolicy::default()
    })
    .inspect(
        &snapshot(format!("{}/oversized", site.base_url())),
        &InspectCommand::default(),
    )
    .await
    {
        Ok(_) => panic!("oversized response was accepted"),
        Err(error) => error,
    };
    assert!(!error.message.contains("secret-token"));
}

#[tokio::test]
async fn denies_private_redirect_destination_before_following_it() {
    let site = spawn().await;
    let error = match DirectHttpExecutor::new(policy())
        .inspect(
            &snapshot(format!("{}/redirect-private", site.base_url())),
            &InspectCommand::default(),
        )
        .await
    {
        Ok(_) => panic!("private redirect was accepted"),
        Err(error) => error,
    };
    assert_eq!(error.code, types::ErrorCode::NetworkPolicyDenied);
}

#[tokio::test]
async fn misleading_content_type_falls_back() {
    let site = spawn().await;
    let candidate = DirectHttpExecutor::new(policy())
        .inspect(
            &snapshot(format!("{}/misleading", site.base_url())),
            &InspectCommand::default(),
        )
        .await
        .expect("inspect");
    assert!(matches!(
        candidate,
        HttpCandidate::FallbackRequired(ExecutionReason::UnsupportedContentType)
    ));
}

#[tokio::test]
async fn interrupted_transfer_is_rejected_without_leaking_request_headers() {
    let site = spawn().await;
    let error = match DirectHttpExecutor::new(policy())
        .inspect(
            &snapshot(format!("{}/interrupted", site.base_url())),
            &InspectCommand::default(),
        )
        .await
    {
        Ok(_) => panic!("interrupted transfer was accepted"),
        Err(error) => error,
    };
    assert_eq!(error.code, types::ErrorCode::HttpTransferFailed);
    assert!(!error.message.contains("secret-token"));
}
