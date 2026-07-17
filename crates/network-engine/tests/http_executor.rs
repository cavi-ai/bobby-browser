use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use network_engine::state::{HttpCookie, HttpCookiePartitionKey, HttpStateSnapshot};
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

fn expect_error(result: Result<HttpCandidate, types::CommandError>) -> types::CommandError {
    match result {
        Ok(_) => panic!("expected error"),
        Err(error) => error,
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
        let HttpCandidate::Inspection {
            evidence,
            meta,
            state,
        } = candidate
        else {
            panic!("{route}")
        };
        let Evidence::Inspection { text, .. } = evidence else {
            panic!("{route}")
        };
        let expected = if route == "latin1" {
            "Latin café fixture"
        } else if route == "redirect-static" {
            "Static Fixture café fixture"
        } else {
            "Compressed compressed fixture"
        };
        assert_eq!(text, expected, "{route}");
        if route == "redirect-static" {
            assert_eq!(meta.redirect_chain.len(), 2);
            assert_eq!(meta.final_url, format!("{}/static", site.base_url()));
            assert_eq!(
                state
                    .cache_validators
                    .get(&meta.final_url)
                    .map(String::as_str),
                Some("\"fixture-v1\"")
            );
        }
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
            assert_eq!(
                meta.sha256,
                "c0613f7c18f7f41e5720bb3d95b6f6411e8a8b2f3b08d1ad011760069f3949ed"
            );
        }
        _ => panic!("unexpected candidate"),
    }
}

#[tokio::test]
async fn enforces_shared_peak_concurrency() {
    let site = spawn().await;
    let executor = Arc::new(DirectHttpExecutor::new(NetworkPolicy {
        max_concurrent_requests: 1,
        allow_loopback: true,
        request_timeout_ms: 1_000,
        ..policy()
    }));
    let started = Instant::now();
    let a = {
        let executor = executor.clone();
        let state = snapshot(format!("{}/slow", site.base_url()));
        tokio::spawn(async move { executor.inspect(&state, &InspectCommand::default()).await })
    };
    let b = {
        let executor = executor.clone();
        let state = snapshot(format!("{}/slow", site.base_url()));
        tokio::spawn(async move { executor.inspect(&state, &InspectCommand::default()).await })
    };
    a.await.unwrap().unwrap();
    b.await.unwrap().unwrap();
    assert_eq!(site.peak_requests(), 1);
    assert!(started.elapsed() >= Duration::from_millis(180));
}

#[tokio::test]
async fn uses_one_end_to_end_redirect_deadline() {
    let site = spawn().await;
    let error = DirectHttpExecutor::new(NetworkPolicy {
        allow_loopback: true,
        request_timeout_ms: 70,
        ..policy()
    })
    .inspect(
        &snapshot(format!("{}/slow-redirect", site.base_url())),
        &InspectCommand::default(),
    )
    .await;
    let error = expect_error(error);
    assert_eq!(error.code, types::ErrorCode::DeadlineExceeded);
}

#[tokio::test]
async fn sanitizes_download_filenames_and_parses_filename_star() {
    let site = spawn().await;
    for (route, expected) in [
        ("download-traversal", "download.bin"),
        ("download-control", "download.bin"),
        ("download-star", "café.txt"),
    ] {
        let command = DownloadUrlCommand {
            url: format!("{}/{route}", site.base_url()),
            expected_content_type: None,
            max_bytes: 64,
        };
        let candidate = DirectHttpExecutor::new(policy())
            .download(&snapshot(site.base_url()), &command)
            .await
            .unwrap();
        let HttpCandidate::Download { filename, .. } = candidate else {
            panic!("{route}")
        };
        assert_eq!(filename, expected, "{route}");
    }
}

#[tokio::test]
async fn rejects_exact_download_header_redirect_and_compressed_limits() {
    let site = spawn().await;
    let command = DownloadUrlCommand {
        url: format!("{}/download", site.base_url()),
        expected_content_type: None,
        max_bytes: 19,
    };
    assert_eq!(
        expect_error(
            DirectHttpExecutor::new(policy())
                .download(&snapshot(site.base_url()), &command)
                .await
        )
        .code,
        types::ErrorCode::HttpResponseTooLarge
    );
    let header_error = expect_error(
        DirectHttpExecutor::new(NetworkPolicy {
            allow_loopback: true,
            max_header_bytes: 8,
            ..policy()
        })
        .inspect(
            &snapshot(format!("{}/static", site.base_url())),
            &InspectCommand::default(),
        )
        .await,
    );
    assert_eq!(header_error.code, types::ErrorCode::HttpResponseTooLarge);
    let redirect_error = expect_error(
        DirectHttpExecutor::new(NetworkPolicy {
            allow_loopback: true,
            max_redirects: 0,
            ..policy()
        })
        .inspect(
            &snapshot(format!("{}/redirect-static", site.base_url())),
            &InspectCommand::default(),
        )
        .await,
    );
    assert_eq!(redirect_error.code, types::ErrorCode::HttpTransferFailed);
    let bomb_error = expect_error(
        DirectHttpExecutor::new(NetworkPolicy {
            allow_loopback: true,
            max_body_bytes: 100,
            ..policy()
        })
        .inspect(
            &snapshot(format!("{}/gzip-bomb", site.base_url())),
            &InspectCommand::default(),
        )
        .await,
    );
    assert_eq!(bomb_error.code, types::ErrorCode::HttpResponseTooLarge);
}

#[tokio::test]
async fn applies_cookie_replacement_and_filters_unsafe_scope() {
    let site = spawn().await;
    let candidate = DirectHttpExecutor::new(policy())
        .inspect(
            &snapshot(format!("{}/cookie-start", site.base_url())),
            &InspectCommand::default(),
        )
        .await
        .unwrap();
    let HttpCandidate::Inspection {
        evidence: Evidence::Inspection { text, .. },
        state,
        ..
    } = candidate
    else {
        panic!("inspection")
    };
    assert_eq!(text, "Cookies session=new");
    assert_eq!(state.cookies.len(), 1);
    assert_eq!(state.cookies[0].value, "new");

    let mut scoped = snapshot(format!("{}/cookie-echo", site.base_url()));
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    scoped.cookies = vec![
        test_cookie("host", "secret", "localhost", true, false, None),
        test_cookie("secure", "secret", "127.0.0.1", true, true, None),
        test_cookie(
            "expired",
            "secret",
            "127.0.0.1",
            true,
            false,
            Some(now - 1.0),
        ),
        HttpCookie {
            partition_key: Some(HttpCookiePartitionKey {
                top_level_site: "https://elsewhere.example".into(),
                has_cross_site_ancestor: false,
            }),
            ..test_cookie("partitioned", "secret", "127.0.0.1", true, false, None)
        },
    ];
    let candidate = DirectHttpExecutor::new(policy())
        .inspect(&scoped, &InspectCommand::default())
        .await
        .unwrap();
    let HttpCandidate::Inspection {
        evidence: Evidence::Inspection { text, .. },
        ..
    } = candidate
    else {
        panic!("inspection")
    };
    assert_eq!(text, "Cookies none");
}

fn test_cookie(
    name: &str,
    value: &str,
    domain: &str,
    host_only: bool,
    secure: bool,
    expires_unix: Option<f64>,
) -> HttpCookie {
    HttpCookie {
        name: name.into(),
        value: value.into(),
        domain: domain.into(),
        host_only,
        path: "/".into(),
        secure,
        http_only: false,
        same_site: None,
        expires_unix,
        priority: None,
        source_scheme: None,
        source_port: None,
        partition_key: None,
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

#[tokio::test]
async fn rejects_all_response_domain_cookies_before_redirect_forwarding() {
    let site = spawn().await;
    for route in ["cookie-domain-public", "cookie-domain-super"] {
        let result = DirectHttpExecutor::new(policy())
            .inspect(
                &snapshot(format!("{}/{route}", site.base_url())),
                &InspectCommand::default(),
            )
            .await;
        assert_eq!(
            expect_error(result).code,
            types::ErrorCode::HttpEquivalenceUnproven,
            "{route}"
        );
    }
}

#[tokio::test]
async fn canonical_cookie_identity_replaces_across_host_only_transition() {
    let site = spawn().await;
    let mut state = snapshot(format!("{}/cookie-echo", site.base_url()));
    state.cookies = vec![
        test_cookie("identity", "old", "127.0.0.1", true, false, None),
        test_cookie("identity", "new", "127.0.0.1", false, false, None),
    ];
    let candidate = DirectHttpExecutor::new(policy())
        .inspect(&state, &InspectCommand::default())
        .await
        .unwrap();
    let HttpCandidate::Inspection {
        evidence: Evidence::Inspection { text, .. },
        ..
    } = candidate
    else {
        panic!("inspection")
    };
    assert_eq!(text, "Cookies identity=new");
}

#[tokio::test]
async fn cached_and_bodyless_inspections_never_return_empty_success() {
    let site = spawn().await;
    let url = format!("{}/validator", site.base_url());
    let mut cached = snapshot(url.clone());
    cached.cache_validators.insert(url, "\"fixture-v1\"".into());
    for state in [cached, snapshot(format!("{}/no-content", site.base_url()))] {
        let candidate = DirectHttpExecutor::new(policy())
            .inspect(&state, &InspectCommand::default())
            .await
            .unwrap();
        assert!(matches!(
            candidate,
            HttpCandidate::FallbackRequired(ExecutionReason::StateConflict)
        ));
    }
}

#[tokio::test]
async fn rejects_nonportable_download_device_and_drive_names() {
    let site = spawn().await;
    for route in [
        "download-colon",
        "download-con",
        "download-lpt",
        "download-trailing",
    ] {
        let command = DownloadUrlCommand {
            url: format!("{}/{route}", site.base_url()),
            expected_content_type: None,
            max_bytes: 64,
        };
        let candidate = DirectHttpExecutor::new(policy())
            .download(&snapshot(site.base_url()), &command)
            .await
            .unwrap();
        let HttpCandidate::Download { filename, .. } = candidate else {
            panic!("download")
        };
        assert_eq!(filename, "download.bin", "{route}");
    }
}
