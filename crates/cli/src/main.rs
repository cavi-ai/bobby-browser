mod bootstrap_local;
mod jobs_client;
mod onboarding;

use anyhow::{Context, Result};
use companion_core::{run_native_host, NativeHostConfig};
use config::AppConfig;
use firefox_companion::selection::NativeHostDescriptor;
pub use firefox_companion::selection::{
    compose_worker_factory, compose_worker_factory_with_enrolled_firefox,
    compose_worker_factory_with_pairing_observer, parse_selection,
    start_firefox_profile_enrollment, EnrolledFirefoxProfile, FirefoxProfileEnrollmentConfig,
};
use std::{
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use url::Url;

#[derive(Clone)]
pub struct NativeHostInstallConfig {
    pub wrapper_path: PathBuf,
    pub manifest_path: PathBuf,
    pub cli_path: PathBuf,
    pub descriptor_path: PathBuf,
}


#[derive(clap::Parser)]
#[command(name = "bobby", version, about = "bobby-browser automation runtime")]
struct Cli {
    #[command(subcommand)]
    command: Option<CliCommand>,
}

impl Cli {
    fn parse_args() -> Self {
        <Self as clap::Parser>::parse()
    }
}

#[derive(clap::Subcommand)]
enum CliCommand {
    /// Generate a loopback bootstrap credential
    Init {
        /// Overwrite an existing bootstrap file
        #[arg(long)]
        force: bool,
        /// Days until the bootstrap credential expires
        #[arg(long, default_value_t = bootstrap_local::DEFAULT_TTL_DAYS as u32)]
        ttl_days: u32,
        /// Bootstrap env file path
        #[arg(long)]
        path: Option<PathBuf>,
        /// Print an MCP client config fragment for an agent host (claude, zed, vscode, json)
        #[arg(long)]
        emit: Option<onboarding::EmitFormat>,
    },
    /// Run the MCP stdio gateway with the bootstrap credential loaded for you.
    /// This is the command agent hosts should point at: no env wiring needed.
    McpStdio {
        /// Path to bootstrap.env (overrides BOBBY_BROWSER_BOOTSTRAP_ENV)
        #[arg(long)]
        bootstrap_env: Option<PathBuf>,
    },
    /// One-command agent setup: credential, host MCP config, agent skill
    Install {
        /// Host to wire (repeatable; non-interactive when given)
        #[arg(long)]
        host: Vec<onboarding::HostKind>,
        /// Install the agent skill (to ~/.claude/skills/, or the project with --project-skill)
        #[arg(long)]
        skill: bool,
        /// Install the skill into this project's .claude/skills/ instead of user-level
        #[arg(long)]
        project_skill: bool,
        /// Install the Firefox companion (extension, native host, descriptor)
        #[arg(long)]
        companion: bool,
        /// Path to a built companion extension (else built from the repo)
        #[arg(long)]
        extension: Option<PathBuf>,
        /// Regenerate the bootstrap credential even if one exists
        #[arg(long)]
        force: bool,
        /// Run with defaults, no interactive checklist
        #[arg(long)]
        yes: bool,
        /// Bootstrap env file path
        #[arg(long)]
        path: Option<PathBuf>,
    },
    /// Run the runtime server (default)
    Serve {
        /// Path to config.toml (overrides BOBBY_BROWSER_CONFIG)
        #[arg(long)]
        config: Option<PathBuf>,
        /// Path to bootstrap.env (overrides BOBBY_BROWSER_BOOTSTRAP_ENV)
        #[arg(long)]
        bootstrap_env: Option<PathBuf>,
    },
    /// Run the Firefox native-messaging host
    FirefoxNativeHost {
        /// Absolute path to the native-host descriptor JSON
        #[arg(long)]
        descriptor: PathBuf,
    },
    /// Install the Firefox native-host wrapper and manifest
    InstallFirefoxNativeHost {
        #[arg(long)]
        wrapper: PathBuf,
        #[arg(long)]
        manifest: PathBuf,
        #[arg(long)]
        cli: PathBuf,
        #[arg(long)]
        descriptor: PathBuf,
    },
    /// Run a one-time Firefox companion enrollment and print the discovered
    /// profile id as a ready-to-use AUTOMATION_RUNTIME_BROWSER_SELECTION value
    EnrollFirefoxProfile {
        /// Absolute path the native-host descriptor is published to
        #[arg(long)]
        descriptor: PathBuf,
        /// Loopback address the pairing server binds to
        #[arg(long, default_value = "127.0.0.1:9876")]
        bind: SocketAddr,
        /// BiDi WebSocket URL of the running Firefox (e.g. ws://127.0.0.1:9222/session)
        #[arg(long)]
        bidi_url: String,
        /// Firefox profile directory the companion extension runs in
        #[arg(long)]
        profile_dir: PathBuf,
        /// Seconds to wait for the extension to pair
        #[arg(long, default_value_t = 120)]
        timeout_secs: u64,
    },
    /// Check local setup: config, bootstrap credential, storage, browsers
    Doctor {
        /// Path to config.toml (overrides BOBBY_BROWSER_CONFIG)
        #[arg(long)]
        config: Option<PathBuf>,
        /// Path to bootstrap.env (overrides BOBBY_BROWSER_BOOTSTRAP_ENV)
        #[arg(long)]
        bootstrap_env: Option<PathBuf>,
        /// Skip probing GET /healthz on the configured bind address
        #[arg(long)]
        skip_health: bool,
    },
    /// Submit / inspect / cancel broker jobs (`/v1/jobs`)
    Jobs {
        #[command(subcommand)]
        command: JobsCommand,
    },
}

#[derive(clap::Subcommand)]
enum JobsCommand {
    /// POST /v1/jobs
    Submit {
        #[command(flatten)]
        common: JobsCommonArgs,
        /// Job handler name (e.g. echo)
        #[arg(long)]
        name: String,
        /// JSON payload string
        #[arg(long, default_value = "{}")]
        payload: String,
        /// Path to a JSON payload file (overrides --payload)
        #[arg(long)]
        payload_file: Option<PathBuf>,
        /// Job priority
        #[arg(long, value_enum, default_value_t = jobs_client::JobPriorityArg::Normal)]
        priority: jobs_client::JobPriorityArg,
        /// Max retries before permanent failure
        #[arg(long, default_value_t = 3)]
        max_retries: u32,
        /// Optional per-job timeout in milliseconds
        #[arg(long)]
        timeout_ms: Option<u64>,
        /// Optional idempotency key header
        #[arg(long)]
        idempotency_key: Option<String>,
    },
    /// GET /v1/jobs/{id}
    Status {
        #[command(flatten)]
        common: JobsCommonArgs,
        /// Job id
        job_id: String,
    },
    /// DELETE /v1/jobs/{id}
    Cancel {
        #[command(flatten)]
        common: JobsCommonArgs,
        /// Job id
        job_id: String,
    },
}

#[derive(clap::Args)]
struct JobsCommonArgs {
    /// Path to config.toml (overrides BOBBY_BROWSER_CONFIG)
    #[arg(long)]
    config: Option<PathBuf>,
    /// Path to bootstrap.env (overrides BOBBY_BROWSER_BOOTSTRAP_ENV)
    #[arg(long)]
    bootstrap_env: Option<PathBuf>,
    /// Broker base URL (else http://{config.server.host}:{config.server.port})
    #[arg(long)]
    base_url: Option<String>,
    /// Bearer token override (else AUTOMATION_RUNTIME_TOKEN, else bootstrap.env)
    #[arg(long)]
    token: Option<String>,
}

pub async fn run() -> Result<()> {
    match Cli::parse_args().command.unwrap_or(CliCommand::Serve {
        config: None,
        bootstrap_env: None,
    }) {
        CliCommand::Init {
            force,
            ttl_days,
            path,
            emit,
        } => run_init(force, ttl_days, path, emit)?,
        CliCommand::McpStdio { bootstrap_env } => {
            let path = resolve_bootstrap_path(bootstrap_env)?;
            onboarding::exec_mcp_stdio(&path)?;
        }
        CliCommand::Install {
            host,
            skill,
            project_skill,
            companion,
            extension,
            force,
            yes,
            path,
        } => {
            let path = match path {
                Some(path) => path,
                None => bootstrap_local::default_bootstrap_path()?,
            };
            onboarding::run_install(
                &path,
                onboarding::InstallOptions {
                    hosts: host,
                    skill,
                    project_skill,
                    companion,
                    extension,
                    force,
                    yes,
                },
            )?;
        }
        CliCommand::Serve {
            config,
            bootstrap_env,
        } => {
            let config_path = resolve_config_path(config);
            let config_existed = config_path.exists();
            let config = AppConfig::load(&config_path)
                .with_context(|| format!("failed to load config from {}", config_path.display()))?;
            let bootstrap_path = resolve_bootstrap_path(bootstrap_env)?;
            let resolved = bootstrap_local::resolve_startup_credential_with(
                &config.server.host,
                &bootstrap_path,
                broker::StartupCredential::from_env,
            )?;
            let startup = match resolved {
                bootstrap_local::ResolveOutcome::FromEnv(c)
                | bootstrap_local::ResolveOutcome::FromFile(c) => c,
                bootstrap_local::ResolveOutcome::Generated {
                    credential,
                    material,
                } => {
                    eprintln!(
                        "Generated loopback bootstrap at {}",
                        bootstrap_path.display()
                    );
                    eprintln!("Bootstrap bearer (copy now; will not be shown again):");
                    eprintln!("{}", material.bearer());
                    credential
                }
            };
            let _telemetry = observability::init(&config.observability)?;
            if config_existed {
                tracing::info!(path = %config_path.display(), "loaded config file");
            } else {
                tracing::info!(
                    path = %config_path.display(),
                    "config file not found, using built-in defaults"
                );
            }
            let selection_json = std::env::var("AUTOMATION_RUNTIME_BROWSER_SELECTION").ok();
            let factory =
                compose_worker_factory(&config, parse_selection(selection_json.as_deref())?)?;
            broker::serve_with_worker_factory(config, startup, factory).await?
        }
        CliCommand::FirefoxNativeHost { descriptor } => {
            let _telemetry = observability::init(&Default::default())?;
            run_configured_native_host(descriptor).await?
        }
        CliCommand::InstallFirefoxNativeHost {
            wrapper,
            manifest,
            cli,
            descriptor,
        } => install_configured_native_host(wrapper, manifest, cli, descriptor)?,
        CliCommand::EnrollFirefoxProfile {
            descriptor,
            bind,
            bidi_url,
            profile_dir,
            timeout_secs,
        } => {
            run_firefox_profile_enroll(descriptor, bind, bidi_url, profile_dir, timeout_secs)
                .await?
        }
        CliCommand::Doctor {
            config,
            bootstrap_env,
            skip_health,
        } => run_doctor(config, bootstrap_env, !skip_health)?,
        CliCommand::Jobs { command } => run_jobs(command)?,
    }

    Ok(())
}

fn run_jobs(command: JobsCommand) -> Result<()> {
    match command {
        JobsCommand::Submit {
            common,
            name,
            payload,
            payload_file,
            priority,
            max_retries,
            timeout_ms,
            idempotency_key,
        } => {
            let (base_url, bearer) = prepare_jobs_client(&common)?;
            let payload = jobs_client::resolve_submit_payload(&payload, payload_file.as_deref())?;
            jobs_client::submit_job(
                &base_url,
                bearer,
                jobs_client::SubmitJobOptions {
                    name: &name,
                    payload,
                    priority,
                    max_retries,
                    timeout_ms,
                    idempotency_key,
                },
            )?;
        }
        JobsCommand::Status { common, job_id } => {
            let (base_url, bearer) = prepare_jobs_client(&common)?;
            jobs_client::job_status(&base_url, bearer, &job_id)?;
        }
        JobsCommand::Cancel { common, job_id } => {
            let (base_url, bearer) = prepare_jobs_client(&common)?;
            jobs_client::cancel_job(&base_url, bearer, &job_id)?;
        }
    }
    Ok(())
}

fn prepare_jobs_client(common: &JobsCommonArgs) -> Result<(String, String)> {
    let config = jobs_client::load_config_for_jobs(common.config.clone())?;
    let bootstrap_path = resolve_bootstrap_path(common.bootstrap_env.clone())?;
    let bearer = jobs_client::resolve_jobs_auth(common.token.clone(), &bootstrap_path)?;
    let base_url = jobs_client::resolve_jobs_base_url(common.base_url.clone(), &config);
    Ok((base_url, bearer))
}

pub(crate) fn resolve_config_path(cli: Option<PathBuf>) -> PathBuf {
    cli.or_else(|| std::env::var_os("BOBBY_BROWSER_CONFIG").map(PathBuf::from))
        .unwrap_or_else(|| PathBuf::from("./config.toml"))
}

fn resolve_bootstrap_path(cli: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = cli {
        return Ok(path);
    }
    if let Some(path) = std::env::var_os("BOBBY_BROWSER_BOOTSTRAP_ENV") {
        return Ok(PathBuf::from(path));
    }
    bootstrap_local::default_bootstrap_path()
}

async fn run_firefox_profile_enroll(
    descriptor: PathBuf,
    bind: SocketAddr,
    bidi_url: String,
    profile_dir: PathBuf,
    timeout_secs: u64,
) -> Result<()> {
    if timeout_secs == 0 {
        anyhow::bail!("--timeout-secs must be positive");
    }
    let enrollment = start_firefox_profile_enrollment(
        FirefoxProfileEnrollmentConfig {
            companion_bind: bind,
            descriptor_path: descriptor.clone(),
            timeout: Duration::from_secs(timeout_secs),
            pairing_code_ttl: Duration::from_secs(300),
            attachment_ttl: Duration::from_secs(300),
        },
        Arc::new(|_| {}),
    )
    .await
    .map_err(|error| anyhow::anyhow!("{}", error.message))?;
    let enrolled = enrollment
        .wait()
        .await
        .map_err(|error| anyhow::anyhow!("{}", error.message))?;
    let profile_id = enrolled.profile_id().0.to_string();
    let selection = serde_json::json!({
        "preference": { "mode": "exact", "engine": "firefox", "profileId": profile_id },
        "firefox": [{
            "profileId": profile_id,
            "bidiUrl": bidi_url,
            "profileDir": profile_dir,
            "companionBind": bind.to_string(),
            "descriptorPath": descriptor,
            "timeoutMs": 30_000,
            "pairingCodeTtlMs": 300_000,
            "attachmentTtlMs": 300_000,
        }],
    });
    println!("{selection}");
    eprintln!("Enrollment paired. Export the line above as AUTOMATION_RUNTIME_BROWSER_SELECTION.");
    Ok(())
}

/// `bobby init` issues a 30-day credential, so a week is enough runway to
/// renew before the gateway starts failing closed.
const BOOTSTRAP_EXPIRY_WARN_DAYS: i64 = 7;

fn run_doctor(
    config_cli: Option<PathBuf>,
    bootstrap_cli: Option<PathBuf>,
    check_health: bool,
) -> Result<()> {
    let mut failures = 0usize;
    let mut warnings = 0usize;
    let report = |status: &str, check: &str, detail: String| {
        eprintln!("[{status}] {check}: {detail}");
    };

    let config_path = resolve_config_path(config_cli);
    let config = match AppConfig::load(&config_path) {
        Ok(config) => {
            let source = if config_path.exists() {
                config_path.display().to_string()
            } else {
                "built-in defaults (no config file)".to_string()
            };
            report("ok", "config", source);
            Some(config)
        }
        Err(error) => {
            failures += 1;
            report("fail", "config", format!("{error:#}"));
            None
        }
    };

    let selection_raw = std::env::var("AUTOMATION_RUNTIME_BROWSER_SELECTION").ok();
    let selection = match parse_selection(selection_raw.as_deref()) {
        Ok(selection) => {
            report(
                "ok",
                "browser-selection",
                if selection_raw.is_some() {
                    "AUTOMATION_RUNTIME_BROWSER_SELECTION parses".to_string()
                } else {
                    "default (Firefox, exact)".to_string()
                },
            );
            Some(selection)
        }
        Err(error) => {
            failures += 1;
            report("fail", "browser-selection", format!("{error:#}"));
            None
        }
    };

    if let (Some(config), Some(selection)) = (&config, &selection) {
        match compose_worker_factory(config, selection.clone()) {
            Ok(_) => report(
                "ok",
                "engine-satisfiability",
                "engine preference can be satisfied by configured registrations".to_string(),
            ),
            Err(error) => {
                failures += 1;
                report("fail", "engine-satisfiability", format!("{error:#}"));
            }
        }
        for profile in &selection.firefox {
            match Url::parse(&profile.bidi_url) {
                Ok(url) if matches!(url.scheme(), "ws" | "wss") => {
                    let reachable = url
                        .socket_addrs(|| Some(if url.scheme() == "wss" { 443 } else { 80 }))
                        .map(|addrs| {
                            addrs.iter().any(|addr| {
                                std::net::TcpStream::connect_timeout(
                                    addr,
                                    Duration::from_millis(500),
                                )
                                .is_ok()
                            })
                        })
                        .unwrap_or(false);
                    if reachable {
                        report(
                            "ok",
                            "firefox-bidi",
                            format!("{} reachable", profile.bidi_url),
                        );
                    } else {
                        warnings += 1;
                        report(
                            "warn",
                            "firefox-bidi",
                            format!(
                                "{} not reachable (is Firefox running with --remote-debugging-port?)",
                                profile.bidi_url
                            ),
                        );
                    }
                }
                _ => {
                    failures += 1;
                    report(
                        "fail",
                        "firefox-bidi",
                        format!(
                            "profile {} has an invalid bidiUrl (expected ws:// or wss://)",
                            profile.profile_id
                        ),
                    );
                }
            }
            if profile.profile_dir.exists() {
                report(
                    "ok",
                    "firefox-profile-dir",
                    profile.profile_dir.display().to_string(),
                );
            } else {
                warnings += 1;
                report(
                    "warn",
                    "firefox-profile-dir",
                    format!("{} does not exist yet", profile.profile_dir.display()),
                );
            }
            if profile.companion_bind.parse::<SocketAddr>().is_err() {
                failures += 1;
                report(
                    "fail",
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
    let mut report_expiry = |expires_at: chrono::DateTime<chrono::Utc>| {
        let remaining = expires_at - chrono::Utc::now();
        if remaining <= chrono::Duration::zero() {
            failures += 1;
            report(
                "fail",
                "bootstrap-expiry",
                format!(
                    "credential expired at {}; run `bobby init --force`",
                    expires_at.to_rfc3339()
                ),
            );
        } else if remaining < chrono::Duration::days(BOOTSTRAP_EXPIRY_WARN_DAYS) {
            warnings += 1;
            report(
                "warn",
                "bootstrap-expiry",
                format!(
                    "credential expires in {} day(s) at {}; run `bobby init --force` before then",
                    remaining.num_days(),
                    expires_at.to_rfc3339()
                ),
            );
        } else {
            report(
                "ok",
                "bootstrap-expiry",
                format!("credential valid for {} more day(s)", remaining.num_days()),
            );
        }
    };

    if let Ok(credential) = broker::StartupCredential::from_env() {
        report("ok", "bootstrap", "credential from environment".to_string());
        report_expiry(credential.expires_at());
    } else {
        match resolve_bootstrap_path(bootstrap_cli.clone()) {
            Ok(path) if path.exists() => {
                report(
                    "ok",
                    "bootstrap",
                    format!("credential file at {}", path.display()),
                );
                match bootstrap_local::load_startup_from_env_file(&path) {
                    Ok(credential) => {
                        report_expiry(credential.expires_at());
                        match bootstrap_local::load_bootstrap_capabilities_csv(&path) {
                            Ok(caps) if !caps.split(',').any(|c| c.trim() == "job:submit") => {
                                warnings += 1;
                                report(
                                    "warn",
                                    "bootstrap-job-caps",
                                    "bootstrap lacks job:submit; run `bobby init --force` for job:* capabilities"
                                        .to_string(),
                                );
                            }
                            Ok(_) => {}
                            Err(error) => {
                                warnings += 1;
                                report(
                                    "warn",
                                    "bootstrap-job-caps",
                                    format!("could not read capabilities ({error:#})"),
                                );
                            }
                        }
                    }
                    Err(error) => {
                        failures += 1;
                        report("fail", "bootstrap-expiry", format!("{error:#}"));
                    }
                }
            }
            Ok(path) => {
                warnings += 1;
                report(
                    "warn",
                    "bootstrap",
                    format!(
                        "no credential yet; `bobby serve` will generate one at {}",
                        path.display()
                    ),
                );
            }
            Err(error) => {
                failures += 1;
                report("fail", "bootstrap", format!("{error:#}"));
            }
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
            resolve_bootstrap_path(bootstrap_cli)
                .ok()
                .filter(|path| path.exists())
                .and_then(|path| bootstrap_local::load_bootstrap_env_map(&path).ok())
        };
    match handshake_env {
        Some(env) => match onboarding::mcp_handshake(&env) {
            Ok(handshake) => {
                if handshake.bytes > mcp_gateway::TOOLS_LIST_BYTE_BUDGET {
                    failures += 1;
                    report(
                        "fail",
                        "mcp-handshake",
                        format!(
                            "tools/list is {} bytes, over the {} byte budget",
                            handshake.bytes,
                            mcp_gateway::TOOLS_LIST_BYTE_BUDGET
                        ),
                    );
                } else {
                    report(
                        "ok",
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
                if message.contains("not found") {
                    warnings += 1;
                    report("warn", "mcp-handshake", message);
                } else {
                    failures += 1;
                    report("fail", "mcp-handshake", message);
                }
            }
        },
        None => {
            warnings += 1;
            report(
                "warn",
                "mcp-handshake",
                "skipped: no bootstrap credential to launch the gateway with".to_string(),
            );
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
                Ok(()) => report("ok", name, dir.display().to_string()),
                Err(error) => {
                    failures += 1;
                    report("fail", name, format!("{}: {error}", dir.display()));
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
        report("ok", "firefox", "found".to_string());
    } else {
        warnings += 1;
        report(
            "warn",
            "firefox",
            "not found on PATH or /Applications (default engine)".to_string(),
        );
    }
    let chromium = which_binary(&["google-chrome", "chromium", "chrome"])
        || Path::new("/Applications/Google Chrome.app").exists()
        || Path::new("/Applications/Chromium.app").exists();
    if chromium {
        report("ok", "chromium", "found".to_string());
    } else {
        warnings += 1;
        report(
            "warn",
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
                Ok(()) => report("ok", "healthz", format!("{url} responded")),
                Err(error) => {
                    warnings += 1;
                    report(
                        "warn",
                        "healthz",
                        format!("{url} not reachable ({error}); is `bobby serve` running?"),
                    );
                }
            }
        }
    }

    eprintln!("doctor: {failures} failure(s), {warnings} warning(s)");
    if failures > 0 {
        std::process::exit(1);
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

fn run_init(
    force: bool,
    ttl_days: u32,
    path: Option<PathBuf>,
    emit: Option<onboarding::EmitFormat>,
) -> Result<()> {
    let path = match path {
        Some(path) => path,
        None => bootstrap_local::default_bootstrap_path()?,
    };
    let material =
        bootstrap_local::generate_bootstrap(chrono::Duration::days(i64::from(ttl_days)))?;
    bootstrap_local::write_bootstrap_env(&path, &material, force)?;
    println!("{}", material.bearer());
    eprintln!("Wrote bootstrap env to {}", path.display());
    eprintln!("Map this bearer to AUTOMATION_RUNTIME_TOKEN / Authorization bearer for the SDK.");
    eprintln!(
        "Passing --force regenerates and invalidates the previous bearer for new enrollment."
    );
    if let Some(format) = emit {
        println!("{}", onboarding::emit_mcp_config(format));
        eprintln!(
            "Source {} into the agent host's environment so the ${{...}} placeholders resolve.",
            path.display()
        );
    }
    Ok(())
}

fn install_configured_native_host(
    wrapper: PathBuf,
    manifest: PathBuf,
    cli: PathBuf,
    descriptor: PathBuf,
) -> Result<()> {
    install_native_host(NativeHostInstallConfig {
        wrapper_path: wrapper,
        manifest_path: manifest,
        cli_path: cli,
        descriptor_path: descriptor,
    })?;
    Ok(())
}

pub fn install_native_host(config: NativeHostInstallConfig) -> Result<()> {
    for path in [
        &config.wrapper_path,
        &config.manifest_path,
        &config.cli_path,
        &config.descriptor_path,
    ] {
        if !path.is_absolute() {
            anyhow::bail!("native-host installation paths must be absolute");
        }
    }
    let _install_lock = NativeHostInstallLock::acquire(&config.manifest_path)?;
    let wrapper = format!(
        "#!/bin/sh\nexec {} firefox-native-host --descriptor {}\n",
        shell_quote(&config.cli_path),
        shell_quote(&config.descriptor_path),
    );
    let manifest = serde_json::to_vec_pretty(&serde_json::json!({
        "name": "com.bobby_browser.companion",
        "description": "Bobby Browser Firefox companion native host",
        "path": config.wrapper_path,
        "type": "stdio",
        "allowed_extensions": ["firefox-companion@bobby-browser.local"],
    }))?;
    preflight_exact_file(&config.wrapper_path, wrapper.as_bytes(), 0o700)?;
    preflight_exact_file(&config.manifest_path, &manifest, 0o600)?;
    let wrapper_install = install_exact_file(&config.wrapper_path, wrapper.as_bytes(), 0o700)?;
    if let Err(error) = install_exact_file(&config.manifest_path, &manifest, 0o600) {
        if let Some(created) = wrapper_install {
            created.rollback(&config.wrapper_path);
        }
        return Err(error.into());
    }
    Ok(())
}

struct NativeHostInstallLock {
    _file: std::fs::File,
}

impl NativeHostInstallLock {
    fn acquire(manifest_path: &Path) -> std::io::Result<Self> {
        let lock_path = manifest_path.with_extension("install.lock");
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        if let Ok(metadata) = std::fs::symlink_metadata(&lock_path) {
            if !metadata.file_type().is_file() {
                return Err(unsafe_install_lock());
            }
        }
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
        }
        let file = options.open(&lock_path)?;
        verify_install_lock_identity(&lock_path, &file)?;
        file.lock()?;
        verify_install_lock_identity(&lock_path, &file)?;
        Ok(Self { _file: file })
    }
}

fn unsafe_install_lock() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "native-host installer lock path is unsafe",
    )
}

fn verify_install_lock_identity(path: &Path, file: &std::fs::File) -> std::io::Result<()> {
    let path_metadata = std::fs::symlink_metadata(path)?;
    let file_metadata = file.metadata()?;
    if !path_metadata.file_type().is_file() || !file_metadata.file_type().is_file() {
        return Err(unsafe_install_lock());
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        if path_metadata.dev() != file_metadata.dev()
            || path_metadata.ino() != file_metadata.ino()
            || file_metadata.permissions().mode() & 0o777 != 0o600
        {
            return Err(unsafe_install_lock());
        }
    }
    Ok(())
}

fn shell_quote(path: &Path) -> String {
    format!("'{}'", path.to_string_lossy().replace('\'', "'\\''"))
}

fn preflight_exact_file(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    match path.symlink_metadata() {
        Ok(_) => verify_exact_file(path, contents, mode),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn verify_exact_file(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    #[cfg(unix)]
    let mode_matches = {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o777 == mode
    };
    #[cfg(not(unix))]
    let mode_matches = {
        let _ = mode;
        true
    };
    if metadata.file_type().is_file() && std::fs::read(path)? == contents && mode_matches {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "native-host installation destination already exists",
        ))
    }
}

struct CreatedInstallFile {
    #[cfg(unix)]
    device: u64,
    #[cfg(unix)]
    inode: u64,
}

impl CreatedInstallFile {
    fn from_metadata(metadata: std::fs::Metadata) -> std::io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            Ok(Self {
                device: metadata.dev(),
                inode: metadata.ino(),
            })
        }
        #[cfg(not(unix))]
        {
            let _ = metadata;
            Ok(Self {})
        }
    }

    fn rollback(self, path: &Path) {
        self.rollback_ref(path);
    }

    fn rollback_ref(&self, path: &Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            if let Ok(metadata) = std::fs::symlink_metadata(path) {
                if metadata.dev() == self.device && metadata.ino() == self.inode {
                    let _ = std::fs::remove_file(path);
                }
            }
        }
        #[cfg(not(unix))]
        let _ = std::fs::remove_file(path);
    }
}

fn install_exact_file(
    path: &Path,
    contents: &[u8],
    mode: u32,
) -> std::io::Result<Option<CreatedInstallFile>> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.symlink_metadata().is_ok() {
        verify_exact_file(path, contents, mode)?;
        return Ok(None);
    }
    let pending = path.with_extension(format!("pending-{}", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    let result = (|| {
        use std::io::Write;
        let mut file = options.open(&pending)?;
        file.write_all(contents)?;
        file.flush()?;
        file.sync_all()?;
        let created = CreatedInstallFile::from_metadata(file.metadata()?)?;
        match std::fs::hard_link(&pending, path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                verify_exact_file(path, contents, mode)?;
                std::fs::remove_file(&pending)?;
                return Ok(None);
            }
            Err(error) => return Err(error),
        }
        if let Err(error) = std::fs::remove_file(&pending) {
            created.rollback(path);
            return Err(error);
        }
        Ok(Some(created))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(pending);
    }
    result
}

async fn run_configured_native_host(descriptor_path: PathBuf) -> Result<()> {
    if !descriptor_path.is_absolute() {
        anyhow::bail!("firefox native-host descriptor path must be absolute");
    }
    let descriptor: NativeHostDescriptor =
        serde_json::from_slice(&std::fs::read(descriptor_path)?)?;
    run_native_host(
        tokio::io::stdin(),
        tokio::io::stdout(),
        NativeHostConfig::new(descriptor.endpoint, descriptor.pairing_code),
    )
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn installed_native_host_wrapper_passes_descriptor_without_environment_injection() {
        let root = std::env::temp_dir().join(format!(
            "bobby-native-host-install-{}",
            uuid::Uuid::new_v4()
        ));
        let wrapper = root.join("firefox-native-host");
        let manifest = root.join("com.bobby_browser.companion.json");
        let descriptor = root.join("dynamic-descriptor.json");
        let config = NativeHostInstallConfig {
            wrapper_path: wrapper.clone(),
            manifest_path: manifest.clone(),
            cli_path: PathBuf::from("/bin/echo"),
            descriptor_path: descriptor.clone(),
        };
        install_native_host(config.clone()).unwrap();
        install_native_host(config).unwrap();
        let output = std::process::Command::new(&wrapper)
            .env_remove("AUTOMATION_RUNTIME_FIREFOX_DESCRIPTOR")
            .output()
            .unwrap();
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert!(stdout.contains("firefox-native-host --descriptor"));
        assert!(stdout.contains(descriptor.to_str().unwrap()));
        assert!(!stdout.contains("AUTOMATION_RUNTIME_FIREFOX_DESCRIPTOR"));
        let installed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        assert_eq!(installed["path"], wrapper.to_string_lossy().as_ref());
        assert!(!String::from_utf8_lossy(&std::fs::read(&wrapper).unwrap()).contains("pairing"));
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&wrapper).unwrap().permissions().mode() & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&manifest).unwrap().permissions().mode() & 0o777,
            0o600
        );
        std::fs::remove_file(wrapper).unwrap();
        std::fs::remove_file(manifest).unwrap();
        std::fs::remove_file(root.join("com.bobby_browser.companion.install.lock")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn native_host_installation_never_clobbers_operator_owned_files() {
        let root = std::env::temp_dir().join(format!(
            "bobby-native-host-no-clobber-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let wrapper = root.join("firefox-native-host");
        let manifest = root.join("com.bobby_browser.companion.json");
        let original = b"operator-owned";
        std::fs::write(&wrapper, original).unwrap();
        let result = install_native_host(NativeHostInstallConfig {
            wrapper_path: wrapper.clone(),
            manifest_path: manifest.clone(),
            cli_path: PathBuf::from("/bin/echo"),
            descriptor_path: root.join("dynamic-descriptor.json"),
        });
        assert!(result.is_err());
        assert_eq!(std::fs::read(&wrapper).unwrap(), original);
        assert!(!manifest.exists());
        std::fs::remove_file(wrapper).unwrap();
        std::fs::remove_file(root.join("com.bobby_browser.companion.install.lock")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn native_host_manifest_conflict_rolls_back_wrapper_created_by_attempt() {
        let root = std::env::temp_dir().join(format!(
            "bobby-native-host-manifest-conflict-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let wrapper = root.join("firefox-native-host");
        let manifest = root.join("com.bobby_browser.companion.json");
        let original = b"operator-owned-manifest";
        std::fs::write(&manifest, original).unwrap();

        let result = install_native_host(NativeHostInstallConfig {
            wrapper_path: wrapper.clone(),
            manifest_path: manifest.clone(),
            cli_path: PathBuf::from("/bin/echo"),
            descriptor_path: root.join("dynamic-descriptor.json"),
        });

        assert!(result.is_err());
        assert!(!wrapper.exists());
        assert_eq!(std::fs::read(&manifest).unwrap(), original);
        std::fs::remove_file(manifest).unwrap();
        std::fs::remove_file(root.join("com.bobby_browser.companion.install.lock")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_identical_native_host_installers_are_both_successful() {
        let root = std::env::temp_dir().join(format!(
            "bobby-native-host-concurrent-{}",
            uuid::Uuid::new_v4()
        ));
        let config = NativeHostInstallConfig {
            wrapper_path: root.join("firefox-native-host"),
            manifest_path: root.join("com.bobby_browser.companion.json"),
            cli_path: PathBuf::from("/bin/echo"),
            descriptor_path: root.join("dynamic-descriptor.json"),
        };
        let barrier = Arc::new(std::sync::Barrier::new(3));
        let attempts = (0..2)
            .map(|_| {
                let config = config.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    barrier.wait();
                    install_native_host(config)
                })
            })
            .collect::<Vec<_>>();
        barrier.wait();
        for attempt in attempts {
            attempt.join().unwrap().unwrap();
        }
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&config.wrapper_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
        assert_eq!(
            std::fs::metadata(&config.manifest_path)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        std::fs::remove_file(config.wrapper_path).unwrap();
        std::fs::remove_file(config.manifest_path).unwrap();
        std::fs::remove_file(root.join("com.bobby_browser.companion.install.lock")).unwrap();
        std::fs::remove_dir(root).unwrap();
    }
    #[test]
    fn jobs_submit_clap_parses_required_name() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "bobby",
            "jobs",
            "submit",
            "--name",
            "echo",
            "--payload",
            r#"{"ok":true}"#,
            "--priority",
            "high",
            "--max-retries",
            "1",
            "--timeout-ms",
            "5000",
            "--idempotency-key",
            "k1",
        ])
        .unwrap();
        match cli.command {
            Some(CliCommand::Jobs {
                command:
                    JobsCommand::Submit {
                        name,
                        payload,
                        priority,
                        max_retries,
                        timeout_ms,
                        idempotency_key,
                        ..
                    },
            }) => {
                assert_eq!(name, "echo");
                assert_eq!(payload, r#"{"ok":true}"#);
                assert_eq!(priority, jobs_client::JobPriorityArg::High);
                assert_eq!(max_retries, 1);
                assert_eq!(timeout_ms, Some(5000));
                assert_eq!(idempotency_key.as_deref(), Some("k1"));
            }
            _ => panic!("unexpected jobs submit parse"),
        }
    }

    #[test]
    fn jobs_status_and_cancel_clap_parse_job_id() {
        use clap::Parser;
        let status = Cli::try_parse_from(["bobby", "jobs", "status", "job-123"]).unwrap();
        match status.command {
            Some(CliCommand::Jobs {
                command: JobsCommand::Status { job_id, .. },
            }) => assert_eq!(job_id, "job-123"),
            _ => panic!("unexpected status parse"),
        }
        let cancel = Cli::try_parse_from(["bobby", "jobs", "cancel", "job-456"]).unwrap();
        match cancel.command {
            Some(CliCommand::Jobs {
                command: JobsCommand::Cancel { job_id, .. },
            }) => assert_eq!(job_id, "job-456"),
            _ => panic!("unexpected cancel parse"),
        }
    }
}
