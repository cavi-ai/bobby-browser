//! `bobby doctor` checks and report rendering.

use std::{
    io::{IsTerminal, Read, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use auth_broker::{AuthCapabilities, AuthDriver, AuthError, AuthProfileId, AuthStrategy};
use config::{AppConfig, VisionConfig};
use url::Url;

use crate::bootstrap_local;
use crate::onboarding;
use crate::{
    compose_worker_factory, default_context_dir, resolve_bootstrap_path, resolve_browser_selection,
    resolve_config_path, SelectionSource,
};

pub(crate) fn repair_vision_config(path: &Path) -> Result<bool> {
    let before = std::fs::read(path)
        .with_context(|| format!("failed to read config from {}", path.display()))?;
    let mut config = AppConfig::load(path)
        .with_context(|| format!("failed to load config from {}", path.display()))?;
    config::ensure_loopback_vision_defaults(&mut config.vision);
    let Some((provider_name, profile)) = config
        .vision
        .selected_provider()
        .map(|(name, profile)| (name.to_string(), profile.clone()))
    else {
        return Ok(false);
    };
    let endpoint_url = config
        .vision
        .endpoint_url
        .as_deref()
        .context("vision endpoint missing after normalization")?;
    let token_env = config
        .vision
        .token_env
        .as_deref()
        .context("vision token env missing after normalization")?;
    config::upsert_vision_platform(path, endpoint_url, token_env, &provider_name, &profile)
        .map_err(|error| anyhow::anyhow!("{error}"))?;
    Ok(std::fs::read(path)? != before)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoctorFixStatus {
    Fixed,
    Noop,
    NeedsAction,
    Failed,
}

#[derive(Debug, Clone)]
pub(crate) struct DoctorFixAction {
    pub(crate) status: DoctorFixStatus,
    pub(crate) name: String,
    pub(crate) detail: String,
}

pub(crate) struct DoctorFixOptions {
    pub(crate) config: Option<PathBuf>,
    pub(crate) bootstrap_env: Option<PathBuf>,
    pub(crate) check_health: bool,
    pub(crate) download_model: bool,
}

pub(crate) struct DoctorFixReport {
    pub(crate) actions: Vec<DoctorFixAction>,
    pub(crate) post_fix: DoctorReport,
}

impl DoctorFixReport {
    pub(crate) fn render(&self) {
        let color = DoctorColorMode::Auto.enabled();
        for action in &self.actions {
            let label = match action.status {
                DoctorFixStatus::Fixed => "fixed",
                DoctorFixStatus::Noop => "unchanged",
                DoctorFixStatus::NeedsAction => "action",
                DoctorFixStatus::Failed => "failed",
            };
            let ansi = match action.status {
                DoctorFixStatus::Fixed => "\x1b[36m",
                DoctorFixStatus::Noop => "\x1b[32m",
                DoctorFixStatus::NeedsAction => "\x1b[33m",
                DoctorFixStatus::Failed => "\x1b[31m",
            };
            if color {
                eprintln!("[{ansi}{label}\x1b[0m] {}: {}", action.name, action.detail);
            } else {
                eprintln!("[{label}] {}: {}", action.name, action.detail);
            }
        }
        self.post_fix.render();
    }
}

pub(crate) fn run_doctor_fix(options: DoctorFixOptions) -> Result<DoctorFixReport> {
    let config_path = resolve_config_path(options.config.clone());
    let bootstrap_path = resolve_bootstrap_path(options.bootstrap_env.clone())?;
    let mut actions = Vec::new();

    if bootstrap_path.exists() {
        match bootstrap_local::ensure_unrestricted_bootstrap(&bootstrap_path) {
            Ok(heal) => actions.push(DoctorFixAction {
                status: if heal.changed() {
                    DoctorFixStatus::Fixed
                } else {
                    DoctorFixStatus::Noop
                },
                name: "bootstrap".to_string(),
                detail: if heal.changed() {
                    "healed the existing unrestricted capability set".to_string()
                } else {
                    "existing bootstrap already uses current capabilities".to_string()
                },
            }),
            Err(error) => actions.push(DoctorFixAction {
                status: DoctorFixStatus::Failed,
                name: "bootstrap".to_string(),
                detail: error.to_string(),
            }),
        }
    } else {
        match bootstrap_local::generate_bootstrap(chrono::Duration::days(
            bootstrap_local::DEFAULT_TTL_DAYS,
        ))
        .and_then(|material| {
            bootstrap_local::write_bootstrap_env(&bootstrap_path, &material, false)
        }) {
            Ok(()) => actions.push(DoctorFixAction {
                status: DoctorFixStatus::Fixed,
                name: "bootstrap".to_string(),
                detail: format!("generated agent credential at {}", bootstrap_path.display()),
            }),
            Err(error) => actions.push(DoctorFixAction {
                status: DoctorFixStatus::Failed,
                name: "bootstrap".to_string(),
                detail: error.to_string(),
            }),
        }
    }

    let vision_token_existed = crate::vision_token::managed_vision_token_path(&bootstrap_path)
        .exists()
        || std::env::var("BOBBY_VISION_TOKEN")
            .ok()
            .is_some_and(|value| !value.trim().is_empty());
    match crate::vision_token::ensure_managed_vision_token(&bootstrap_path) {
        Ok(_) => actions.push(DoctorFixAction {
            status: if vision_token_existed {
                DoctorFixStatus::Noop
            } else {
                DoctorFixStatus::Fixed
            },
            name: "vision-token".to_string(),
            detail: format!(
                "private vision credential is available at {}",
                crate::vision_token::managed_vision_token_path(&bootstrap_path).display()
            ),
        }),
        Err(error) => actions.push(DoctorFixAction {
            status: DoctorFixStatus::Failed,
            name: "vision-token".to_string(),
            detail: error.to_string(),
        }),
    }

    if config_path.exists() {
        match repair_vision_config(&config_path) {
            Ok(changed) => actions.push(DoctorFixAction {
                status: if changed {
                    DoctorFixStatus::Fixed
                } else {
                    DoctorFixStatus::Noop
                },
                name: "vision-config".to_string(),
                detail: if changed {
                    "normalized the selected provider into the canonical vision node".to_string()
                } else {
                    "selected vision provider is already canonical".to_string()
                },
            }),
            Err(error) => actions.push(DoctorFixAction {
                status: DoctorFixStatus::Failed,
                name: "vision-config".to_string(),
                detail: error.to_string(),
            }),
        }

        if let Ok(config) = AppConfig::load(&config_path) {
            if let Some((provider_name, profile)) = config.vision.selected_provider() {
                let readiness = crate::vision_readiness::check_provider_readiness(
                    provider_name,
                    profile,
                    &crate::vision_readiness::ReadinessOptions {
                        timeout: Duration::from_secs(45),
                        allow_download: options.download_model,
                    },
                );
                match readiness {
                    Ok(crate::vision_readiness::ReadinessOutcome::Ready { provider, model }) => {
                        actions.push(DoctorFixAction {
                            status: DoctorFixStatus::Fixed,
                            name: "vision-readiness".to_string(),
                            detail: format!("loaded and readiness-tested {provider} model {model}"),
                        });
                    }
                    Ok(outcome @ crate::vision_readiness::ReadinessOutcome::NeedsAction { .. }) => {
                        actions.push(DoctorFixAction {
                            status: DoctorFixStatus::NeedsAction,
                            name: "vision-readiness".to_string(),
                            detail: outcome.detail().to_string(),
                        })
                    }
                    Err(error) => actions.push(DoctorFixAction {
                        status: DoctorFixStatus::Failed,
                        name: "vision-readiness".to_string(),
                        detail: error.to_string(),
                    }),
                }
            }
        }
    }

    let post_fix = run_doctor(
        Some(config_path),
        Some(bootstrap_path),
        options.check_health,
    )?;
    Ok(DoctorFixReport { actions, post_fix })
}

/// `bobby init` issues a 30-day credential, so a week is enough runway to
/// renew before the gateway starts failing closed.
pub(crate) const BOOTSTRAP_EXPIRY_WARN_DAYS: i64 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DoctorStatus {
    Ok,
    Warn,
    Fail,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum DoctorColorMode {
    Auto,
    Always,
    Never,
}

impl DoctorColorMode {
    fn enabled(self) -> bool {
        match self {
            Self::Always => true,
            Self::Never => false,
            Self::Auto => std::env::var_os("NO_COLOR").is_none() && std::io::stderr().is_terminal(),
        }
    }
}

impl DoctorStatus {
    fn label(self) -> &'static str {
        match self {
            DoctorStatus::Ok => "ok",
            DoctorStatus::Warn => "warn",
            DoctorStatus::Fail => "fail",
        }
    }

    fn ansi(self) -> &'static str {
        match self {
            DoctorStatus::Ok => "\x1b[32m",
            DoctorStatus::Warn => "\x1b[33m",
            DoctorStatus::Fail => "\x1b[31m",
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct DoctorCheck {
    pub(crate) status: DoctorStatus,
    pub(crate) name: String,
    pub(crate) detail: String,
}

/// Structured outcome of a `bobby doctor` run: every check in order, so the
/// CLI can render it and tests can assert on it without capturing stderr.
#[derive(Debug, Default)]
pub(crate) struct DoctorReport {
    pub(crate) checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    pub(crate) fn record(&mut self, status: DoctorStatus, name: &str, detail: String) {
        self.checks.push(DoctorCheck {
            status,
            name: name.to_string(),
            detail,
        });
    }

    pub(crate) fn ok(&mut self, name: &str, detail: String) {
        self.record(DoctorStatus::Ok, name, detail);
    }

    pub(crate) fn warn(&mut self, name: &str, detail: String) {
        self.record(DoctorStatus::Warn, name, detail);
    }

    pub(crate) fn fail(&mut self, name: &str, detail: String) {
        self.record(DoctorStatus::Fail, name, detail);
    }

    pub(crate) fn failures(&self) -> usize {
        self.checks
            .iter()
            .filter(|check| check.status == DoctorStatus::Fail)
            .count()
    }

    pub(crate) fn warnings(&self) -> usize {
        self.checks
            .iter()
            .filter(|check| check.status == DoctorStatus::Warn)
            .count()
    }

    #[cfg(test)]
    pub(crate) fn check(&self, name: &str) -> Option<&DoctorCheck> {
        self.checks.iter().find(|check| check.name == name)
    }

    pub(crate) fn render_to(
        &self,
        writer: &mut dyn Write,
        color_mode: DoctorColorMode,
    ) -> std::io::Result<()> {
        let color = color_mode.enabled();
        for check in &self.checks {
            if color {
                writeln!(
                    writer,
                    "[{}{}\x1b[0m] {}: {}",
                    check.status.ansi(),
                    check.status.label(),
                    check.name,
                    check.detail
                )?;
            } else {
                writeln!(
                    writer,
                    "[{}] {}: {}",
                    check.status.label(),
                    check.name,
                    check.detail
                )?;
            }
        }
        if color {
            let failure_ansi = if self.failures() == 0 {
                "\x1b[32m"
            } else {
                "\x1b[31m"
            };
            let warning_ansi = if self.warnings() == 0 {
                "\x1b[32m"
            } else {
                "\x1b[33m"
            };
            writeln!(
                writer,
                "\x1b[1mdoctor:\x1b[0m {failure_ansi}{} failure(s)\x1b[0m, {warning_ansi}{} warning(s)\x1b[0m",
                self.failures(),
                self.warnings()
            )
        } else {
            writeln!(
                writer,
                "doctor: {} failure(s), {} warning(s)",
                self.failures(),
                self.warnings()
            )
        }
    }

    pub(crate) fn render(&self) {
        let _ = self.render_to(&mut std::io::stderr().lock(), DoctorColorMode::Auto);
    }
}

pub(crate) fn check_bootstrap_expiry(expires_at: chrono::DateTime<chrono::Utc>) -> DoctorCheck {
    let remaining = expires_at - chrono::Utc::now();
    if remaining <= chrono::Duration::zero() {
        DoctorCheck {
            status: DoctorStatus::Fail,
            name: "bootstrap-expiry".to_string(),
            detail: format!(
                "credential expired at {}; run `bobby init --force`",
                expires_at.to_rfc3339()
            ),
        }
    } else if remaining < chrono::Duration::days(BOOTSTRAP_EXPIRY_WARN_DAYS) {
        DoctorCheck {
            status: DoctorStatus::Warn,
            name: "bootstrap-expiry".to_string(),
            detail: format!(
                "credential expires in {} day(s) at {}; run `bobby init --force` before then",
                remaining.num_days(),
                expires_at.to_rfc3339()
            ),
        }
    } else {
        DoctorCheck {
            status: DoctorStatus::Ok,
            name: "bootstrap-expiry".to_string(),
            detail: format!("credential valid for {} more day(s)", remaining.num_days()),
        }
    }
}

/// A gateway binary that cannot be spawned at all is a warning (it may be
/// installed separately); a gateway that starts but fails the handshake is a
/// failure, because the host will only report it as a dead server.
pub(crate) fn handshake_error_status(message: &str) -> DoctorStatus {
    if message.contains("not found") {
        DoctorStatus::Warn
    } else {
        DoctorStatus::Fail
    }
}

fn vision_endpoint_is_loopback(endpoint: &str) -> bool {
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
            "{endpoint} is stopped; Bobby starts the vision service on demand (`bobby vision start` runs it manually)"
        )
    } else {
        format!("{endpoint} not reachable (verify the external vision endpoint is running)")
    }
}

/// Whether config names any usable vision route via `NodeRegistry` merge
/// policy (HTTP nodes and/or ACP profiles) or a selected ACP backend.
fn vision_route_configured(config: &AppConfig) -> bool {
    let registry = node_registry::NodeRegistry::from_config(config);
    if !registry.is_empty() {
        return true;
    }
    matches!(
        config.vision.selected_backend(),
        Some(config::VisionBackendSelection::Acp { .. })
    )
}

fn check_vision_config_dual(config: &AppConfig) -> Option<DoctorCheck> {
    if !node_registry::NodeRegistry::has_dual_vision_config(config) {
        return None;
    }
    Some(DoctorCheck {
        status: DoctorStatus::Warn,
        name: "vision-config-dual".to_string(),
        detail: "both [nodes] and [vision].endpoint_url are set; [nodes] wins and [vision] endpoint is ignored — move it into [nodes.<name>] with kind = \"vision\"".to_string(),
    })
}

fn bootstrap_csv_holds(caps_csv: &str, capability: &str) -> bool {
    caps_csv.split(',').any(|entry| entry.trim() == capability)
}

fn check_vision_route_for_assist(
    config: &AppConfig,
    holds_vision_assist: bool,
) -> Option<DoctorCheck> {
    if !holds_vision_assist {
        return None;
    }
    if vision_route_configured(config) {
        Some(DoctorCheck {
            status: DoctorStatus::Ok,
            name: "vision-route".to_string(),
            detail: "vision:assist has a configured route".to_string(),
        })
    } else {
        Some(DoctorCheck {
            status: DoctorStatus::Warn,
            name: "vision-route".to_string(),
            detail: "vision:assist is granted but no vision route is configured; run `bobby vision connect`".to_string(),
        })
    }
}

/// Remind that `vision:assist` still needs session `executionPolicy.visionAssist`.
fn check_vision_session_gate(holds_vision_assist: bool) -> Option<DoctorCheck> {
    if !holds_vision_assist {
        return None;
    }
    Some(DoctorCheck {
        status: DoctorStatus::Ok,
        name: "vision-session-gate".to_string(),
        detail: "vision:assist is held; sessions still need executionPolicy.visionAssist=true (cap alone is not enough)".to_string(),
    })
}

/// Remind that `javascript:evaluate` still needs session `executionPolicy.javascriptEvaluation`.
fn check_javascript_session_gate(holds_javascript_evaluate: bool) -> Option<DoctorCheck> {
    if !holds_javascript_evaluate {
        return None;
    }
    Some(DoctorCheck {
        status: DoctorStatus::Ok,
        name: "javascript-session-gate".to_string(),
        detail: "javascript:evaluate is held; sessions still need executionPolicy.javascriptEvaluation=true (cap alone is not enough)".to_string(),
    })
}

fn check_builtin_job_handlers() -> DoctorCheck {
    DoctorCheck {
        status: DoctorStatus::Ok,
        name: "job-handlers".to_string(),
        detail: format!(
            "builtin job handlers: {} (job_submit name=…)",
            broker::BUILTIN_JOB_HANDLERS.join(", ")
        ),
    }
}

fn check_bootstrap_preset(path: Option<&Path>, caps_csv: Option<&str>) -> DoctorCheck {
    let preset = bootstrap_local::read_bootstrap_preset(path);
    let holds_admin = caps_csv.is_some_and(|caps| bootstrap_csv_holds(caps, "authority:admin"));
    match preset {
        bootstrap_local::BootstrapPreset::Agent if holds_admin => DoctorCheck {
            status: DoctorStatus::Warn,
            name: "bootstrap-preset".to_string(),
            detail: "preset is agent but capability list still includes authority:admin; re-run `bobby init --preset agent --force`".to_string(),
        },
        bootstrap_local::BootstrapPreset::Agent => DoctorCheck {
            status: DoctorStatus::Ok,
            name: "bootstrap-preset".to_string(),
            detail: "agent (no authority:admin)".to_string(),
        },
        bootstrap_local::BootstrapPreset::Unrestricted => DoctorCheck {
            status: DoctorStatus::Ok,
            name: "bootstrap-preset".to_string(),
            detail: if holds_admin {
                "unrestricted (includes authority:admin)".to_string()
            } else {
                "unrestricted (authority:admin not present; heal will add it)".to_string()
            },
        },
    }
}

/// 1x1 transparent PNG for the doctor propose probe.
const DOCTOR_PROBE_PNG: &[u8] = &[
    0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, 0x49, 0x48, 0x44, 0x52,
    0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f, 0x15, 0xc4,
    0x89, 0x00, 0x00, 0x00, 0x0a, 0x49, 0x44, 0x41, 0x54, 0x78, 0x9c, 0x63, 0x00, 0x01, 0x00, 0x00,
    0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, 0x49, 0x45, 0x4e, 0x44, 0xae,
    0x42, 0x60, 0x82,
];

/// One propose round-trip against the registry-resolved HTTP vision node.
/// Runs on its own thread+runtime because `run_doctor` is sync inside an
/// async process.
fn check_vision_propose_probe(
    config: &AppConfig,
    bootstrap_path: Option<&Path>,
) -> Option<DoctorCheck> {
    if matches!(config.vision.backend, Some(config::VisionBackendKind::Acp)) {
        return None;
    }
    let registry = node_registry::NodeRegistry::from_config(config);
    let (_, node) = registry.primary_http_vision_node()?;
    let endpoint = node.endpoint_url.clone();
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
                status: DoctorStatus::Ok,
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
    Some(match probe {
        Some(elapsed) => DoctorCheck {
            status: DoctorStatus::Ok,
            name: "vision-service".to_string(),
            detail: format!("propose round-trip ok in {}ms", elapsed.as_millis()),
        },
        None => DoctorCheck {
            status: DoctorStatus::Warn,
            name: "vision-service".to_string(),
            detail:
                "propose round-trip failed (endpoint unreachable, auth rejected, or invalid reply)"
                    .to_string(),
        },
    })
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

fn check_vision_model(vision: &VisionConfig) -> Option<DoctorCheck> {
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

fn push_doctor_check(report: &mut DoctorReport, check: DoctorCheck) {
    match check.status {
        DoctorStatus::Ok => report.ok(&check.name, check.detail),
        DoctorStatus::Warn => report.warn(&check.name, check.detail),
        DoctorStatus::Fail => report.fail(&check.name, check.detail),
    }
}

pub(crate) fn run_doctor(
    config_cli: Option<PathBuf>,
    bootstrap_cli: Option<PathBuf>,
    check_health: bool,
) -> Result<DoctorReport> {
    let mut report = DoctorReport::default();

    let config_path = resolve_config_path(config_cli);
    let bootstrap_path = resolve_bootstrap_path(bootstrap_cli.clone()).ok();
    let config = match AppConfig::load(&config_path) {
        Ok(config) => {
            let source = if config_path.exists() {
                config_path.display().to_string()
            } else {
                "built-in defaults (no config file)".to_string()
            };
            report.ok("config", source);
            Some(config)
        }
        Err(error) => {
            report.fail("config", format!("{error:#}"));
            None
        }
    };

    if let Some(config) = &config {
        if let Some(check) = check_vision_config_dual(config) {
            push_doctor_check(&mut report, check);
        }
        for check in check_vision_acp(config) {
            push_doctor_check(&mut report, check);
        }
        if let Some(check) = check_vision_provider(&config.vision) {
            push_doctor_check(&mut report, check);
        }
        if let Some(check) = check_vision_model(&config.vision) {
            push_doctor_check(&mut report, check);
        }
        if let Some(check) = check_vision_upstream_key(&config.vision) {
            push_doctor_check(&mut report, check);
        }
        if let Some(check) = check_vision_propose_probe(config, bootstrap_path.as_deref()) {
            push_doctor_check(&mut report, check);
        }

        if config.cdp.enabled {
            report.ok(
                "cdp-listen",
                format!("{}:{}", config.cdp.host, config.cdp.port),
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

    let selection = match resolve_browser_selection() {
        Ok((selection, source)) => {
            report.ok(
                "browser-selection",
                match source {
                    SelectionSource::Environment => {
                        "AUTOMATION_RUNTIME_BROWSER_SELECTION parses".to_string()
                    }
                    SelectionSource::Persisted(path) => {
                        format!("persisted selection at {}", path.display())
                    }
                    SelectionSource::Default => "default (Firefox, exact)".to_string(),
                },
            );
            Some(selection)
        }
        Err(error) => {
            report.fail("browser-selection", format!("{error:#}"));
            None
        }
    };

    // Context store: reported without claiming the single-writer lock, so
    // doctor is safe against a live runtime. Lockfile present means a writer
    // holds it (or one crashed); that is lock health, not an error.
    {
        let root = config
            .as_ref()
            .and_then(|config| config.context.dir.clone())
            .or_else(|| default_context_dir().ok());
        match root {
            Some(root) if root.is_dir() => {
                let mut sites = 0_u64;
                let mut bytes = 0_u64;
                let mut locked = false;
                if let Ok(mut entries) = std::fs::read_dir(&root) {
                    while let Some(Ok(profile)) = entries.next() {
                        if let Ok(mut files) = std::fs::read_dir(profile.path()) {
                            while let Some(Ok(file)) = files.next() {
                                let name = file.file_name();
                                let name = name.to_string_lossy();
                                if name == ".context-store.lock" {
                                    locked = true;
                                } else if name.ends_with(".json") {
                                    sites += 1;
                                    bytes += file.metadata().map(|m| m.len()).unwrap_or(0);
                                }
                            }
                        }
                    }
                }
                let lock = if locked { "lock held" } else { "lock free" };
                report.ok(
                    "context-store",
                    format!(
                        "{} · {} site files · {} bytes · {lock}",
                        root.display(),
                        sites,
                        bytes
                    ),
                );
            }
            Some(root) => report.ok(
                "context-store",
                format!("{} · no store yet (first run creates it)", root.display()),
            ),
            None => report.warn(
                "context-store",
                "no [context].dir and config directory unavailable".to_string(),
            ),
        }
    }

    if let (Some(config), Some(selection)) = (&config, &selection) {
        match compose_worker_factory(config, selection.clone()) {
            Ok(_) => report.ok(
                "engine-satisfiability",
                "engine preference can be satisfied by configured registrations".to_string(),
            ),
            Err(error) => {
                if selection.firefox.is_empty() && format!("{error:#}").contains("Firefox") {
                    report.warn(
                        "firefox-enrollment",
                        "Firefox is not paired yet. Run `bobby install --companion`, then `make firefox-start`, and click Pair in the Bobby companion toolbar popup. Re-run `bobby doctor` afterward."
                            .to_string(),
                    );
                } else {
                    report.fail("engine-satisfiability", format!("{error:#}"));
                }
            }
        }
        for profile in &selection.firefox {
            match Url::parse(&profile.bidi_url) {
                Ok(url) if matches!(url.scheme(), "ws" | "wss") => {
                    match probe_firefox_bidi(&profile.bidi_url) {
                        Ok(()) => report.ok(
                            "firefox-bidi",
                            format!("{} accepted a WebDriver BiDi handshake", profile.bidi_url),
                        ),
                        Err(error) => {
                            report.warn(
                            "firefox-bidi",
                            format!(
                                "{} is not a Firefox WebDriver BiDi endpoint ({error:#}); another service may own the port",
                                profile.bidi_url,
                            ),
                        );
                        }
                    }
                }
                _ => {
                    report.fail(
                        "firefox-bidi",
                        format!(
                            "profile {} has an invalid bidiUrl (expected ws:// or wss://)",
                            profile.profile_id
                        ),
                    );
                }
            }
            if profile.profile_dir.exists() {
                report.ok(
                    "firefox-profile-dir",
                    profile.profile_dir.display().to_string(),
                );
            } else {
                report.warn(
                    "firefox-profile-dir",
                    format!("{} does not exist yet", profile.profile_dir.display()),
                );
            }
            if profile.companion_bind.parse::<SocketAddr>().is_err() {
                report.fail(
                    "firefox-companion-bind",
                    format!(
                        "profile {} has an invalid companionBind",
                        profile.profile_id
                    ),
                );
            }
        }
    }

    // The bootstrap expiry is pinned into MCP client config and the stdio
    // gateway refuses to start once it passes, which a host reports only as a
    // dead server. Warn while there is still time to run `bobby init`.
    // Heal stale capability lists first so doctor validates what serve/mcp will
    // actually use after unrestricted-default heal.
    let bootstrap_path_for_heal = resolve_bootstrap_path(bootstrap_cli.clone()).ok();
    if let Some(path) = bootstrap_path_for_heal.as_ref() {
        match bootstrap_local::ensure_unrestricted_bootstrap(path) {
            Ok(heal) if heal.changed() => {
                report.ok(
                    "bootstrap-capabilities",
                    format!(
                        "healed {} missing default(s): {}",
                        heal.added.len(),
                        heal.added.join(", ")
                    ),
                );
            }
            Ok(_)
                if path.exists()
                    || std::env::var("AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES").is_ok() =>
            {
                report.ok(
                    "bootstrap-capabilities",
                    "current (matches defaults)".to_string(),
                );
            }
            Ok(_) => {}
            Err(error) => {
                report.warn(
                    "bootstrap-capabilities",
                    format!("could not heal bootstrap capabilities ({error:#})"),
                );
            }
        }
        if path.exists() {
            match bootstrap_local::load_bootstrap_capabilities_csv(path) {
                Ok(caps) if !caps.split(',').any(|c| c.trim() == "browser:fingerprint") => {
                    report.warn(
                        "bootstrap-capabilities",
                        "bootstrap still lacks browser:fingerprint after heal".to_string(),
                    );
                }
                Ok(_) => {}
                Err(error) => {
                    report.warn(
                        "bootstrap-capabilities",
                        format!("could not read capabilities ({error:#})"),
                    );
                }
            }
        }
    } else if let Ok(heal) = bootstrap_local::heal_process_env_capabilities() {
        if heal.changed() {
            report.ok(
                "bootstrap-capabilities",
                format!(
                    "healed process env with {} missing default(s): {}",
                    heal.added.len(),
                    heal.added.join(", ")
                ),
            );
        }
    }

    let holds_vision_assist = {
        let from_file = bootstrap_path_for_heal
            .as_ref()
            .filter(|path| path.exists())
            .and_then(|path| bootstrap_local::load_bootstrap_capabilities_csv(path).ok());
        let from_env = std::env::var("AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES").ok();
        from_file
            .or(from_env)
            .is_some_and(|caps| bootstrap_csv_holds(&caps, "vision:assist"))
    };
    let holds_javascript_evaluate = {
        let from_file = bootstrap_path_for_heal
            .as_ref()
            .filter(|path| path.exists())
            .and_then(|path| bootstrap_local::load_bootstrap_capabilities_csv(path).ok());
        let from_env = std::env::var("AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES").ok();
        from_file
            .or(from_env)
            .is_some_and(|caps| bootstrap_csv_holds(&caps, "javascript:evaluate"))
    };
    if let Some(config) = &config {
        if let Some(check) = check_vision_route_for_assist(config, holds_vision_assist) {
            push_doctor_check(&mut report, check);
        }
    }
    if let Some(check) = check_vision_session_gate(holds_vision_assist) {
        push_doctor_check(&mut report, check);
    }
    if let Some(check) = check_javascript_session_gate(holds_javascript_evaluate) {
        push_doctor_check(&mut report, check);
    }
    push_doctor_check(&mut report, check_builtin_job_handlers());

    let caps_for_preset = bootstrap_path_for_heal
        .as_ref()
        .filter(|path| path.exists())
        .and_then(|path| bootstrap_local::load_bootstrap_capabilities_csv(path).ok())
        .or_else(|| std::env::var("AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES").ok());
    if bootstrap_path_for_heal
        .as_ref()
        .is_some_and(|path| path.exists())
        || caps_for_preset.is_some()
    {
        push_doctor_check(
            &mut report,
            check_bootstrap_preset(
                bootstrap_path_for_heal.as_deref(),
                caps_for_preset.as_deref(),
            ),
        );
    }

    if let Ok(credential) = broker::StartupCredential::from_env() {
        report.ok("bootstrap", "credential from environment".to_string());
        let expiry = check_bootstrap_expiry(credential.expires_at());
        report.record(expiry.status, &expiry.name, expiry.detail);
    } else {
        match resolve_bootstrap_path(bootstrap_cli.clone()) {
            Ok(path) if path.exists() => {
                report.ok(
                    "bootstrap",
                    format!("credential file at {}", path.display()),
                );
                match bootstrap_local::load_startup_from_env_file(&path) {
                    Ok(credential) => {
                        let expiry = check_bootstrap_expiry(credential.expires_at());
                        report.record(expiry.status, &expiry.name, expiry.detail);
                    }
                    Err(error) => {
                        report.fail("bootstrap-expiry", format!("{error:#}"));
                    }
                }
            }
            Ok(path) => {
                report.warn(
                    "bootstrap",
                    format!(
                        "no credential yet; `bobby serve` will generate one at {}",
                        path.display()
                    ),
                );
            }
            Err(error) => {
                report.fail("bootstrap", format!("{error:#}"));
            }
        }
    }

    // Sidecar gateways must sit beside bobby (or on PATH) for mcp-stdio /
    // acp-stdio. Missing binaries are a warning with an install hint.
    for (name, command) in [
        ("mcp-gateway", onboarding::mcp_gateway_command()),
        ("acp-gateway", onboarding::acp_gateway_command()),
    ] {
        match onboarding::find_sidecar_binary(command) {
            Some(path) => report.ok(name, path.display().to_string()),
            None => report.warn(
                name,
                format!(
                    "{command} not found next to bobby or on PATH; install with `bobby install --cli`, re-run scripts/install.sh, or `cargo build -p {command} --release`"
                ),
            ),
        }
    }

    // MCP handshake: the stdio gateway an agent host launches must answer
    // `initialize` and `tools/list` within the advertised byte budget. A
    // missing gateway binary is a warning (it may be installed separately);
    // a gateway that starts but fails the handshake is a failure, because the
    // host will only report it as a dead server.
    let handshake_env: Option<std::collections::BTreeMap<String, String>> =
        if broker::StartupCredential::from_env().is_ok() {
            Some(std::collections::BTreeMap::new())
        } else {
            bootstrap_path
                .filter(|path| path.exists())
                .and_then(|path| bootstrap_local::load_bootstrap_env_map(&path).ok())
        };
    // Hand the same config path doctor validated into the gateway child so
    // `[mcp] startup_toolset` (and the rest of the file) apply to handshake —
    // without this, doctor always probes explore defaults while gauntlet/agent
    // hosts that set BOBBY_BROWSER_CONFIG see a different surface.
    let handshake_env = handshake_env.map(|mut env| {
        if config_path.exists() {
            env.insert(
                "BOBBY_BROWSER_CONFIG".into(),
                config_path.display().to_string(),
            );
        }
        env
    });
    match handshake_env {
        Some(env) => match onboarding::mcp_handshake(&env) {
            Ok(handshake) => {
                if handshake.bytes > mcp_gateway::TOOLS_LIST_BYTE_BUDGET {
                    report.fail(
                        "mcp-handshake",
                        format!(
                            "tools/list is {} bytes, over the {} byte budget",
                            handshake.bytes,
                            mcp_gateway::TOOLS_LIST_BYTE_BUDGET
                        ),
                    );
                } else {
                    report.ok(
                        "mcp-handshake",
                        format!(
                            "gateway {} answered initialize + tools/list: {} tools, {} bytes ({}% of budget)",
                            handshake.server_version,
                            handshake.tools,
                            handshake.bytes,
                            handshake.bytes * 100 / mcp_gateway::TOOLS_LIST_BYTE_BUDGET
                        ),
                    );
                }
            }
            Err(error) => {
                let message = format!("{error:#}");
                report.record(handshake_error_status(&message), "mcp-handshake", message);
            }
        },
        None => {
            report.warn(
                "mcp-handshake",
                "skipped: no bootstrap credential to launch the gateway with".to_string(),
            );
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        if let Some((ok, detail)) = crate::openshell::doctor_pack_detail(&cwd) {
            if ok {
                report.ok("openshell-pack", detail);
            } else {
                report.warn("openshell-pack", detail);
            }

            let firefox_enrolled = match resolve_browser_selection() {
                Ok((selection, _)) => !selection.firefox.is_empty(),
                Err(_) => false,
            };
            let bootstrap_for_openshell = resolve_bootstrap_path(bootstrap_cli.clone()).ok();
            let extras = crate::openshell::doctor_openshell_extras(
                &cwd,
                bootstrap_for_openshell.as_deref(),
                config.as_ref(),
                firefox_enrolled,
            );
            let mut record = |name: &str, (ok, detail): (bool, String)| {
                if ok {
                    report.ok(name, detail);
                } else {
                    report.warn(name, detail);
                }
            };
            record("openshell-admin", extras.admin);
            record("openshell-companion", extras.companion);
            if let Some(mcp) = extras.mcp_url {
                record("openshell-mcp-url", mcp);
            }
            if let Some(cleartext) = extras.cleartext {
                record("openshell-cleartext", cleartext);
            }
            record("openshell-sandboxes", extras.local_sandboxes);
        }
    }

    if let Some(config) = &config {
        for (name, dir) in [
            (
                "storage-journal-dir",
                config
                    .storage
                    .journal_path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from(".")),
            ),
            (
                "storage-scheduler-journal-dir",
                config
                    .storage
                    .scheduler_journal_path
                    .parent()
                    .map(Path::to_path_buf)
                    .unwrap_or_else(|| PathBuf::from(".")),
            ),
            (
                "storage-checkpoints-dir",
                config.storage.checkpoints_dir.clone(),
            ),
            ("artifacts-dir", config.browser.artifacts_dir.clone()),
        ] {
            match std::fs::create_dir_all(&dir) {
                Ok(()) => report.ok(name, dir.display().to_string()),
                Err(error) => {
                    report.fail(name, format!("{}: {error}", dir.display()));
                }
            }
        }
    }

    let firefox = which_binary(&["firefox", "firefox-esr"])
        || [
            "/Applications/Firefox.app",
            "/Applications/Firefox Developer Edition.app",
            "/Applications/Firefox Nightly.app",
        ]
        .iter()
        .any(|bundle| Path::new(bundle).exists());
    if firefox {
        report.ok("firefox", "found".to_string());
    } else {
        report.warn(
            "firefox",
            "not found on PATH or /Applications (default engine)".to_string(),
        );
    }
    let chromium = which_binary(&["google-chrome", "chromium", "chrome"])
        || Path::new("/Applications/Google Chrome.app").exists()
        || Path::new("/Applications/Chromium.app").exists();
    if chromium {
        report.ok("chromium", "found".to_string());
    } else {
        report.warn(
            "chromium",
            "not found (required for Chromium engine selection)".to_string(),
        );
    }

    if check_health {
        if let Some(config) = &config {
            let url = format!(
                "http://{}:{}/healthz",
                config.server.host, config.server.port
            );
            match probe_healthz(&url) {
                Ok(()) => report.ok("healthz", format!("{url} responded")),
                Err(error) => {
                    report.warn(
                        "healthz",
                        format!("{url} not reachable ({error}); is `bobby serve` running?"),
                    );
                }
            }
        }
    }

    Ok(report)
}

fn probe_firefox_bidi(endpoint: &str) -> Result<()> {
    let url = Url::parse(endpoint).context("invalid WebDriver BiDi URL")?;
    if url.scheme() != "ws" {
        anyhow::bail!("doctor currently probes loopback ws:// BiDi endpoints only");
    }
    let host = url.host_str().context("BiDi URL has no host")?;
    let port = url
        .port_or_known_default()
        .context("BiDi URL has no port")?;
    let address = url
        .socket_addrs(|| Some(port))?
        .into_iter()
        .next()
        .context("BiDi host resolved to no addresses")?;
    let mut stream = std::net::TcpStream::connect_timeout(&address, Duration::from_millis(500))?;
    stream.set_read_timeout(Some(Duration::from_secs(1)))?;
    stream.set_write_timeout(Some(Duration::from_secs(1)))?;
    let path = if url.path().is_empty() {
        "/"
    } else {
        url.path()
    };
    write!(
        stream,
        "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
    )?;
    let mut response = [0_u8; 4096];
    let read = stream.read(&mut response)?;
    let head = String::from_utf8_lossy(&response[..read]);
    let status = head.lines().next().unwrap_or("empty response");
    if !status.contains(" 101 ") {
        anyhow::bail!("WebSocket handshake returned {status}");
    }
    Ok(())
}

fn probe_healthz(url: &str) -> Result<()> {
    let url = url.to_owned();
    match std::thread::spawn(move || probe_healthz_blocking(&url)).join() {
        Ok(result) => result,
        Err(_) => anyhow::bail!("healthz probe thread panicked"),
    }
}

fn probe_healthz_blocking(url: &str) -> Result<()> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(2))
        .no_proxy()
        .build()
        .context("failed to build healthz HTTP client")?;
    let response = client
        .get(url)
        .send()
        .with_context(|| format!("GET {url}"))?;
    if !response.status().is_success() {
        anyhow::bail!("unexpected status {}", response.status());
    }
    Ok(())
}

fn which_binary(names: &[&str]) -> bool {
    let Some(path_var) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path_var).any(|dir| {
        names.iter().any(|name| {
            let candidate = dir.join(name);
            candidate.is_file() && is_executable(&candidate)
        })
    })
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(_path: &Path) -> bool {
    true
}

#[cfg(test)]
mod bidi_probe_tests {
    use super::*;
    use std::io::{Read, Write};

    #[test]
    fn firefox_bidi_probe_rejects_an_http_service_on_the_port() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            let (mut stream, _) = listener.accept().unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut chunk = [0_u8; 256];
                let read = stream.read(&mut chunk).unwrap();
                assert_ne!(read, 0, "probe closed before completing the handshake");
                request.extend_from_slice(&chunk[..read]);
            }
            stream
                .write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n")
                .unwrap();
        });

        let error = probe_firefox_bidi(&format!("ws://{address}/session")).unwrap_err();
        assert!(error.to_string().contains("404"), "{error:#}");
        server.join().unwrap();
    }
}
