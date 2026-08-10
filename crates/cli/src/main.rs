mod bootstrap_local;
mod doctor;
mod jobs_client;
mod onboarding;
mod openshell;
mod v1_client;
mod vision_child;
mod vision_collect;
mod vision_connect;
mod vision_login;

use anyhow::{Context, Result};
use companion_core::{
    run_native_host_with_enroll, EnrollFinalize, EnrollHostError, NativeConnectRequest,
    NativeHostConfig, NativeHostEnroll,
};
use config::{ensure_loopback_vision_defaults, upsert_vision_platform, AppConfig};
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
use vision_proxy::{
    serve as serve_vision_proxy, MlxUpstream, OllamaUpstream, OpenAiUpstream, ProxyConfig,
    UpstreamKind,
};

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
        /// Capability floor: agent (default, no authority:admin) or unrestricted
        #[arg(long, value_enum, default_value_t = bootstrap_local::BootstrapPreset::Agent)]
        preset: bootstrap_local::BootstrapPreset,
        /// Bootstrap env file path
        #[arg(long)]
        path: Option<PathBuf>,
        /// Print an MCP client config fragment for an agent host (claude, zed, vscode, json, openshell)
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
    /// Run the ACP stdio gateway with the bootstrap credential loaded for you.
    /// ACP hosts should point here: no env wiring needed in the host config.
    AcpStdio {
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
        /// Install the agent skill (to ~/.agents/skills/, or the project with --project-skill)
        #[arg(long)]
        skill: bool,
        /// Install the agents skill into this project's .agents/skills/ instead of user-level
        #[arg(long)]
        project_skill: bool,
        /// Also install the skill for Claude Code (~/.claude/skills/, or project with --project-skill)
        #[arg(long)]
        skill_claude: bool,
        /// Also install the skill for OpenClaw (~/.openclaw/skills/)
        #[arg(long)]
        skill_openclaw: bool,
        /// Install the Firefox companion (extension, native host, descriptor)
        #[arg(long)]
        companion: bool,
        /// Path to a built companion extension (else built from the repo)
        #[arg(long)]
        extension: Option<PathBuf>,
        /// Install `bobby` (+ `mcp-gateway`, `acp-gateway`) onto PATH (~/.cargo/bin or ~/.local/bin)
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
    /// Run the runtime with authenticated CDP enabled on the dedicated port
    Cdp {
        /// Path to config.toml (overrides BOBBY_BROWSER_CONFIG)
        #[arg(long)]
        config: Option<PathBuf>,
        /// Path to bootstrap.env (overrides BOBBY_BROWSER_BOOTSTRAP_ENV)
        #[arg(long)]
        bootstrap_env: Option<PathBuf>,
        /// CDP listen port override
        #[arg(long)]
        cdp_port: Option<u16>,
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
    /// NVIDIA OpenShell host: pack, provision, revoke sandbox principals
    Openshell {
        #[command(subcommand)]
        command: OpenshellCommand,
    },
    /// Inspect or erase remembered site context (durable context graph)
    Context {
        #[command(subcommand)]
        command: ContextCommands,
    },
    /// Vision provider setup and loopback proxy
    Vision {
        #[command(subcommand)]
        command: VisionCommands,
    },
    /// Configure a vision provider profile in config.toml (deprecated alias)
    #[command(name = "vision-connect", hide = true)]
    VisionConnect(VisionConnectArgs),
    /// Run the loopback vision proxy (propose/extract → OpenAI or Ollama)
    VisionProxy {
        /// Bind address (loopback default)
        #[arg(long, default_value = "127.0.0.1:9100")]
        bind: String,
        /// HTTP path for propose/extract POST
        #[arg(long, default_value = "/vision")]
        path: String,
        /// Upstream provider: "openai", "ollama", or "mlx"
        #[arg(long, default_value = "openai")]
        upstream: String,
        /// Model id (defaults per upstream: gpt-4o / llava:7b / Qwen2.5-VL-3B)
        #[arg(long)]
        model: Option<String>,
        /// Vision upstream base URL (defaults per upstream: openai / 11434 / 9101)
        #[arg(long)]
        vision_base_url: Option<String>,
        /// Spawn the canonical Python vision server as a managed child (mlx upstream)
        #[arg(long)]
        spawn_server: bool,
        /// Path to vision_server.py when spawning (env BOBBY_VISION_SERVER_SCRIPT; auto-detect from repo layout)
        #[arg(long)]
        server_script: Option<PathBuf>,
        /// Enable training data collection (logs all vision proposals to disk)
        #[arg(long)]
        collect_training_data: bool,
        /// Output directory for training data (default: data/vision/)
        #[arg(long, default_value = "data/vision/")]
        training_data_dir: String,
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

#[derive(clap::Args)]
struct VisionLoginArgs {
    /// Path to config.toml (overrides BOBBY_BROWSER_CONFIG)
    #[arg(long)]
    config: Option<PathBuf>,
    /// Named ACP vision profile to authenticate
    name: String,
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
    /// Establish or verify the configured ACP harness login
    Login(VisionLoginArgs),
    /// Collect training data from gauntlet runs
    Collect {
        /// Output directory (default: data/vision/)
        #[arg(long, default_value = "data/vision/")]
        output: String,
        /// Number of examples to collect per journey (default: 100)
        #[arg(long, default_value_t = 100)]
        examples: usize,
        /// Specific journey to collect (default: all)
        #[arg(long)]
        journey: Option<String>,
    },
}

#[derive(clap::Subcommand)]
enum ContextCommands {
    /// List remembered sites for a profile
    List {
        /// Durable profile id (Firefox companion enrollment)
        #[arg(long)]
        profile: String,
        /// Store root (defaults to <config-dir>/bobby-browser/context)
        #[arg(long)]
        dir: Option<PathBuf>,
    },
    /// Erase every remembered byte for one site, immediately and totally
    Forget {
        /// Site key, e.g. https://example.com
        site: String,
        /// Durable profile id (Firefox companion enrollment)
        #[arg(long)]
        profile: String,
        /// Store root (defaults to <config-dir>/bobby-browser/context)
        #[arg(long)]
        dir: Option<PathBuf>,
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

#[derive(clap::Subcommand)]
enum OpenshellCommand {
    /// Write the openshell/ pack (policy, mcp.json, skill, README)
    Install {
        /// Project root (default: cwd)
        #[arg(long)]
        path: Option<PathBuf>,
        /// Host gateway hostname the sandbox dials (OpenShell policy + MCP URL)
        #[arg(long, default_value = "host.docker.internal")]
        mcp_host: String,
        /// Host bobby port
        #[arg(long, default_value_t = 7777)]
        mcp_port: u16,
        /// Agent binary path listed in the OpenShell policy
        #[arg(long, default_value = "/usr/local/bin/claude")]
        agent_binary: String,
    },
    /// Mint one agent-scoped principal for a sandbox; write injection env (0600)
    Provision {
        #[command(flatten)]
        common: JobsCommonArgs,
        /// Sandbox id (1–128 chars of [A-Za-z0-9_-]); one principal per id
        #[arg(long)]
        sandbox: String,
        /// Principal TTL in hours (default 12)
        #[arg(long)]
        ttl_hours: Option<i64>,
        /// Host gateway hostname recorded in the injection env MCP URL
        #[arg(long, default_value = "host.docker.internal")]
        mcp_host: String,
        #[arg(long, default_value_t = 7777)]
        mcp_port: u16,
        /// Capability floor: openshell (default, narrow) or agent (full minus admin)
        #[arg(long, value_enum, default_value_t = openshell::OpenshellCapabilityPreset::Openshell)]
        capabilities_preset: openshell::OpenshellCapabilityPreset,
    },
    /// Alias for provision (revokes prior principal, mints a fresh one)
    Rotate {
        #[command(flatten)]
        common: JobsCommonArgs,
        #[arg(long)]
        sandbox: String,
        #[arg(long)]
        ttl_hours: Option<i64>,
        #[arg(long, default_value = "host.docker.internal")]
        mcp_host: String,
        #[arg(long, default_value_t = 7777)]
        mcp_port: u16,
        #[arg(long, value_enum, default_value_t = openshell::OpenshellCapabilityPreset::Openshell)]
        capabilities_preset: openshell::OpenshellCapabilityPreset,
    },
    /// List locally recorded OpenShell sandboxes (no secrets)
    List,
    /// Show non-secret status for one sandbox
    Status {
        #[arg(long)]
        sandbox: String,
    },
    /// Revoke the principal previously provisioned for a sandbox
    Revoke {
        #[command(flatten)]
        common: JobsCommonArgs,
        /// Sandbox id that was passed to provision
        #[arg(long)]
        sandbox: String,
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
            preset,
            path,
            emit,
        } => run_init(force, ttl_days, preset, path, emit)?,
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
        CliCommand::AcpStdio {
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
                onboarding::run_acp_stdio_with_sidecar(&bootstrap_path, &config_path, child)?;
            } else {
                onboarding::exec_acp_stdio(&bootstrap_path, &config_path)?;
            }
        }
        CliCommand::Install {
            host,
            skill,
            project_skill,
            skill_claude,
            skill_openclaw,
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
                    skill_claude,
                    skill_openclaw,
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
            let policy = policy_from_flags(vision, no_vision);
            run_broker_serve(config, bootstrap_env, policy, false, None).await?;
        }
        CliCommand::Cdp {
            config,
            bootstrap_env,
            cdp_port,
            vision,
            no_vision,
        } => {
            let policy = policy_from_flags(vision, no_vision);
            run_broker_serve(config, bootstrap_env, policy, true, cdp_port).await?;
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
            let report = doctor::run_doctor(config, bootstrap_env, !skip_health)?;
            report.render();
            if report.failures() > 0 {
                std::process::exit(1);
            }
        }
        CliCommand::Jobs { command } => run_jobs(command)?,
        CliCommand::Openshell { command } => run_openshell(command)?,
        CliCommand::Context { command } => run_context(command).await?,
        CliCommand::Vision { command } => match command {
            VisionCommands::Connect(args) => vision_connect::connect(args.into())?,
            VisionCommands::Login(args) => vision_login::login(args.config, &args.name).await?,
            VisionCommands::Collect {
                output,
                examples,
                journey,
            } => {
                vision_collect::run_collect(output, examples, journey)?;
            }
        },
        CliCommand::VisionConnect(args) => vision_connect::connect(args.into())?,
        CliCommand::VisionProxy {
            bind,
            path,
            upstream,
            model,
            vision_base_url,
            spawn_server,
            server_script,
            collect_training_data,
            training_data_dir,
            api_key_env,
        } => {
            run_vision_proxy(VisionProxyRunArgs {
                bind,
                path,
                upstream,
                model,
                vision_base_url,
                spawn_server,
                server_script,
                collect_training_data,
                training_data_dir,
                api_key_env,
            })
            .await?;
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

async fn run_broker_serve(
    config_cli: Option<PathBuf>,
    bootstrap_env: Option<PathBuf>,
    policy: VisionSpawnPolicy,
    force_cdp: bool,
    cdp_port: Option<u16>,
) -> Result<()> {
    let config_path = resolve_config_path(config_cli);
    let config_existed = config_path.exists();
    let config = AppConfig::load(&config_path)
        .with_context(|| format!("failed to load config from {}", config_path.display()))?;
    let (mut config, decision, _vision_child) = prepare_vision_child(&config_path, config, policy)?;
    if force_cdp {
        config.cdp.enabled = true;
        if let Some(port) = cdp_port {
            config.cdp.port = port;
        }
    }
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
    let durable_profile_id = match &selection.preference {
        config::EnginePreferenceConfig::Exact {
            engine: config::BrowserEngineConfig::Firefox,
            profile_id: Some(profile_id),
        } => Some(profile_id.clone()),
        _ => None,
    };
    if durable_profile_id.is_some() && config.context.dir.is_none() {
        config.context.dir = Some(
            dirs::config_dir()
                .ok_or_else(|| anyhow::anyhow!("config directory unavailable"))?
                .join("bobby-browser")
                .join("context"),
        );
    }
    let factory = compose_worker_factory(&config, selection)?;
    match durable_profile_id {
        Some(profile_id) => {
            broker::serve_with_context_promotion(config, startup, factory, profile_id).await?
        }
        None => broker::serve_with_worker_factory(config, startup, factory).await?,
    }
    Ok(())
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

struct VisionProxyRunArgs {
    bind: String,
    path: String,
    upstream: String,
    model: Option<String>,
    vision_base_url: Option<String>,
    spawn_server: bool,
    server_script: Option<PathBuf>,
    collect_training_data: bool,
    training_data_dir: String,
    api_key_env: Option<String>,
}

async fn run_vision_proxy(args: VisionProxyRunArgs) -> Result<()> {
    let VisionProxyRunArgs {
        bind,
        path,
        upstream,
        model,
        vision_base_url,
        spawn_server,
        server_script,
        collect_training_data,
        training_data_dir,
        api_key_env,
    } = args;
    let bind: SocketAddr = bind.parse().context("invalid --bind address")?;
    let bearer_token = require_vision_proxy_bearer()?;

    let collector = collect_training_data.then(|| {
        Arc::new(vision_proxy::VisionDataCollector::new(
            vision_proxy::DataCollectorConfig {
                output_dir: PathBuf::from(training_data_dir),
                enabled: true,
                flush_interval_ms: 1000,
            },
        ))
    });

    let upstream_kind = upstream.trim().to_ascii_lowercase();
    // Managed Python child (mlx --spawn-server) must outlive serve.
    let mut _python_child: Option<ManagedPythonServer> = None;
    let (upstream, kind): (Arc<dyn vision_proxy::Upstream>, UpstreamKind) =
        match upstream_kind.as_str() {
            "openai" => {
                let api_key = match api_key_env {
                    None => require_upstream_api_key(Some("OPENAI_API_KEY"))?,
                    Some(name) if name.trim().is_empty() => require_upstream_api_key(None)?,
                    Some(name) => require_upstream_api_key(Some(name.trim()))?,
                };
                let model = model.unwrap_or_else(|| vision_proxy::OPENAI_DEFAULT_MODEL.to_string());
                let base_url = vision_base_url
                    .unwrap_or_else(|| vision_proxy::OPENAI_DEFAULT_BASE_URL.to_string());
                (
                    Arc::new(OpenAiUpstream::new(api_key, model, base_url)),
                    UpstreamKind::OpenAi,
                )
            }
            "ollama" => {
                let model = model.unwrap_or_else(|| vision_proxy::OLLAMA_DEFAULT_MODEL.to_string());
                let base_url = vision_base_url
                    .unwrap_or_else(|| vision_proxy::OLLAMA_DEFAULT_BASE_URL.to_string());
                let mut upstream = OllamaUpstream::new(model, base_url);
                if let Some(collector) = &collector {
                    upstream = upstream.with_data_collector(collector.clone());
                }
                (Arc::new(upstream), UpstreamKind::Ollama)
            }
            "mlx" => {
                let base_url = vision_base_url
                    .unwrap_or_else(|| vision_proxy::MLX_DEFAULT_BASE_URL.to_string());
                if spawn_server {
                    _python_child = Some(spawn_mlx_server(&base_url, server_script)?);
                }
                let mut upstream = MlxUpstream::new(base_url);
                if let Some(collector) = &collector {
                    upstream = upstream.with_data_collector(collector.clone());
                }
                (Arc::new(upstream), UpstreamKind::Mlx)
            }
            _other => anyhow::bail!(
                "unsupported upstream {upstream:?}; expected \"openai\", \"ollama\", or \"mlx\""
            ),
        };

    let config = ProxyConfig {
        bind,
        path,
        bearer_token,
        upstream_kind: kind,
    };

    serve_vision_proxy(config, upstream)
        .await
        .context("vision-proxy server failed")?;

    Ok(())
}

/// Spawn the canonical Python vision server as a managed child and wait for
/// it to accept connections. The child is killed when it goes out of scope.
fn spawn_mlx_server(base_url: &str, server_script: Option<PathBuf>) -> Result<ManagedPythonServer> {
    let script = server_script
        .or_else(|| std::env::var_os("BOBBY_VISION_SERVER_SCRIPT").map(PathBuf::from))
        .map(Ok)
        .unwrap_or_else(find_vision_server_script)
        .context(
            "could not locate vision_server.py; pass --server-script or set \
             BOBBY_VISION_SERVER_SCRIPT",
        )?;
    let bind = url::Url::parse(base_url)
        .ok()
        .and_then(|url| {
            let host = url.host_str()?.to_string();
            let port = url.port_or_known_default()?;
            Some(format!("{host}:{port}"))
        })
        .unwrap_or_else(|| "127.0.0.1:9101".to_string());

    let mut cmd = std::process::Command::new("python3");
    cmd.arg(&script)
        .arg("--bind")
        .arg(&bind)
        .arg("--provider")
        .arg("mlx-vlm");
    // Homebrew Python + MLX both link libomp; without this the child dies
    // on startup with a duplicate-runtime abort.
    cmd.env("KMP_DUPLICATE_LIB_OK", "TRUE");
    cmd.stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::inherit());

    let mut child = cmd.spawn().context("failed to spawn vision_server.py")?;

    // Wait for readiness: probe the bind for up to 30s (model load is slow).
    let addr: SocketAddr = bind.parse().context("invalid mlx server bind")?;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        if std::net::TcpStream::connect_timeout(&addr, Duration::from_millis(250)).is_ok() {
            break;
        }
        if let Some(status) = child.try_wait().context("mlx server wait failed")? {
            anyhow::bail!("vision_server.py exited early with {status}");
        }
        if std::time::Instant::now() > deadline {
            let _ = child.kill();
            let _ = child.wait();
            anyhow::bail!("vision_server.py did not become reachable on {addr}");
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    Ok(ManagedPythonServer { child })
}

/// Locate vision_server.py from the repo layout relative to the bobby
/// executable (source checkout or installed alongside the repo).
fn find_vision_server_script() -> Result<PathBuf> {
    let exe = std::env::current_exe().context("failed to resolve current executable")?;
    for ancestor in exe.ancestors().skip(1) {
        let candidate = ancestor.join("scripts/vision-mlx/vision_server.py");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    let cwd_candidate = PathBuf::from("scripts/vision-mlx/vision_server.py");
    if cwd_candidate.is_file() {
        return Ok(cwd_candidate);
    }
    anyhow::bail!("vision_server.py not found near {}", exe.display())
}

/// Kill-on-drop guard for the spawned Python vision server.
struct ManagedPythonServer {
    child: std::process::Child,
}

impl Drop for ManagedPythonServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub(crate) fn default_context_dir() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .ok_or_else(|| anyhow::anyhow!("config directory unavailable"))?
        .join("bobby-browser")
        .join("context"))
}

async fn run_context(command: ContextCommands) -> Result<()> {
    match command {
        ContextCommands::List { profile, dir } => {
            let dir = match dir {
                Some(dir) => dir,
                None => default_context_dir()?,
            };
            let (store, report) = context_store::ContextStore::open(&dir, &profile)
                .await
                .map_err(|error| match error {
                    context_store::ContextStoreError::AlreadyLocked => anyhow::anyhow!(
                        "context store is held by a running bobby process; stop it first"
                    ),
                    context_store::ContextStoreError::LockUnusable(reason) => {
                        anyhow::anyhow!("context store lockfile is unusable: {reason}")
                    }
                    other => anyhow::anyhow!("{other}"),
                })?;
            for skipped in &report.skipped {
                eprintln!(
                    "skipped unreadable site file {} ({})",
                    skipped.file.display(),
                    skipped.reason
                );
            }
            let sites = store.list_sites().await;
            if sites.is_empty() {
                println!("no remembered sites for profile {profile}");
            } else {
                for site in sites {
                    println!("{site}");
                }
            }
        }
        ContextCommands::Forget { site, profile, dir } => {
            let dir = match dir {
                Some(dir) => dir,
                None => default_context_dir()?,
            };
            let (store, _) = context_store::ContextStore::open(&dir, &profile)
                .await
                .map_err(|error| match error {
                    context_store::ContextStoreError::AlreadyLocked => anyhow::anyhow!(
                        "context store is held by a running bobby process; stop it first"
                    ),
                    context_store::ContextStoreError::LockUnusable(reason) => {
                        anyhow::anyhow!("context store lockfile is unusable: {reason}")
                    }
                    other => anyhow::anyhow!("{other}"),
                })?;
            store.forget(&site).await?;
            drop(store);
            let (reopened, _) = context_store::ContextStore::open(&dir, &profile).await?;
            anyhow::ensure!(
                reopened.site(&site).await.is_none(),
                "forget of {site} did not take"
            );
            println!("forgot {site}");
        }
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

fn run_openshell(command: OpenshellCommand) -> Result<()> {
    match command {
        OpenshellCommand::Install {
            path,
            mcp_host,
            mcp_port,
            agent_binary,
        } => {
            let root = match path {
                Some(path) => path,
                None => std::env::current_dir()?,
            };
            let options = openshell::PackOptions {
                mcp_host,
                mcp_port,
                agent_binary,
            };
            let pack = openshell::install_pack(&root, &options)?;
            println!("ok: wrote OpenShell pack at {}", pack.dir.display());
            println!("  policy  {}", pack.policy.display());
            println!("  network {}", pack.policy_network.display());
            println!("  mcp     {}", pack.mcp.display());
            println!("  readme  {}", pack.readme.display());
            println!("  skill   {}", pack.skill.display());
            println!("next: `bobby serve`, then `bobby openshell provision --sandbox <id>`");
        }
        OpenshellCommand::Provision {
            common,
            sandbox,
            ttl_hours,
            mcp_host,
            mcp_port,
            capabilities_preset,
        }
        | OpenshellCommand::Rotate {
            common,
            sandbox,
            ttl_hours,
            mcp_host,
            mcp_port,
            capabilities_preset,
        } => {
            let config_path = resolve_config_path(common.config);
            let config = AppConfig::load(&config_path)
                .with_context(|| format!("failed to load config from {}", config_path.display()))?;
            let bootstrap_path = resolve_bootstrap_path(common.bootstrap_env)?;
            let pack = openshell::PackOptions {
                mcp_host,
                mcp_port,
                agent_binary: "/usr/local/bin/claude".to_owned(),
            };
            let result = openshell::provision_sandbox(
                &sandbox,
                common.base_url,
                &config,
                &bootstrap_path,
                common.token,
                ttl_hours,
                &pack,
                capabilities_preset,
            )?;
            if result.replaced_prior {
                println!(
                    "ok: replaced prior principal for sandbox `{}`",
                    result.sandbox
                );
            }
            println!(
                "ok: provisioned sandbox `{}` principal {} expires {}",
                result.sandbox, result.principal_id, result.expires_at
            );
            println!(
                "injection env (0600, do not commit): {}",
                result.env_path.display()
            );
            println!(
                "inject AUTOMATION_RUNTIME_TOKEN from that file into the OpenShell sandbox; MCP URL {}",
                result.mcp_url
            );
        }
        OpenshellCommand::List => {
            let list = openshell::list_sandboxes()?;
            if list.is_empty() {
                println!("no local OpenShell sandboxes");
            } else {
                for status in list {
                    println!(
                        "{}\t{}\t{}\t{}\t{}",
                        status.sandbox,
                        status.principal_id,
                        status.capabilities_preset,
                        status.expires_at,
                        status.mcp_url
                    );
                }
            }
        }
        OpenshellCommand::Status { sandbox } => {
            let status = openshell::read_sandbox_status(&sandbox)?;
            println!("{}", serde_json::to_string_pretty(&status)?);
        }
        OpenshellCommand::Revoke { common, sandbox } => {
            let config_path = resolve_config_path(common.config);
            let config = AppConfig::load(&config_path)
                .with_context(|| format!("failed to load config from {}", config_path.display()))?;
            let bootstrap_path = resolve_bootstrap_path(common.bootstrap_env)?;
            let meta = openshell::revoke_sandbox(
                &sandbox,
                common.base_url,
                &config,
                &bootstrap_path,
                common.token,
            )?;
            println!(
                "ok: revoked sandbox `{sandbox}` (cleared {})",
                meta.display()
            );
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

pub(crate) fn resolve_bootstrap_path(cli: Option<PathBuf>) -> Result<PathBuf> {
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

fn run_init(
    force: bool,
    ttl_days: u32,
    preset: bootstrap_local::BootstrapPreset,
    path: Option<PathBuf>,
    emit: Option<onboarding::EmitFormat>,
) -> Result<()> {
    let path = match path {
        Some(path) => path,
        None => bootstrap_local::default_bootstrap_path()?,
    };
    let material = bootstrap_local::generate_bootstrap_for_preset(
        chrono::Duration::days(i64::from(ttl_days)),
        preset,
    )?;
    bootstrap_local::write_bootstrap_env(&path, &material, force)?;
    println!("{}", material.bearer());
    eprintln!("Wrote bootstrap env to {}", path.display());
    eprintln!(
        "Preset: {} ({})",
        preset.as_str(),
        match preset {
            bootstrap_local::BootstrapPreset::Agent => "no authority:admin",
            bootstrap_local::BootstrapPreset::Unrestricted => "includes authority:admin",
        }
    );
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
        if let Err(rollback_error) = wrapper_install.rollback(&config.wrapper_path) {
            return Err(anyhow::Error::new(error).context(format!(
                "native-host manifest installation failed and wrapper rollback also failed: {rollback_error}"
            )));
        }
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
    let Some(command) = text
        .strip_prefix("#!/bin/sh\nexec ")
        .and_then(|text| text.strip_suffix('\n'))
    else {
        return false;
    };
    if command.contains('\n') {
        return false;
    }
    let Some(rest) = consume_shell_quoted(command) else {
        return false;
    };
    let Some(rest) = rest.strip_prefix(" firefox-native-host --descriptor ") else {
        return false;
    };
    consume_shell_quoted(rest).is_some_and(str::is_empty)
}

/// Consume exactly the single-quoted form emitted by [`shell_quote`],
/// including its close-escape-reopen form for an embedded apostrophe.
fn consume_shell_quoted(input: &str) -> Option<&str> {
    let bytes = input.as_bytes();
    if bytes.first() != Some(&b'\'') {
        return None;
    }
    let mut index = 1;
    while index < bytes.len() {
        if bytes[index] != b'\'' {
            index += 1;
            continue;
        }
        if bytes.get(index..index + 4) == Some(b"'\\''") {
            index += 4;
            continue;
        }
        return Some(&input[index + 1..]);
    }
    None
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
    fn rollback(self, path: &Path) -> std::io::Result<()> {
        match self {
            Self::Unchanged => Ok(()),
            Self::Created(created) => created.rollback(path),
            Self::Replaced { previous, mode } => write_exact_file_atomic(path, &previous, mode),
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

    fn rollback(self, path: &Path) -> std::io::Result<()> {
        self.rollback_ref(path)
    }

    fn rollback_ref(&self, path: &Path) -> std::io::Result<()> {
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let metadata = match std::fs::symlink_metadata(path) {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) => return Err(error),
            };
            if metadata.dev() == self.device && metadata.ino() == self.inode {
                std::fs::remove_file(path)?;
            }
            Ok(())
        }
        #[cfg(not(unix))]
        match std::fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
        }
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
        let previous_mode = installed_file_mode(path, mode)?;
        if !kind.is_managed(&previous) {
            return Err(destination_already_exists());
        }
        write_exact_file_atomic(path, contents, mode)?;
        return Ok(InstallFileOutcome::Replaced {
            previous,
            mode: previous_mode,
        });
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
                let previous_mode = installed_file_mode(path, mode)?;
                if !kind.is_managed(&previous) {
                    return Err(destination_already_exists());
                }
                std::fs::rename(&pending, path)?;
                return Ok(InstallFileOutcome::Replaced {
                    previous,
                    mode: previous_mode,
                });
            }
            Err(error) => return Err(error),
        }
        if let Err(error) = std::fs::remove_file(&pending) {
            if let Err(rollback_error) = created.rollback(path) {
                return Err(std::io::Error::new(
                    rollback_error.kind(),
                    format!(
                        "failed to remove pending native-host file ({error}); rollback also failed: {rollback_error}"
                    ),
                ));
            }
            return Err(error);
        }
        Ok(InstallFileOutcome::Created(created))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(pending);
    }
    result
}

fn installed_file_mode(path: &Path, _fallback: u32) -> std::io::Result<u32> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        Ok(std::fs::symlink_metadata(path)?.permissions().mode() & 0o7777)
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        Ok(_fallback)
    }
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
    use super::doctor::{
        check_bootstrap_expiry, check_vision_acp, check_vision_provider, check_vision_upstream_key,
        handshake_error_status, run_doctor, vision_auth_discovery_check,
        vision_endpoint_unreachable_detail, DoctorReport, DoctorStatus, BOOTSTRAP_EXPIRY_WARN_DAYS,
    };
    use super::*;
    use auth_broker::{AuthCapabilities, AuthStrategy};
    use companion_protocol::{
        BrowserEngine, BrowserIdentity, CompanionCapabilities, PROTOCOL_VERSION,
    };
    use config::VisionConfig;
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

    #[test]
    fn native_host_wrapper_ownership_requires_exact_generated_grammar() {
        for managed in [
            b"#!/bin/sh\nexec '/opt/bobby' firefox-native-host --descriptor '/tmp/descriptor.json'\n"
                .as_slice(),
            b"#!/bin/sh\nexec '/opt/bob'\\''by' firefox-native-host --descriptor '/tmp/descriptor with space.json'\n"
                .as_slice(),
        ] {
            assert!(is_bobby_managed_wrapper(managed), "managed: {managed:?}");
        }

        for operator_owned in [
            b"#!/bin/sh\n# example firefox-native-host --descriptor config.json\nexec /opt/operator\n"
                .as_slice(),
            b"#!/bin/sh\nexec /opt/operator\nprintf ' firefox-native-host --descriptor '\n"
                .as_slice(),
            b"#!/bin/sh\n'/opt/bobby' firefox-native-host --descriptor '/tmp/descriptor.json'\n"
                .as_slice(),
            b"#!/bin/sh\nexec '/opt/bobby' firefox-native-host --descriptor '/tmp/descriptor.json'\necho extra\n"
                .as_slice(),
            b"#!/bin/sh\nexec '/opt/bobby firefox-native-host --descriptor '/tmp/descriptor.json'\n"
                .as_slice(),
            b"#!/bin/sh\nexec '/opt/bobby' other-command --descriptor '/tmp/descriptor.json'\n"
                .as_slice(),
            b"\xff\xfe".as_slice(),
        ] {
            assert!(
                !is_bobby_managed_wrapper(operator_owned),
                "operator-owned: {operator_owned:?}"
            );
        }
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
    fn native_host_replacement_rollback_restores_original_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!(
            "bobby-native-host-rollback-mode-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let wrapper = root.join("firefox-native-host");
        let original =
            b"#!/bin/sh\nexec '/old/bobby' firefox-native-host --descriptor '/old/descriptor.json'\n";
        std::fs::write(&wrapper, original).unwrap();
        let mut permissions = std::fs::metadata(&wrapper).unwrap().permissions();
        permissions.set_mode(0o750);
        std::fs::set_permissions(&wrapper, permissions).unwrap();

        let outcome = install_exact_file(
            &wrapper,
            b"#!/bin/sh\nexec '/new/bobby' firefox-native-host --descriptor '/new/descriptor.json'\n",
            0o700,
            NativeHostFileKind::Wrapper,
        )
        .unwrap();
        outcome.rollback(&wrapper).unwrap();

        assert_eq!(std::fs::read(&wrapper).unwrap(), original);
        assert_eq!(
            std::fs::metadata(&wrapper).unwrap().permissions().mode() & 0o777,
            0o750
        );
        std::fs::remove_file(wrapper).unwrap();
        std::fs::remove_dir(root).unwrap();
    }

    #[test]
    fn native_host_replacement_rollback_reports_restore_failure() {
        let root = std::env::temp_dir().join(format!(
            "bobby-native-host-rollback-failure-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let wrapper = root.join("firefox-native-host");
        std::fs::write(
            &wrapper,
            b"#!/bin/sh\nexec '/old/bobby' firefox-native-host --descriptor '/old/descriptor.json'\n",
        )
        .unwrap();
        let outcome = install_exact_file(
            &wrapper,
            b"#!/bin/sh\nexec '/new/bobby' firefox-native-host --descriptor '/new/descriptor.json'\n",
            0o700,
            NativeHostFileKind::Wrapper,
        )
        .unwrap();
        std::fs::remove_file(&wrapper).unwrap();
        std::fs::create_dir(&wrapper).unwrap();

        let error = outcome.rollback(&wrapper).unwrap_err();
        assert_ne!(error.kind(), std::io::ErrorKind::NotFound);

        std::fs::remove_dir(wrapper).unwrap();
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
    fn openshell_clap_parses_install_provision_revoke() {
        use clap::Parser;
        let install = Cli::try_parse_from([
            "bobby",
            "openshell",
            "install",
            "--mcp-host",
            "host.containers.internal",
            "--mcp-port",
            "8888",
        ])
        .unwrap();
        match install.command {
            Some(CliCommand::Openshell {
                command:
                    OpenshellCommand::Install {
                        mcp_host, mcp_port, ..
                    },
            }) => {
                assert_eq!(mcp_host, "host.containers.internal");
                assert_eq!(mcp_port, 8888);
            }
            _ => panic!("unexpected openshell install parse"),
        }
        let provision = Cli::try_parse_from([
            "bobby",
            "openshell",
            "provision",
            "--sandbox",
            "demo-1",
            "--capabilities-preset",
            "agent",
        ])
        .unwrap();
        match provision.command {
            Some(CliCommand::Openshell {
                command:
                    OpenshellCommand::Provision {
                        sandbox,
                        capabilities_preset,
                        ..
                    },
            }) => {
                assert_eq!(sandbox, "demo-1");
                assert_eq!(
                    capabilities_preset,
                    openshell::OpenshellCapabilityPreset::Agent
                );
            }
            _ => panic!("unexpected openshell provision parse"),
        }
        let revoke =
            Cli::try_parse_from(["bobby", "openshell", "revoke", "--sandbox", "demo-1"]).unwrap();
        match revoke.command {
            Some(CliCommand::Openshell {
                command: OpenshellCommand::Revoke { sandbox, .. },
            }) => assert_eq!(sandbox, "demo-1"),
            _ => panic!("unexpected openshell revoke parse"),
        }
    }

    #[test]
    fn context_command_parses() {
        use clap::Parser;
        let parsed = Cli::try_parse_from([
            "bobby",
            "context",
            "forget",
            "https://example.com",
            "--profile",
            "profile-a",
        ])
        .unwrap();
        match parsed.command {
            Some(CliCommand::Context {
                command:
                    ContextCommands::Forget {
                        site,
                        profile,
                        dir: None,
                    },
            }) => {
                assert_eq!(site, "https://example.com");
                assert_eq!(profile, "profile-a");
            }
            _ => panic!("unexpected context parse"),
        }
    }

    #[tokio::test]
    async fn context_forget_is_immediate_and_total() {
        let root = tempfile::tempdir().unwrap();
        let (store, _) = context_store::ContextStore::open(root.path(), "profile-a")
            .await
            .unwrap();
        let mut intents = std::collections::BTreeMap::new();
        intents.insert(
            "fill".to_string(),
            context_store::IntentStats {
                success_count: 1,
                failure_count: 0,
                last_verified_day: Some(100),
                source: None,
            },
        );
        let mut forms = std::collections::BTreeMap::new();
        forms.insert(
            "page".to_string(),
            context_store::FormContext {
                controls: vec![context_store::ControlContext {
                    role: "textbox".into(),
                    accessible_name: "Email".into(),
                    ordinal: None,
                    form_membership: "page".into(),
                    intents,
                }],
            },
        );
        let mut pages = std::collections::BTreeMap::new();
        pages.insert("/login".to_string(), context_store::PageContext { forms });
        store
            .upsert_site("https://example.com", context_store::SiteContext { pages })
            .await;
        assert!(store.flush().await.is_empty());
        drop(store);

        run_context(ContextCommands::Forget {
            site: "https://example.com".to_string(),
            profile: "profile-a".to_string(),
            dir: Some(root.path().to_path_buf()),
        })
        .await
        .unwrap();

        let (reopened, report) = context_store::ContextStore::open(root.path(), "profile-a")
            .await
            .unwrap();
        assert_eq!(report.sites_loaded, 0);
        assert!(reopened.list_sites().await.is_empty());
        assert!(std::fs::read_dir(reopened.root())
            .unwrap()
            .all(|entry| !entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .ends_with(".json")));
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
    fn doctor_reports_context_store_without_claiming_its_lock() {
        let _lock = DOCTOR_ENV_LOCK.lock().unwrap();
        let env = DoctorEnvGuard::clear();
        env.set(
            "AUTOMATION_RUNTIME_BROWSER_SELECTION",
            r#"{"preference":{"mode":"managedChromium"}}"#,
        );
        let root = tempfile::tempdir().unwrap();
        let config = doctor_config_fixture(root.path());
        std::fs::write(
            &config,
            format!(
                "{}\n[context]\ndir = \"{}\"\n",
                std::fs::read_to_string(&config).unwrap(),
                root.path().join("context").display()
            ),
        )
        .unwrap();
        let profile = root.path().join("context").join("profile-a");
        std::fs::create_dir_all(&profile).unwrap();
        std::fs::write(profile.join("https___example.com.json"), b"{}").unwrap();
        std::fs::write(profile.join(".context-store.lock"), b"1\n").unwrap();

        let report = run_doctor(Some(config), None, false).unwrap();
        let check = report
            .checks
            .iter()
            .find(|check| check.name == "context-store")
            .expect("a context-store check");
        assert!(check.detail.contains("1 site files"), "{check:?}");
        assert!(check.detail.contains("lock held"), "{check:?}");
        assert_eq!(check.status, DoctorStatus::Ok);
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
    fn vision_login_clap_parses_named_profile() {
        use clap::Parser;
        let cli = Cli::try_parse_from([
            "bobby",
            "vision",
            "login",
            "codex",
            "--config",
            "/tmp/config.toml",
        ])
        .unwrap();
        match cli.command {
            Some(CliCommand::Vision {
                command: VisionCommands::Login(VisionLoginArgs { name, config }),
            }) => {
                assert_eq!(name, "codex");
                assert_eq!(config.as_deref(), Some(Path::new("/tmp/config.toml")));
            }
            _ => panic!("unexpected vision login parse"),
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
        let config = AppConfig::from_toml_str(
            r#"
[vision]
backend = "acp"
profile = "fake"
[vision.acp_profiles.fake]
command = "definitely-not-a-real-acp-harness"
auth = "oauth-device-code"
"#,
        )
        .unwrap();
        let checks = check_vision_acp(&config);
        assert_eq!(checks.len(), 3);
        assert_eq!(checks[0].name, "vision-routing");
        assert_eq!(checks[1].name, "vision-acp-reachability");
        assert_eq!(checks[2].name, "vision-auth-path");
        assert!(checks[2].detail.contains("could not discover"));
    }

    #[test]
    fn doctor_auth_path_warns_when_configured_strategy_is_not_advertised() {
        let check = vision_auth_discovery_check(
            AuthStrategy::OAuthDeviceCode,
            Ok(AuthCapabilities::new([AuthStrategy::Advertised])),
        );
        assert_eq!(check.status, DoctorStatus::Warn);
        assert!(check.detail.contains("OAuthDeviceCode"));
        assert!(check.detail.contains("Advertised"));
        assert!(check.detail.contains("misconfigured"));
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
    fn doctor_warns_when_vision_assist_has_no_route() {
        let _lock = DOCTOR_ENV_LOCK.lock().unwrap();
        let _env = DoctorEnvGuard::clear();
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        std::fs::write(&config, "").unwrap();
        let bootstrap = root.path().join("bootstrap.env");
        let material = bootstrap_local::generate_bootstrap(chrono::Duration::days(30)).unwrap();
        bootstrap_local::write_bootstrap_env(&bootstrap, &material, true).unwrap();

        let report = run_doctor(Some(config), Some(bootstrap), false).unwrap();
        let check = report.check("vision-route").expect("vision-route check");
        assert_eq!(check.status, DoctorStatus::Warn);
        assert!(check.detail.contains("bobby vision connect"));
    }

    #[test]
    fn doctor_ok_when_vision_assist_has_endpoint_route() {
        let _lock = DOCTOR_ENV_LOCK.lock().unwrap();
        let _env = DoctorEnvGuard::clear();
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        std::fs::write(
            &config,
            r#"[vision]
endpoint_url = "http://127.0.0.1:9100/vision"
"#,
        )
        .unwrap();
        let bootstrap = root.path().join("bootstrap.env");
        let material = bootstrap_local::generate_bootstrap(chrono::Duration::days(30)).unwrap();
        bootstrap_local::write_bootstrap_env(&bootstrap, &material, true).unwrap();

        let report = run_doctor(Some(config), Some(bootstrap), false).unwrap();
        let check = report.check("vision-route").expect("vision-route check");
        assert_eq!(check.status, DoctorStatus::Ok);
        let gate = report
            .check("vision-session-gate")
            .expect("vision-session-gate check");
        assert_eq!(gate.status, DoctorStatus::Ok);
        assert!(gate.detail.contains("executionPolicy.visionAssist"));
        let js_gate = report
            .check("javascript-session-gate")
            .expect("javascript-session-gate check");
        assert_eq!(js_gate.status, DoctorStatus::Ok);
        assert!(js_gate
            .detail
            .contains("executionPolicy.javascriptEvaluation"));
        let jobs = report.check("job-handlers").expect("job-handlers check");
        assert_eq!(jobs.status, DoctorStatus::Ok);
        assert!(jobs.detail.contains("echo"));
        assert!(jobs.detail.contains("sleep"));
        assert!(jobs.detail.contains("http_probe"));
        assert!(jobs.detail.contains("http_wait"));
        assert!(jobs.detail.contains("http_fetch"));
    }

    #[test]
    fn doctor_warns_on_dual_vision_and_nodes_config() {
        let _lock = DOCTOR_ENV_LOCK.lock().unwrap();
        let _env = DoctorEnvGuard::clear();
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        std::fs::write(
            &config,
            r#"[vision]
endpoint_url = "https://legacy.example/propose"

[nodes.local]
kind = "vision"
endpoint_url = "http://127.0.0.1:8080/propose"
"#,
        )
        .unwrap();
        let bootstrap = root.path().join("bootstrap.env");
        let material = bootstrap_local::generate_bootstrap(chrono::Duration::days(30)).unwrap();
        bootstrap_local::write_bootstrap_env(&bootstrap, &material, true).unwrap();

        let report = run_doctor(Some(config), Some(bootstrap), false).unwrap();
        let check = report
            .check("vision-config-dual")
            .expect("vision-config-dual check");
        assert_eq!(check.status, DoctorStatus::Warn);
        assert!(check.detail.contains("[nodes] wins"));
        let route = report.check("vision-route").expect("vision-route check");
        assert_eq!(route.status, DoctorStatus::Ok);
    }

    #[test]
    fn doctor_reports_agent_bootstrap_preset() {
        let _lock = DOCTOR_ENV_LOCK.lock().unwrap();
        let _env = DoctorEnvGuard::clear();
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("config.toml");
        std::fs::write(&config, "").unwrap();
        let bootstrap = root.path().join("bootstrap.env");
        let material = bootstrap_local::generate_bootstrap_for_preset(
            chrono::Duration::days(30),
            bootstrap_local::BootstrapPreset::Agent,
        )
        .unwrap();
        bootstrap_local::write_bootstrap_env(&bootstrap, &material, true).unwrap();

        let report = run_doctor(Some(config), Some(bootstrap), false).unwrap();
        let check = report
            .check("bootstrap-preset")
            .expect("bootstrap-preset check");
        assert_eq!(check.status, DoctorStatus::Ok);
        assert!(check.detail.contains("agent"));
        assert!(check.detail.contains("no authority:admin"));
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
        assert!(Cli::try_parse_from(["bobby", "acp-stdio", "--vision", "--no-vision"]).is_err());
    }

    #[test]
    fn acp_stdio_command_parses() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["bobby", "acp-stdio"]).unwrap();
        assert!(matches!(cli.command, Some(CliCommand::AcpStdio { .. })));
    }

    #[test]
    fn doctor_warns_when_sidecar_gateways_are_missing() {
        let _lock = DOCTOR_ENV_LOCK.lock().unwrap();
        let _env = DoctorEnvGuard::clear();
        // Clear PATH so siblings of the test harness are the only candidates;
        // the cargo test binary has no mcp-gateway / acp-gateway next to it.
        let previous = std::env::var_os("PATH");
        unsafe { std::env::set_var("PATH", "") };
        let root = tempfile::tempdir().unwrap();
        let report = run_doctor(
            Some(root.path().join("missing-config.toml")),
            Some(root.path().join("missing-bootstrap.env")),
            false,
        )
        .unwrap();
        assert_eq!(
            report.check("mcp-gateway").unwrap().status,
            DoctorStatus::Warn
        );
        assert_eq!(
            report.check("acp-gateway").unwrap().status,
            DoctorStatus::Warn
        );
        assert!(report
            .check("mcp-gateway")
            .unwrap()
            .detail
            .contains("bobby install --cli"));
        match previous {
            Some(value) => unsafe { std::env::set_var("PATH", value) },
            None => unsafe { std::env::remove_var("PATH") },
        }
    }

    #[test]
    fn cdp_command_parses_port_override() {
        use clap::Parser;
        let cli = Cli::try_parse_from(["bobby", "cdp", "--cdp-port", "9333"]).unwrap();
        match cli.command {
            Some(CliCommand::Cdp {
                cdp_port,
                vision,
                no_vision,
                ..
            }) => {
                assert_eq!(cdp_port, Some(9333));
                assert!(!vision);
                assert!(!no_vision);
            }
            _ => panic!("expected Cdp command"),
        }
    }

    #[test]
    fn doctor_reports_cdp_listen_when_enabled_in_config() {
        let _lock = DOCTOR_ENV_LOCK.lock().unwrap();
        let env = DoctorEnvGuard::clear();
        env.set(
            "AUTOMATION_RUNTIME_BROWSER_SELECTION",
            r#"{"preference":{"mode":"managedChromium"}}"#,
        );
        let root = tempfile::tempdir().unwrap();
        let config = doctor_config_fixture(root.path());
        let mut text = std::fs::read_to_string(&config).unwrap();
        text.push_str(
            r#"
[cdp]
enabled = true
host = "127.0.0.1"
port = 9333
"#,
        );
        std::fs::write(&config, text).unwrap();

        let report = run_doctor(
            Some(config),
            Some(root.path().join("missing-bootstrap.env")),
            false,
        )
        .unwrap();

        let check = report.check("cdp-listen").expect("cdp-listen check");
        assert_eq!(check.status, DoctorStatus::Ok);
        assert!(check.detail.contains("127.0.0.1:9333"));
    }
}
