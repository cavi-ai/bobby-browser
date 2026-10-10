//! `bobby doctor` checks and report rendering.

mod checks;
#[cfg(unix)]
mod skill_permissions;

use std::{
    io::{IsTerminal, Write},
    net::SocketAddr,
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result};
use auth_broker::{AuthCapabilities, AuthError, AuthProfileId, AuthStrategy};
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
    pub(crate) profile: Option<crate::deployment_profiles::DeploymentProfile>,
}

pub(crate) struct DoctorFixReport {
    pub(crate) actions: Vec<DoctorFixAction>,
    pub(crate) post_fix: DoctorReport,
}

fn idempotency_ledgers(config: &AppConfig) -> [(&'static str, PathBuf); 3] {
    [
        (
            "idempotency-commands",
            config
                .storage
                .journal_path
                .with_extension("idempotency.json"),
        ),
        (
            "idempotency-lifecycle",
            config
                .storage
                .journal_path
                .with_extension("lifecycle-idempotency.json"),
        ),
        (
            "idempotency-jobs",
            config
                .storage
                .scheduler_journal_path
                .with_extension("idempotency.json"),
        ),
    ]
}

pub(crate) fn run_idempotency_downgrade(options: DoctorFixOptions) -> Result<DoctorFixReport> {
    let config_path = resolve_config_path(options.config);
    let config = AppConfig::load(&config_path)?;
    let mut actions = Vec::new();
    for (name, path) in idempotency_ledgers(&config) {
        let target = path.clone();
        let result =
            block_on_inspect(
                async move { interface_core::downgrade_idempotency_ledger(target).await },
            );
        let (status, detail) = match result {
            Ok(Some(backup)) => (
                DoctorFixStatus::Fixed,
                format!(
                    "converted {} to v1; source preserved at {}",
                    path.display(),
                    backup.display()
                ),
            ),
            Ok(None) => (
                DoctorFixStatus::Noop,
                format!("{} is missing or already v1", path.display()),
            ),
            Err(error) => (
                DoctorFixStatus::Failed,
                format!(
                    "{}: {error}; preserve the ledger and any .v2.backup for inspection",
                    path.display()
                ),
            ),
        };
        actions.push(DoctorFixAction {
            status,
            name: name.into(),
            detail,
        });
    }
    // Rollback checks only the converted ledgers; it must not launch gateways
    // or change credentials, models, or host configuration.
    let mut post_fix = DoctorReport::default();
    record_idempotency_ledgers(&mut post_fix, &config);
    Ok(DoctorFixReport { actions, post_fix })
}

fn repair_host_configs(project_root: &Path) -> Vec<DoctorFixAction> {
    let statuses = match onboarding::configured_host_statuses(project_root) {
        Ok(statuses) => statuses,
        Err(error) => {
            return vec![DoctorFixAction {
                status: DoctorFixStatus::Failed,
                name: "host-config".to_string(),
                detail: error.to_string(),
            }]
        }
    };
    let mut actions = Vec::new();
    for (kind, path, status) in statuses {
        let name = format!("host-{}", kind.name());
        match status {
            onboarding::HostConfigStatus::Drifted => {
                match onboarding::merge_host_config(kind, project_root) {
                    Ok(_) => actions.push(DoctorFixAction {
                        status: DoctorFixStatus::Fixed,
                        name,
                        detail: format!("updated {}", path.display()),
                    }),
                    Err(error) => actions.push(DoctorFixAction {
                        status: DoctorFixStatus::Failed,
                        name,
                        detail: error.to_string(),
                    }),
                }
            }
            onboarding::HostConfigStatus::Invalid => actions.push(DoctorFixAction {
                status: DoctorFixStatus::NeedsAction,
                name,
                detail: format!("repair invalid JSON in {}", path.display()),
            }),
            onboarding::HostConfigStatus::Missing | onboarding::HostConfigStatus::Current => {}
        }
    }
    actions
}

impl DoctorFixReport {
    pub(crate) fn render_actions(&self) {
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
    }

    pub(crate) fn render(&self) {
        self.render_actions();
        self.post_fix.render();
    }
}

pub(crate) fn run_doctor_fix(options: DoctorFixOptions) -> Result<DoctorFixReport> {
    let config_path = resolve_config_path(options.config.clone());
    let bootstrap_path = resolve_bootstrap_path(options.bootstrap_env.clone())?;
    let mut actions = Vec::new();
    actions.extend(repair_host_configs(&std::env::current_dir()?));

    if bootstrap_path.exists() {
        match bootstrap_local::ensure_unrestricted_bootstrap(&bootstrap_path) {
            Ok(heal) => {
                match bootstrap_local::rotate_expired_bootstrap(
                    &bootstrap_path,
                    chrono::Duration::days(bootstrap_local::DEFAULT_TTL_DAYS),
                ) {
                    Ok(rotate) if rotate.rotated => actions.push(DoctorFixAction {
                        status: DoctorFixStatus::Fixed,
                        name: "bootstrap".to_string(),
                        detail: "rotated expired bootstrap credential".to_string(),
                    }),
                    Ok(_) => actions.push(DoctorFixAction {
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
            }
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
                        allow_download: options.download_model
                            || provider_name.eq_ignore_ascii_case("ollama"),
                        allow_start: true,
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

    if let Ok(config) = AppConfig::load(&config_path) {
        for (name, dir) in configured_storage_dirs(&config) {
            let existed = dir.is_dir();
            match std::fs::create_dir_all(&dir) {
                Ok(()) => actions.push(DoctorFixAction {
                    status: if existed {
                        DoctorFixStatus::Noop
                    } else {
                        DoctorFixStatus::Fixed
                    },
                    name: name.to_string(),
                    detail: format!("ensured {}", dir.display()),
                }),
                Err(error) => actions.push(DoctorFixAction {
                    status: DoctorFixStatus::Failed,
                    name: name.to_string(),
                    detail: format!("{}: {error}", dir.display()),
                }),
            }
        }
        #[cfg(unix)]
        actions.push(skill_permissions::repair(&config.storage.checkpoints_dir));
    }

    let post_fix = run_doctor_with_profile(
        Some(config_path),
        Some(bootstrap_path),
        options.check_health,
        options.profile,
    )?;
    Ok(DoctorFixReport { actions, post_fix })
}

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DoctorNextAction {
    pub(crate) command: String,
    pub(crate) reason: String,
}

const GROUP_ORDER: [&str; 6] = [
    "setup",
    "browser",
    "vision",
    "persistence",
    "runtime",
    "openshell",
];

pub(crate) fn check_group(name: &str) -> &'static str {
    if name.starts_with("openshell-") {
        return "openshell";
    }
    if name.starts_with("vision-") || name == "javascript-session-gate" {
        return "vision";
    }
    if name.starts_with("storage-")
        || matches!(
            name,
            "context-store"
                | "artifacts-dir"
                | "command-journal"
                | "scheduler-journal"
                | "vision-corpus"
        )
    {
        return "persistence";
    }
    if matches!(
        name,
        "healthz" | "mcp-handshake" | "job-handlers" | "jobs-queue"
    ) {
        return "runtime";
    }
    if name.starts_with("firefox-")
        || matches!(
            name,
            "browser-selection"
                | "engine-satisfiability"
                | "firefox"
                | "chromium"
                | "companion-port"
                | "cdp-listen"
                | "cdp-port"
        )
    {
        return "browser";
    }
    "setup"
}

fn ignored_for_next_action(name: &str) -> bool {
    matches!(name, "healthz" | "job-handlers" | "firefox" | "chromium")
}

fn repair_command(name: &str) -> &'static str {
    match name {
        "bootstrap" => "bobby install",
        "bootstrap-expiry" => "bobby init --force",
        "mcp-gateway" | "acp-gateway" | "sidecar-version" => "bobby install --cli",
        "firefox-enrollment" => "bobby install --companion",
        "cli-path" => "put the install dir first on PATH",
        "vision-readiness" => "bobby doctor --fix",
        "bootstrap-capabilities" => "bobby doctor --fix",
        "config" => "fix config.toml",
        "context-store" => "bobby context verify",
        other
            if other.starts_with("storage-")
                || matches!(
                    other,
                    "command-journal" | "scheduler-journal" | "vision-corpus"
                ) =>
        {
            "bobby doctor --fix"
        }
        _ => "bobby doctor --fix",
    }
}

fn check_detail_with_fix(check: &DoctorCheck) -> String {
    if check.status != DoctorStatus::Fail || ignored_for_next_action(&check.name) {
        return check.detail.clone();
    }
    if check.detail.contains("fix:") || check.name.starts_with("host-") {
        return check.detail.clone();
    }
    format!("{} · fix: {}", check.detail, repair_command(&check.name))
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

    pub(crate) fn next_action(&self) -> Option<DoctorNextAction> {
        if let Some(check) = self.checks.iter().find(|check| {
            check.status == DoctorStatus::Fail && !ignored_for_next_action(&check.name)
        }) {
            return Some(DoctorNextAction {
                command: repair_command(&check.name).to_string(),
                reason: check.detail.clone(),
            });
        }
        if let Some(check) = self
            .checks
            .iter()
            .find(|check| check.name == "bootstrap" && check.status == DoctorStatus::Warn)
        {
            return Some(DoctorNextAction {
                command: "bobby install".to_string(),
                reason: check.detail.clone(),
            });
        }
        None
    }

    pub(crate) fn render_json_to(&self, writer: &mut dyn Write) -> std::io::Result<()> {
        let next = self.next_action().map(|action| {
            serde_json::json!({
                "command": action.command,
                "reason": action.reason,
            })
        });
        let groups: Vec<serde_json::Value> = GROUP_ORDER
            .iter()
            .filter_map(|group_name| {
                let checks: Vec<serde_json::Value> = self
                    .checks
                    .iter()
                    .filter(|check| check_group(&check.name) == *group_name)
                    .map(|check| {
                        serde_json::json!({
                            "status": check.status.label(),
                            "name": check.name,
                            "detail": check.detail,
                        })
                    })
                    .collect();
                if checks.is_empty() {
                    None
                } else {
                    Some(serde_json::json!({
                        "name": group_name,
                        "checks": checks,
                    }))
                }
            })
            .collect();
        let body = serde_json::json!({
            "version": 1,
            "failures": self.failures(),
            "warnings": self.warnings(),
            "nextAction": next,
            "groups": groups,
        });
        let encoded = serde_json::to_string_pretty(&body)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
        writeln!(writer, "{encoded}")
    }

    pub(crate) fn render_to(
        &self,
        writer: &mut dyn Write,
        color_mode: DoctorColorMode,
    ) -> std::io::Result<()> {
        let color = color_mode.enabled();
        let next_line = match self.next_action() {
            Some(action) => format!("next: {}", action.command),
            None => "next: ok".to_string(),
        };
        let lead_with_next = self.failures() > 0 || self.warnings() > 0;
        if lead_with_next {
            writeln!(writer, "{next_line}")?;
            writeln!(writer)?;
        }
        let mut last_group: Option<&str> = None;
        for check in &self.checks {
            let detail = check_detail_with_fix(check);
            if color {
                let group = check_group(&check.name);
                if last_group != Some(group) {
                    if last_group.is_some() {
                        writeln!(writer)?;
                    }
                    writeln!(writer, "{group}")?;
                    last_group = Some(group);
                }
                writeln!(
                    writer,
                    "[{}{}\x1b[0m] {}: {}",
                    check.status.ansi(),
                    check.status.label(),
                    check.name,
                    detail
                )?;
            } else {
                writeln!(
                    writer,
                    "[{}] {}: {}",
                    check.status.label(),
                    check.name,
                    detail
                )?;
            }
        }
        if !lead_with_next {
            writeln!(writer, "{next_line}")?;
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

fn push_doctor_check(report: &mut DoctorReport, check: DoctorCheck) {
    match check.status {
        DoctorStatus::Ok => report.ok(&check.name, check.detail),
        DoctorStatus::Warn => report.warn(&check.name, check.detail),
        DoctorStatus::Fail => report.fail(&check.name, check.detail),
    }
}

#[cfg(test)]
pub(crate) fn run_doctor(
    config_cli: Option<PathBuf>,
    bootstrap_cli: Option<PathBuf>,
    check_health: bool,
) -> Result<DoctorReport> {
    run_doctor_with_profile(config_cli, bootstrap_cli, check_health, None)
}

pub(crate) fn run_doctor_with_profile(
    config_cli: Option<PathBuf>,
    bootstrap_cli: Option<PathBuf>,
    check_health: bool,
    profile: Option<crate::deployment_profiles::DeploymentProfile>,
) -> Result<DoctorReport> {
    checks::run(config_cli, bootstrap_cli, check_health, profile)
}

#[cfg(test)]
pub(crate) use checks::vision_and_gateway_configuration::vision_endpoint_unreachable_detail;

#[cfg(test)]
pub(crate) use checks::vision_and_gateway_configuration::check_vision_provider;

#[cfg(test)]
pub(crate) use checks::vision_and_gateway_configuration::check_vision_upstream_key;

#[cfg(test)]
pub(crate) use checks::vision_and_gateway_configuration::vision_auth_discovery_check;

#[cfg(test)]
pub(crate) use checks::vision_and_gateway_configuration::check_vision_acp;

#[cfg(test)]
pub(crate) use checks::sidecars::sidecar_version_status;

#[cfg(test)]
pub(crate) use checks::credential::check_bootstrap_expiry;

use checks::storage::record_idempotency_ledgers;

use checks::storage::configured_storage_dirs;

use checks::storage::block_on_inspect;

#[cfg(test)]
pub(crate) use checks::mcp_handshake::handshake_error_status;

#[cfg(test)]
pub(crate) use checks::credential::BOOTSTRAP_EXPIRY_WARN_DAYS;

pub(crate) struct DoctorCliOptions {
    pub options: DoctorFixOptions,
    pub fix: bool,
    pub downgrade_idempotency: bool,
    pub json: bool,
}
pub(crate) fn run_cli(command: DoctorCliOptions) -> Result<()> {
    let DoctorCliOptions {
        options,
        fix,
        downgrade_idempotency,
        json,
    } = command;

    if fix {
        let report = if downgrade_idempotency {
            run_idempotency_downgrade(options)?
        } else {
            run_doctor_fix(options)?
        };
        if json {
            report.render_actions();
            report
                .post_fix
                .render_json_to(&mut std::io::stdout().lock())?;
        } else {
            report.render();
        }
        if report.post_fix.failures() > 0
            || report
                .actions
                .iter()
                .any(|action| action.status == DoctorFixStatus::Failed)
        {
            std::process::exit(1);
        }
    } else {
        let report = run_doctor_with_profile(
            options.config,
            options.bootstrap_env,
            options.check_health,
            options.profile,
        )?;
        if json {
            report.render_json_to(&mut std::io::stdout().lock())?;
        } else {
            report.render();
        }
        if report.failures() > 0 {
            std::process::exit(1);
        }
    }
    Ok(())
}
