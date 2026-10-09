//! `bobby doctor` checks and report rendering.

mod checks;

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

fn record_idempotency_ledgers(report: &mut DoctorReport, config: &AppConfig) {
    for (name, path) in idempotency_ledgers(config) {
        let target = path.clone();
        match block_on_inspect(
            async move { interface_core::inspect_idempotency_ledger(target).await },
        ) {
            Ok(health) if !health.exists => report.ok(name, "no ledger yet".into()),
            Ok(health) if health.integrity_issue.is_some() => report.fail(
                name,
                format!(
                    "{} requires repair; preserve the ledger and its reservations",
                    path.display()
                ),
            ),
            Ok(health) => report.ok(
                name,
                format!(
                    "{} · v{} · {} keys",
                    path.display(),
                    health.format.unwrap_or(0),
                    health.entries
                ),
            ),
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => report.warn(
                name,
                "ledger in use; stop Bobby for offline inspection or downgrade".into(),
            ),
            Err(error) => report.fail(name, format!("{}: {error}", path.display())),
        }
    }
}

fn record_host_config_checks(report: &mut DoctorReport, project_root: &Path) {
    match onboarding::configured_host_statuses(project_root) {
        Ok(statuses) => {
            for (kind, path, status) in statuses {
                let name = format!("host-{}", kind.name());
                match status {
                    onboarding::HostConfigStatus::Missing => {}
                    onboarding::HostConfigStatus::Current => {
                        report.ok(&name, path.display().to_string())
                    }
                    onboarding::HostConfigStatus::Drifted => {
                        report.fail(&name, onboarding::host_config_drift_detail(kind, &path))
                    }
                    onboarding::HostConfigStatus::Invalid => {
                        report.fail(&name, format!("{} is not valid host JSON", path.display()))
                    }
                }
            }
        }
        Err(error) => report.fail("host-config", format!("{error:#}")),
    }
}

fn record_cli_path_check(report: &mut DoctorReport) {
    let Ok(canonical) = onboarding::canonical_cli_path() else {
        return;
    };
    match onboarding::path_cli() {
        None => report.warn(
            "cli-path",
            format!(
                "bobby is not on PATH; hosts launch {}",
                canonical.display()
            ),
        ),
        Some(path_hit) if onboarding::same_cli(&path_hit, &canonical) => {
            report.ok("cli-path", canonical.display().to_string())
        }
        Some(path_hit) => report.warn(
            "cli-path",
            format!(
                "PATH `bobby` is {}; hosts launch {}. Put the install dir first on PATH or run that binary",
                path_hit.display(),
                canonical.display()
            ),
        ),
    }
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
    }

    let post_fix = run_doctor_with_profile(
        Some(config_path),
        Some(bootstrap_path),
        options.check_health,
        options.profile,
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

pub(crate) fn sidecar_version_status(
    bobby: &str,
    mcp: Option<&str>,
    acp: Option<&str>,
) -> Option<DoctorCheck> {
    if mcp.is_none() && acp.is_none() {
        return None;
    }
    if let Some((name, version)) = [("mcp-gateway", mcp), ("acp-gateway", acp)]
        .into_iter()
        .find_map(|(name, version)| {
            version.and_then(|version| (version != bobby).then_some((name, version)))
        })
    {
        return Some(DoctorCheck {
            status: DoctorStatus::Fail,
            name: "sidecar-version".to_string(),
            detail: format!("{name} {version} does not match bobby {bobby}"),
        });
    }
    Some(DoctorCheck {
        status: DoctorStatus::Ok,
        name: "sidecar-version".to_string(),
        detail: format!("matches bobby {bobby}"),
    })
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

/// A browser composition that failed only because this scope's running
/// runtime holds the enrolled profile is that runtime working: it composed the
/// same registrations to start. Every other composition error stays a failure.
fn engine_composition_finding(
    error: &anyhow::Error,
    running_owner: Option<String>,
    firefox_unenrolled: bool,
) -> (DoctorStatus, &'static str, String) {
    let held_by_a_runtime = error
        .chain()
        .any(|cause| cause.is::<firefox_companion::selection::ProfileOwned>());
    match running_owner {
        Some(origin) if held_by_a_runtime => (
            DoctorStatus::Ok,
            "engine-satisfiability",
            format!("the running runtime at {origin} holds this scope's browser registrations"),
        ),
        _ if firefox_unenrolled && format!("{error:#}").contains("Firefox") => (
            DoctorStatus::Warn,
            "firefox-enrollment",
            "Firefox is not paired yet. Run `bobby install --companion`, then `make firefox-start`, and click Pair in the Bobby companion toolbar popup. Re-run `bobby doctor` afterward."
                .to_string(),
        ),
        _ => (
            DoctorStatus::Fail,
            "engine-satisfiability",
            format!("{error:#}"),
        ),
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
            "nothing listens on {endpoint} and no vision provider is selected, so no runtime starts a proxy for it; select one with `bobby vision connect`"
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
        detail: "both [nodes] and [vision].endpoint_url are set; [nodes] wins and [vision] endpoint is ignored -- move it into [nodes.<name>] with kind = \"vision\"".to_string(),
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
    let floor = preset.capability_preset();
    let held: Vec<&str> = caps_csv
        .map(|caps| {
            caps.split(',')
                .map(str::trim)
                .filter(|capability| !capability.is_empty())
                .collect()
        })
        .unwrap_or_default();
    let beyond: Vec<&str> = held
        .iter()
        .copied()
        .filter(|capability| {
            !floor
                .capabilities()
                .iter()
                .any(|allowed| allowed.as_str() == *capability)
        })
        .collect();
    if !beyond.is_empty() {
        return DoctorCheck {
            status: DoctorStatus::Warn,
            name: "bootstrap-preset".to_string(),
            detail: format!(
                "preset is {} but the capability list also holds {}; re-run `bobby init --preset {} --force`",
                preset.as_str(),
                beyond.join(", "),
                preset.as_str()
            ),
        };
    }
    let holds_admin = held.contains(&"authority:admin");
    DoctorCheck {
        status: DoctorStatus::Ok,
        name: "bootstrap-preset".to_string(),
        detail: match preset {
            bootstrap_local::BootstrapPreset::Unrestricted if holds_admin => {
                "unrestricted (includes authority:admin)".to_string()
            }
            bootstrap_local::BootstrapPreset::Unrestricted => {
                "unrestricted (authority:admin not present; heal will add it)".to_string()
            }
            _ => format!("{} ({})", preset.as_str(), floor.summary()),
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

/// The probe verdict, pure so the budget comparison is unit-testable: a
/// failed round-trip warns; a round-trip over the configured
/// `[vision].proposeBudgetMs` warns; otherwise the check is green and names
/// the budget it was measured against.
fn vision_probe_verdict(probe: Option<Duration>, budget_ms: Option<u64>) -> DoctorCheck {
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

fn check_vision_readiness(vision: &VisionConfig) -> Option<DoctorCheck> {
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

fn configured_storage_dirs(config: &AppConfig) -> [(&'static str, PathBuf); 4] {
    [
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
    ]
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

/// Whether the configured CDP port is serving the gateway, sitting free, or
/// already owned by something else.
///
/// The default port is 9222, which is also Firefox's default remote-debugging
/// port, so an occupied port is the common first-run failure — and it surfaces
/// only as a bind error at startup. Reporting it here names the collision
/// before `bobby cdp` is ever run.
/// What the CDP port probe found, for checks that need the outcome itself
/// rather than the wording of its message.
#[derive(Clone, Copy, PartialEq, Eq)]
enum CdpPortState {
    /// Authenticated CDP discovery answered; `bobby cdp` owns the port.
    Serving,
    /// Nothing is listening; `bobby cdp` can bind it.
    Free,
    /// Something that is not this gateway holds the port.
    Occupied,
}

fn check_cdp_port(config: &config::CdpConfig) -> (DoctorCheck, CdpPortState) {
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

fn cdp_port_accepts_connections(host: &str, port: u16) -> bool {
    use std::net::ToSocketAddrs;
    let Ok(addresses) = (host, port).to_socket_addrs() else {
        return false;
    };
    addresses.into_iter().any(|address| {
        std::net::TcpStream::connect_timeout(&address, Duration::from_millis(500)).is_ok()
    })
}

fn probe_cdp_discovery(url: &str) -> Result<u16> {
    let url = url.to_owned();
    match std::thread::spawn(move || probe_http_status_blocking(&url)).join() {
        Ok(result) => result,
        Err(_) => anyhow::bail!("cdp discovery probe thread panicked"),
    }
}

fn probe_http_status_blocking(url: &str) -> Result<u16> {
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

/// The companion never binds its configured port: the OS picks a free one at
/// every start and the native-host descriptor publishes it.
fn check_companion_port(bind: SocketAddr) -> DoctorCheck {
    let detail = if bind.port() == 0 {
        format!(
            "the runtime binds a free loopback port on {} at every start",
            bind.ip()
        )
    } else {
        format!(
            "the runtime binds a free loopback port on {} at every start; configured port {} is not used",
            bind.ip(),
            bind.port()
        )
    };
    DoctorCheck {
        status: DoctorStatus::Ok,
        name: "companion-port".to_string(),
        detail,
    }
}

/// Why a WebDriver BiDi probe failed.
///
/// A refused connection and a live socket speaking something else need opposite
/// repairs. Reporting both as "another service may own the port" told operators
/// a port was taken when in fact nothing was listening on it.
enum BidiProbeFailure {
    /// Nothing accepted the connection at that address.
    Unreachable(anyhow::Error),
    /// Something answered, but not with a WebSocket upgrade.
    NotBidi(anyhow::Error),
}

impl std::fmt::Display for BidiProbeFailure {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(error) | Self::NotBidi(error) => write!(formatter, "{error:#}"),
        }
    }
}

fn probe_firefox_bidi(endpoint: &str) -> std::result::Result<(), BidiProbeFailure> {
    use BidiProbeFailure::{NotBidi, Unreachable};
    let probe = || -> Result<std::net::TcpStream> {
        let url = Url::parse(endpoint).context("invalid WebDriver BiDi URL")?;
        if url.scheme() != "ws" {
            anyhow::bail!("doctor currently probes loopback ws:// BiDi endpoints only");
        }
        let port = url
            .port_or_known_default()
            .context("BiDi URL has no port")?;
        let address = url
            .socket_addrs(|| Some(port))?
            .into_iter()
            .next()
            .context("BiDi host resolved to no addresses")?;
        Ok(std::net::TcpStream::connect_timeout(
            &address,
            Duration::from_millis(500),
        )?)
    };
    let mut stream = probe().map_err(Unreachable)?;
    let mut handshake = || -> Result<String> {
        let url = Url::parse(endpoint)?;
        let host = url.host_str().context("BiDi URL has no host")?.to_owned();
        let port = url
            .port_or_known_default()
            .context("BiDi URL has no port")?;
        stream.set_read_timeout(Some(Duration::from_secs(1)))?;
        stream.set_write_timeout(Some(Duration::from_secs(1)))?;
        let path = if url.path().is_empty() {
            "/".to_owned()
        } else {
            url.path().to_owned()
        };
        write!(
            stream,
            "GET {path} HTTP/1.1\r\nHost: {host}:{port}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n"
        )?;
        let mut response = [0_u8; 4096];
        let read = stream.read(&mut response)?;
        let head = String::from_utf8_lossy(&response[..read]);
        Ok(head.lines().next().unwrap_or("empty response").to_owned())
    };
    let status = handshake().map_err(NotBidi)?;
    if !status.contains(" 101 ") {
        return Err(NotBidi(anyhow::anyhow!(
            "WebSocket handshake returned {status}"
        )));
    }
    Ok(())
}

fn sidecar_versions(
    mcp: Option<&Path>,
    acp: Option<&Path>,
) -> Result<(Option<String>, Option<String>), String> {
    let read = |path: Option<&Path>| match path {
        None => Ok(None),
        Some(path) => onboarding::sidecar_version(path)
            .map(Some)
            .map_err(|error| format!("{}: {error:#}", path.display())),
    };
    Ok((read(mcp)?, read(acp)?))
}

fn block_on_inspect<T, F>(fut: F) -> T
where
    F: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => tokio::task::block_in_place(|| handle.block_on(fut)),
        Err(_) => std::thread::spawn(move || {
            tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("inspect runtime")
                .block_on(fut)
        })
        .join()
        .expect("inspect thread panicked"),
    }
}

struct JsonlHealth {
    exists: bool,
    records: usize,
    bytes: u64,
    torn_tail: bool,
    incompatible_records: usize,
    corrupt_line: Option<usize>,
}

fn record_jsonl_health(report: &mut DoctorReport, name: &str, path: &Path, health: JsonlHealth) {
    if !health.exists {
        report.ok(name, format!("{} · not created yet", path.display()));
        return;
    }
    if let Some(line) = health.corrupt_line {
        report.fail(name, format!("corrupt line {line} in {}", path.display()));
        return;
    }
    if health.torn_tail {
        report.warn(
            name,
            format!(
                "torn tail · {} records · {} bytes; run `bobby doctor --fix`",
                health.records, health.bytes
            ),
        );
        return;
    }
    if health.incompatible_records > 0 && name == "scheduler-journal" {
        report.fail(
            name,
            format!(
                "{} unreadable records; scheduler history is read-only and requires repair",
                health.incompatible_records
            ),
        );
        return;
    }
    if health.incompatible_records > 0 {
        report.warn(
            name,
            format!(
                "{} unreadable records skipped in {}",
                health.incompatible_records,
                path.display()
            ),
        );
        return;
    }
    report.ok(
        name,
        format!(
            "{} · {} records · {} bytes",
            path.display(),
            health.records,
            health.bytes
        ),
    );
}

fn record_command_journal(report: &mut DoctorReport, path: &Path) {
    let path_buf = path.to_path_buf();
    match block_on_inspect(async move { workflow_journal::JsonlJournal::inspect(path_buf).await }) {
        Ok(health) => record_jsonl_health(
            report,
            "command-journal",
            path,
            JsonlHealth {
                exists: health.exists,
                records: health.records,
                bytes: health.bytes,
                torn_tail: health.torn_tail,
                incompatible_records: health.incompatible_records,
                corrupt_line: None,
            },
        ),
        Err(error) => report.fail("command-journal", format!("{error:#}")),
    }
}

fn record_scheduler_journal(report: &mut DoctorReport, path: &Path) {
    let path_buf = path.to_path_buf();
    match block_on_inspect(async move { task_scheduler::JournalJobStore::inspect(path_buf).await })
    {
        Ok(health) => record_jsonl_health(
            report,
            "scheduler-journal",
            path,
            JsonlHealth {
                exists: health.exists,
                records: health.records,
                bytes: health.bytes,
                torn_tail: health.torn_tail,
                incompatible_records: health.incompatible_records,
                corrupt_line: None,
            },
        ),
        Err(error) => report.fail("scheduler-journal", format!("{error:#}")),
    }
}

fn record_vision_corpus(report: &mut DoctorReport, path: &Path) {
    match intent_engine::VisionCorpus::inspect(path) {
        Ok(health) => record_jsonl_health(
            report,
            "vision-corpus",
            path,
            JsonlHealth {
                exists: health.exists,
                records: health.records,
                bytes: health.bytes,
                torn_tail: health.torn_tail,
                incompatible_records: 0,
                corrupt_line: health.corrupt_line,
            },
        ),
        Err(error) => report.fail("vision-corpus", format!("{error}")),
    }
}

fn record_jobs_queue(
    report: &mut DoctorReport,
    config: &AppConfig,
    bootstrap: Option<&Path>,
) -> Option<types::RuntimeInfo> {
    let bootstrap = bootstrap.unwrap_or_else(|| Path::new(""));
    let bearer = match crate::jobs_client::resolve_jobs_auth(None, bootstrap) {
        Ok(bearer) => bearer,
        Err(error) => {
            report.warn(
                "jobs-queue",
                format!("no bearer to read runtime ({error:#})"),
            );
            return None;
        }
    };
    let url = match crate::v1_client::v1_url(
        &format!("http://{}:{}", config.server.host, config.server.port),
        "/v1/runtime",
    ) {
        Ok(url) => url,
        Err(error) => {
            report.warn("jobs-queue", format!("{error:#}"));
            return None;
        }
    };
    match crate::v1_client::v1_request_with_limits(
        crate::v1_client::V1Request {
            method: reqwest::Method::GET,
            url,
            bearer,
            body: None,
            idempotency_key: None,
        },
        Duration::from_secs(1),
        chrono::Duration::seconds(5),
    ) {
        Ok(response) if response.status.as_u16() == 401 => {
            report.warn(
                "jobs-queue",
                "credential cannot read runtime (HTTP 401)".to_string(),
            );
            None
        }
        Ok(response) if response.status.is_success() => {
            match serde_json::from_str::<types::RuntimeInfo>(&response.body) {
                Ok(info) => {
                    report.ok(
                        "jobs-queue",
                        format!(
                            "queued_jobs={} · sessions={} · uptime_ms={}",
                            info.queued_jobs, info.active_sessions, info.uptime_ms
                        ),
                    );
                    Some(info)
                }
                Err(error) => {
                    report.warn("jobs-queue", format!("GET /v1/runtime: {error:#}"));
                    None
                }
            }
        }
        Ok(response) => {
            report.warn(
                "jobs-queue",
                format!("GET /v1/runtime HTTP {}", response.status),
            );
            None
        }
        Err(error) => {
            report.warn("jobs-queue", format!("{error:#}"));
            None
        }
    }
}

/// Operator-facing SLO enforcement: provider health from `/v1/runtime`, plus
/// the `[observability.slo]` objectives evaluated against the runtime's
/// operational metrics. Unset objectives are not evaluated.
fn record_operational_slos(
    report: &mut DoctorReport,
    config: &AppConfig,
    info: Option<&types::RuntimeInfo>,
) {
    let Some(info) = info else {
        report.warn(
            "provider-health",
            "runtime unreadable; provider health and SLOs not evaluated".to_string(),
        );
        return;
    };
    if info.storage_integrity.is_empty() {
        report.ok(
            "storage-integrity",
            "runtime storage has no reported integrity issue".into(),
        );
    } else {
        report.fail(
            "storage-integrity",
            "durable history requires repair; affected mutations are disabled".into(),
        );
    }
    match &info.provider_health {
        None => report.ok(
            "provider-health",
            "no vision provider configured".to_string(),
        ),
        Some(modes) if modes.is_empty() => report.ok(
            "provider-health",
            "no provider calls recorded yet".to_string(),
        ),
        Some(modes) => {
            let unhealthy: Vec<&str> = modes
                .iter()
                .filter(|mode| mode.status == types::ProviderHealthStatus::Unhealthy)
                .map(|mode| mode.provider_mode.as_str())
                .collect();
            let degraded: Vec<&str> = modes
                .iter()
                .filter(|mode| mode.status == types::ProviderHealthStatus::Degraded)
                .map(|mode| mode.provider_mode.as_str())
                .collect();
            if !unhealthy.is_empty() {
                report.fail(
                    "provider-health",
                    format!(
                        "unhealthy: {} (consecutive failures reached threshold)",
                        unhealthy.join(", ")
                    ),
                );
            } else if !degraded.is_empty() {
                report.warn(
                    "provider-health",
                    format!(
                        "degraded: {} (consecutive propose-budget violations reached threshold)",
                        degraded.join(", ")
                    ),
                );
            } else {
                report.ok(
                    "provider-health",
                    modes
                        .iter()
                        .map(|mode| {
                            format!(
                                "{} ok ({} ok / {} failed)",
                                mode.provider_mode, mode.successes, mode.failures
                            )
                        })
                        .collect::<Vec<_>>()
                        .join(" · "),
                );
            }
        }
    }
    let violations: u64 = info
        .provider_health
        .as_deref()
        .unwrap_or_default()
        .iter()
        .map(|mode| mode.budget_violations)
        .sum();
    if let Some(budget_ms) = config.vision.propose_budget_ms {
        if violations > 0 {
            report.warn(
                "slo-vision-latency-budget",
                format!("{violations} propose round-trips exceeded the {budget_ms}ms budget"),
            );
        } else {
            report.ok(
                "slo-vision-latency-budget",
                format!("no propose round-trip exceeded the {budget_ms}ms budget"),
            );
        }
    }
    let slo = &config.observability.slo;
    let Some(metrics) = &info.operational_metrics else {
        if slo.vision_max_failure_rate.is_some() || slo.vision_min_acceptance_rate.is_some() {
            report.warn(
                "slo-vision-rates",
                "runtime reported no operational metrics; SLO rates not evaluated".to_string(),
            );
        }
        return;
    };
    let attempted = metrics.vision.attempted;
    if let Some(max_failure_rate) = slo.vision_max_failure_rate {
        if attempted == 0 {
            report.ok(
                "slo-vision-failure-rate",
                "no vision proposals observed".to_string(),
            );
        } else {
            let failures = metrics.vision.failed + metrics.vision.timed_out;
            let rate = failures as f64 / attempted as f64;
            if rate > max_failure_rate {
                report.fail(
                    "slo-vision-failure-rate",
                    format!("{failures}/{attempted} failed or timed out ({rate:.2} > {max_failure_rate:.2})"),
                );
            } else {
                report.ok(
                    "slo-vision-failure-rate",
                    format!("{failures}/{attempted} failed or timed out ({rate:.2} <= {max_failure_rate:.2})"),
                );
            }
        }
    }
    if let Some(min_acceptance_rate) = slo.vision_min_acceptance_rate {
        if attempted == 0 {
            report.ok(
                "slo-vision-acceptance-rate",
                "no vision proposals observed".to_string(),
            );
        } else {
            let rate = metrics.vision.accepted as f64 / attempted as f64;
            if rate < min_acceptance_rate {
                report.fail(
                    "slo-vision-acceptance-rate",
                    format!(
                        "{}/{} accepted ({rate:.2} < {min_acceptance_rate:.2})",
                        metrics.vision.accepted, attempted
                    ),
                );
            } else {
                report.ok(
                    "slo-vision-acceptance-rate",
                    format!(
                        "{}/{} accepted ({rate:.2} >= {min_acceptance_rate:.2})",
                        metrics.vision.accepted, attempted
                    ),
                );
            }
        }
    }
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
mod host_config_tests {
    use super::*;

    #[test]
    fn doctor_reports_and_repairs_drifted_host_entries() {
        let _lock = onboarding::INSTALL_ENV_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let path =
            onboarding::merge_host_config(onboarding::HostKind::Claude, root.path()).unwrap();
        let mut config: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        config["mcpServers"]["bobby-browser"]["args"] = serde_json::json!(["serve"]);
        std::fs::write(&path, serde_json::to_vec_pretty(&config).unwrap()).unwrap();

        let mut report = DoctorReport::default();
        record_host_config_checks(&mut report, root.path());
        assert_eq!(
            report.check("host-claude").unwrap().status,
            DoctorStatus::Fail
        );
        assert!(
            report
                .check("host-claude")
                .unwrap()
                .detail
                .contains("fix: bobby doctor --fix"),
            "{}",
            report.check("host-claude").unwrap().detail
        );

        let actions = repair_host_configs(root.path());
        let action = actions
            .iter()
            .find(|action| action.name == "host-claude")
            .unwrap();
        assert_eq!(action.status, DoctorFixStatus::Fixed);

        let mut repaired = DoctorReport::default();
        record_host_config_checks(&mut repaired, root.path());
        assert_eq!(
            repaired.check("host-claude").unwrap().status,
            DoctorStatus::Ok
        );
    }
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
        assert!(
            matches!(error, BidiProbeFailure::NotBidi(_)),
            "a live socket answering HTTP is not an unreachable endpoint: {error}"
        );
        assert!(error.to_string().contains("404"), "{error}");
        server.join().unwrap();
    }

    /// A refused connection is nobody listening, which is the opposite of the
    /// port being owned. Classifying both the same way sent operators hunting
    /// for a process that was never there.
    #[test]
    fn firefox_bidi_probe_separates_a_refused_connection_from_a_wrong_protocol() {
        let port = std::net::TcpListener::bind("127.0.0.1:0")
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let error = probe_firefox_bidi(&format!("ws://127.0.0.1:{port}/session")).unwrap_err();
        assert!(
            matches!(error, BidiProbeFailure::Unreachable(_)),
            "nothing is listening, so this is unreachable, not a protocol mismatch: {error}"
        );
    }
}

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

#[cfg(test)]
mod slo_tests {
    use super::*;

    fn health_snapshot(
        status: types::ProviderHealthStatus,
        budget_violations: u64,
    ) -> types::ProviderHealthSnapshot {
        types::ProviderHealthSnapshot {
            provider_mode: "http".to_string(),
            status,
            successes: 8,
            failures: 2,
            consecutive_failures: 0,
            budget_violations,
            last_latency_ms: Some(120),
            latency_budget_ms: Some(1_500),
            failure_threshold: 3,
        }
    }

    fn runtime_info(
        provider_health: Option<Vec<types::ProviderHealthSnapshot>>,
        metrics: &observability::OperationalMetrics,
    ) -> types::RuntimeInfo {
        types::RuntimeInfo {
            storage_integrity: Vec::new(),
            version: "0.14.0".to_string(),
            capabilities: Vec::new(),
            active_sessions: 0,
            queued_jobs: 0,
            uptime_ms: 1,
            vision_propose_budget_ms: None,
            operational_metrics: Some(metrics.snapshot()),
            provider_health,
        }
    }

    fn config_with_slo(
        max_failure: Option<f64>,
        min_acceptance: Option<f64>,
        budget_ms: Option<u64>,
    ) -> AppConfig {
        let mut config = AppConfig::default();
        config.observability.slo.vision_max_failure_rate = max_failure;
        config.observability.slo.vision_min_acceptance_rate = min_acceptance;
        config.vision.propose_budget_ms = budget_ms;
        config
    }

    fn record_proposals(
        metrics: &observability::OperationalMetrics,
        outcome: observability::VisionProposalOutcome,
        count: u64,
    ) {
        for _ in 0..count {
            metrics.record_vision_proposal(observability::VisionProposalMetric {
                provider_mode: observability::ProviderMode::Http,
                latency_ms: 100,
                confidence: None,
                outcome,
            });
        }
    }

    #[test]
    fn unhealthy_provider_fails_doctor() {
        let mut report = DoctorReport::default();
        let info = runtime_info(
            Some(vec![health_snapshot(
                types::ProviderHealthStatus::Unhealthy,
                0,
            )]),
            &observability::OperationalMetrics::default(),
        );
        record_operational_slos(&mut report, &config_with_slo(None, None, None), Some(&info));
        let check = report.check("provider-health").expect("recorded");
        assert_eq!(check.status, DoctorStatus::Fail, "{}", check.detail);
        assert!(check.detail.contains("http"), "{}", check.detail);
    }

    #[test]
    fn degraded_provider_warns_and_healthy_is_ok() {
        let mut report = DoctorReport::default();
        let info = runtime_info(
            Some(vec![health_snapshot(
                types::ProviderHealthStatus::Degraded,
                2,
            )]),
            &observability::OperationalMetrics::default(),
        );
        record_operational_slos(&mut report, &config_with_slo(None, None, None), Some(&info));
        assert_eq!(
            report.check("provider-health").unwrap().status,
            DoctorStatus::Warn
        );

        let mut report = DoctorReport::default();
        let info = runtime_info(
            Some(vec![health_snapshot(
                types::ProviderHealthStatus::Healthy,
                0,
            )]),
            &observability::OperationalMetrics::default(),
        );
        record_operational_slos(&mut report, &config_with_slo(None, None, None), Some(&info));
        assert_eq!(
            report.check("provider-health").unwrap().status,
            DoctorStatus::Ok
        );
    }

    #[test]
    fn missing_provider_health_reads_as_no_provider_configured() {
        let mut report = DoctorReport::default();
        let info = runtime_info(None, &observability::OperationalMetrics::default());
        record_operational_slos(&mut report, &config_with_slo(None, None, None), Some(&info));
        let check = report.check("provider-health").unwrap();
        assert_eq!(check.status, DoctorStatus::Ok);
        assert!(
            check.detail.contains("no vision provider"),
            "{}",
            check.detail
        );
    }

    #[test]
    fn unreadable_runtime_warns_instead_of_failing() {
        let mut report = DoctorReport::default();
        record_operational_slos(&mut report, &config_with_slo(Some(0.1), None, None), None);
        assert_eq!(
            report.check("provider-health").unwrap().status,
            DoctorStatus::Warn
        );
        assert_eq!(report.failures(), 0);
    }

    #[test]
    fn vision_failure_rate_slo_fails_only_when_breached() {
        let metrics = observability::OperationalMetrics::default();
        record_proposals(&metrics, observability::VisionProposalOutcome::Accepted, 3);
        record_proposals(&metrics, observability::VisionProposalOutcome::Failed, 1);
        let info = runtime_info(None, &metrics);

        let mut report = DoctorReport::default();
        record_operational_slos(
            &mut report,
            &config_with_slo(Some(0.5), None, None),
            Some(&info),
        );
        assert_eq!(
            report.check("slo-vision-failure-rate").unwrap().status,
            DoctorStatus::Ok
        );

        let mut report = DoctorReport::default();
        record_operational_slos(
            &mut report,
            &config_with_slo(Some(0.1), None, None),
            Some(&info),
        );
        assert_eq!(
            report.check("slo-vision-failure-rate").unwrap().status,
            DoctorStatus::Fail
        );
    }

    #[test]
    fn vision_acceptance_rate_slo_fails_below_the_floor() {
        let metrics = observability::OperationalMetrics::default();
        record_proposals(&metrics, observability::VisionProposalOutcome::Accepted, 1);
        record_proposals(&metrics, observability::VisionProposalOutcome::Rejected, 3);
        let info = runtime_info(None, &metrics);

        let mut report = DoctorReport::default();
        record_operational_slos(
            &mut report,
            &config_with_slo(None, Some(0.5), None),
            Some(&info),
        );
        assert_eq!(
            report.check("slo-vision-acceptance-rate").unwrap().status,
            DoctorStatus::Fail
        );

        let mut report = DoctorReport::default();
        record_operational_slos(
            &mut report,
            &config_with_slo(None, Some(0.2), None),
            Some(&info),
        );
        assert_eq!(
            report.check("slo-vision-acceptance-rate").unwrap().status,
            DoctorStatus::Ok
        );
    }

    #[test]
    fn latency_budget_violations_warn_without_failing() {
        let mut report = DoctorReport::default();
        let info = runtime_info(
            Some(vec![health_snapshot(
                types::ProviderHealthStatus::Healthy,
                5,
            )]),
            &observability::OperationalMetrics::default(),
        );
        record_operational_slos(
            &mut report,
            &config_with_slo(None, None, Some(1_500)),
            Some(&info),
        );
        let check = report.check("slo-vision-latency-budget").unwrap();
        assert_eq!(check.status, DoctorStatus::Warn, "{}", check.detail);
        assert!(check.detail.contains('5'), "{}", check.detail);

        let mut report = DoctorReport::default();
        let info = runtime_info(
            Some(vec![health_snapshot(
                types::ProviderHealthStatus::Healthy,
                0,
            )]),
            &observability::OperationalMetrics::default(),
        );
        record_operational_slos(
            &mut report,
            &config_with_slo(None, None, Some(1_500)),
            Some(&info),
        );
        assert_eq!(
            report.check("slo-vision-latency-budget").unwrap().status,
            DoctorStatus::Ok
        );
    }

    #[test]
    fn unset_slos_are_not_evaluated() {
        let mut report = DoctorReport::default();
        let metrics = observability::OperationalMetrics::default();
        record_proposals(&metrics, observability::VisionProposalOutcome::Failed, 10);
        let info = runtime_info(None, &metrics);
        record_operational_slos(&mut report, &config_with_slo(None, None, None), Some(&info));
        assert!(report.check("slo-vision-failure-rate").is_none());
        assert!(report.check("slo-vision-acceptance-rate").is_none());
        assert!(report.check("slo-vision-latency-budget").is_none());
    }
}
