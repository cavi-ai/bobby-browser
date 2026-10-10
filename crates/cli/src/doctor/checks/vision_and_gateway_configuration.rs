//! vision and gateway configuration diagnostics and probes.
#[cfg(test)]
use super::bootstrap_capabilities::engine_composition_finding;
#[cfg(test)]
use super::firefox_endpoints::check_companion_port;
use super::DoctorContext;
use crate::doctor::{
    push_doctor_check, AppConfig, AuthCapabilities, AuthError, AuthProfileId, AuthStrategy,
    DoctorCheck, DoctorReport, DoctorStatus, Duration, Path, Result, Url, VisionConfig,
};
use auth_broker::AuthDriver;

use anyhow::Context;
pub(super) fn vision_and_gateway_configuration(
    context: &mut DoctorContext,
    report: &mut DoctorReport,
) -> Result<()> {
    let bootstrap_path = context.bootstrap_path.clone();
    let config = &context.config;
    if let Some(config) = &config {
        report.ok(
            "vision-timeout",
            format!("{} ms configured", config.vision.timeout_ms),
        );
        if let Some(check) = check_vision_config_dual(config) {
            push_doctor_check(report, check);
        }
        for check in check_vision_acp(config) {
            push_doctor_check(report, check);
        }
        if let Some(check) = check_vision_provider(&config.vision) {
            push_doctor_check(report, check);
        }
        if let Some(check) = check_vision_model(&config.vision) {
            push_doctor_check(report, check);
        }
        if let Some(check) = check_vision_readiness(&config.vision) {
            push_doctor_check(report, check);
        }
        if let Some(check) = check_vision_upstream_key(&config.vision) {
            push_doctor_check(report, check);
        }
        if let Some(check) = check_vision_propose_probe(config, bootstrap_path.as_deref()) {
            push_doctor_check(report, check);
        }

        if config.cdp.enabled {
            report.ok(
                "cdp-listen",
                format!(
                    "{}:{} · discovery http://{}:{}/json/version (Authorization: Bearer required)",
                    config.cdp.host, config.cdp.port, config.cdp.host, config.cdp.port
                ),
            );
        } else {
            report.ok(
                "cdp-listen",
                format!(
                    "disabled ([cdp].enabled = false); `bobby cdp` binds {}:{}",
                    config.cdp.host, config.cdp.port
                ),
            );
        }

        if !matches!(config.vision.backend, Some(config::VisionBackendKind::Acp)) {
            let registry = node_registry::NodeRegistry::from_config(config);
            if let Some((_name, node)) = registry.primary_http_vision_node() {
                match node.token_env.as_deref() {
                    Some(env_name) if !env_name.is_empty() => {
                        let available = std::env::var(env_name)
                            .ok()
                            .is_some_and(|value| !value.is_empty())
                            || bootstrap_path
                                .as_deref()
                                .and_then(|path| {
                                    crate::vision_token::resolve_vision_token(path).ok()
                                })
                                .is_some();
                        if available {
                            report.ok(
                                "vision-token",
                                "private vision credential is available".to_string(),
                            );
                        } else {
                            report.warn(
                                "vision-token",
                                "private vision credential is missing; run `bobby doctor --fix`"
                                    .to_string(),
                            );
                        }
                    }
                    _ => {
                        report.warn(
                            "vision-token",
                            "token_env unset; bobby will call the provider without a bearer"
                                .to_string(),
                        );
                    }
                }
            }
        }
    }

    Ok(())
}

pub(in crate::doctor) fn vision_endpoint_is_loopback(endpoint: &str) -> bool {
    Url::parse(endpoint).is_ok_and(|url| {
        matches!(
            url.host_str(),
            Some("localhost" | "127.0.0.1" | "::1" | "[::1]")
        )
    })
}

pub(crate) fn vision_endpoint_unreachable_detail(endpoint: &str) -> String {
    if vision_endpoint_is_loopback(endpoint) {
        format!(
            "nothing listens on {endpoint} and no vision provider is selected, so no runtime starts a proxy for it; select one with `bobby vision connect`"
        )
    } else {
        format!("{endpoint} not reachable (verify the external vision endpoint is running)")
    }
}

pub(in crate::doctor) fn vision_route_configured(config: &AppConfig) -> bool {
    let registry = node_registry::NodeRegistry::from_config(config);
    if !registry.is_empty() {
        return true;
    }
    matches!(
        config.vision.selected_backend(),
        Some(config::VisionBackendSelection::Acp { .. })
    )
}

pub(in crate::doctor) fn check_vision_config_dual(config: &AppConfig) -> Option<DoctorCheck> {
    if !node_registry::NodeRegistry::has_dual_vision_config(config) {
        return None;
    }
    Some(DoctorCheck {
        status: DoctorStatus::Warn,
        name: "vision-config-dual".to_string(),
        detail: "both [nodes] and [vision].endpoint_url are set; [nodes] wins and [vision] endpoint is ignored -- move it into [nodes.<name>] with kind = \"vision\"".to_string(),
    })
}

pub(in crate::doctor) fn check_vision_propose_probe(
    config: &AppConfig,
    bootstrap_path: Option<&Path>,
) -> Option<DoctorCheck> {
    if matches!(config.vision.backend, Some(config::VisionBackendKind::Acp)) {
        return None;
    }
    let registry = node_registry::NodeRegistry::from_config(config);
    let (_, node) = registry.primary_http_vision_node()?;
    let endpoint = node.endpoint_url.clone();
    if vision_endpoint_is_loopback(&endpoint) && config.vision.selected_provider().is_some() {
        // The runtime starts its own proxy on a port the OS picks, so the
        // configured port says nothing about it; provider-health reports its
        // calls once a runtime has made any.
        return Some(DoctorCheck {
            status: DoctorStatus::Ok,
            name: "vision-service".to_string(),
            detail: "each runtime starts its own vision proxy on a free loopback port".to_string(),
        });
    }
    if vision_endpoint_is_loopback(&endpoint) {
        let running = Url::parse(&endpoint)
            .ok()
            .and_then(|url| {
                url.socket_addrs(|| Some(url.port_or_known_default().unwrap_or(80)))
                    .ok()
            })
            .is_some_and(|addresses| {
                addresses.iter().any(|address| {
                    std::net::TcpStream::connect_timeout(address, Duration::from_millis(250))
                        .is_ok()
                })
            });
        if !running {
            return Some(DoctorCheck {
                status: DoctorStatus::Warn,
                name: "vision-service".to_string(),
                detail: vision_endpoint_unreachable_detail(&endpoint),
            });
        }
    }
    let bearer = node
        .token_env
        .as_ref()
        .and_then(|name| std::env::var(name).ok())
        .or_else(|| {
            bootstrap_path.and_then(|path| crate::vision_token::resolve_vision_token(path).ok())
        });
    let timeout = std::time::Duration::from_millis(node.timeout_ms.max(1_000));
    let probe = std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .ok()?;
        runtime.block_on(async move {
            let assist = intent_engine::HttpVisionAssist::new(endpoint, bearer, timeout).ok()?;
            let started = std::time::Instant::now();
            intent_engine::VisionAssist::propose(
                &assist,
                intent_engine::VisionProposeRequest {
                    purpose: "doctor probe".to_string(),
                    intent_kind: "locate".to_string(),
                    screenshot_png: DOCTOR_PROBE_PNG.to_vec(),
                    corpus_screenshot_png: None,
                    stuck: intent_engine::StuckKind::TargetMissing,
                    context: None,
                },
            )
            .await
            .ok()?;
            Some(started.elapsed())
        })
    })
    .join()
    .ok()
    .flatten();
    Some(vision_probe_verdict(probe, config.vision.propose_budget_ms))
}

pub(in crate::doctor) fn vision_probe_verdict(
    probe: Option<Duration>,
    budget_ms: Option<u64>,
) -> DoctorCheck {
    match probe {
        Some(elapsed) => {
            let elapsed_ms = elapsed.as_millis() as u64;
            match budget_ms {
                Some(budget_ms) if elapsed_ms > budget_ms => DoctorCheck {
                    status: DoctorStatus::Warn,
                    name: "vision-service".to_string(),
                    detail: format!(
                        "propose round-trip {elapsed_ms}ms exceeds the configured {budget_ms}ms budget ([vision].proposeBudgetMs)"
                    ),
                },
                Some(budget_ms) => DoctorCheck {
                    status: DoctorStatus::Ok,
                    name: "vision-service".to_string(),
                    detail: format!("propose round-trip ok in {elapsed_ms}ms (budget {budget_ms}ms)"),
                },
                None => DoctorCheck {
                    status: DoctorStatus::Ok,
                    name: "vision-service".to_string(),
                    detail: format!("propose round-trip ok in {elapsed_ms}ms"),
                },
            }
        }
        None => DoctorCheck {
            status: DoctorStatus::Warn,
            name: "vision-service".to_string(),
            detail:
                "propose round-trip failed (endpoint unreachable, auth rejected, or invalid reply)"
                    .to_string(),
        },
    }
}

pub(crate) fn check_vision_provider(vision: &VisionConfig) -> Option<DoctorCheck> {
    let name = vision.provider.as_deref()?.trim();
    if name.is_empty() {
        return None;
    }
    if vision.providers.contains_key(name) {
        Some(DoctorCheck {
            status: DoctorStatus::Ok,
            name: "vision-config".to_string(),
            detail: format!("provider \"{name}\" configured"),
        })
    } else {
        Some(DoctorCheck {
            status: DoctorStatus::Warn,
            name: "vision-config".to_string(),
            detail: format!("provider \"{name}\" is set but missing from [vision.providers]"),
        })
    }
}

pub(in crate::doctor) fn check_vision_model(vision: &VisionConfig) -> Option<DoctorCheck> {
    let (provider, profile) = vision.selected_provider()?;
    if provider.eq_ignore_ascii_case("mlx") {
        return Some(
            match crate::vision_readiness::cached_hugging_face_model(&profile.model) {
                Ok(true) => DoctorCheck {
                    status: DoctorStatus::Ok,
                    name: "vision-model".to_string(),
                    detail: format!("{} is cached and loadable", profile.model),
                },
                Ok(false) => DoctorCheck {
                    status: DoctorStatus::Warn,
                    name: "vision-model".to_string(),
                    detail: format!(
                        "{} is not cached; run `bobby doctor --fix --download-model`",
                        profile.model
                    ),
                },
                Err(error) => DoctorCheck {
                    status: DoctorStatus::Warn,
                    name: "vision-model".to_string(),
                    detail: error.to_string(),
                },
            },
        );
    }
    Some(DoctorCheck {
        status: DoctorStatus::Ok,
        name: "vision-model".to_string(),
        detail: format!("{} / {} is configured", provider, profile.model),
    })
}

pub(in crate::doctor) fn check_vision_readiness(vision: &VisionConfig) -> Option<DoctorCheck> {
    let (provider, profile) = vision.selected_provider()?;
    match crate::vision_readiness::check_provider_readiness(
        provider,
        profile,
        &crate::vision_readiness::ReadinessOptions {
            timeout: Duration::from_secs(3),
            allow_download: false,
            allow_start: false,
        },
    ) {
        Ok(crate::vision_readiness::ReadinessOutcome::Ready { provider, model }) => {
            Some(DoctorCheck {
                status: DoctorStatus::Ok,
                name: "vision-readiness".to_string(),
                detail: format!("{provider} / {model} is reachable"),
            })
        }
        Ok(crate::vision_readiness::ReadinessOutcome::NeedsAction { detail, .. }) => {
            Some(DoctorCheck {
                status: DoctorStatus::Fail,
                name: "vision-readiness".to_string(),
                detail: format!("{detail} · fix: bobby doctor --fix"),
            })
        }
        Err(error) => Some(DoctorCheck {
            status: DoctorStatus::Warn,
            name: "vision-readiness".to_string(),
            detail: error.to_string(),
        }),
    }
}

pub(crate) fn check_vision_upstream_key(vision: &VisionConfig) -> Option<DoctorCheck> {
    let (provider_name, profile) = vision.selected_provider()?;
    let api_key_env = profile.api_key_env.as_deref()?.trim();
    if api_key_env.is_empty() {
        return None;
    }
    match std::env::var(api_key_env) {
        Ok(value) if !value.is_empty() => Some(DoctorCheck {
            status: DoctorStatus::Ok,
            name: "vision-upstream-key".to_string(),
            detail: format!("{api_key_env} is set"),
        }),
        _ => Some(DoctorCheck {
            status: DoctorStatus::Warn,
            name: "vision-upstream-key".to_string(),
            detail: format!(
                "{api_key_env} is unset or empty (required for provider \"{provider_name}\")"
            ),
        }),
    }
}

pub(crate) fn vision_auth_discovery_check(
    configured: AuthStrategy,
    discovered: Result<AuthCapabilities, AuthError>,
) -> DoctorCheck {
    match discovered {
        Ok(capabilities) => {
            let advertised = capabilities
                .strategies()
                .map(|strategy| format!("{strategy:?}"))
                .collect::<Vec<_>>()
                .join(", ");
            DoctorCheck {
                status: if capabilities.supports(configured) {
                    DoctorStatus::Ok
                } else {
                    DoctorStatus::Warn
                },
                name: "vision-auth-path".into(),
                detail: format!(
                    "configured {configured:?}; harness advertises: {advertised}; {}",
                    if capabilities.supports(configured) {
                        "authentication path is supported"
                    } else {
                        "authentication is misconfigured"
                    }
                ),
            }
        }
        Err(error) => DoctorCheck {
            status: DoctorStatus::Warn,
            name: "vision-auth-path".into(),
            detail: format!("could not discover harness authentication methods: {error}"),
        },
    }
}

pub(crate) fn check_vision_acp(config: &AppConfig) -> Vec<DoctorCheck> {
    let Some(config::VisionBackendSelection::Acp { name, profile }) =
        config.vision.selected_backend()
    else {
        return Vec::new();
    };
    let registry = node_registry::NodeRegistry::from_config(config);
    let configured = registry
        .auth_strategy(name)
        .unwrap_or_else(|_| node_registry::vision_auth_strategy(profile.auth));
    let discovered = registry.auth_driver(name).and_then(|driver| {
        let profile = AuthProfileId::new(name.to_owned()).map_err(|error| {
            node_registry::NodeError::Unreachable {
                name: name.to_owned(),
                reason: error.to_string(),
            }
        })?;
        std::thread::scope(|scope| {
            scope
                .spawn(|| {
                    tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .expect("doctor auth runtime builds")
                        .block_on(
                            driver
                                .with_timeout(Duration::from_secs(5))
                                .discover(&profile),
                        )
                })
                .join()
                .unwrap_or_else(|_| Err(AuthError::Transport("discovery thread panicked".into())))
        })
        .map_err(|error| node_registry::NodeError::Unreachable {
            name: name.to_owned(),
            reason: error.to_string(),
        })
    });
    let (reachable, auth_check) = match discovered {
        Ok(capabilities) => (
            true,
            vision_auth_discovery_check(configured, Ok(capabilities)),
        ),
        Err(error) => (
            false,
            vision_auth_discovery_check(configured, Err(AuthError::Transport(error.to_string()))),
        ),
    };
    vec![
        DoctorCheck {
            status: DoctorStatus::Ok,
            name: "vision-routing".into(),
            detail: format!("ACP profile {name:?} selected"),
        },
        DoctorCheck {
            status: if reachable {
                DoctorStatus::Ok
            } else {
                DoctorStatus::Warn
            },
            name: "vision-acp-reachability".into(),
            detail: if reachable {
                format!("ACP harness {:?} initialized successfully", profile.command)
            } else {
                format!("ACP harness {:?} was not launchable", profile.command)
            },
        },
        auth_check,
    ]
}

pub(in crate::doctor) fn check_cdp_port(config: &config::CdpConfig) -> (DoctorCheck, CdpPortState) {
    let address = format!("{}:{}", config.host, config.port);
    let check = |status, detail| DoctorCheck {
        status,
        name: "cdp-port".to_string(),
        detail,
    };

    // Occupancy is decided by the TCP connect, not by the HTTP answer. A
    // service that holds the port without speaking HTTP still stops `bobby cdp`
    // from binding it, and judging by the HTTP probe alone reported exactly
    // that case as free.
    if !cdp_port_accepts_connections(&config.host, config.port) {
        return if config.enabled {
            (
                check(
                    DoctorStatus::Warn,
                    format!("{address} is not accepting connections; is `bobby cdp` running?"),
                ),
                CdpPortState::Free,
            )
        } else {
            (
                check(
                    DoctorStatus::Ok,
                    format!("{address} is free for `bobby cdp`"),
                ),
                CdpPortState::Free,
            )
        };
    }

    let discovery = format!("http://{address}/json/version");
    match probe_cdp_discovery(&discovery) {
        // Authenticated discovery refuses a request with no bearer, so 401 is
        // the gateway answering correctly.
        Ok(401) => (
            check(
                DoctorStatus::Ok,
                format!("{address} is serving authenticated CDP discovery"),
            ),
            CdpPortState::Serving,
        ),
        Ok(status) if config.enabled => (
            check(
                DoctorStatus::Warn,
                format!(
                    "{address} answered {status}; authenticated CDP answers 401 without a bearer, \
                     so another service may own the port"
                ),
            ),
            CdpPortState::Occupied,
        ),
        Ok(status) => (
            check(
                DoctorStatus::Warn,
                format!(
                    "{address} is already in use (answered {status}); `bobby cdp` cannot bind it \
                     -- free the port or run `bobby cdp --cdp-port <port>`"
                ),
            ),
            CdpPortState::Occupied,
        ),
        Err(error) if config.enabled => (
            check(
                DoctorStatus::Warn,
                format!("{discovery} did not answer ({error}); is `bobby cdp` running?"),
            ),
            CdpPortState::Occupied,
        ),
        Err(_) => (
            check(
                DoctorStatus::Warn,
                format!(
                    "{address} is already in use by a service that does not answer CDP discovery; \
                     `bobby cdp` cannot bind it -- free the port or run `bobby cdp --cdp-port <port>`"
                ),
            ),
            CdpPortState::Occupied,
        ),
    }
}

pub(in crate::doctor) fn cdp_port_accepts_connections(host: &str, port: u16) -> bool {
    use std::net::ToSocketAddrs;
    let Ok(addresses) = (host, port).to_socket_addrs() else {
        return false;
    };
    addresses.into_iter().any(|address| {
        std::net::TcpStream::connect_timeout(&address, Duration::from_millis(500)).is_ok()
    })
}

pub(in crate::doctor) fn probe_cdp_discovery(url: &str) -> Result<u16> {
    let url = url.to_owned();
    match std::thread::spawn(move || probe_http_status_blocking(&url)).join() {
        Ok(result) => result,
        Err(_) => anyhow::bail!("cdp discovery probe thread panicked"),
    }
}

pub(in crate::doctor) fn probe_http_status_blocking(url: &str) -> Result<u16> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .no_proxy()
        .build()
        .context("failed to build doctor HTTP client")?;
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?;
    Ok(response.status().as_u16())
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(in crate::doctor) enum CdpPortState {
    /// Authenticated CDP discovery answered; `bobby cdp` owns the port.
    Serving,
    /// Nothing is listening; `bobby cdp` can bind it.
    Free,
    /// Something that is not this gateway holds the port.
    Occupied,
}

const DOCTOR_PROBE_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

#[cfg(test)]
mod cdp_port_tests {
    use super::*;
    use std::io::{Read, Write};

    fn config_for(port: u16, enabled: bool) -> config::CdpConfig {
        config::CdpConfig {
            enabled,
            host: "127.0.0.1".to_string(),
            port,
            auto_session: true,
        }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port()
    }

    #[test]
    fn a_free_port_is_reported_as_available_for_bobby_cdp() {
        let (check, state) = check_cdp_port(&config_for(free_port(), false));
        assert_eq!(check.status, DoctorStatus::Ok);
        assert!(state == CdpPortState::Free);
        assert!(check.detail.contains("is free"), "{}", check.detail);
    }

    #[test]
    fn an_occupied_port_is_named_before_bobby_cdp_fails_to_bind_on_it() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        // Read the request before answering: closing a socket with unread bytes
        // can reset the connection before the client reads the response.
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut chunk = [0_u8; 256];
                let read = stream.read(&mut chunk).unwrap();
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..read]);
            }
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
            stream.flush().unwrap();
        });

        let (check, state) = check_cdp_port(&config_for(port, false));
        assert_eq!(check.status, DoctorStatus::Warn, "{}", check.detail);
        assert!(state == CdpPortState::Occupied);
        assert!(check.detail.contains("already in use"), "{}", check.detail);
        assert!(check.detail.contains("--cdp-port"), "{}", check.detail);
        server.join().unwrap();
    }

    /// The failure that hid behind the HTTP probe: something holds the port but
    /// never speaks HTTP. It still blocks the bind, so it cannot read as free.
    #[test]
    fn a_silent_occupier_is_reported_as_in_use_not_free() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = std::thread::spawn(move || {
            let _accepted = listener.accept().map(|(stream, _)| {
                std::thread::sleep(std::time::Duration::from_millis(50));
                drop(stream);
            });
        });

        let (check, state) = check_cdp_port(&config_for(port, false));
        assert_eq!(check.status, DoctorStatus::Warn, "{}", check.detail);
        assert!(state == CdpPortState::Occupied);
        assert!(check.detail.contains("already in use"), "{}", check.detail);
        assert!(check.detail.contains("--cdp-port"), "{}", check.detail);
        let _ = server.join();
    }

    #[test]
    fn an_enabled_listener_that_is_not_answering_is_a_warning() {
        let (check, state) = check_cdp_port(&config_for(free_port(), true));
        assert_eq!(check.status, DoctorStatus::Warn);
        assert!(state == CdpPortState::Free);
        assert!(check.detail.contains("bobby cdp"), "{}", check.detail);
    }

    #[test]
    fn the_companion_port_check_never_depends_on_the_configured_port() {
        let held = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let check = check_companion_port(held.local_addr().unwrap());
        assert_eq!(check.status, DoctorStatus::Ok, "{}", check.detail);
        assert_eq!(check.name, "companion-port");
        assert!(check.detail.contains("is not used"), "{}", check.detail);
        let check = check_companion_port("127.0.0.1:0".parse().unwrap());
        assert_eq!(
            check.detail,
            "the runtime binds a free loopback port on 127.0.0.1 at every start"
        );
    }

    #[test]
    fn vision_probe_verdict_warns_when_round_trip_exceeds_the_budget() {
        let check = vision_probe_verdict(Some(Duration::from_millis(900)), Some(500));
        assert_eq!(check.status, DoctorStatus::Warn);
        assert!(check
            .detail
            .contains("900ms exceeds the configured 500ms budget"));
    }

    #[test]
    fn vision_probe_verdict_ok_names_the_budget_it_was_measured_against() {
        let check = vision_probe_verdict(Some(Duration::from_millis(120)), Some(500));
        assert_eq!(check.status, DoctorStatus::Ok);
        assert!(check.detail.contains("ok in 120ms (budget 500ms)"));
    }

    #[test]
    fn vision_probe_verdict_without_a_budget_keeps_the_plain_detail() {
        let check = vision_probe_verdict(Some(Duration::from_millis(120)), None);
        assert_eq!(check.status, DoctorStatus::Ok);
        assert_eq!(check.detail, "propose round-trip ok in 120ms");
    }

    #[test]
    fn vision_probe_verdict_warns_on_a_failed_round_trip() {
        let check = vision_probe_verdict(None, Some(500));
        assert_eq!(check.status, DoctorStatus::Warn);
        assert!(check.detail.contains("propose round-trip failed"));
    }

    /// With a provider selected the runtime runs its own proxy on a free port,
    /// so whatever holds the configured port (here: a stranger) is not probed.
    #[test]
    fn a_runtime_managed_vision_proxy_is_not_probed_on_the_configured_port() {
        let stranger = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut config = AppConfig::default();
        config.vision.endpoint_url =
            Some(format!("http://{}/vision", stranger.local_addr().unwrap()));
        config.vision.provider = Some("ollama".into());
        config.vision.providers.insert(
            "ollama".into(),
            config::VisionProviderConfig {
                base_url: "http://127.0.0.1:11434/v1".into(),
                model: "llava:7b".into(),
                api_key_env: None,
            },
        );
        let check = check_vision_propose_probe(&config, None).unwrap();
        assert_eq!(check.status, DoctorStatus::Ok, "{}", check.detail);
        assert!(
            check.detail.contains("its own vision proxy"),
            "{}",
            check.detail
        );
    }

    fn profile_owned() -> anyhow::Error {
        anyhow::Error::new(firefox_companion::selection::ProfileOwned {
            profile: "/profiles/enrolled".into(),
        })
        .context("compose browser workers")
    }

    /// With no provider selected no runtime starts a proxy, so a loopback URL
    /// that nothing answers is a vision route that cannot work.
    #[test]
    fn an_unanswered_loopback_url_without_a_provider_warns() {
        let vacant = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = vacant.local_addr().unwrap();
        drop(vacant);
        let mut config = AppConfig::default();
        config.vision.endpoint_url = Some(format!("http://{address}/vision"));
        let check = check_vision_propose_probe(&config, None).unwrap();
        assert_eq!(check.status, DoctorStatus::Warn, "{}", check.detail);
        assert!(
            check.detail.contains("no vision provider is selected"),
            "{}",
            check.detail
        );
    }

    #[test]
    fn a_profile_held_by_the_scopes_running_runtime_is_not_a_failure() {
        let (status, name, detail) = engine_composition_finding(
            &profile_owned(),
            Some("http://127.0.0.1:55371".into()),
            false,
        );
        assert_eq!(status, DoctorStatus::Ok, "{detail}");
        assert_eq!(name, "engine-satisfiability");
        assert!(detail.contains("http://127.0.0.1:55371"), "{detail}");
    }

    #[test]
    fn a_held_profile_with_no_running_runtime_still_fails() {
        let (status, name, _) = engine_composition_finding(&profile_owned(), None, false);
        assert_eq!(
            (status, name),
            (DoctorStatus::Fail, "engine-satisfiability")
        );
    }

    #[test]
    fn other_composition_errors_fail_even_with_a_running_runtime() {
        let (status, name, detail) = engine_composition_finding(
            &anyhow::anyhow!("chromium executable not found"),
            Some("http://127.0.0.1:55371".into()),
            false,
        );
        assert_eq!(status, DoctorStatus::Fail, "{detail}");
        assert_eq!(name, "engine-satisfiability");
    }
}
