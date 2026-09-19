use clap::ValueEnum;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum DeploymentProfile {
    Desktop,
    HeadlessCi,
    Openshell,
    Remote,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct DeploymentProfileContract {
    pub(crate) name: &'static str,
    pub(crate) transport: &'static str,
    pub(crate) authentication: &'static str,
    pub(crate) bind_scope: &'static str,
    pub(crate) browser: &'static str,
    pub(crate) storage: &'static str,
    pub(crate) command: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct DeploymentProfileCheck {
    pub(crate) name: &'static str,
    pub(crate) ok: bool,
    pub(crate) detail: String,
}

impl DeploymentProfile {
    pub(crate) const ALL: [Self; 4] = [
        Self::Desktop,
        Self::HeadlessCi,
        Self::Openshell,
        Self::Remote,
    ];

    pub(crate) const fn contract(self) -> DeploymentProfileContract {
        match self {
            Self::Desktop => DeploymentProfileContract {
                name: "desktop",
                transport: "stdio",
                authentication: "bootstrap",
                bind_scope: "loopback",
                browser: "firefox",
                storage: "durable-local",
                command: "bobby mcp-stdio",
            },
            Self::HeadlessCi => DeploymentProfileContract {
                name: "headless-ci",
                transport: "http",
                authentication: "bootstrap",
                bind_scope: "isolated-runtime",
                browser: "headless",
                storage: "ephemeral-or-mounted",
                command: "bobby serve",
            },
            Self::Openshell => DeploymentProfileContract {
                name: "openshell",
                transport: "streamable-http",
                authentication: "sandbox-principal",
                bind_scope: "loopback",
                browser: "host-managed",
                storage: "host-durable",
                command: "bobby openshell install",
            },
            Self::Remote => DeploymentProfileContract {
                name: "remote",
                transport: "http",
                authentication: "bootstrap",
                bind_scope: "operator-controlled",
                browser: "remote-managed",
                storage: "operator-managed",
                command: "bobby serve --config <path>",
            },
        }
    }
}

pub(crate) fn catalog_json() -> String {
    let values: Vec<_> = DeploymentProfile::ALL
        .into_iter()
        .map(|profile| {
            let contract = profile.contract();
            serde_json::json!({
                "name": contract.name,
                "transport": contract.transport,
                "authentication": contract.authentication,
                "bind_scope": contract.bind_scope,
                "browser": contract.browser,
                "storage": contract.storage,
                "command": contract.command,
            })
        })
        .collect();
    serde_json::to_string_pretty(&values).expect("deployment profile catalog is serializable")
}

pub(crate) fn evaluate(
    profile: DeploymentProfile,
    config: &config::AppConfig,
    bootstrap_exists: bool,
) -> Vec<DeploymentProfileCheck> {
    let contract = profile.contract();
    let loopback = config.server.host == "localhost"
        || config
            .server
            .host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|host| host.is_loopback());
    let bind_ok = match profile {
        DeploymentProfile::Desktop | DeploymentProfile::Openshell => loopback,
        DeploymentProfile::HeadlessCi | DeploymentProfile::Remote => {
            !config.server.host.trim().is_empty()
        }
    };
    let browser_ok = match profile {
        DeploymentProfile::Desktop => true,
        DeploymentProfile::HeadlessCi => config.browser.headless,
        DeploymentProfile::Openshell | DeploymentProfile::Remote => true,
    };
    let storage_ok = [
        config.storage.journal_path.as_path(),
        config.storage.checkpoints_dir.as_path(),
        config.storage.authority_path.as_path(),
        config.storage.scheduler_journal_path.as_path(),
        config.browser.artifacts_dir.as_path(),
    ]
    .into_iter()
    .all(|path| !path.as_os_str().is_empty());
    vec![
        DeploymentProfileCheck {
            name: "transport",
            ok: true,
            detail: format!("{} via {}", contract.transport, contract.command),
        },
        DeploymentProfileCheck {
            name: "auth",
            ok: bootstrap_exists,
            detail: if bootstrap_exists {
                contract.authentication.to_string()
            } else {
                "bootstrap credential is missing".to_string()
            },
        },
        DeploymentProfileCheck {
            name: "bind",
            ok: bind_ok,
            detail: format!("{} ({})", config.server.host, contract.bind_scope),
        },
        DeploymentProfileCheck {
            name: "browser",
            ok: browser_ok,
            detail: format!(
                "{} ({})",
                if config.browser.headless {
                    "headless"
                } else {
                    "headed"
                },
                contract.browser
            ),
        },
        DeploymentProfileCheck {
            name: "storage",
            ok: storage_ok,
            detail: contract.storage.to_string(),
        },
    ]
}

pub(crate) fn print_catalog(json: bool) {
    if json {
        println!("{}", catalog_json());
        return;
    }
    for profile in DeploymentProfile::ALL {
        let contract = profile.contract();
        println!(
            "{}\t{}\t{}\t{}\t{}\t{}\t{}",
            contract.name,
            contract.transport,
            contract.authentication,
            contract.bind_scope,
            contract.browser,
            contract.storage,
            contract.command
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_covers_every_supported_deployment_boundary() {
        let names: Vec<_> = DeploymentProfile::ALL
            .iter()
            .map(|profile| profile.contract().name)
            .collect();
        assert_eq!(names, ["desktop", "headless-ci", "openshell", "remote"]);
    }

    #[test]
    fn contracts_make_transport_auth_bind_browser_and_storage_explicit() {
        for profile in DeploymentProfile::ALL {
            let contract = profile.contract();
            assert!(!contract.transport.is_empty());
            assert!(!contract.authentication.is_empty());
            assert!(!contract.bind_scope.is_empty());
            assert!(!contract.browser.is_empty());
            assert!(!contract.storage.is_empty());
            assert!(!contract.command.is_empty());
        }
    }

    #[test]
    fn catalog_json_is_stable_and_machine_readable() {
        let catalog: serde_json::Value =
            serde_json::from_str(&catalog_json()).expect("catalog JSON");
        assert_eq!(catalog.as_array().unwrap().len(), 4);
        assert_eq!(catalog[1]["name"], "headless-ci");
        assert_eq!(catalog[2]["transport"], "streamable-http");
        assert_eq!(catalog[3]["bind_scope"], "operator-controlled");
    }

    #[test]
    fn profile_checks_enforce_auth_bind_browser_and_storage() {
        let mut config = config::AppConfig::default();
        let desktop = evaluate(DeploymentProfile::Desktop, &config, false);
        assert!(
            !desktop
                .iter()
                .find(|check| check.name == "auth")
                .unwrap()
                .ok
        );
        assert!(
            desktop
                .iter()
                .find(|check| check.name == "bind")
                .unwrap()
                .ok
        );
        assert!(
            desktop
                .iter()
                .find(|check| check.name == "browser")
                .unwrap()
                .ok
        );

        config.browser.headless = true;
        let headless = evaluate(DeploymentProfile::HeadlessCi, &config, true);
        assert!(headless.iter().all(|check| check.ok));

        config.server.host = "0.0.0.0".to_string();
        let container = evaluate(DeploymentProfile::HeadlessCi, &config, true);
        assert!(container.iter().all(|check| check.ok));
        let unsafe_local = evaluate(DeploymentProfile::Openshell, &config, true);
        assert!(
            !unsafe_local
                .iter()
                .find(|check| check.name == "bind")
                .unwrap()
                .ok
        );
        let remote = evaluate(DeploymentProfile::Remote, &config, true);
        assert!(remote.iter().all(|check| check.ok));
    }
}
