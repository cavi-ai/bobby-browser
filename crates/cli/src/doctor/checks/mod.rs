//! Ordered diagnostic checks and explicit prerequisites.
//! A check may emit several related report findings (for example one per
//! enrolled Firefox profile). Prerequisites govern execution, not severity.
use crate::doctor::DoctorReport;
use crate::{resolve_bootstrap_path, resolve_config_path};
use anyhow::{Context, Result};
use config::AppConfig;
use std::path::PathBuf;

struct DoctorContext {
    config_path: PathBuf,
    bootstrap_cli: Option<PathBuf>,
    bootstrap_path: Option<PathBuf>,
    bootstrap_path_for_heal: Option<PathBuf>,
    config: Option<AppConfig>,
    selection: Option<config::BrowserSelectionConfig>,
    unreachable_bidi: Vec<String>,
    check_health: bool,
    profile: Option<crate::deployment_profiles::DeploymentProfile>,
}

#[derive(Clone, Copy)]
enum Needs {
    Always,
    Config,
    ConfigAndSelection,
    ProfileAndConfig,
    HealthAndConfig,
}

impl Needs {
    fn satisfied(self, context: &DoctorContext) -> bool {
        match self {
            Self::Always => true,
            Self::Config => context.config.is_some(),
            Self::ConfigAndSelection => context.config.is_some() && context.selection.is_some(),
            Self::ProfileAndConfig => context.config.is_some() && context.profile.is_some(),
            Self::HealthAndConfig => context.config.is_some() && context.check_health,
        }
    }
}

struct Check {
    name: &'static str,
    needs: Needs,
    run: fn(&mut DoctorContext, &mut DoctorReport) -> Result<()>,
}

const CHECKS: &[Check] = &[
    Check {
        name: "host_configuration",
        needs: Needs::Always,
        run: host_configuration,
    },
    Check {
        name: "cli_path",
        needs: Needs::Always,
        run: cli_path,
    },
    Check {
        name: "configuration",
        needs: Needs::Always,
        run: configuration,
    },
    Check {
        name: "deployment_profile",
        needs: Needs::ProfileAndConfig,
        run: deployment_profile,
    },
    Check {
        name: "vision_and_gateway_configuration",
        needs: Needs::Config,
        run: vision_and_gateway_configuration,
    },
    Check {
        name: "browser_selection",
        needs: Needs::Always,
        run: browser_selection,
    },
    Check {
        name: "context_store",
        needs: Needs::Always,
        run: context_store,
    },
    Check {
        name: "firefox_endpoints",
        needs: Needs::ConfigAndSelection,
        run: firefox_endpoints,
    },
    Check {
        name: "bootstrap_capabilities",
        needs: Needs::Always,
        run: bootstrap_capabilities,
    },
    Check {
        name: "credential",
        needs: Needs::Always,
        run: credential,
    },
    Check {
        name: "sidecars",
        needs: Needs::Always,
        run: sidecars,
    },
    Check {
        name: "mcp_handshake",
        needs: Needs::Always,
        run: mcp_handshake,
    },
    Check {
        name: "openshell",
        needs: Needs::Always,
        run: openshell,
    },
    Check {
        name: "storage",
        needs: Needs::Config,
        run: storage,
    },
    Check {
        name: "browser_binaries",
        needs: Needs::Always,
        run: browser_binaries,
    },
    Check {
        name: "health",
        needs: Needs::HealthAndConfig,
        run: health,
    },
];

pub(super) fn run(
    config_cli: Option<PathBuf>,
    bootstrap_cli: Option<PathBuf>,
    check_health: bool,
    profile: Option<crate::deployment_profiles::DeploymentProfile>,
) -> Result<DoctorReport> {
    let mut report = DoctorReport::default();
    let mut context = DoctorContext {
        config_path: resolve_config_path(config_cli),
        bootstrap_path: resolve_bootstrap_path(bootstrap_cli.clone()).ok(),
        bootstrap_path_for_heal: None,
        bootstrap_cli,
        config: None,
        selection: None,
        unreachable_bidi: Vec::new(),
        check_health,
        profile,
    };
    for check in CHECKS {
        if check.needs.satisfied(&context) {
            (check.run)(&mut context, &mut report)
                .with_context(|| format!("doctor check {} failed", check.name))?;
        }
    }
    Ok(report)
}

pub(super) mod host_configuration;
use host_configuration::host_configuration;

pub(super) mod cli_path;
use cli_path::cli_path;

pub(super) mod configuration;
use configuration::configuration;

pub(super) mod deployment_profile;
use deployment_profile::deployment_profile;

pub(super) mod vision_and_gateway_configuration;
use vision_and_gateway_configuration::vision_and_gateway_configuration;

pub(super) mod browser_selection;
use browser_selection::browser_selection;

pub(super) mod context_store;
use context_store::context_store;

pub(super) mod firefox_endpoints;
use firefox_endpoints::firefox_endpoints;

pub(super) mod bootstrap_capabilities;
use bootstrap_capabilities::bootstrap_capabilities;

pub(super) mod credential;
use credential::credential;

pub(super) mod sidecars;
use sidecars::sidecars;

pub(super) mod mcp_handshake;
use mcp_handshake::mcp_handshake;

pub(super) mod openshell;
use openshell::openshell;

pub(super) mod storage;
use storage::storage;

pub(super) mod browser_binaries;
use browser_binaries::browser_binaries;

pub(super) mod health;
use health::health;
