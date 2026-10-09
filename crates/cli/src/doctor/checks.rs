//! Ordered diagnostic checks and explicit prerequisites.
//! A check may emit several related report findings (for example one per
//! enrolled Firefox profile). Prerequisites govern execution, not severity.
use super::*;

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

fn host_configuration(_context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    record_host_config_checks(report, &std::env::current_dir()?);
    Ok(())
}

fn cli_path(_context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    record_cli_path_check(report);

    Ok(())
}

fn configuration(context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    let config_path = context.config_path.clone();
    context.config = match AppConfig::load(&config_path) {
        Ok(mut config) => {
            crate::runtime_scopes::use_owner_address(&mut config);
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

    Ok(())
}

fn deployment_profile(context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    let bootstrap_path = context.bootstrap_path.clone();
    let config = &context.config;
    let profile = context.profile;
    if let (Some(profile), Some(config)) = (profile, config.as_ref()) {
        let profile_name = profile.contract().name;
        report.ok("deployment-profile", profile_name.to_string());
        for check in crate::deployment_profiles::evaluate(
            profile,
            config,
            bootstrap_path.as_deref().is_some_and(Path::exists),
        ) {
            let name = format!("deployment-{}", check.name);
            if check.ok {
                report.ok(&name, check.detail);
            } else {
                report.fail(&name, check.detail);
            }
        }
    }

    Ok(())
}

fn vision_and_gateway_configuration(
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

fn browser_selection(context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    context.selection = match resolve_browser_selection() {
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

    Ok(())
}

fn context_store(context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    let config = &context.config;
    // Context store: reported without claiming the single-writer lock, so
    // doctor is safe against a live runtime. Lockfile present means a writer
    // holds it (or one crashed); that is lock health, not an error.
    {
        let limits = config
            .as_ref()
            .map(|config| config.context.limits)
            .unwrap_or_default();
        let root = config
            .as_ref()
            .and_then(|config| config.context.dir.clone())
            .or_else(|| default_context_dir().ok());
        match root {
            Some(root) if root.is_dir() => {
                let mut sites = 0_u64;
                let mut bytes = 0_u64;
                let mut locked = false;
                let mut invalid_json: Option<(PathBuf, String)> = None;
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
                                    if invalid_json.is_none() {
                                        if let Err(reason) =
                                            context_store::inspect_site_file(&file.path(), limits)
                                        {
                                            invalid_json = Some((
                                                file.path(),
                                                reason.chars().take(512).collect(),
                                            ));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                if let Some((path, reason)) = invalid_json {
                    if reason.contains("limit") {
                        report.warn("context-store", format!("{}: {reason}; file preserved; increase [context.limits] only if appropriate", path.display()));
                    } else {
                        report.fail(
                            "context-store",
                            format!("invalid JSON in {} ({reason})", path.display()),
                        );
                    }
                } else {
                    let lock = if locked { "lock held" } else { "lock free" };
                    report.ok(
                        "context-store",
                        format!(
                            "{} · {} site files · {} bytes · {lock} · cache limits: {} sites / {} accounted bytes; site limits: {} bytes / {} records",
                            root.display(),
                            sites,
                            bytes, limits.max_resident_sites, limits.max_resident_bytes,
                            limits.max_file_bytes, limits.max_site_records
                        ),
                    );
                }
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

    Ok(())
}

fn firefox_endpoints(context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    let config = &context.config;
    let selection = &context.selection;
    // Enrolled BiDi endpoints that nothing is listening on. Held so the CDP-port
    // check below can join the two halves: a companion answering on the CDP port
    // while its enrolled port is dead is one fault, not two warnings.

    if let (Some(config), Some(selection)) = (&config, &selection) {
        match compose_worker_factory(config, selection.clone()) {
            Ok(_) => report.ok(
                "engine-satisfiability",
                "engine preference can be satisfied by configured registrations".to_string(),
            ),
            Err(error) => {
                let (status, name, detail) = engine_composition_finding(
                    &error,
                    crate::runtime_scopes::current_origin(),
                    selection.firefox.is_empty(),
                );
                report.record(status, name, detail);
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
                        Err(BidiProbeFailure::Unreachable(error)) => {
                            context.unreachable_bidi.push(profile.bidi_url.clone());
                            report.warn(
                                "firefox-bidi",
                                format!(
                                    "nothing is listening on {} ({error:#}); the enrolled Firefox companion is not running there",
                                    profile.bidi_url,
                                ),
                            );
                        }
                        Err(failure @ BidiProbeFailure::NotBidi(_)) => {
                            report.warn(
                                "firefox-bidi",
                                format!(
                                    "{} answered but is not a Firefox WebDriver BiDi endpoint ({failure}); another service may own the port",
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
            match profile.companion_bind.parse::<SocketAddr>() {
                Ok(bind) => {
                    let companion_check = check_companion_port(bind);
                    push_doctor_check(report, companion_check);
                }
                Err(_) => {
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
    }

    Ok(())
}

fn bootstrap_capabilities(context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    let config = &context.config;
    // The bootstrap expiry is pinned into MCP client config and the stdio
    // gateway refuses to start once it passes, which a host reports only as a
    // dead server. Warn while there is still time to run `bobby init`.
    context.bootstrap_path_for_heal = resolve_bootstrap_path(context.bootstrap_cli.clone()).ok();
    let bootstrap_path_for_heal = context.bootstrap_path_for_heal.clone();
    if let Some(path) = bootstrap_path_for_heal.as_ref() {
        if path.exists() {
            match bootstrap_local::load_bootstrap_capabilities_csv(path) {
                Ok(caps) => {
                    let preset = bootstrap_local::read_bootstrap_preset(Some(path));
                    let floor = bootstrap_local::capabilities_for_preset(preset);
                    match bootstrap_local::union_capabilities_csv_with(&caps, floor) {
                        Ok((_, added)) if added.is_empty() => {
                            report.ok(
                                "bootstrap-capabilities",
                                "current (matches defaults)".to_string(),
                            );
                        }
                        Ok((_, added)) => {
                            report.warn(
                                "bootstrap-capabilities",
                                format!(
                                    "missing default(s): {}; run `bobby doctor --fix`",
                                    added.join(", ")
                                ),
                            );
                        }
                        Err(error) => {
                            report.warn(
                                "bootstrap-capabilities",
                                format!("could not read capabilities ({error:#})"),
                            );
                        }
                    }
                    if preset
                        .capability_preset()
                        .contains(types::Capability::BrowserFingerprint)
                        && !caps.split(',').any(|c| c.trim() == "browser:fingerprint")
                    {
                        report.warn(
                            "bootstrap-capabilities",
                            "bootstrap lacks browser:fingerprint; run `bobby doctor --fix`"
                                .to_string(),
                        );
                    }
                }
                Err(error) => {
                    report.warn(
                        "bootstrap-capabilities",
                        format!("could not read capabilities ({error:#})"),
                    );
                }
            }
        }
    } else if let Ok(caps) = std::env::var("AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES") {
        let preset = bootstrap_local::read_bootstrap_preset(None);
        let floor = bootstrap_local::capabilities_for_preset(preset);
        if let Ok((_, added)) = bootstrap_local::union_capabilities_csv_with(&caps, floor) {
            if added.is_empty() {
                report.ok(
                    "bootstrap-capabilities",
                    "current (matches defaults)".to_string(),
                );
            } else {
                report.warn(
                    "bootstrap-capabilities",
                    format!(
                        "missing default(s): {}; run `bobby doctor --fix`",
                        added.join(", ")
                    ),
                );
            }
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
            push_doctor_check(report, check);
        }
    }
    if let Some(check) = check_vision_session_gate(holds_vision_assist) {
        push_doctor_check(report, check);
    }
    if let Some(check) = check_javascript_session_gate(holds_javascript_evaluate) {
        push_doctor_check(report, check);
    }
    push_doctor_check(report, check_builtin_job_handlers());

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
            report,
            check_bootstrap_preset(
                bootstrap_path_for_heal.as_deref(),
                caps_for_preset.as_deref(),
            ),
        );
    }

    Ok(())
}

fn credential(context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    let bootstrap_cli = context.bootstrap_cli.clone();
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

    Ok(())
}

fn sidecars(_context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    // Sidecar gateways must sit beside bobby (or on PATH) for mcp-stdio /
    // acp-stdio. Missing binaries are a warning with an install hint.
    let mcp_bin = onboarding::find_sidecar_binary(onboarding::mcp_gateway_command());
    let acp_bin = onboarding::find_sidecar_binary(onboarding::acp_gateway_command());
    for (name, command, path) in [
        (
            "mcp-gateway",
            onboarding::mcp_gateway_command(),
            mcp_bin.as_deref(),
        ),
        (
            "acp-gateway",
            onboarding::acp_gateway_command(),
            acp_bin.as_deref(),
        ),
    ] {
        match path {
            Some(path) => report.ok(name, path.display().to_string()),
            None => report.warn(
                name,
                format!(
                    "{command} not found next to bobby or on PATH; install with `bobby install --cli`, re-run scripts/install.sh, or `cargo build -p {command} --release`"
                ),
            ),
        }
    }
    match sidecar_versions(mcp_bin.as_deref(), acp_bin.as_deref()) {
        Ok((mcp, acp)) => {
            if let Some(check) =
                sidecar_version_status(env!("CARGO_PKG_VERSION"), mcp.as_deref(), acp.as_deref())
            {
                push_doctor_check(report, check);
            }
        }
        Err(detail) => report.fail("sidecar-version", detail),
    }

    Ok(())
}

fn mcp_handshake(context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    let config_path = context.config_path.clone();
    let bootstrap_path = context.bootstrap_path.clone();
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
    // without this, doctor always probes explore defaults while agent
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

    Ok(())
}

fn openshell(context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    let bootstrap_cli = context.bootstrap_cli.clone();
    let config = &context.config;
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

    Ok(())
}

fn storage(context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    let config = &context.config;
    if let Some(config) = &config {
        for (name, dir) in configured_storage_dirs(config) {
            if dir.is_dir() {
                report.ok(name, dir.display().to_string());
            } else {
                report.fail(
                    name,
                    format!("{} missing; run `bobby doctor --fix`", dir.display()),
                );
            }
        }
        record_command_journal(report, &config.storage.journal_path);
        record_scheduler_journal(report, &config.storage.scheduler_journal_path);
        record_idempotency_ledgers(report, config);
        if let Some(dir) = &config.vision.corpus_dir {
            record_vision_corpus(report, &dir.join("vision-corpus.jsonl"));
        }
    }

    Ok(())
}

fn browser_binaries(_context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
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

    Ok(())
}

fn health(context: &mut DoctorContext, report: &mut DoctorReport) -> Result<()> {
    let bootstrap_path_for_heal = context.bootstrap_path_for_heal.clone();
    let config = &context.config;
    let check_health = context.check_health;
    let unreachable_bidi = &context.unreachable_bidi;
    if check_health {
        if let Some(config) = &config {
            let url = format!(
                "http://{}:{}/healthz",
                config.server.host, config.server.port
            );
            match probe_healthz(&url) {
                Ok(()) => {
                    report.ok("healthz", format!("{url} responded"));
                    let info =
                        record_jobs_queue(report, config, bootstrap_path_for_heal.as_deref());
                    record_operational_slos(report, config, info.as_ref());
                }
                Err(_) => {
                    report.ok("healthz", "not running".to_string());
                }
            }
            let (cdp_check, cdp_state) = check_cdp_port(&config.cdp);
            push_doctor_check(report, cdp_check);
            // Two warnings, one fault: something else holds the CDP port and the
            // enrolled BiDi endpoint answers nothing. A companion launched on the
            // CDP port instead of its enrolled one produces exactly this pair,
            // and it leaves every browser call dead -- so it fails, not warns.
            if cdp_state == CdpPortState::Occupied && !unreachable_bidi.is_empty() {
                report.fail(
                    "firefox-bidi-port-mismatch",
                    format!(
                        "{}:{} is held by another service while enrolled BiDi endpoint(s) {} accept \
                         nothing -- a Firefox companion launched on the CDP port rather than its \
                         enrolled port matches this exactly. Relaunch the companion on the enrolled \
                         port (`make firefox-start`); do not re-enroll onto {}:{}, `bobby cdp` \
                         needs that port free.",
                        config.cdp.host,
                        config.cdp.port,
                        unreachable_bidi.join(", "),
                        config.cdp.host,
                        config.cdp.port,
                    ),
                );
            }
        }
    }

    Ok(())
}
