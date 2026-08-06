mod bootstrap_local;
mod jobs_client;
mod onboarding;
mod vision_child;
mod vision_connect;

use anyhow::{Context, Result};
use companion_core::{
    run_native_host_with_enroll, EnrollFinalize, EnrollHostError, NativeConnectRequest,
    NativeHostConfig, NativeHostEnroll,
};
use config::{ensure_loopback_vision_defaults, upsert_vision_platform, AppConfig, VisionConfig};
use firefox_companion::read_bidi_url_from_profile_dir;
#[cfg(test)]
use firefox_companion::selection::write_enroll_defaults;
pub use firefox_companion::selection::{
    build_enrolled_browser_selection, compose_worker_factory,
    compose_worker_factory_with_enrolled_firefox, compose_worker_factory_with_pairing_observer,
    default_selection_path, parse_selection, persist_browser_selection, resolve_browser_selection,
    start_firefox_profile_enrollment, EnrolledFirefoxProfile, FirefoxProfileEnrollmentConfig,
    SelectionSource, SELECTION_ENV,
};
use firefox_companion::selection::{
    enroll_defaults_path, read_enroll_defaults, FirefoxEnrollDefaults, FirefoxProfileEnrollment,
    NativeHostDescriptor,
};
use std::{
    future::Future,
    net::SocketAddr,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};
use tokio::sync::Mutex;
use url::Url;
use vision_child::{
    decide_vision_child, enforce_force_on_spawn, ManagedVisionProxy, VisionChildDecision,
    VisionSpawnPolicy,
};
use vision_proxy::{serve as serve_vision_proxy, OpenAiUpstream, ProxyConfig};

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
        /// Path to config.toml (overrides BOBBY_BROWSER_CONFIG)
        #[arg(long)]
        config: Option<PathBuf>,
        #[arg(long, conflicts_with = "no_vision")]
        vision: bool,
        #[arg(long, conflicts_with = "vision")]
        no_vision: bool,
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
        /// Install `bobby` (+ `mcp-gateway`) onto PATH (~/.cargo/bin or ~/.local/bin)
        #[arg(long)]
        cli: bool,
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
        #[arg(long, conflicts_with = "no_vision")]
        vision: bool,
        #[arg(long, conflicts_with = "vision")]
        no_vision: bool,
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
    /// Vision provider setup and loopback proxy
    Vision {
        #[command(subcommand)]
        command: VisionCommands,
    },
    /// Configure a vision provider profile in config.toml (deprecated alias)
    #[command(name = "vision-connect", hide = true)]
    VisionConnect(VisionConnectArgs),
    /// Run the loopback vision proxy (propose/extract → OpenAI)
    VisionProxy {
        /// Bind address (loopback default)
        #[arg(long, default_value = "127.0.0.1:9100")]
        bind: String,
        /// HTTP path for propose/extract POST
        #[arg(long, default_value = "/vision")]
        path: String,
        /// Upstream provider (v1: openai only)
        #[arg(long, default_value = "openai")]
        upstream: String,
        /// OpenAI model id
        #[arg(long, default_value = "gpt-4o")]
        model: String,
        /// OpenAI API base URL (tests / proxies)
        #[arg(long, default_value = "https://api.openai.com/v1")]
        openai_base_url: String,
        /// Upstream API key env var (default OPENAI_API_KEY; empty value skips key)
        #[arg(long)]
        api_key_env: Option<String>,
    },
}

#[derive(clap::Args)]
struct VisionConnectArgs {
    /// Path to config.toml (overrides BOBBY_BROWSER_CONFIG)
    #[arg(long)]
    config: Option<PathBuf>,
    /// Provider preset: openai, ollama, lmstudio, or custom
    #[arg(long)]
    provider: Option<String>,
    /// Backend transport: direct or acp
    #[arg(long, default_value = "direct")]
    backend: String,
    /// ACP harness executable (for --backend acp)
    #[arg(long)]
    command: Option<String>,
    /// Argument passed to the ACP harness executable; repeatable
    #[arg(long = "arg")]
    args: Vec<String>,
    /// Authentication path: advertised, oauth-authorization-code, oauth-device-code, environment, existing-session, or none
    #[arg(long, default_value = "advertised")]
    auth: String,
    /// Upstream base URL (required for custom; overrides preset default)
    #[arg(long)]
    base_url: Option<String>,
    /// Model id (required for custom; overrides preset default)
    #[arg(long)]
    model: Option<String>,
    /// Env var holding upstream API key (custom only; empty omits)
    #[arg(long)]
    api_key_env: Option<String>,
    /// Accept defaults without interactive prompts
    #[arg(long)]
    yes: bool,
}

impl From<VisionConnectArgs> for vision_connect::ConnectOpts {
    fn from(args: VisionConnectArgs) -> Self {
        Self {
            config: args.config,
            provider: args.provider,
            backend: args.backend,
            command: args.command,
            args: args.args,
            auth: args.auth,
            base_url: args.base_url,
            model: args.model,
            api_key_env: args.api_key_env,
            yes: args.yes,
        }
    }
}

#[derive(clap::Subcommand)]
enum VisionCommands {
    /// Configure a vision provider profile in config.toml
    Connect(VisionConnectArgs),
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
        vision: false,
        no_vision: false,
    }) {
        CliCommand::Init {
            force,
            ttl_days,
            path,
            emit,
        } => run_init(force, ttl_days, path, emit)?,
        CliCommand::McpStdio {
            bootstrap_env,
            config,
            vision,
            no_vision,
        } => {
            let bootstrap_path = resolve_bootstrap_path(bootstrap_env)?;
            let config_path = resolve_config_path(config);
            let policy = policy_from_flags(vision, no_vision);
            let config = AppConfig::load(&config_path)
                .with_context(|| format!("failed to load config from {}", config_path.display()))?;
            let (_config, decision, vision_child) =
                prepare_vision_child(&config_path, config, policy)?;
            if decision.should_spawn {
                let child = vision_child.ok_or_else(|| {
                    anyhow::anyhow!("vision sidecar missing after spawn decision")
                })?;
                onboarding::run_mcp_stdio_with_sidecar(&bootstrap_path, &config_path, child)?;
            } else {
                onboarding::exec_mcp_stdio(&bootstrap_path, &config_path)?;
            }
        }
        CliCommand::Install {
            host,
            skill,
            project_skill,
            companion,
            extension,
            cli,
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
                    cli,
                    force,
                    yes,
                },
            )?;
        }
        CliCommand::Serve {
            config,
            bootstrap_env,
            vision,
            no_vision,
        } => {
            let config_path = resolve_config_path(config);
            let config_existed = config_path.exists();
            let policy = policy_from_flags(vision, no_vision);
            let config = AppConfig::load(&config_path)
                .with_context(|| format!("failed to load config from {}", config_path.display()))?;
            let (config, decision, _vision_child) =
                prepare_vision_child(&config_path, config, policy)?;
            if decision.should_spawn {
                tracing::info!(
                    bind = %decision.bind,
                    path = %decision.path,
                    reason = %decision.reason,
                    "spawned loopback vision-proxy sidecar"
                );
            } else if matches!(policy, VisionSpawnPolicy::ForceOn) {
                tracing::info!(reason = %decision.reason, "vision sidecar not spawned");
            }
            let bootstrap_path = resolve_bootstrap_path(bootstrap_env)?;
            let heal = bootstrap_local::ensure_unrestricted_bootstrap(&bootstrap_path)
                .context("failed to heal bootstrap capabilities")?;
            if heal.changed() {
                tracing::info!(
                    added = ?heal.added,
                    file_rewritten = heal.file_rewritten,
                    env_updated = heal.env_updated,
                    "healed bootstrap capabilities to current defaults"
                );
            }
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
            let (selection, _source) = resolve_browser_selection()?;
            let factory = compose_worker_factory(&config, selection)?;
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
        } => {
            let report = run_doctor(config, bootstrap_env, !skip_health)?;
            report.render();
            if report.failures() > 0 {
                std::process::exit(1);
            }
        }
        CliCommand::Jobs { command } => run_jobs(command)?,
        CliCommand::Vision { command } => match command {
            VisionCommands::Connect(args) => vision_connect::connect(args.into())?,
        },
        CliCommand::VisionConnect(args) => vision_connect::connect(args.into())?,
        CliCommand::VisionProxy {
            bind,
            path,
            upstream,
            model,
            openai_base_url,
            api_key_env,
        } => {
            run_vision_proxy(bind, path, upstream, model, openai_base_url, api_key_env).await?;
        }
    }

    Ok(())
}

fn policy_from_flags(vision: bool, no_vision: bool) -> VisionSpawnPolicy {
    if no_vision {
        VisionSpawnPolicy::Off
    } else if vision {
        VisionSpawnPolicy::ForceOn
    } else {
        VisionSpawnPolicy::Auto
    }
}

fn prepare_vision_child(
    config_path: &Path,
    mut config: AppConfig,
    policy: VisionSpawnPolicy,
) -> Result<(AppConfig, VisionChildDecision, Option<ManagedVisionProxy>)> {
    if matches!(policy, VisionSpawnPolicy::ForceOn) {
        ensure_loopback_vision_defaults(&mut config.vision);
        let Some((provider_name, profile)) = config.vision.selected_provider() else {
            anyhow::bail!("no vision provider configured; run `bobby vision connect` first");
        };
        let endpoint_url = config
            .vision
            .endpoint_url
            .as_deref()
            .context("vision endpoint_url missing after defaults")?;
        let token_env = config
            .vision
            .token_env
            .as_deref()
            .context("vision token_env missing after defaults")?;
        upsert_vision_platform(config_path, endpoint_url, token_env, provider_name, profile)
            .map_err(|error| anyhow::anyhow!("{error}"))?;
        config = AppConfig::load(config_path)
            .with_context(|| format!("failed to reload config from {}", config_path.display()))?;
    }

    let decision = decide_vision_child(&config, policy);
    enforce_force_on_spawn(policy, &decision)?;

    let vision_child = if decision.should_spawn {
        let (_, profile) = config
            .vision
            .selected_provider()
            .context("no vision provider configured; run `bobby vision connect` first")?;
        let token_env = config
            .vision
            .token_env
            .as_deref()
            .context("vision token_env not configured")?;
        Some(ManagedVisionProxy::spawn_from_current_exe(
            &decision, profile, token_env,
        )?)
    } else {
        None
    };

    Ok((config, decision, vision_child))
}

fn require_vision_proxy_bearer() -> Result<String> {
    std::env::var("BOBBY_VISION_TOKEN")
        .ok()
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("BOBBY_VISION_TOKEN is missing or empty"))
}

fn require_upstream_api_key(api_key_env: Option<&str>) -> Result<String> {
    match api_key_env.map(str::trim).filter(|s| !s.is_empty()) {
        None => Ok(String::new()),
        Some(name) => std::env::var(name)
            .ok()
            .filter(|value| !value.is_empty())
            .ok_or_else(|| anyhow::anyhow!("{name} is missing or empty")),
    }
}

async fn run_vision_proxy(
    bind: String,
    path: String,
    upstream: String,
    model: String,
    openai_base_url: String,
    api_key_env: Option<String>,
) -> Result<()> {
    if upstream != "openai" {
        anyhow::bail!("unsupported upstream {upstream:?}; v1 supports only \"openai\"");
    }

    let bearer_token = require_vision_proxy_bearer()?;
    let api_key = match api_key_env {
        None => require_upstream_api_key(Some("OPENAI_API_KEY"))?,
        Some(name) if name.trim().is_empty() => require_upstream_api_key(None)?,
        Some(name) => require_upstream_api_key(Some(name.trim()))?,
    };
    let bind: SocketAddr = bind.parse().context("invalid --bind address")?;

    let upstream = Arc::new(OpenAiUpstream::new(api_key, model, openai_base_url));
    let config = ProxyConfig {
        bind,
        path,
        bearer_token,
    };

    serve_vision_proxy(config, upstream)
        .await
        .context("vision-proxy server failed")?;

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
    let selection = build_enrolled_browser_selection(
        enrolled.profile_id(),
        &bidi_url,
        &profile_dir,
        bind,
        &descriptor,
    );
    println!("{}", serde_json::to_string(&selection)?);
    let persist = default_selection_path()
        .and_then(|path| persist_browser_selection(&path, &selection).map(|()| path));
    match persist {
        Ok(path) => eprintln!(
            "Enrollment paired and persisted to {}. `bobby serve`, the MCP gateway, and \
             `bobby doctor` now resolve this selection with no environment wiring; \
             {SELECTION_ENV} remains an override.",
            path.display()
        ),
        Err(error) => eprintln!(
            "Enrollment paired but could not persist the selection ({error:#}). \
             Export the line above as {SELECTION_ENV}."
        ),
    }
    Ok(())
}

/// `bobby init` issues a 30-day credential, so a week is enough runway to
/// renew before the gateway starts failing closed.
const BOOTSTRAP_EXPIRY_WARN_DAYS: i64 = 7;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum DoctorStatus {
    Ok,
    Warn,
    Fail,
}

impl DoctorStatus {
    fn label(self) -> &'static str {
        match self {
            DoctorStatus::Ok => "ok",
            DoctorStatus::Warn => "warn",
            DoctorStatus::Fail => "fail",
        }
    }
}

#[derive(Debug, Clone)]
struct DoctorCheck {
    status: DoctorStatus,
    name: String,
    detail: String,
}

/// Structured outcome of a `bobby doctor` run: every check in order, so the
/// CLI can render it and tests can assert on it without capturing stderr.
#[derive(Debug, Default)]
struct DoctorReport {
    checks: Vec<DoctorCheck>,
}

impl DoctorReport {
    fn record(&mut self, status: DoctorStatus, name: &str, detail: String) {
        self.checks.push(DoctorCheck {
            status,
            name: name.to_string(),
            detail,
        });
    }

    fn ok(&mut self, name: &str, detail: String) {
        self.record(DoctorStatus::Ok, name, detail);
    }

    fn warn(&mut self, name: &str, detail: String) {
        self.record(DoctorStatus::Warn, name, detail);
    }

    fn fail(&mut self, name: &str, detail: String) {
        self.record(DoctorStatus::Fail, name, detail);
    }

    fn failures(&self) -> usize {
        self.checks
            .iter()
            .filter(|check| check.status == DoctorStatus::Fail)
            .count()
    }

    fn warnings(&self) -> usize {
        self.checks
            .iter()
            .filter(|check| check.status == DoctorStatus::Warn)
            .count()
    }

    #[cfg(test)]
    fn check(&self, name: &str) -> Option<&DoctorCheck> {
        self.checks.iter().find(|check| check.name == name)
    }

    fn render(&self) {
        for check in &self.checks {
            eprintln!(
                "[{}] {}: {}",
                check.status.label(),
                check.name,
                check.detail
            );
        }
        eprintln!(
            "doctor: {} failure(s), {} warning(s)",
            self.failures(),
            self.warnings()
        );
    }
}

fn check_bootstrap_expiry(expires_at: chrono::DateTime<chrono::Utc>) -> DoctorCheck {
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
fn handshake_error_status(message: &str) -> DoctorStatus {
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

fn vision_endpoint_unreachable_detail(endpoint: &str) -> String {
    if vision_endpoint_is_loopback(endpoint) {
        format!(
            "{endpoint} not reachable (start with `bobby serve --vision` to auto-spawn the proxy, or run `bobby vision-proxy` manually)"
        )
    } else {
        format!("{endpoint} not reachable (verify the external vision endpoint is running)")
    }
}

fn check_vision_provider(vision: &VisionConfig) -> Option<DoctorCheck> {
    let name = vision.provider.as_deref()?.trim();
    if name.is_empty() {
        return None;
    }
    if vision.providers.contains_key(name) {
        Some(DoctorCheck {
            status: DoctorStatus::Ok,
            name: "vision-provider".to_string(),
            detail: format!("provider \"{name}\" configured"),
        })
    } else {
        Some(DoctorCheck {
            status: DoctorStatus::Warn,
            name: "vision-provider".to_string(),
            detail: format!("provider \"{name}\" is set but missing from [vision.providers]"),
        })
    }
}

fn check_vision_upstream_key(vision: &VisionConfig) -> Option<DoctorCheck> {
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

fn executable_available(command: &str) -> bool {
    let path = Path::new(command);
    if path.components().count() > 1 {
        return path.is_file();
    }
    std::env::var_os("PATH").is_some_and(|paths| {
        std::env::split_paths(&paths).any(|directory| directory.join(command).is_file())
    })
}

fn check_vision_acp(vision: &VisionConfig) -> Vec<DoctorCheck> {
    let Some(config::VisionBackendSelection::Acp { name, profile }) = vision.selected_backend()
    else {
        return Vec::new();
    };
    vec![
        DoctorCheck {
            status: DoctorStatus::Ok,
            name: "vision-routing".into(),
            detail: format!("ACP profile {name:?} selected"),
        },
        DoctorCheck {
            status: if executable_available(&profile.command) {
                DoctorStatus::Ok
            } else {
                DoctorStatus::Warn
            },
            name: "vision-acp-reachability".into(),
            detail: if executable_available(&profile.command) {
                format!("ACP harness executable {:?} is available", profile.command)
            } else {
                format!(
                    "ACP harness executable {:?} was not found on PATH",
                    profile.command
                )
            },
        },
        DoctorCheck {
            status: DoctorStatus::Ok,
            name: "vision-auth-path".into(),
            detail: format!(
                "{:?}; credentials remain owned by the harness",
                profile.auth
            ),
        },
    ]
}

fn push_doctor_check(report: &mut DoctorReport, check: DoctorCheck) {
    match check.status {
        DoctorStatus::Ok => report.ok(&check.name, check.detail),
        DoctorStatus::Warn => report.warn(&check.name, check.detail),
        DoctorStatus::Fail => report.fail(&check.name, check.detail),
    }
}

fn run_doctor(
    config_cli: Option<PathBuf>,
    bootstrap_cli: Option<PathBuf>,
    check_health: bool,
) -> Result<DoctorReport> {
    let mut report = DoctorReport::default();

    let config_path = resolve_config_path(config_cli);
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
        for check in check_vision_acp(&config.vision) {
            push_doctor_check(&mut report, check);
        }
        if let Some(check) = check_vision_provider(&config.vision) {
            push_doctor_check(&mut report, check);
        }
        if let Some(check) = check_vision_upstream_key(&config.vision) {
            push_doctor_check(&mut report, check);
        }

        if !matches!(config.vision.backend, Some(config::VisionBackendKind::Acp)) {
            if let Some(endpoint) = config.vision.endpoint_url.as_deref() {
                match Url::parse(endpoint) {
                    Ok(url) => {
                        let reachable = url
                            .socket_addrs(|| Some(url.port_or_known_default().unwrap_or(80)))
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
                            report.ok("vision-endpoint", endpoint.to_string());
                        } else {
                            report.warn(
                                "vision-endpoint",
                                vision_endpoint_unreachable_detail(endpoint),
                            );
                        }
                    }
                    Err(error) => {
                        report.warn("vision-endpoint", format!("invalid URL: {error}"));
                    }
                }

                match config.vision.token_env.as_deref() {
                    Some(name) if !name.is_empty() => match std::env::var(name) {
                        Ok(value) if !value.is_empty() => {
                            report.ok("vision-token-env", format!("{name} is set"));
                        }
                        _ => {
                            report.warn("vision-token-env", format!("{name} is unset or empty"));
                        }
                    },
                    _ => {
                        report.warn(
                            "vision-token-env",
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

    if let (Some(config), Some(selection)) = (&config, &selection) {
        match compose_worker_factory(config, selection.clone()) {
            Ok(_) => report.ok(
                "engine-satisfiability",
                "engine preference can be satisfied by configured registrations".to_string(),
            ),
            Err(error) => {
                report.fail("engine-satisfiability", format!("{error:#}"));
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
                        report.ok("firefox-bidi", format!("{} reachable", profile.bidi_url));
                    } else {
                        report.warn(
                            "firefox-bidi",
                            format!(
                                "{} not reachable (is Firefox running with --remote-debugging-port?)",
                                profile.bidi_url
                            ),
                        );
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
    let manifest = native_host_manifest_bytes(&config.wrapper_path)?;
    // Refuse operator-owned destinations before writing either file.
    preflight_install_destination(
        &config.wrapper_path,
        wrapper.as_bytes(),
        0o700,
        NativeHostFileKind::Wrapper,
    )?;
    preflight_install_destination(
        &config.manifest_path,
        &manifest,
        0o600,
        NativeHostFileKind::Manifest,
    )?;
    let wrapper_install = install_exact_file(
        &config.wrapper_path,
        wrapper.as_bytes(),
        0o700,
        NativeHostFileKind::Wrapper,
    )?;
    if let Err(error) = install_exact_file(
        &config.manifest_path,
        &manifest,
        0o600,
        NativeHostFileKind::Manifest,
    ) {
        wrapper_install.rollback(&config.wrapper_path);
        return Err(error.into());
    }
    Ok(())
}

/// Stable Firefox native-messaging manifest bytes (alphabetical keys).
fn native_host_manifest_bytes(wrapper_path: &Path) -> Result<Vec<u8>> {
    // Insert alphabetically so reinstalls stay byte-stable across serde versions.
    let mut map = serde_json::Map::new();
    map.insert(
        "allowed_extensions".to_owned(),
        serde_json::json!(["firefox-companion@bobby-browser.local"]),
    );
    map.insert(
        "description".to_owned(),
        serde_json::json!("Bobby Browser Firefox companion native host"),
    );
    map.insert(
        "name".to_owned(),
        serde_json::json!("com.bobby_browser.companion"),
    );
    map.insert(
        "path".to_owned(),
        serde_json::Value::String(wrapper_path.display().to_string()),
    );
    map.insert("type".to_owned(), serde_json::json!("stdio"));
    Ok(serde_json::to_vec_pretty(&serde_json::Value::Object(map))?)
}

#[derive(Clone, Copy)]
enum NativeHostFileKind {
    Wrapper,
    Manifest,
}

impl NativeHostFileKind {
    fn is_managed(self, contents: &[u8]) -> bool {
        match self {
            Self::Wrapper => is_bobby_managed_wrapper(contents),
            Self::Manifest => is_bobby_managed_manifest(contents),
        }
    }
}

fn is_bobby_managed_wrapper(contents: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(contents) else {
        return false;
    };
    text.starts_with("#!/bin/sh\n") && text.contains(" firefox-native-host --descriptor ")
}

fn is_bobby_managed_manifest(contents: &[u8]) -> bool {
    let Ok(value) = serde_json::from_slice::<serde_json::Value>(contents) else {
        return false;
    };
    let Some(object) = value.as_object() else {
        return false;
    };
    object.get("name").and_then(|value| value.as_str()) == Some("com.bobby_browser.companion")
        && object.get("type").and_then(|value| value.as_str()) == Some("stdio")
        && object
            .get("allowed_extensions")
            .and_then(|value| value.as_array())
            .is_some_and(|extensions| {
                extensions.iter().any(|extension| {
                    extension.as_str() == Some("firefox-companion@bobby-browser.local")
                })
            })
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

fn destination_already_exists() -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::AlreadyExists,
        "native-host installation destination already exists",
    )
}

fn preflight_install_destination(
    path: &Path,
    contents: &[u8],
    mode: u32,
    kind: NativeHostFileKind,
) -> std::io::Result<()> {
    match path.symlink_metadata() {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
        Ok(_) => match verify_exact_file(path, contents, mode) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let existing = std::fs::read(path)?;
                if kind.is_managed(&existing) {
                    Ok(())
                } else {
                    Err(error)
                }
            }
            Err(error) => Err(error),
        },
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
        Err(destination_already_exists())
    }
}

enum InstallFileOutcome {
    Unchanged,
    Created(CreatedInstallFile),
    Replaced { previous: Vec<u8>, mode: u32 },
}

impl InstallFileOutcome {
    fn rollback(self, path: &Path) {
        match self {
            Self::Unchanged => {}
            Self::Created(created) => created.rollback(path),
            Self::Replaced { previous, mode } => {
                let _ = write_exact_file_atomic(path, &previous, mode);
            }
        }
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

fn write_exact_file_atomic(path: &Path, contents: &[u8], mode: u32) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let pending = path.with_extension(format!("pending-{}", uuid::Uuid::new_v4()));
    let result = (|| {
        use std::io::Write;
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        let mut file = options.open(&pending)?;
        file.write_all(contents)?;
        file.flush()?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&pending, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&pending);
    }
    result
}

fn install_exact_file(
    path: &Path,
    contents: &[u8],
    mode: u32,
    kind: NativeHostFileKind,
) -> std::io::Result<InstallFileOutcome> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path.symlink_metadata().is_ok() {
        if verify_exact_file(path, contents, mode).is_ok() {
            return Ok(InstallFileOutcome::Unchanged);
        }
        let previous = std::fs::read(path)?;
        if !kind.is_managed(&previous) {
            return Err(destination_already_exists());
        }
        write_exact_file_atomic(path, contents, mode)?;
        return Ok(InstallFileOutcome::Replaced { previous, mode });
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
                if verify_exact_file(path, contents, mode).is_ok() {
                    std::fs::remove_file(&pending)?;
                    return Ok(InstallFileOutcome::Unchanged);
                }
                let previous = std::fs::read(path)?;
                if !kind.is_managed(&previous) {
                    return Err(destination_already_exists());
                }
                std::fs::rename(&pending, path)?;
                return Ok(InstallFileOutcome::Replaced { previous, mode });
            }
            Err(error) => return Err(error),
        }
        if let Err(error) = std::fs::remove_file(&pending) {
            created.rollback(path);
            return Err(error);
        }
        Ok(InstallFileOutcome::Created(created))
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
    let config = match std::fs::read(&descriptor_path) {
        Ok(bytes) => {
            let descriptor: NativeHostDescriptor = serde_json::from_slice(&bytes)?;
            Some(NativeHostConfig::new(
                descriptor.endpoint,
                descriptor.pairing_code,
            ))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => return Err(error.into()),
    };
    let config_dir = descriptor_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("firefox native-host descriptor path has no parent"))?
        .to_path_buf();
    let enroll = NativeHostFirefoxEnroll::new(config_dir, Duration::from_secs(120));
    run_native_host_with_enroll(
        tokio::io::stdin(),
        tokio::io::stdout(),
        config,
        Some(enroll),
    )
    .await?;
    Ok(())
}

struct NativeHostFirefoxEnroll {
    defaults_path: PathBuf,
    timeout: Duration,
    state: Mutex<NativeHostFirefoxEnrollState>,
}

#[derive(Default)]
struct NativeHostFirefoxEnrollState {
    enrollment: Option<FirefoxProfileEnrollment>,
    /// True when enroll reused a reachable serve descriptor (no temp bind).
    used_live_descriptor: bool,
    bidi_url: Option<String>,
    defaults: Option<FirefoxEnrollDefaults>,
}

impl NativeHostFirefoxEnroll {
    fn new(config_dir: PathBuf, timeout: Duration) -> Self {
        Self {
            defaults_path: enroll_defaults_path(&config_dir),
            timeout,
            state: Mutex::new(NativeHostFirefoxEnrollState::default()),
        }
    }
}

/// Prefer a serve-published descriptor when its endpoint is already accepting.
fn load_usable_live_descriptor(path: &Path) -> Option<NativeHostConfig> {
    let bytes = std::fs::read(path).ok()?;
    let descriptor: NativeHostDescriptor = serde_json::from_slice(&bytes).ok()?;
    if descriptor.pairing_code.is_empty() || descriptor.pairing_code.len() > 512 {
        return None;
    }
    let url = Url::parse(&descriptor.endpoint).ok()?;
    if url.scheme() != "ws" {
        return None;
    }
    let host = url.host_str()?;
    let loopback = host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .map(|address| address.is_loopback())
            .unwrap_or(false);
    if !loopback {
        return None;
    }
    let port = url.port()?;
    let addr = if host.eq_ignore_ascii_case("localhost") {
        SocketAddr::from(([127, 0, 0, 1], port))
    } else {
        SocketAddr::new(host.parse().ok()?, port)
    };
    if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(200)).is_err() {
        return None;
    }
    Some(NativeHostConfig::new(
        descriptor.endpoint,
        descriptor.pairing_code,
    ))
}

fn companion_bind_in_use(addr: SocketAddr) -> bool {
    match std::net::TcpListener::bind(addr) {
        Err(error) if error.kind() == std::io::ErrorKind::AddrInUse => true,
        Ok(listener) => {
            drop(listener);
            false
        }
        Err(_) => false,
    }
}

impl NativeHostEnroll for NativeHostFirefoxEnroll {
    #[allow(clippy::manual_async_fn)]
    fn enroll_and_wait_for_pair(
        &self,
        _pair: NativeConnectRequest,
    ) -> impl Future<Output = Result<NativeHostConfig, EnrollHostError>> + Send {
        async move {
            let defaults = read_enroll_defaults(&self.defaults_path)
                .map_err(|_| EnrollHostError::DefaultsMissing)?;
            let bidi_url = read_bidi_url_from_profile_dir(&defaults.profile_dir)
                .map_err(|_| EnrollHostError::BidiMissing)?;

            // Day-2 Re-pair: serve already holds the bind and published a descriptor.
            // Reachability probe is blocking TCP — keep it off the async runtime.
            let descriptor_path = defaults.descriptor_path.clone();
            let live =
                tokio::task::spawn_blocking(move || load_usable_live_descriptor(&descriptor_path))
                    .await
                    .ok()
                    .flatten();
            if let Some(config) = live {
                let mut state = self.state.lock().await;
                state.enrollment = None;
                state.used_live_descriptor = true;
                state.bidi_url = Some(bidi_url.to_string());
                state.defaults = Some(defaults);
                return Ok(config);
            }

            // First-time enroll: bootstrap a temporary companion on the defaults bind.
            let enrollment = match start_firefox_profile_enrollment(
                FirefoxProfileEnrollmentConfig {
                    companion_bind: defaults.companion_bind,
                    descriptor_path: defaults.descriptor_path.clone(),
                    timeout: self.timeout,
                    pairing_code_ttl: Duration::from_secs(300),
                    attachment_ttl: Duration::from_secs(300),
                },
                Arc::new(|_| {}),
            )
            .await
            {
                Ok(enrollment) => enrollment,
                Err(_) if companion_bind_in_use(defaults.companion_bind) => {
                    return Err(EnrollHostError::BindInUse);
                }
                Err(_) => return Err(EnrollHostError::ListenerUnavailable),
            };
            let descriptor: NativeHostDescriptor = serde_json::from_slice(
                &std::fs::read(&defaults.descriptor_path)
                    .map_err(|_| EnrollHostError::ListenerUnavailable)?,
            )
            .map_err(|_| EnrollHostError::ListenerUnavailable)?;
            let config = NativeHostConfig::new(descriptor.endpoint, descriptor.pairing_code);
            let mut state = self.state.lock().await;
            state.enrollment = Some(enrollment);
            state.used_live_descriptor = false;
            state.bidi_url = Some(bidi_url.to_string());
            state.defaults = Some(defaults);
            Ok(config)
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn complete_enrollment(
        &self,
        pair: &NativeConnectRequest,
    ) -> impl Future<Output = Result<EnrollFinalize, EnrollHostError>> + Send {
        let profile_id = pair.profile_id.clone();
        async move {
            let (enrollment, bidi_url, defaults, used_live) = {
                let mut state = self.state.lock().await;
                (
                    state.enrollment.take(),
                    state
                        .bidi_url
                        .take()
                        .ok_or(EnrollHostError::ListenerUnavailable)?,
                    state
                        .defaults
                        .take()
                        .ok_or(EnrollHostError::DefaultsMissing)?,
                    state.used_live_descriptor,
                )
            };

            if used_live {
                let selection = build_enrolled_browser_selection(
                    &profile_id,
                    &bidi_url,
                    &defaults.profile_dir,
                    defaults.companion_bind,
                    &defaults.descriptor_path,
                );
                let path =
                    default_selection_path().map_err(|_| EnrollHostError::ListenerUnavailable)?;
                persist_browser_selection(&path, &selection)
                    .map_err(|_| EnrollHostError::ListenerUnavailable)?;
                return Ok(EnrollFinalize::KeepRelay);
            }

            let enrollment = enrollment.ok_or(EnrollHostError::ListenerUnavailable)?;
            let enrolled = enrollment.wait().await.map_err(|error| {
                if error.message.to_ascii_lowercase().contains("timed out") {
                    EnrollHostError::Timeout
                } else {
                    EnrollHostError::ListenerUnavailable
                }
            })?;
            if enrolled.profile_id() != &profile_id {
                return Err(EnrollHostError::ListenerUnavailable);
            }
            let selection = build_enrolled_browser_selection(
                enrolled.profile_id(),
                &bidi_url,
                &defaults.profile_dir,
                defaults.companion_bind,
                &defaults.descriptor_path,
            );
            let path =
                default_selection_path().map_err(|_| EnrollHostError::ListenerUnavailable)?;
            persist_browser_selection(&path, &selection)
                .map_err(|_| EnrollHostError::ListenerUnavailable)?;
            // Drop the temporary companion so day-2 `bobby serve` can bind.
            drop(enrolled);
            Ok(EnrollFinalize::ReleaseListener)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use companion_protocol::{
        BrowserEngine, BrowserIdentity, CompanionCapabilities, PROTOCOL_VERSION,
    };
    use config::VisionProviderConfig;
    use std::collections::BTreeMap;
    use types::{CompanionId, ProfileId};

    fn sample_native_connect_request() -> NativeConnectRequest {
        NativeConnectRequest {
            protocol_version: PROTOCOL_VERSION,
            companion_id: CompanionId::new(),
            profile_id: ProfileId::new(),
            identity: BrowserIdentity {
                engine: BrowserEngine::Firefox,
                browser_name: "Firefox".into(),
                browser_version: "stable".into(),
                os: "macos".into(),
                profile_label: "default-release".into(),
            },
            capabilities: CompanionCapabilities {
                observe: true,
                navigate: true,
                native_input: false,
                tabs: true,
                frames: true,
                native_dialogs: false,
            },
        }
    }

    #[tokio::test]
    async fn native_host_enroll_maps_missing_defaults() {
        let root = tempfile::tempdir().unwrap();
        let enroll =
            NativeHostFirefoxEnroll::new(root.path().to_path_buf(), Duration::from_secs(5));
        let error = enroll
            .enroll_and_wait_for_pair(sample_native_connect_request())
            .await
            .unwrap_err();
        assert_eq!(error, EnrollHostError::DefaultsMissing);
        assert_eq!(error.code(), "defaultsMissing");
    }

    #[tokio::test]
    async fn native_host_enroll_maps_missing_bidi_endpoint() {
        let root = tempfile::tempdir().unwrap();
        let profile_dir = root.path().join("firefox-profile");
        std::fs::create_dir_all(&profile_dir).unwrap();
        let defaults = FirefoxEnrollDefaults {
            profile_dir,
            companion_bind: "127.0.0.1:9876".parse().unwrap(),
            descriptor_path: root.path().join("firefox-native-host-descriptor.json"),
        };
        write_enroll_defaults(&enroll_defaults_path(root.path()), &defaults).unwrap();
        let enroll =
            NativeHostFirefoxEnroll::new(root.path().to_path_buf(), Duration::from_secs(5));
        let error = enroll
            .enroll_and_wait_for_pair(sample_native_connect_request())
            .await
            .unwrap_err();
        assert_eq!(error, EnrollHostError::BidiMissing);
        assert_eq!(error.code(), "bidiMissing");
    }

    fn write_test_bidi_endpoint(profile_dir: &Path) {
        std::fs::create_dir_all(profile_dir).unwrap();
        std::fs::write(
            profile_dir.join("WebDriverBiDiServer.json"),
            br#"{"ws_host":"127.0.0.1","ws_port":9222}"#,
        )
        .unwrap();
    }

    #[tokio::test]
    async fn native_host_enroll_uses_live_descriptor_without_temp_bind() {
        use companion_core::{CompanionServer, CompanionServerConfig};

        let root = tempfile::tempdir().unwrap();
        let profile_dir = root.path().join("firefox-profile");
        write_test_bidi_endpoint(&profile_dir);

        let server = CompanionServer::bind_loopback(CompanionServerConfig {
            bind_addr: "127.0.0.1:0".parse().unwrap(),
            pairing_code_ttl: Duration::from_secs(60),
            attachment_ttl: Duration::from_secs(300),
        })
        .await
        .unwrap();
        let bind = server.local_addr();
        let pairing_code = server.registry().issue_pairing_code().await;
        let descriptor_path = root.path().join("firefox-native-host-descriptor.json");
        let descriptor = NativeHostDescriptor {
            endpoint: format!("ws://{bind}/v1/companion"),
            pairing_code: pairing_code.clone(),
            ownership_id: uuid::Uuid::new_v4().to_string(),
        };
        std::fs::write(&descriptor_path, serde_json::to_vec(&descriptor).unwrap()).unwrap();

        let defaults = FirefoxEnrollDefaults {
            profile_dir,
            // Same bind serve holds — bootstrap would AddrInUse if attempted.
            companion_bind: bind,
            descriptor_path,
        };
        write_enroll_defaults(&enroll_defaults_path(root.path()), &defaults).unwrap();

        let enroll =
            NativeHostFirefoxEnroll::new(root.path().to_path_buf(), Duration::from_secs(5));
        let config = enroll
            .enroll_and_wait_for_pair(sample_native_connect_request())
            .await
            .expect("live descriptor enroll must succeed without temp bind");
        let state = enroll.state.lock().await;
        assert!(state.used_live_descriptor);
        assert!(state.enrollment.is_none());
        drop(state);

        // Config must authenticate against the live listener.
        let request = config
            .pair_request(sample_native_connect_request())
            .expect("pair request from live descriptor");
        let _ = request;
        assert!(pairing_code.len() > 8);
        drop(server);
    }

    #[tokio::test]
    async fn native_host_enroll_maps_bind_in_use_without_live_descriptor() {
        let root = tempfile::tempdir().unwrap();
        let profile_dir = root.path().join("firefox-profile");
        write_test_bidi_endpoint(&profile_dir);

        let holder = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bind = holder.local_addr().unwrap();
        let defaults = FirefoxEnrollDefaults {
            profile_dir,
            companion_bind: bind,
            // No descriptor file — live path unavailable.
            descriptor_path: root.path().join("missing-descriptor.json"),
        };
        write_enroll_defaults(&enroll_defaults_path(root.path()), &defaults).unwrap();

        let enroll =
            NativeHostFirefoxEnroll::new(root.path().to_path_buf(), Duration::from_secs(5));
        let error = enroll
            .enroll_and_wait_for_pair(sample_native_connect_request())
            .await
            .unwrap_err();
        assert_eq!(error, EnrollHostError::BindInUse);
        assert_eq!(error.code(), "bindInUse");
        drop(holder);
    }

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

    #[cfg(unix)]
    #[test]
    fn native_host_installation_upgrades_bobby_managed_files() {
        let root = std::env::temp_dir().join(format!(
            "bobby-native-host-upgrade-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let wrapper = root.join("firefox-native-host");
        let manifest = root.join("com.bobby_browser.companion.json");
        let descriptor = root.join("dynamic-descriptor.json");
        let first = NativeHostInstallConfig {
            wrapper_path: wrapper.clone(),
            manifest_path: manifest.clone(),
            cli_path: PathBuf::from("/bin/echo"),
            descriptor_path: descriptor.clone(),
        };
        install_native_host(first).unwrap();

        // Stale key order + different bobby path (common upgrade / make firefox case).
        let stale_manifest = br#"{
  "name": "com.bobby_browser.companion",
  "description": "Bobby Browser Firefox companion native host",
  "path": "/tmp/old-wrapper",
  "type": "stdio",
  "allowed_extensions": [
    "firefox-companion@bobby-browser.local"
  ]
}"#;
        std::fs::write(&manifest, stale_manifest).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&manifest).unwrap().permissions();
            perms.set_mode(0o600);
            std::fs::set_permissions(&manifest, perms).unwrap();
        }
        std::fs::write(
            &wrapper,
            b"#!/bin/sh\nexec '/old/bobby' firefox-native-host --descriptor '/old/descriptor.json'\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(&wrapper).unwrap().permissions();
            perms.set_mode(0o700);
            std::fs::set_permissions(&wrapper, perms).unwrap();
        }

        let upgraded = NativeHostInstallConfig {
            wrapper_path: wrapper.clone(),
            manifest_path: manifest.clone(),
            cli_path: PathBuf::from("/usr/bin/true"),
            descriptor_path: descriptor.clone(),
        };
        install_native_host(upgraded.clone()).unwrap();
        install_native_host(upgraded).unwrap();

        let wrapper_text = String::from_utf8(std::fs::read(&wrapper).unwrap()).unwrap();
        assert!(wrapper_text.contains("/usr/bin/true"));
        assert!(!wrapper_text.contains("/old/bobby"));
        let installed: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        assert_eq!(installed["path"], wrapper.to_string_lossy().as_ref());
        assert_eq!(
            installed["allowed_extensions"][0],
            "firefox-companion@bobby-browser.local"
        );

        std::fs::remove_file(wrapper).unwrap();
        std::fs::remove_file(manifest).unwrap();
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

    #[test]
    fn doctor_report_counts_and_indexes_checks() {
        let mut report = DoctorReport::default();
        report.ok("a", "fine".to_string());
        report.warn("b", "meh".to_string());
        report.fail("c", "broken".to_string());
        report.fail("d", "also broken".to_string());

        assert_eq!(report.failures(), 2);
        assert_eq!(report.warnings(), 1);
        assert_eq!(report.check("c").unwrap().status, DoctorStatus::Fail);
        assert!(report.check("missing").is_none());
    }

    #[test]
    fn bootstrap_expiry_check_fails_when_expired_warns_when_near_and_passes_beyond() {
        let expired = check_bootstrap_expiry(chrono::Utc::now() - chrono::Duration::hours(1));
        assert_eq!(expired.status, DoctorStatus::Fail);
        assert!(expired.detail.contains("expired"));

        let soon = check_bootstrap_expiry(
            chrono::Utc::now() + chrono::Duration::days(BOOTSTRAP_EXPIRY_WARN_DAYS - 1),
        );
        assert_eq!(soon.status, DoctorStatus::Warn);
        assert!(soon.detail.contains("expires in"));

        let later = check_bootstrap_expiry(
            chrono::Utc::now() + chrono::Duration::days(BOOTSTRAP_EXPIRY_WARN_DAYS + 10),
        );
        assert_eq!(later.status, DoctorStatus::Ok);
        assert!(later.detail.contains("valid"));
    }

    #[test]
    fn handshake_error_classification_distinguishes_missing_binary_from_failed_handshake() {
        assert_eq!(
            handshake_error_status("failed to spawn /usr/local/bin/mcp-gateway: not found"),
            DoctorStatus::Warn
        );
        assert_eq!(
            handshake_error_status("initialize: gateway did not answer within 15s"),
            DoctorStatus::Fail
        );
    }

    /// `run_doctor` reads process env; serialize these tests and restore every
    /// variable they touch so they cannot leak into each other or the host.
    struct DoctorEnvGuard {
        saved: Vec<(&'static str, Option<std::ffi::OsString>)>,
    }

    impl DoctorEnvGuard {
        const VARS: [&'static str; 8] = [
            "AUTOMATION_RUNTIME_BROWSER_SELECTION",
            "AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN",
            "AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL",
            "AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES",
            "AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT",
            "BOBBY_BROWSER_BOOTSTRAP_ENV",
            "BOBBY_VISION_TOKEN",
            "OPENAI_API_KEY",
        ];

        fn clear() -> Self {
            let saved = Self::VARS
                .iter()
                .map(|name| {
                    let value = std::env::var_os(name);
                    unsafe { std::env::remove_var(name) };
                    (*name, value)
                })
                .collect();
            Self { saved }
        }

        fn set(&self, name: &str, value: &str) {
            assert!(Self::VARS.contains(&name));
            unsafe { std::env::set_var(name, value) };
        }
    }

    impl Drop for DoctorEnvGuard {
        fn drop(&mut self) {
            for (name, value) in &self.saved {
                match value {
                    Some(value) => unsafe { std::env::set_var(name, value) },
                    None => unsafe { std::env::remove_var(name) },
                }
            }
        }
    }

    static DOCTOR_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn doctor_config_fixture(root: &Path) -> PathBuf {
        let path = root.join("config.toml");
        let text = format!(
            r#"[server]
host = "127.0.0.1"
port = 17987

[browser]
profiles_dir = "{0}/profiles"
headless = true
max_active = 1
upload_roots = []
downloads_dir = "{0}/downloads"
artifacts_dir = "{0}/artifacts"
max_artifact_bytes = 1048576
max_screenshot_dimension = 1024
max_js_result_bytes = 65536
max_js_timeout_ms = 5000

[storage]
journal_path = "{0}/storage/journal.jsonl"
checkpoints_dir = "{0}/storage/checkpoints"
authority_path = "{0}/storage/authority.json"
scheduler_journal_path = "{0}/storage/scheduler-jobs.jsonl"
"#,
            root.display()
        );
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn doctor_flags_a_malformed_config_and_skips_dependent_checks() {
        let _lock = DOCTOR_ENV_LOCK.lock().unwrap();
        let env = DoctorEnvGuard::clear();
        env.set(
            "AUTOMATION_RUNTIME_BROWSER_SELECTION",
            r#"{"preference":{"mode":"managedChromium"}}"#,
        );
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        std::fs::write(&config, "not = [valid").unwrap();

        let report = run_doctor(
            Some(config),
            Some(root.path().join("missing-bootstrap.env")),
            false,
        )
        .unwrap();

        assert_eq!(report.failures(), 1, "{:?}", report.checks);
        assert_eq!(report.check("config").unwrap().status, DoctorStatus::Fail);
        assert!(report.check("engine-satisfiability").is_none());
        assert!(report.check("storage-journal-dir").is_none());
        assert_eq!(
            report.check("browser-selection").unwrap().status,
            DoctorStatus::Ok
        );
    }

    #[test]
    fn doctor_accepts_a_valid_config_and_a_satisfiable_selection() {
        let _lock = DOCTOR_ENV_LOCK.lock().unwrap();
        let env = DoctorEnvGuard::clear();
        env.set(
            "AUTOMATION_RUNTIME_BROWSER_SELECTION",
            r#"{"preference":{"mode":"managedChromium"}}"#,
        );
        let root = tempfile::tempdir().unwrap();
        let config = doctor_config_fixture(root.path());

        let report = run_doctor(
            Some(config),
            Some(root.path().join("missing-bootstrap.env")),
            false,
        )
        .unwrap();

        assert_eq!(report.failures(), 0, "{:?}", report.checks);
        assert_eq!(report.check("config").unwrap().status, DoctorStatus::Ok);
        assert_eq!(
            report.check("browser-selection").unwrap().status,
            DoctorStatus::Ok
        );
        assert_eq!(
            report.check("engine-satisfiability").unwrap().status,
            DoctorStatus::Ok
        );
        assert_eq!(
            report.check("artifacts-dir").unwrap().status,
            DoctorStatus::Ok
        );
        assert!(root.path().join("artifacts").is_dir());
    }

    #[test]
    fn doctor_fails_on_an_unparseable_browser_selection() {
        let _lock = DOCTOR_ENV_LOCK.lock().unwrap();
        let env = DoctorEnvGuard::clear();
        env.set("AUTOMATION_RUNTIME_BROWSER_SELECTION", "{not json");
        let root = tempfile::tempdir().unwrap();
        let config = doctor_config_fixture(root.path());

        let report = run_doctor(
            Some(config),
            Some(root.path().join("missing-bootstrap.env")),
            false,
        )
        .unwrap();

        assert_eq!(report.failures(), 1, "{:?}", report.checks);
        assert_eq!(
            report.check("browser-selection").unwrap().status,
            DoctorStatus::Fail
        );
        assert!(report.check("engine-satisfiability").is_none());
    }

    #[test]
    fn policy_from_flags_maps_cli_vision_switches() {
        assert_eq!(policy_from_flags(false, false), VisionSpawnPolicy::Auto);
        assert_eq!(policy_from_flags(true, false), VisionSpawnPolicy::ForceOn);
        assert_eq!(policy_from_flags(false, true), VisionSpawnPolicy::Off);
    }

    #[test]
    fn vision_connect_clap_parses_nested_subcommand() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "bobby",
            "vision",
            "connect",
            "--yes",
            "--provider",
            "lmstudio",
            "--config",
            "/tmp/config.toml",
        ])
        .unwrap();
        match cli.command {
            Some(CliCommand::Vision {
                command:
                    VisionCommands::Connect(VisionConnectArgs {
                        yes,
                        provider,
                        config,
                        ..
                    }),
            }) => {
                assert!(yes);
                assert_eq!(provider.as_deref(), Some("lmstudio"));
                assert_eq!(config.as_deref(), Some(Path::new("/tmp/config.toml")));
            }
            _ => panic!("unexpected vision connect parse"),
        }
    }

    #[test]
    fn vision_connect_clap_parses_legacy_alias() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["bobby", "vision-connect", "--yes", "--provider", "openai"])
            .unwrap();
        match cli.command {
            Some(CliCommand::VisionConnect(VisionConnectArgs { yes, provider, .. })) => {
                assert!(yes);
                assert_eq!(provider.as_deref(), Some("openai"));
            }
            _ => panic!("unexpected vision-connect alias parse"),
        }
    }

    #[test]
    fn vision_provider_check_warns_when_profile_missing() {
        let vision = VisionConfig {
            provider: Some("ghost".into()),
            ..VisionConfig::default()
        };
        let check = check_vision_provider(&vision).unwrap();
        assert_eq!(check.status, DoctorStatus::Warn);
        assert_eq!(check.name, "vision-provider");
        assert!(check.detail.contains("ghost"));
    }

    #[test]
    fn vision_upstream_key_skipped_for_local_provider() {
        let vision = VisionConfig {
            provider: Some("lmstudio".into()),
            providers: BTreeMap::from([(
                "lmstudio".into(),
                VisionProviderConfig {
                    base_url: "http://127.0.0.1:1234/v1".into(),
                    model: "local-model".into(),
                    api_key_env: None,
                },
            )]),
            ..VisionConfig::default()
        };
        assert!(check_vision_upstream_key(&vision).is_none());
    }

    #[test]
    fn doctor_acp_checks_are_separate_and_do_not_call_a_model() {
        let vision = AppConfig::from_toml_str(
            r#"
[vision]
backend = "acp"
profile = "fake"
[vision.acp_profiles.fake]
command = "definitely-not-a-real-acp-harness"
auth = "oauth-device-code"
"#,
        )
        .unwrap()
        .vision;
        let checks = check_vision_acp(&vision);
        assert_eq!(checks.len(), 3);
        assert_eq!(checks[0].name, "vision-routing");
        assert_eq!(checks[1].name, "vision-acp-reachability");
        assert_eq!(checks[2].name, "vision-auth-path");
    }

    #[test]
    fn vision_endpoint_unreachable_detail_distinguishes_loopback_from_external() {
        let loopback = vision_endpoint_unreachable_detail("http://127.0.0.1:9100/vision");
        assert!(loopback.contains("bobby serve --vision"));
        assert!(loopback.contains("bobby vision-proxy"));

        let external = vision_endpoint_unreachable_detail("https://vision.example.com/propose");
        assert!(external.contains("external vision endpoint"));
        assert!(!external.contains("auto-spawn"));
    }

    #[test]
    fn doctor_warns_on_missing_vision_provider_profile() {
        let _lock = DOCTOR_ENV_LOCK.lock().unwrap();
        let _env = DoctorEnvGuard::clear();
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        std::fs::write(
            &config,
            r#"[vision]
provider = "ghost"
endpoint_url = "http://127.0.0.1:9100/vision"
"#,
        )
        .unwrap();

        let report = run_doctor(
            Some(config),
            Some(root.path().join("missing-bootstrap.env")),
            false,
        )
        .unwrap();

        assert_eq!(
            report.check("vision-provider").unwrap().status,
            DoctorStatus::Warn
        );
        assert!(report.check("vision-upstream-key").is_none());
    }

    #[test]
    fn doctor_warns_when_upstream_api_key_env_is_empty() {
        let _lock = DOCTOR_ENV_LOCK.lock().unwrap();
        let _env = DoctorEnvGuard::clear();
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        std::fs::write(
            &config,
            r#"[vision]
provider = "openai"
endpoint_url = "http://127.0.0.1:9100/vision"

[vision.providers.openai]
base_url = "https://api.openai.com/v1"
model = "gpt-4o-mini"
api_key_env = "OPENAI_API_KEY"
"#,
        )
        .unwrap();

        let report = run_doctor(
            Some(config),
            Some(root.path().join("missing-bootstrap.env")),
            false,
        )
        .unwrap();

        assert_eq!(
            report.check("vision-upstream-key").unwrap().status,
            DoctorStatus::Warn
        );
        assert!(report
            .check("vision-upstream-key")
            .unwrap()
            .detail
            .contains("OPENAI_API_KEY"));
    }

    #[test]
    fn serve_and_mcp_stdio_reject_conflicting_vision_flags() {
        use clap::Parser;
        assert!(Cli::try_parse_from(["bobby", "serve", "--vision", "--no-vision"]).is_err());
        assert!(Cli::try_parse_from(["bobby", "mcp-stdio", "--vision", "--no-vision"]).is_err());
    }
}
