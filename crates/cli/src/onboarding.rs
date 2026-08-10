//! Agent onboarding: `bobby init --emit` client config fragments, the
//! `bobby doctor` MCP handshake check, `bobby mcp-stdio` (zero-wiring MCP
//! entrypoint), and the `bobby install` interactive installer.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{anyhow, Context, Result};
use clap::ValueEnum;
use dialoguer::{theme::ColorfulTheme, Confirm, MultiSelect, Select};

use crate::vision_readiness::{cached_hugging_face_model, download_and_verify_mlx_model};

/// Agent host config dialect for `bobby init --emit`.
#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum EmitFormat {
    /// Claude Code project `.mcp.json` (`mcpServers`).
    Claude,
    /// Zed `settings.json` (`context_servers`).
    Zed,
    /// VS Code `.vscode/mcp.json` (`servers`).
    Vscode,
    /// Generic MCP JSON fragment (same shape as Claude's).
    Json,
    /// OpenShell sandbox MCP client JSON (streamable HTTP to host bobby).
    Openshell,
}

const GATEWAY_COMMAND: &str = if cfg!(windows) {
    "mcp-gateway.exe"
} else {
    "mcp-gateway"
};

const ACP_GATEWAY_COMMAND: &str = if cfg!(windows) {
    "acp-gateway.exe"
} else {
    "acp-gateway"
};

fn bootstrap_env_placeholders() -> serde_json::Value {
    serde_json::json!({
        "AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN": "${AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN}",
        "AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL": "${AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL}",
        "AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES": "${AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES}",
        "AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT": "${AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT}",
    })
}

/// Render the MCP client config fragment for one agent host. The fragment
/// never carries the credential itself: each host expands `${VAR}` from its
/// own environment, which is where `bootstrap.env` is expected to be sourced.
pub fn emit_mcp_config(format: EmitFormat) -> String {
    match format {
        EmitFormat::Openshell => {
            return crate::openshell::emit_mcp_config(&crate::openshell::PackOptions::default());
        }
        EmitFormat::Claude | EmitFormat::Json => {}
        EmitFormat::Vscode | EmitFormat::Zed => {}
    }
    let fragment = match format {
        EmitFormat::Claude | EmitFormat::Json => serde_json::json!({
            "mcpServers": {
                "bobby-browser": {
                    "command": GATEWAY_COMMAND,
                    "env": bootstrap_env_placeholders(),
                }
            }
        }),
        EmitFormat::Vscode => serde_json::json!({
            "servers": {
                "bobby-browser": {
                    "type": "stdio",
                    "command": GATEWAY_COMMAND,
                    "env": bootstrap_env_placeholders(),
                }
            }
        }),
        EmitFormat::Zed => serde_json::json!({
            "context_servers": {
                "bobby-browser": {
                    "command": {
                        "path": GATEWAY_COMMAND,
                        "args": [],
                        "env": bootstrap_env_placeholders(),
                    }
                }
            }
        }),
        EmitFormat::Openshell => unreachable!("openshell returned above"),
    };
    serde_json::to_string_pretty(&fragment).expect("emitted config is serializable")
}

pub struct HandshakeReport {
    pub tools: usize,
    pub bytes: usize,
    pub server_version: String,
}

/// Resolve the gateway binary: a sibling of the current executable first
/// (workspace `target/` layouts keep them together), then PATH.
fn resolve_gateway() -> Result<PathBuf> {
    resolve_sibling_or_path(GATEWAY_COMMAND)
}

fn resolve_acp_gateway() -> Result<PathBuf> {
    resolve_sibling_or_path(ACP_GATEWAY_COMMAND)
}

fn resolve_sibling_or_path(command: &str) -> Result<PathBuf> {
    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            let sibling = dir.join(command);
            if sibling.is_file() {
                return Ok(sibling);
            }
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(command);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(anyhow!(
        "{command} not found next to the bobby binary or on PATH"
    ))
}

/// Absolute path to a sidecar binary next to bobby or on PATH, if present.
pub fn find_sidecar_binary(command: &str) -> Option<PathBuf> {
    resolve_sibling_or_path(command).ok()
}

pub fn mcp_gateway_command() -> &'static str {
    GATEWAY_COMMAND
}

pub fn acp_gateway_command() -> &'static str {
    ACP_GATEWAY_COMMAND
}

struct JsonRpcChild {
    child: Child,
    stdin: std::process::ChildStdin,
    /// Lines from the child's stdout, produced on a reader thread so a mute
    /// gateway trips the 15s deadline instead of blocking `read_line`
    /// forever. The thread exits at EOF; if the child never closes, the
    /// thread leaks until process exit (doctor is short-lived).
    lines: std::sync::mpsc::Receiver<std::io::Result<String>>,
    next_id: u64,
}

impl JsonRpcChild {
    fn call(&mut self, method: &str, params: serde_json::Value) -> Result<serde_json::Value> {
        self.next_id += 1;
        let id = self.next_id;
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        writeln!(self.stdin, "{}", serde_json::to_string(&frame)?)?;
        self.stdin.flush()?;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            if remaining.is_zero() {
                return Err(anyhow!("{method}: gateway did not answer within 15s"));
            }
            let line = match self.lines.recv_timeout(remaining) {
                Ok(Ok(line)) => line,
                Ok(Err(error)) => return Err(error.into()),
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    return Err(anyhow!("{method}: gateway did not answer within 15s"));
                }
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                    return Err(anyhow!("{method}: gateway closed stdout"));
                }
            };
            let read = line.len();
            if read == 0 {
                return Err(anyhow!("{method}: gateway closed stdout"));
            }
            let frame: serde_json::Value = serde_json::from_str(line.trim())
                .with_context(|| format!("{method}: unparseable frame from gateway"))?;
            if frame.get("id").and_then(serde_json::Value::as_u64) != Some(id) {
                continue;
            }
            if let Some(error) = frame.get("error") {
                return Err(anyhow!("{method}: gateway returned {error}"));
            }
            return Ok(frame
                .get("result")
                .cloned()
                .unwrap_or(serde_json::Value::Null));
        }
    }

    fn notify(&mut self, method: &str) -> Result<()> {
        let frame = serde_json::json!({"jsonrpc": "2.0", "method": method});
        writeln!(self.stdin, "{}", serde_json::to_string(&frame)?)?;
        self.stdin.flush()?;
        Ok(())
    }
}

impl Drop for JsonRpcChild {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Spawn the stdio gateway with the bootstrap credential, run `initialize`
/// and `tools/list`, and report the advertised surface size. Run by
/// `bobby doctor` to catch an oversized or empty catalog.
pub fn mcp_handshake(bootstrap: &BTreeMap<String, String>) -> Result<HandshakeReport> {
    let gateway = resolve_gateway()?;
    let mut command = Command::new(&gateway);
    command
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    for (key, value) in bootstrap {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .with_context(|| format!("failed to spawn {}", gateway.display()))?;
    let stdin = child.stdin.take().expect("stdin piped");
    let mut stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
    let (sender, lines) = std::sync::mpsc::channel();
    std::thread::spawn(move || loop {
        let mut line = String::new();
        let result = stdout.read_line(&mut line).map(|_| line);
        let done = matches!(&result, Ok(line) if line.is_empty()) || result.is_err();
        if sender.send(result).is_err() || done {
            return;
        }
    });
    let mut rpc = JsonRpcChild {
        child,
        stdin,
        lines,
        next_id: 0,
    };
    let initialized = rpc.call(
        "initialize",
        serde_json::json!({
            "protocolVersion": "2025-11-25",
            "capabilities": {},
            "clientInfo": {"name": "bobby-doctor", "version": env!("CARGO_PKG_VERSION")},
        }),
    )?;
    let server_version = initialized
        .pointer("/serverInfo/version")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("unknown")
        .to_string();
    rpc.notify("notifications/initialized")?;
    let listed = rpc.call("tools/list", serde_json::json!({}))?;
    let bytes = serde_json::to_string(&listed)?.len();
    let tools = listed
        .get("tools")
        .and_then(serde_json::Value::as_array)
        .map_or(0, Vec::len);
    Ok(HandshakeReport {
        tools,
        bytes,
        server_version,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_format_emits_valid_json_with_its_top_level_key_and_no_secret() {
        for (format, key) in [
            (EmitFormat::Claude, "mcpServers"),
            (EmitFormat::Json, "mcpServers"),
            (EmitFormat::Vscode, "servers"),
            (EmitFormat::Zed, "context_servers"),
        ] {
            let emitted = emit_mcp_config(format);
            let parsed: serde_json::Value =
                serde_json::from_str(&emitted).expect("emitted config parses");
            assert!(parsed.get(key).is_some(), "missing {key} in {emitted}");
            let server = parsed
                .pointer(&format!("/{key}/bobby-browser"))
                .expect("bobby-browser entry");
            // The fragment must name the gateway but never carry credential
            // material — only `${VAR}` placeholders.
            let text = serde_json::to_string(server).expect("serializable");
            assert!(text.contains("mcp-gateway"));
            assert!(text.contains("${AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN}"));
            assert!(!text.contains("=Bearer"));
        }
    }

    #[test]
    fn openshell_emit_is_streamable_http_with_token_placeholder() {
        let emitted = emit_mcp_config(EmitFormat::Openshell);
        let parsed: serde_json::Value =
            serde_json::from_str(&emitted).expect("openshell emit parses");
        let server = &parsed["mcpServers"]["bobby-browser"];
        assert_eq!(server["transport"], "streamable-http");
        assert!(server["url"].as_str().unwrap().ends_with("/v1/mcp"));
        assert_eq!(
            server["headers"]["Authorization"],
            "Bearer ${AUTOMATION_RUNTIME_TOKEN}"
        );
        assert!(!emitted.contains("mcp-gateway"));
    }
}

// ---------------------------------------------------------------------------
// `bobby mcp-stdio` and `bobby install`
// ---------------------------------------------------------------------------

/// Agent hosts the installer can wire up.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum HostKind {
    /// Claude Code: project `.mcp.json` (+ optional agent skill).
    Claude,
    /// Zed: `~/.config/zed/settings.json` (`context_servers`).
    Zed,
    /// VS Code: project `.vscode/mcp.json` (`servers`).
    Vscode,
    /// ACP stdio: project `.acp.json` launching `acp-gateway` with bootstrap env.
    Acp,
    /// NVIDIA OpenShell: project `openshell/` pack (policy + MCP HTTP + skill).
    Openshell,
}

const SKILL_SOURCE: &str = include_str!("../../../skill/SKILL.md");
const SKILL_NAME: &str = "bobby-browser";

thread_local! {
    /// Set when this process installs `bobby` onto PATH, so host MCP merges
    /// point at the durable bin path instead of a transient `target/` binary.
    static INSTALLED_CLI: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// The MCP / ACP host entry an agent launches: this same binary, absolute,
/// running `mcp-stdio` or `acp-stdio`, which loads the bootstrap credential
/// itself. No env wiring in the host config, no secrets in any file the host
/// reads.
fn static_cli_entry(subcommand: &str) -> Result<(String, Vec<String>)> {
    let exe = match INSTALLED_CLI.with(|slot| slot.borrow().clone()) {
        Some(path) => path,
        None => std::env::current_exe().context("current executable unknown")?,
    };
    Ok((
        exe.to_str()
            .ok_or_else(|| anyhow!("executable path is not valid UTF-8"))?
            .to_owned(),
        vec![subcommand.to_owned()],
    ))
}

fn static_server_entry() -> Result<(String, Vec<String>)> {
    static_cli_entry("mcp-stdio")
}

fn static_acp_host_entry() -> Result<(String, Vec<String>)> {
    static_cli_entry("acp-stdio")
}

/// The host config file `kind` reads, rooted at the current project for
/// project-scoped hosts and at the user config dir for Zed.
fn host_config_path(kind: HostKind, project_root: &Path) -> Result<PathBuf> {
    match kind {
        HostKind::Claude => Ok(project_root.join(".mcp.json")),
        HostKind::Vscode => Ok(project_root.join(".vscode").join("mcp.json")),
        HostKind::Zed => Ok(dirs::config_dir()
            .context("config directory unavailable")?
            .join("zed")
            .join("settings.json")),
        HostKind::Acp => Ok(project_root.join(".acp.json")),
        HostKind::Openshell => Ok(project_root.join("openshell").join("mcp.json")),
    }
}

/// Merge the bobby-browser server entry into one host's config file,
/// preserving everything already there. Returns the file written.
pub fn merge_host_config(kind: HostKind, project_root: &Path) -> Result<PathBuf> {
    if kind == HostKind::Openshell {
        let pack = crate::openshell::install_pack(
            project_root,
            &crate::openshell::PackOptions::default(),
        )?;
        return Ok(pack.dir);
    }
    let path = host_config_path(kind, project_root)?;
    let mut config: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", path.display()))?,
        _ => serde_json::json!({}),
    };
    let (entry, section) = match kind {
        HostKind::Claude => {
            let (command, args) = static_server_entry()?;
            (
                serde_json::json!({"command": command, "args": args}),
                "mcpServers",
            )
        }
        HostKind::Vscode => {
            let (command, args) = static_server_entry()?;
            (
                serde_json::json!({"type": "stdio", "command": command, "args": args}),
                "servers",
            )
        }
        HostKind::Zed => {
            let (command, args) = static_server_entry()?;
            (
                serde_json::json!({"command": {"path": command, "args": args, "env": {}}}),
                "context_servers",
            )
        }
        HostKind::Acp => {
            // Same zero-wiring shape as MCP: bobby acp-stdio loads bootstrap
            // and execs acp-gateway. No secrets in the host config file.
            let (command, args) = static_acp_host_entry()?;
            (
                serde_json::json!({"command": command, "args": args}),
                "agentServers",
            )
        }
        HostKind::Openshell => unreachable!("openshell returned above"),
    };
    let table = config
        .as_object_mut()
        .ok_or_else(|| anyhow!("{} must contain a JSON object", path.display()))?;
    let servers = table
        .entry(section)
        .or_insert_with(|| serde_json::json!({}));
    let servers = servers
        .as_object_mut()
        .ok_or_else(|| anyhow!("{section} in {} must be an object", path.display()))?;
    servers.insert("bobby-browser".to_owned(), entry);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut serialized = serde_json::to_string_pretty(&config)?;
    serialized.push('\n');
    std::fs::write(&path, serialized)?;
    Ok(path)
}

/// Which skill tree `install_skill` writes into.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillKind {
    /// Universal agent skills: `.agents/skills/` or `~/.agents/skills/`.
    Agents,
    /// Claude Code only: `.claude/skills/` or `~/.claude/skills/`.
    Claude,
    /// OpenClaw: always `~/.openclaw/skills/` (no project tree).
    OpenClaw,
}

/// Install the agent skill for one host family. `project` selects the project
/// tree for [`SkillKind::Agents`] and [`SkillKind::Claude`]; OpenClaw is
/// always user-level. Returns the file written.
pub fn install_skill(kind: SkillKind, project: bool, project_root: &Path) -> Result<PathBuf> {
    let base = match kind {
        SkillKind::Agents => {
            if project {
                project_root.join(".agents").join("skills")
            } else {
                dirs::home_dir()
                    .context("home directory unavailable")?
                    .join(".agents")
                    .join("skills")
            }
        }
        SkillKind::Claude => {
            if project {
                project_root.join(".claude").join("skills")
            } else {
                dirs::home_dir()
                    .context("home directory unavailable")?
                    .join(".claude")
                    .join("skills")
            }
        }
        SkillKind::OpenClaw => dirs::home_dir()
            .context("home directory unavailable")?
            .join(".openclaw")
            .join("skills"),
    };
    let dir = base.join(SKILL_NAME);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("SKILL.md");
    std::fs::write(&path, SKILL_SOURCE)?;
    Ok(path)
}

/// Point the gateway at the same config file used for vision prepare/upsert.
fn apply_config_env(config_path: &Path) {
    let absolute = config_path.canonicalize().unwrap_or_else(|_| {
        std::env::current_dir()
            .map(|cwd| cwd.join(config_path))
            .unwrap_or_else(|_| config_path.to_path_buf())
    });
    unsafe { std::env::set_var("BOBBY_BROWSER_CONFIG", absolute) };
}

/// Load bootstrap credential env vars into this process when they are not
/// already present in the environment.
fn load_bootstrap_into_env(bootstrap_path: &Path) -> Result<()> {
    // Expand stale capability lists before credential load so local installs
    // stay unrestricted as DEFAULT_CAPABILITIES grows.
    crate::bootstrap_local::ensure_unrestricted_bootstrap(bootstrap_path).with_context(|| {
        format!(
            "failed to heal bootstrap capabilities at {}",
            bootstrap_path.display()
        )
    })?;
    if broker::StartupCredential::from_env().is_err() {
        let env =
            crate::bootstrap_local::load_bootstrap_env_map(bootstrap_path).with_context(|| {
                format!(
                    "no startup credential in the environment and bootstrap env unreadable at {}",
                    bootstrap_path.display()
                )
            })?;
        for (key, value) in env {
            // Credentials only enter this process's environment, which the
            // spawned gateway inherits. They are never printed or written.
            unsafe { std::env::set_var(key, value) };
        }
    }
    Ok(())
}

/// Spawn the stdio gateway with inherited stdio and wait for it to exit.
fn spawn_gateway_inherited_stdio(gateway: &Path) -> Result<()> {
    let status = Command::new(gateway)
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .status()
        .with_context(|| format!("failed to run {}", gateway.display()))?;
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// `bobby mcp-stdio`: point an agent host at this and nothing else. Loads the
/// bootstrap credential (process env wins, then the bootstrap.env file) and
/// execs the stdio gateway, replacing this process.
pub fn exec_mcp_stdio(bootstrap_path: &Path, config_path: &Path) -> Result<()> {
    apply_config_env(config_path);
    load_bootstrap_into_env(bootstrap_path)?;
    let gateway = resolve_gateway()?;
    exec_gateway(&gateway)
}

/// Like [`exec_mcp_stdio`], but stays resident as parent so a vision sidecar
/// child can be torn down when the gateway exits. Used on Unix when a loopback
/// vision-proxy must outlive the gateway process.
pub fn run_mcp_stdio_with_sidecar(
    bootstrap_path: &Path,
    config_path: &Path,
    _vision_child: crate::vision_child::ManagedVisionProxy,
) -> Result<()> {
    apply_config_env(config_path);
    load_bootstrap_into_env(bootstrap_path)?;
    let gateway = resolve_gateway()?;
    spawn_gateway_inherited_stdio(&gateway)
}

/// `bobby acp-stdio`: ACP-host entrypoint. Loads bootstrap and execs
/// `acp-gateway` the same way `mcp-stdio` launches `mcp-gateway`.
pub fn exec_acp_stdio(bootstrap_path: &Path, config_path: &Path) -> Result<()> {
    apply_config_env(config_path);
    load_bootstrap_into_env(bootstrap_path)?;
    let gateway = resolve_acp_gateway()?;
    exec_gateway(&gateway)
}

/// Like [`exec_acp_stdio`], but stays resident so a vision sidecar can be
/// torn down when the gateway exits.
pub fn run_acp_stdio_with_sidecar(
    bootstrap_path: &Path,
    config_path: &Path,
    _vision_child: crate::vision_child::ManagedVisionProxy,
) -> Result<()> {
    apply_config_env(config_path);
    load_bootstrap_into_env(bootstrap_path)?;
    let gateway = resolve_acp_gateway()?;
    spawn_gateway_inherited_stdio(&gateway)
}

/// Unix replaces this process outright, so the agent host keeps talking to a
/// single pid over the stdio pipes it already opened.
#[cfg(unix)]
fn exec_gateway(gateway: &Path) -> Result<()> {
    use std::os::unix::process::CommandExt;

    let error = Command::new(gateway).exec();
    Err(anyhow!("failed to exec {}: {error}", gateway.display()))
}

/// Windows has no exec. Stay resident as a thin parent instead: the child
/// inherits this process's stdio, so the agent host still sees one stream, and
/// the gateway's exit status becomes ours.
#[cfg(windows)]
fn exec_gateway(gateway: &Path) -> Result<()> {
    spawn_gateway_inherited_stdio(gateway)
}

/// One toggleable line of the interactive installer.
struct InstallItem {
    label: String,
    enabled: bool,
    run: Box<dyn Fn() -> Result<String>>,
}

/// What `bobby install` was asked to set up, flag form. The interactive
/// checklist starts from these as its initial toggles.
#[derive(Debug, Default)]
pub struct InstallOptions {
    pub hosts: Vec<HostKind>,
    /// Install the universal agents skill (`~/.agents/skills/` or project).
    pub skill: bool,
    /// Prefer the project `.agents/skills/` (and `.claude/skills/` when
    /// `--skill-claude`) tree instead of the user-level tree.
    pub project_skill: bool,
    /// Also install into Claude Code's skill tree.
    pub skill_claude: bool,
    /// Also install into OpenClaw's skill tree (`~/.openclaw/skills/`).
    pub skill_openclaw: bool,
    pub companion: bool,
    pub extension: Option<PathBuf>,
    /// Copy `bobby` (+ sibling `mcp-gateway` / `acp-gateway`) onto a writable bin dir on PATH.
    pub cli: bool,
    pub vision: bool,
    pub no_vision: bool,
    pub vision_provider: Option<String>,
    pub vision_model: Option<String>,
    pub download_vision_model: bool,
    pub collect_training_data: bool,
    pub no_collect_training_data: bool,
    pub training_data_dir: PathBuf,
    pub config: Option<PathBuf>,
    pub force: bool,
    pub yes: bool,
}

/// Named flags select only that work; no named flags (interactive or `--yes`
/// alone) uses the built-in defaults (Claude host, companion if Firefox is
/// present, credential if missing, CLI on PATH).
fn use_install_defaults(options: &InstallOptions) -> bool {
    options.hosts.is_empty()
        && !options.skill
        && !options.project_skill
        && !options.skill_claude
        && !options.skill_openclaw
        && !options.companion
        && !options.cli
        && !options.vision
        && !options.no_vision
        && options.vision_provider.is_none()
        && options.vision_model.is_none()
        && !options.download_vision_model
        && !options.collect_training_data
        && !options.no_collect_training_data
        && !options.force
}

fn bootstrap_item_enabled(
    credential_exists: bool,
    force: bool,
    use_defaults: bool,
    hosts: &[HostKind],
) -> bool {
    force || (!credential_exists && (use_defaults || !hosts.is_empty()))
}

fn agents_skill_item_enabled(skill: bool, project_skill: bool, use_defaults: bool) -> bool {
    skill || project_skill || use_defaults
}

const MLX_MODELS: [(&str, &str); 3] = [
    ("Small (3B)", "mlx-community/Qwen2.5-VL-3B-Instruct-4bit"),
    ("Balanced (7B)", "mlx-community/Qwen2.5-VL-7B-Instruct-4bit"),
    ("Large (32B)", "mlx-community/Qwen2.5-VL-32B-Instruct-4bit"),
];

#[derive(Debug, Clone, PartialEq, Eq)]
struct VisionInstallState {
    enabled: bool,
    provider: String,
    model: String,
    collect_training_data: bool,
    training_data_dir: PathBuf,
}

impl Default for VisionInstallState {
    fn default() -> Self {
        Self {
            enabled: true,
            provider: "ollama".into(),
            model: crate::vision_connect::preset("ollama")
                .expect("ollama preset")
                .1
                .model,
            collect_training_data: false,
            training_data_dir: "data/vision".into(),
        }
    }
}

impl VisionInstallState {
    fn select_provider(&mut self, provider: &str) -> Result<()> {
        let provider = provider.trim().to_ascii_lowercase();
        let (_, profile) = crate::vision_connect::preset(&provider)
            .ok_or_else(|| anyhow!("unsupported install vision provider {provider:?}"))?;
        self.enabled = true;
        self.provider = provider;
        self.model = profile.model;
        Ok(())
    }

    fn profile(&self) -> Result<config::VisionProviderConfig> {
        let (_, mut profile) = crate::vision_connect::preset(&self.provider)
            .ok_or_else(|| anyhow!("unsupported install vision provider {:?}", self.provider))?;
        profile.model.clone_from(&self.model);
        Ok(profile)
    }
}

/// Prefer `~/.cargo/bin` when it is already on PATH (rustup users), else
/// `~/.local/bin` (created if needed; operator may still need to add it).
fn resolve_cli_bin_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("home directory unavailable")?;
    let cargo_bin = home.join(".cargo").join("bin");
    let local_bin = home.join(".local").join("bin");
    if directory_on_path(&cargo_bin) {
        return Ok(cargo_bin);
    }
    if directory_on_path(&local_bin) {
        return Ok(local_bin);
    }
    Ok(local_bin)
}

fn directory_on_path(dir: &Path) -> bool {
    let Ok(dir) = dir.canonicalize() else {
        // Not created yet — still count a PATH entry that matches by string.
        return path_var_contains(dir);
    };
    path_var_contains(&dir)
        || std::env::var_os("PATH")
            .map(|paths| {
                std::env::split_paths(&paths)
                    .any(|entry| entry.canonicalize().ok().as_ref() == Some(&dir) || entry == dir)
            })
            .unwrap_or(false)
}

fn path_var_contains(dir: &Path) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|entry| entry == dir))
        .unwrap_or(false)
}

/// Copy this `bobby` binary (and sibling gateways when present) into
/// `dest_dir`. Returns the installed `bobby` path and whether `dest_dir` is
/// on PATH.
pub fn install_cli_into(dest_dir: &Path) -> Result<(PathBuf, bool)> {
    std::fs::create_dir_all(dest_dir)
        .with_context(|| format!("could not create {}", dest_dir.display()))?;
    let exe = std::env::current_exe().context("current executable unknown")?;
    let bobby_dest = dest_dir.join(if cfg!(windows) { "bobby.exe" } else { "bobby" });
    copy_executable(&exe, &bobby_dest)?;

    if let Some(dir) = exe.parent() {
        for command in [GATEWAY_COMMAND, ACP_GATEWAY_COMMAND] {
            let gateway_src = dir.join(command);
            if gateway_src.is_file() {
                let gateway_dest = dest_dir.join(command);
                copy_executable(&gateway_src, &gateway_dest)?;
            }
        }
    }

    Ok((bobby_dest, directory_on_path(dest_dir)))
}

fn copy_executable(src: &Path, dest: &Path) -> Result<()> {
    let pending = dest.with_extension(format!("pending-{}", uuid::Uuid::new_v4().simple()));
    std::fs::copy(src, &pending)
        .with_context(|| format!("copy {} → {}", src.display(), pending.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&pending, std::fs::Permissions::from_mode(0o755))?;
    }
    std::fs::rename(&pending, dest).with_context(|| format!("install {}", dest.display()))?;
    Ok(())
}

fn remember_installed_cli(bobby: &Path) {
    INSTALLED_CLI.with(|slot| {
        *slot.borrow_mut() = Some(bobby.to_path_buf());
    });
}

fn mlx_supported_host() -> bool {
    cfg!(all(target_os = "macos", target_arch = "aarch64"))
}

fn configure_mlx_model_interactive(state: &mut VisionInstallState) -> Result<bool> {
    if !mlx_supported_host() {
        anyhow::bail!("MLX vision requires Apple Silicon macOS; choose Ollama on this host");
    }
    if cached_hugging_face_model(&state.model)? {
        return Ok(true);
    }
    let already_downloaded = Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt("Do you already have a compatible MLX vision model downloaded?")
        .default(false)
        .interact()?;
    if already_downloaded {
        let model = dialoguer::Input::<String>::with_theme(&ColorfulTheme::default())
            .with_prompt("Hugging Face model id")
            .default(state.model.clone())
            .interact_text()?;
        if !cached_hugging_face_model(&model)? {
            anyhow::bail!("no complete Hugging Face cache snapshot found for {model}");
        }
        state.model = model;
        return Ok(true);
    }
    let labels = [
        format!("{} — {}", MLX_MODELS[0].0, MLX_MODELS[0].1),
        format!("{} — {}", MLX_MODELS[1].0, MLX_MODELS[1].1),
        format!("{} — {}", MLX_MODELS[2].0, MLX_MODELS[2].1),
        "Back without downloading".to_owned(),
    ];
    let choice = Select::with_theme(&ColorfulTheme::default())
        .with_prompt("Download an MLX model (resumable Hugging Face cache)")
        .items(&labels)
        .default(0)
        .interact()?;
    if choice == MLX_MODELS.len() {
        return Ok(false);
    }
    let model = MLX_MODELS[choice].1;
    if !Confirm::with_theme(&ColorfulTheme::default())
        .with_prompt(format!("Download {model}?"))
        .default(false)
        .interact()?
    {
        return Ok(false);
    }
    download_and_verify_mlx_model(model)?;
    state.model = model.to_owned();
    Ok(true)
}

fn choose_vision_interactive(state: &mut VisionInstallState) -> Result<bool> {
    loop {
        let training = if state.collect_training_data {
            "on"
        } else {
            "off"
        };
        let labels = [
            "Off".to_owned(),
            "Ollama (recommended, portable)".to_owned(),
            "MLX-VLM (Apple Silicon)".to_owned(),
            "LM Studio".to_owned(),
            "OpenAI".to_owned(),
            format!("Training data collection: {training}"),
            "Back".to_owned(),
        ];
        let default = match state.provider.as_str() {
            "ollama" => 1,
            "mlx" => 2,
            "lmstudio" => 3,
            "openai" => 4,
            _ => 1,
        };
        match Select::with_theme(&ColorfulTheme::default())
            .with_prompt("Vision setup")
            .items(&labels)
            .default(if state.enabled { default } else { 0 })
            .interact()?
        {
            0 => {
                state.enabled = false;
                return Ok(true);
            }
            1 => {
                state.select_provider("ollama")?;
                return Ok(true);
            }
            2 => {
                let previous = state.clone();
                state.select_provider("mlx")?;
                if configure_mlx_model_interactive(state)? {
                    return Ok(true);
                }
                *state = previous;
            }
            3 => {
                state.select_provider("lmstudio")?;
                return Ok(true);
            }
            4 => {
                state.select_provider("openai")?;
                return Ok(true);
            }
            5 => state.collect_training_data = !state.collect_training_data,
            6 => return Ok(false),
            _ => unreachable!(),
        }
    }
}

fn apply_vision_install(config_path: &Path, state: &VisionInstallState) -> Result<String> {
    let profile = state.profile()?;
    config::set_vision_install_state(
        config_path,
        state.enabled,
        &state.provider,
        &profile,
        state.collect_training_data,
        &state.training_data_dir,
    )
    .map_err(|error| anyhow!("{error}"))?;
    Ok(format!(
        "vision {} with {} / {} (training collection {}) in {}",
        if state.enabled { "enabled" } else { "disabled" },
        state.provider,
        state.model,
        if state.collect_training_data {
            "on"
        } else {
            "off"
        },
        config_path.display()
    ))
}

/// `bobby install`: the one-command setup. Non-interactive when flags name
/// the work; otherwise a checklist the operator toggles.
pub fn run_install(bootstrap_path: &Path, options: InstallOptions) -> Result<()> {
    let InstallOptions {
        hosts,
        skill,
        project_skill,
        skill_claude,
        skill_openclaw,
        companion,
        extension,
        cli,
        vision,
        no_vision,
        vision_provider,
        vision_model,
        download_vision_model,
        collect_training_data,
        no_collect_training_data,
        training_data_dir,
        config,
        force,
        yes,
    } = &options;
    let hosts = hosts.as_slice();
    let extension = extension.as_deref();
    let skill = *skill;
    let project_skill = *project_skill;
    let skill_claude = *skill_claude;
    let skill_openclaw = *skill_openclaw;
    let companion = *companion;
    let cli = *cli;
    let vision = *vision;
    let no_vision = *no_vision;
    let download_vision_model = *download_vision_model;
    let collect_training_data = *collect_training_data;
    let no_collect_training_data = *no_collect_training_data;
    let force = *force;
    let yes = *yes;
    let readiness_requested =
        vision_provider.is_some() || vision_model.is_some() || download_vision_model;
    let project_root = std::env::current_dir()?;
    let mut items: Vec<InstallItem> = Vec::new();
    let use_defaults = use_install_defaults(&options);
    let vision_named = vision
        || no_vision
        || vision_provider.is_some()
        || vision_model.is_some()
        || download_vision_model
        || collect_training_data
        || no_collect_training_data;
    let configure_vision = use_defaults || vision_named;
    if no_vision && (vision_provider.is_some() || vision_model.is_some() || download_vision_model) {
        anyhow::bail!("--no-vision cannot be combined with provider, model, or download flags");
    }
    if vision_model.is_some() && vision_provider.as_deref() != Some("mlx") {
        anyhow::bail!("--vision-model currently requires --vision-provider mlx");
    }
    let mut vision_state = VisionInstallState {
        enabled: !no_vision,
        training_data_dir: training_data_dir.clone(),
        ..VisionInstallState::default()
    };
    if let Some(provider) = vision_provider.as_deref() {
        vision_state.select_provider(provider)?;
    }
    if let Some(model) = vision_model.as_deref() {
        vision_state.model = model.to_owned();
    }
    if collect_training_data {
        vision_state.collect_training_data = true;
    } else if no_collect_training_data {
        vision_state.collect_training_data = false;
    }
    let config_path = crate::resolve_config_path(config.clone());
    if configure_vision && vision_state.provider == "mlx" {
        if !mlx_supported_host() {
            anyhow::bail!("MLX vision requires Apple Silicon macOS; choose Ollama on this host");
        }
        if !cached_hugging_face_model(&vision_state.model)? {
            if !download_vision_model {
                anyhow::bail!(
                    "MLX model {} is not cached; pass --download-vision-model or run interactively",
                    vision_state.model
                );
            }
            download_and_verify_mlx_model(&vision_state.model)?;
        }
    } else if download_vision_model {
        anyhow::bail!("--download-vision-model requires --vision-provider mlx");
    }

    let credential_exists = bootstrap_path.exists();
    let credential_label = if credential_exists && !force {
        format!(
            "Bootstrap credential: keep the existing one at {}",
            bootstrap_path.display()
        )
    } else {
        format!(
            "Bootstrap credential: generate a 30-day credential at {}",
            bootstrap_path.display()
        )
    };
    // Named flags (--host/--skill/--companion/--cli/--force) select only that
    // work. No named flags (interactive checklist, or --yes alone) uses defaults.

    items.push(InstallItem {
        label: credential_label,
        enabled: bootstrap_item_enabled(credential_exists, force, use_defaults, hosts),
        run: Box::new({
            let path = bootstrap_path.to_path_buf();
            move || {
                let material = crate::bootstrap_local::generate_bootstrap(chrono::Duration::days(
                    crate::bootstrap_local::DEFAULT_TTL_DAYS,
                ))?;
                crate::bootstrap_local::write_bootstrap_env(&path, &material, true)?;
                Ok(format!("wrote {}", path.display()))
            }
        }),
    });

    let cli_enabled = cli || use_defaults;
    let cli_label = match resolve_cli_bin_dir() {
        Ok(bin_dir) if directory_on_path(&bin_dir) => format!(
            "CLI on PATH: install bobby (+ mcp-gateway, acp-gateway) to {}",
            bin_dir.display()
        ),
        Ok(bin_dir) => format!(
            "CLI on PATH: install bobby (+ mcp-gateway, acp-gateway) to {} (add this dir to PATH)",
            bin_dir.display()
        ),
        Err(_) => {
            "CLI on PATH: install bobby (+ mcp-gateway, acp-gateway) to ~/.cargo/bin or ~/.local/bin"
                .to_owned()
        }
    };
    items.push(InstallItem {
        label: cli_label,
        enabled: cli_enabled,
        run: Box::new(move || {
            let dest = resolve_cli_bin_dir()?;
            let (bobby, on_path) = install_cli_into(&dest)?;
            remember_installed_cli(&bobby);
            if on_path {
                Ok(format!("installed {}", bobby.display()))
            } else {
                Ok(format!(
                    "installed {} — add {} to PATH to run `bobby`",
                    bobby.display(),
                    dest.display()
                ))
            }
        }),
    });

    for host in [
        HostKind::Claude,
        HostKind::Zed,
        HostKind::Vscode,
        HostKind::Acp,
        HostKind::Openshell,
    ] {
        let selected = hosts.contains(&host);
        let default_on = matches!(host, HostKind::Claude);
        let root = project_root.clone();
        let label = match host {
            HostKind::Acp => {
                "Acp: merge bobby acp-stdio into .acp.json (no bootstrap env in the file)"
                    .to_owned()
            }
            HostKind::Openshell => {
                "Openshell: write openshell/ pack (policy.yaml, mcp.json, skill, README)".to_owned()
            }
            _ => format!("{host:?}: merge the MCP server entry into its config"),
        };
        items.push(InstallItem {
            label,
            enabled: selected || (use_defaults && default_on),
            run: Box::new(move || {
                let path = merge_host_config(host, &root)?;
                Ok(format!("merged into {}", path.display()))
            }),
        });
    }

    let extension_path = extension.map(Path::to_path_buf);
    items.push(InstallItem {
        label: "Firefox companion: install extension and native host (pair at first use)".to_owned(),
        enabled: companion || (use_defaults && firefox_present()),
        run: Box::new(move || {
            let install = install_firefox_companion(extension_path.as_deref())?;
            Ok(format!(
                "sideloaded into {}; config copy at {}; native host at {}. Next: `make firefox-start` (Bobby profile at {}; BiDi on :9222), then Pair from the toolbar popup. Local agents use `bobby mcp-stdio` — no `bobby serve` required",
                install.sideload_dir.display(),
                install.extension_dir.display(),
                install.manifest_path.display(),
                install.profile_dir.display()
            ))
        }),
    });

    items.push(InstallItem {
        label: if project_skill {
            "Agent skill (agents): install into this project's .agents/skills/".to_owned()
        } else {
            "Agent skill (agents): install into ~/.agents/skills/".to_owned()
        },
        // `--project-skill` alone means "install agents skill into the project".
        enabled: agents_skill_item_enabled(skill, project_skill, use_defaults),
        run: Box::new({
            let root = project_root.clone();
            move || {
                let path = install_skill(SkillKind::Agents, project_skill, &root)?;
                Ok(format!("installed to {}", path.display()))
            }
        }),
    });

    items.push(InstallItem {
        label: if project_skill {
            "Agent skill (claude): install into this project's .claude/skills/".to_owned()
        } else {
            "Agent skill (claude): install into ~/.claude/skills/".to_owned()
        },
        enabled: skill_claude,
        run: Box::new({
            let root = project_root.clone();
            move || {
                let path = install_skill(SkillKind::Claude, project_skill, &root)?;
                Ok(format!("installed to {}", path.display()))
            }
        }),
    });

    items.push(InstallItem {
        label: "Agent skill (openclaw): install into ~/.openclaw/skills/".to_owned(),
        enabled: skill_openclaw,
        run: Box::new(|| {
            let path = install_skill(SkillKind::OpenClaw, false, Path::new("."))?;
            Ok(format!("installed to {}", path.display()))
        }),
    });

    let interactive = !yes
        && hosts.is_empty()
        && !skill
        && !project_skill
        && !skill_claude
        && !skill_openclaw
        && !companion
        && !cli
        && !vision_named
        && !force;
    if interactive {
        if !std::io::IsTerminal::is_terminal(&std::io::stdin())
            || !std::io::IsTerminal::is_terminal(&std::io::stdout())
        {
            anyhow::bail!(
                "bobby install needs a terminal for its checklist, or explicit flags: --host <claude|zed|vscode|acp|openshell> --skill [--skill-claude] [--skill-openclaw] --cli --yes"
            );
        }
        loop {
            let mut labels: Vec<String> = items.iter().map(|item| item.label.clone()).collect();
            labels.push(format!(
                "Vision: {}{}",
                if vision_state.enabled {
                    format!("{} / {}", vision_state.provider, vision_state.model)
                } else {
                    "off".to_owned()
                },
                if vision_state.collect_training_data {
                    " (training collection on)"
                } else {
                    ""
                }
            ));
            let mut defaults: Vec<bool> = items.iter().map(|item| item.enabled).collect();
            defaults.push(vision_state.enabled);
            let Some(selected) = MultiSelect::with_theme(&ColorfulTheme::default())
                .with_prompt("bobby install — ↑/↓ move, space toggles, enter continues (esc quits)")
                .items(&labels)
                .defaults(&defaults)
                .interact_opt()?
            else {
                println!("nothing changed");
                return Ok(());
            };
            for (index, item) in items.iter_mut().enumerate() {
                item.enabled = selected.contains(&index);
            }
            let vision_index = items.len();
            if !selected.contains(&vision_index) {
                vision_state.enabled = false;
                break;
            }
            if choose_vision_interactive(&mut vision_state)? {
                break;
            }
            // Provider-menu Back returns here with every checklist toggle and
            // pending vision/training choice preserved.
        }
        if !Confirm::with_theme(&ColorfulTheme::default())
            .with_prompt("Apply this installation?")
            .default(true)
            .interact()?
        {
            println!("nothing changed");
            return Ok(());
        }
    }

    let companion_selected = items
        .iter()
        .any(|item| item.enabled && item.label.starts_with("Firefox companion:"));
    if companion_selected {
        find_companion_dist(extension)?;
    }

    let mut ran = 0;
    if configure_vision || interactive {
        drop(crate::vision_token::ensure_managed_vision_token(
            bootstrap_path,
        )?);
        println!(
            "applied: {}",
            apply_vision_install(&config_path, &vision_state)?
        );
        ran += 1;
    }
    let mut selected = items.iter().filter(|item| item.enabled).peekable();
    while let Some(item) = selected.next() {
        match (item.run)() {
            Ok(outcome) => {
                println!("applied: {outcome}");
                ran += 1;
            }
            Err(error) => {
                eprintln!("failed: {}: {error:#}", item.label);
                for pending in selected {
                    eprintln!("not run: {}", pending.label);
                }
                anyhow::bail!(
                    "installation incomplete; resolve the failed item and re-run the same `bobby install` command"
                );
            }
        }
    }
    if vision_state.enabled && (readiness_requested || interactive) {
        let profile = vision_state.profile()?;
        match crate::vision_readiness::check_provider_readiness(
            &vision_state.provider,
            &profile,
            &crate::vision_readiness::ReadinessOptions {
                timeout: std::time::Duration::from_secs(45),
                allow_download: false,
            },
        )? {
            crate::vision_readiness::ReadinessOutcome::Ready { provider, model } => {
                println!("ok: configured and readiness-tested {provider} / {model}");
            }
            outcome @ crate::vision_readiness::ReadinessOutcome::NeedsAction { .. } => {
                anyhow::bail!("vision readiness: {}", outcome.detail());
            }
        }
    }
    if ran == 0 {
        println!("nothing selected; nothing changed");
    } else {
        println!("installation applied. Next: run `bobby doctor`.");
    }
    Ok(())
}

#[cfg(test)]
mod install_tests {
    use super::*;

    /// Companion install tests mutate process-global `HOME` (and sometimes
    /// `XDG_CONFIG_HOME`); serialize so parallel `cargo test` stays deterministic.
    static INSTALL_ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn named_hosts_require_a_missing_bootstrap_and_defaults_include_the_agent_skill() {
        assert!(bootstrap_item_enabled(
            false,
            false,
            false,
            &[HostKind::Claude]
        ));
        assert!(!bootstrap_item_enabled(
            true,
            false,
            false,
            &[HostKind::Claude]
        ));
        assert!(agents_skill_item_enabled(false, false, true));
        assert!(!agents_skill_item_enabled(false, false, false));
    }

    #[test]
    fn companion_only_flag_does_not_use_full_install_defaults() {
        assert!(use_install_defaults(&InstallOptions::default()));
        assert!(use_install_defaults(&InstallOptions {
            yes: true,
            ..InstallOptions::default()
        }));
        assert!(!use_install_defaults(&InstallOptions {
            companion: true,
            ..InstallOptions::default()
        }));
        assert!(!use_install_defaults(&InstallOptions {
            cli: true,
            ..InstallOptions::default()
        }));
        assert!(!use_install_defaults(&InstallOptions {
            skill: true,
            ..InstallOptions::default()
        }));
        assert!(!use_install_defaults(&InstallOptions {
            project_skill: true,
            ..InstallOptions::default()
        }));
        assert!(!use_install_defaults(&InstallOptions {
            skill_claude: true,
            ..InstallOptions::default()
        }));
        assert!(!use_install_defaults(&InstallOptions {
            skill_openclaw: true,
            ..InstallOptions::default()
        }));
        assert!(!use_install_defaults(&InstallOptions {
            hosts: vec![HostKind::Claude],
            ..InstallOptions::default()
        }));
        assert!(!use_install_defaults(&InstallOptions {
            vision: true,
            ..InstallOptions::default()
        }));
        assert!(!use_install_defaults(&InstallOptions {
            no_vision: true,
            ..InstallOptions::default()
        }));
    }

    #[test]
    fn vision_state_survives_back_off_and_reenable_transitions() {
        let mut state = VisionInstallState::default();
        state.select_provider("mlx").unwrap();
        state.model = MLX_MODELS[1].1.into();
        state.collect_training_data = true;
        let before_back = state.clone();

        // Returning to the main menu does not mutate pending submenu state.
        assert_eq!(state, before_back);
        state.enabled = false;
        assert_eq!(state.provider, "mlx");
        assert_eq!(state.model, MLX_MODELS[1].1);
        assert!(state.collect_training_data);

        state.enabled = true;
        assert_eq!(state.provider, "mlx");
        assert_eq!(state.model, MLX_MODELS[1].1);
        assert!(state.collect_training_data);
    }

    #[test]
    fn ollama_is_the_portable_install_default_and_three_mlx_choices_are_stable() {
        let state = VisionInstallState::default();
        assert!(state.enabled);
        assert_eq!(state.provider, "ollama");
        assert_eq!(state.model, "llava");
        assert_eq!(
            MLX_MODELS.map(|(_, model)| model),
            [
                "mlx-community/Qwen2.5-VL-3B-Instruct-4bit",
                "mlx-community/Qwen2.5-VL-7B-Instruct-4bit",
                "mlx-community/Qwen2.5-VL-32B-Instruct-4bit",
            ]
        );
    }

    #[test]
    fn mlx_cache_detection_rejects_incomplete_and_accepts_complete_snapshot() {
        let _lock = INSTALL_ENV_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("HF_HOME");
        unsafe { std::env::set_var("HF_HOME", home.path()) };
        let model = MLX_MODELS[0].1;
        let snapshot = home
            .path()
            .join("hub/models--mlx-community--Qwen2.5-VL-3B-Instruct-4bit/snapshots/abc");
        std::fs::create_dir_all(&snapshot).unwrap();
        std::fs::write(snapshot.join("config.json"), "{}").unwrap();
        assert!(!cached_hugging_face_model(model).unwrap());
        std::fs::write(snapshot.join("preprocessor_config.json"), "{}").unwrap();
        assert!(cached_hugging_face_model(model).unwrap());
        match previous {
            Some(value) => unsafe { std::env::set_var("HF_HOME", value) },
            None => unsafe { std::env::remove_var("HF_HOME") },
        }
    }

    #[test]
    fn noninteractive_vision_only_install_writes_ollama_without_other_install_work() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let bootstrap_path = dir.path().join("bootstrap.env");
        run_install(
            &bootstrap_path,
            InstallOptions {
                vision: true,
                yes: true,
                config: Some(config_path.clone()),
                training_data_dir: dir.path().join("training"),
                ..InstallOptions::default()
            },
        )
        .unwrap();
        assert!(!bootstrap_path.exists());
        let loaded = config::AppConfig::load(&config_path).unwrap();
        assert!(loaded.nodes.contains_key("vision"));
        assert_eq!(loaded.vision.provider.as_deref(), Some("ollama"));
        assert_eq!(loaded.vision.selected_provider().unwrap().1.model, "llava");
        assert!(!loaded.vision.collect_training_data);
    }

    #[test]
    fn vision_readiness_named_install_persists_selection_and_surfaces_failure() {
        let dir = tempfile::tempdir().unwrap();
        let config_path = dir.path().join("config.toml");
        let error = run_install(
            &dir.path().join("bootstrap.env"),
            InstallOptions {
                vision: true,
                vision_provider: Some("openai".into()),
                yes: true,
                config: Some(config_path.clone()),
                training_data_dir: dir.path().join("training"),
                ..InstallOptions::default()
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("readiness"));
        let loaded = config::AppConfig::load(&config_path).unwrap();
        assert_eq!(loaded.vision.provider.as_deref(), Some("openai"));
        assert_eq!(
            loaded.vision.selected_provider().unwrap().1.model,
            "gpt-4o-mini"
        );
    }

    #[test]
    fn unavailable_vision_does_not_prevent_selected_firefox_companion_install() {
        let _lock = INSTALL_ENV_LOCK.lock().unwrap();
        let dist = tempfile::tempdir().unwrap();
        std::fs::write(dist.path().join("manifest.json"), "{}").unwrap();
        std::fs::write(dist.path().join("background.js"), "//").unwrap();
        let home = tempfile::tempdir().unwrap();
        let config_path = home.path().join("config.toml");
        let previous_home = std::env::var_os("HOME");
        let previous_xdg = std::env::var_os("XDG_CONFIG_HOME");
        unsafe {
            std::env::set_var("HOME", home.path());
            #[cfg(not(target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", home.path().join(".config"));
        }
        #[cfg(target_os = "macos")]
        std::fs::create_dir_all(home.path().join("Library/Application Support")).unwrap();
        let companion_dir = bobby_config_dir().unwrap().join("firefox-companion");

        let result = run_install(
            &home.path().join("bootstrap.env"),
            InstallOptions {
                companion: true,
                extension: Some(dist.path().to_path_buf()),
                vision_provider: Some("openai".into()),
                yes: true,
                config: Some(config_path),
                training_data_dir: home.path().join("training"),
                ..InstallOptions::default()
            },
        );

        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match previous_xdg {
            Some(value) => unsafe { std::env::set_var("XDG_CONFIG_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }

        assert!(result.unwrap_err().to_string().contains("readiness"));
        assert!(companion_dir.join("manifest.json").is_file());
        assert!(companion_dir.join("background.js").is_file());
    }

    #[test]
    fn cli_install_copies_bobby_into_the_bin_dir() {
        let dest = tempfile::tempdir().unwrap();
        let (bobby, _) = install_cli_into(dest.path()).unwrap();
        assert!(bobby.is_file());
        let expected = if cfg!(windows) { "bobby.exe" } else { "bobby" };
        assert_eq!(bobby.file_name().unwrap(), expected);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&bobby).unwrap().permissions().mode();
            assert_eq!(mode & 0o111, 0o111, "installed bobby must be executable");
        }
    }

    #[test]
    fn merging_into_a_fresh_claude_config_creates_the_section() {
        let root = tempfile::tempdir().unwrap();
        let path = merge_host_config(HostKind::Claude, root.path()).unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let server = &written["mcpServers"]["bobby-browser"];
        // Under `cargo test --lib` current_exe is the `cli` test harness, not
        // the `bobby` bin — assert the host entry shape, not the binary name.
        let command = server["command"].as_str().unwrap();
        assert!(!command.is_empty(), "command must be set");
        assert_eq!(server["args"][0].as_str().unwrap(), "mcp-stdio");
        // No credential material anywhere in the file.
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN"));
    }

    #[test]
    fn merging_preserves_existing_servers_and_keys() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join(".mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers": {"other": {"command": "other-bin"}}, "unrelated": true}"#,
        )
        .unwrap();
        merge_host_config(HostKind::Claude, root.path()).unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(written["mcpServers"]["other"]["command"], "other-bin");
        assert_eq!(written["unrelated"], true);
        assert!(written["mcpServers"]["bobby-browser"].is_object());
    }

    #[test]
    fn vscode_and_zed_use_their_own_shapes() {
        let root = tempfile::tempdir().unwrap();
        let vscode_path = merge_host_config(HostKind::Vscode, root.path()).unwrap();
        let vscode: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&vscode_path).unwrap()).unwrap();
        assert_eq!(vscode["servers"]["bobby-browser"]["type"], "stdio");
    }

    #[test]
    fn acp_merge_writes_bobby_acp_stdio_without_bootstrap_env() {
        let root = tempfile::tempdir().unwrap();
        let path = merge_host_config(HostKind::Acp, root.path()).unwrap();
        assert!(path.ends_with(".acp.json"));
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let server = &written["agentServers"]["bobby-browser"];
        let command = server["command"].as_str().unwrap();
        assert!(!command.is_empty(), "command must be set");
        assert_eq!(server["args"][0].as_str().unwrap(), "acp-stdio");
        assert!(server.get("env").is_none() || server["env"].is_null());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN"));
        assert!(!text.contains("mcp-stdio"));
    }

    #[test]
    fn the_agents_skill_installs_into_the_project_tree() {
        let root = tempfile::tempdir().unwrap();
        let path = install_skill(SkillKind::Agents, true, root.path()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("---\nname: bobby-browser"));
        assert!(
            text.contains("saveAs"),
            "installed skill must teach the direct download path"
        );
        assert!(path.ends_with(".agents/skills/bobby-browser/SKILL.md"));
    }

    #[test]
    fn the_claude_skill_installs_into_the_claude_tree() {
        let root = tempfile::tempdir().unwrap();
        let path = install_skill(SkillKind::Claude, true, root.path()).unwrap();
        assert!(path.ends_with(".claude/skills/bobby-browser/SKILL.md"));
    }

    #[test]
    fn the_openclaw_skill_installs_into_the_user_openclaw_tree() {
        let _lock = INSTALL_ENV_LOCK.lock().unwrap();
        let home = tempfile::tempdir().unwrap();
        let previous = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home.path()) };
        let path = install_skill(SkillKind::OpenClaw, true, Path::new("/unused")).unwrap();
        assert!(path.ends_with(".openclaw/skills/bobby-browser/SKILL.md"));
        assert!(path.starts_with(home.path()));
        match previous {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
    }

    #[test]
    fn companion_install_copies_the_extension_and_installs_the_native_host() {
        let _lock = INSTALL_ENV_LOCK.lock().unwrap();
        let dist = tempfile::tempdir().unwrap();
        std::fs::write(dist.path().join("manifest.json"), "{}").unwrap();
        std::fs::write(dist.path().join("background.js"), "//").unwrap();
        let home = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded test process env; restored after.
        let previous = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home.path()) };
        #[cfg(target_os = "macos")]
        std::fs::create_dir_all(home.path().join("Library/Application Support")).unwrap();
        let result = install_firefox_companion(Some(dist.path()));
        match previous {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        let install = result.expect("companion installs");
        assert!(install.extension_dir.join("manifest.json").is_file());
        assert!(install.extension_dir.join("background.js").is_file());
        assert!(install.wrapper_path.is_file());
        assert!(install.manifest_path.is_file());
        // The descriptor is written by enrollment, not the installer.
        assert!(!install.descriptor_path.exists());
        assert_eq!(
            install.descriptor_path.parent().unwrap(),
            install.extension_dir.parent().unwrap()
        );
        assert!(install.sideload_dir.join("manifest.json").is_file());
        assert!(install.sideload_dir.join("background.js").is_file());
        assert_eq!(
            install.sideload_dir,
            install
                .profile_dir
                .join("extensions")
                .join(COMPANION_GECKO_ID)
        );
        let user_js = std::fs::read_to_string(install.profile_dir.join("user.js")).unwrap();
        for &(name, _) in FIREFOX_PROFILE_PREFS {
            assert!(
                user_js.contains(&format!("user_pref(\"{name}\"")),
                "missing pref {name} in user.js:\n{user_js}"
            );
        }
    }

    #[test]
    fn companion_install_sideload_upgrades_and_preserves_custom_user_js() {
        let _lock = INSTALL_ENV_LOCK.lock().unwrap();
        let dist = tempfile::tempdir().unwrap();
        std::fs::write(dist.path().join("manifest.json"), r#"{"v":1}"#).unwrap();
        std::fs::write(dist.path().join("background.js"), "// v1").unwrap();
        let home = tempfile::tempdir().unwrap();
        let previous_home = std::env::var_os("HOME");
        let previous_xdg = std::env::var_os("XDG_CONFIG_HOME");
        unsafe {
            std::env::set_var("HOME", home.path());
            #[cfg(not(target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", home.path().join(".config"));
        }
        #[cfg(target_os = "macos")]
        std::fs::create_dir_all(home.path().join("Library/Application Support")).unwrap();

        let first = install_firefox_companion(Some(dist.path())).expect("first install");
        let user_js_path = first.profile_dir.join("user.js");
        let mut user_js = std::fs::read_to_string(&user_js_path).unwrap();
        user_js.push_str("user_pref(\"bobby.test.custom\", true);\n");
        std::fs::write(&user_js_path, &user_js).unwrap();
        // Stale xpi from an older manual install must not block unpacked sideload.
        let stale_xpi = first
            .profile_dir
            .join("extensions")
            .join(format!("{COMPANION_GECKO_ID}.xpi"));
        std::fs::write(&stale_xpi, b"stale").unwrap();
        std::fs::write(first.sideload_dir.join("stale-asset.js"), "// gone").unwrap();

        std::fs::write(dist.path().join("manifest.json"), r#"{"v":2}"#).unwrap();
        std::fs::write(dist.path().join("background.js"), "// v2").unwrap();
        std::fs::write(dist.path().join("popup.js"), "// new").unwrap();
        let second = install_firefox_companion(Some(dist.path())).expect("reinstall");

        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match previous_xdg {
            Some(value) => unsafe { std::env::set_var("XDG_CONFIG_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }

        assert_eq!(
            std::fs::read_to_string(second.sideload_dir.join("manifest.json")).unwrap(),
            r#"{"v":2}"#
        );
        assert_eq!(
            std::fs::read_to_string(second.sideload_dir.join("background.js")).unwrap(),
            "// v2"
        );
        assert!(second.sideload_dir.join("popup.js").is_file());
        assert!(!second.sideload_dir.join("stale-asset.js").exists());
        assert!(!stale_xpi.exists());
        let preserved = std::fs::read_to_string(second.profile_dir.join("user.js")).unwrap();
        assert!(preserved.contains("user_pref(\"bobby.test.custom\", true);"));
        for &(name, _) in FIREFOX_PROFILE_PREFS {
            assert!(preserved.contains(&format!("user_pref(\"{name}\"")));
        }
    }

    #[test]
    fn install_writes_enroll_defaults_next_to_descriptor() {
        use firefox_companion::selection::{enroll_defaults_path, read_enroll_defaults};

        let _lock = INSTALL_ENV_LOCK.lock().unwrap();
        let dist = tempfile::tempdir().unwrap();
        std::fs::write(dist.path().join("manifest.json"), "{}").unwrap();
        std::fs::write(dist.path().join("background.js"), "//").unwrap();
        let home = tempfile::tempdir().unwrap();
        let previous_home = std::env::var_os("HOME");
        let previous_xdg = std::env::var_os("XDG_CONFIG_HOME");
        unsafe {
            std::env::set_var("HOME", home.path());
            #[cfg(not(target_os = "macos"))]
            std::env::set_var("XDG_CONFIG_HOME", home.path().join(".config"));
        }
        #[cfg(target_os = "macos")]
        std::fs::create_dir_all(home.path().join("Library/Application Support")).unwrap();

        let install = install_firefox_companion(Some(dist.path()));
        match previous_home {
            Some(value) => unsafe { std::env::set_var("HOME", value) },
            None => unsafe { std::env::remove_var("HOME") },
        }
        match previous_xdg {
            Some(value) => unsafe { std::env::set_var("XDG_CONFIG_HOME", value) },
            None => unsafe { std::env::remove_var("XDG_CONFIG_HOME") },
        }

        let install = install.expect("companion installs");
        let config_dir = install.descriptor_path.parent().unwrap();
        let defaults_path = enroll_defaults_path(config_dir);
        assert!(defaults_path.is_file());
        assert_eq!(
            defaults_path.parent().unwrap(),
            install.descriptor_path.parent().unwrap()
        );

        let defaults = read_enroll_defaults(&defaults_path).unwrap();
        assert_eq!(defaults.profile_dir, config_dir.join("firefox-profile"));
        assert!(defaults.profile_dir.is_dir());
        assert_eq!(defaults.companion_bind.to_string(), "127.0.0.1:9876");
        assert_eq!(defaults.descriptor_path, install.descriptor_path);
    }

    #[test]
    fn companion_install_reports_a_missing_extension_build() {
        let missing = tempfile::tempdir().unwrap();
        let error = install_firefox_companion(Some(missing.path()))
            .expect_err("an empty directory is not a built extension");
        assert!(format!("{error:#}").contains("companion extension build not found"));
    }

    #[test]
    fn merging_rejects_a_config_that_is_not_an_object() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join(".mcp.json"), "[1,2,3]").unwrap();
        assert!(merge_host_config(HostKind::Claude, root.path()).is_err());
    }
}

// ---------------------------------------------------------------------------
// Browser companions
// ---------------------------------------------------------------------------

/// Where the Firefox companion pieces live after installation: extension
/// copy, native-host wrapper, and pairing descriptor under the bobby config
/// dir; the manifest goes to Mozilla's per-platform native-messaging dir;
/// the profile receives an unpacked sideload plus `user.js` prefs.
#[derive(Debug)]
pub struct CompanionInstall {
    pub extension_dir: PathBuf,
    pub wrapper_path: PathBuf,
    pub manifest_path: PathBuf,
    pub descriptor_path: PathBuf,
    pub profile_dir: PathBuf,
    pub sideload_dir: PathBuf,
}

/// Gecko add-on id from `packages/firefox-companion/manifest.json`.
const COMPANION_GECKO_ID: &str = "firefox-companion@bobby-browser.local";

/// Prefs required for unsigned permanent sideload on Dev Edition / Nightly / ESR.
const FIREFOX_PROFILE_PREFS: &[(&str, &str)] = &[
    ("xpinstall.signatures.required", "false"),
    ("extensions.autoDisableScopes", "14"),
    ("privacy.resistFingerprinting", "false"),
    ("ui.systemUsesDarkTheme", "1"),
];

fn bobby_config_dir() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("config directory unavailable")?
        .join("bobby-browser"))
}

fn native_messaging_dir() -> Result<PathBuf> {
    let home = dirs::home_dir().context("home directory unavailable")?;
    Ok(match std::env::consts::OS {
        "macos" => home
            .join("Library")
            .join("Application Support")
            .join("Mozilla")
            .join("NativeMessagingHosts"),
        "linux" => home.join(".mozilla").join("native-messaging-hosts"),
        other => anyhow::bail!("Firefox native messaging is unsupported on {other}"),
    })
}

/// Whether any Firefox binary is discoverable — the companion item defaults
/// off when there is nothing to pair with.
fn firefox_present() -> bool {
    if std::env::consts::OS == "macos" {
        for candidate in [
            "/Applications/Firefox.app/Contents/MacOS/firefox",
            "/Applications/Firefox Developer Edition.app/Contents/MacOS/firefox",
        ] {
            if Path::new(candidate).is_file() {
                return true;
            }
        }
    }
    std::env::var_os("PATH")
        .map(|paths| {
            std::env::split_paths(&paths).any(|dir| {
                dir.join("firefox").is_file() || dir.join("firefox-developer-edition").is_file()
            })
        })
        .unwrap_or(false)
}

/// Locate the built companion extension: an explicit flag first, then the
/// repo layout relative to the running binary (`target/<profile>/bobby` →
/// repo root two levels up), then the current directory.
fn find_companion_dist(explicit: Option<&Path>) -> Result<PathBuf> {
    let mut candidates: Vec<PathBuf> = explicit.into_iter().map(Path::to_path_buf).collect();
    if let Ok(exe) = std::env::current_exe() {
        if let Some(prefix) = exe.parent().and_then(Path::parent) {
            candidates.push(prefix.join("share/bobby-browser/firefox-companion"));
        }
        if let Some(root) = exe.parent().and_then(Path::parent).and_then(Path::parent) {
            candidates.push(root.join("packages/firefox-companion/dist"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("packages/firefox-companion/dist"));
    }
    candidates
        .into_iter()
        .find(|candidate| candidate.join("manifest.json").is_file())
        .ok_or_else(|| {
            anyhow!(
                "companion extension build not found; run `pnpm --filter @cavi-ai/bobby-firefox-companion build` or pass --extension <dir>"
            )
        })
}

/// Install the Firefox companion: copy the built extension into the bobby
/// config dir, install the native-host wrapper and manifest, ensure the Bobby
/// Firefox profile prefs, and permanently sideload an unpacked extension into
/// that profile. Pairing is a later step (toolbar Pair).
pub fn install_firefox_companion(extension: Option<&Path>) -> Result<CompanionInstall> {
    let dist = find_companion_dist(extension)?;
    let config = bobby_config_dir()?;
    let extension_dir = config.join("firefox-companion");
    copy_dir(&dist, &extension_dir)?;
    let profile_dir = config.join("firefox-profile");
    std::fs::create_dir_all(&profile_dir)?;
    ensure_firefox_profile_user_js(&profile_dir)?;
    let sideload_dir = profile_dir.join("extensions").join(COMPANION_GECKO_ID);
    sideload_unpacked_extension(&dist, &sideload_dir)?;
    let install = CompanionInstall {
        extension_dir,
        wrapper_path: config.join("firefox-native-host"),
        manifest_path: native_messaging_dir()?.join("com.bobby_browser.companion.json"),
        descriptor_path: config.join("firefox-native-host-descriptor.json"),
        profile_dir: profile_dir.clone(),
        sideload_dir,
    };
    let exe = std::env::current_exe().context("current executable unknown")?;
    crate::install_native_host(crate::NativeHostInstallConfig {
        wrapper_path: install.wrapper_path.clone(),
        manifest_path: install.manifest_path.clone(),
        cli_path: exe,
        descriptor_path: install.descriptor_path.clone(),
    })?;
    let defaults = firefox_companion::selection::FirefoxEnrollDefaults {
        profile_dir,
        companion_bind: firefox_companion::selection::DEFAULT_COMPANION_BIND
            .parse()
            .expect("default companion bind is valid"),
        descriptor_path: install.descriptor_path.clone(),
    };
    firefox_companion::selection::write_enroll_defaults(
        &firefox_companion::selection::enroll_defaults_path(&config),
        &defaults,
    )?;
    Ok(install)
}

/// Create or append missing `user.js` prefs without wiping operator-owned lines.
fn ensure_firefox_profile_user_js(profile_dir: &Path) -> Result<()> {
    let path = profile_dir.join("user.js");
    let existing = if path.is_file() {
        std::fs::read_to_string(&path)?
    } else {
        String::new()
    };
    let mut additions = String::new();
    for &(name, value) in FIREFOX_PROFILE_PREFS {
        let marker = format!("user_pref(\"{name}\"");
        if !existing.contains(&marker) {
            additions.push_str(&format!("user_pref(\"{name}\", {value});\n"));
        }
    }
    if additions.is_empty() {
        return Ok(());
    }
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&additions);
    std::fs::write(path, out)?;
    Ok(())
}

/// Replace the unpacked sideload directory so upgrades drop removed assets.
/// Also removes a stale `{id}.xpi` if present so Firefox does not see two copies.
fn sideload_unpacked_extension(dist: &Path, sideload_dir: &Path) -> Result<()> {
    let extensions_dir = sideload_dir
        .parent()
        .ok_or_else(|| anyhow!("sideload path has no parent"))?;
    std::fs::create_dir_all(extensions_dir)?;
    let stale_xpi = extensions_dir.join(format!("{COMPANION_GECKO_ID}.xpi"));
    if stale_xpi.is_file() {
        std::fs::remove_file(&stale_xpi)?;
    }
    if sideload_dir.exists() {
        std::fs::remove_dir_all(sideload_dir)?;
    }
    copy_dir(dist, sideload_dir)?;
    Ok(())
}

fn copy_dir(source: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(source)? {
        let entry = entry?;
        let target = dest.join(entry.file_name());
        if entry.file_type()?.is_dir() {
            copy_dir(&entry.path(), &target)?;
        } else {
            std::fs::copy(entry.path(), &target)?;
        }
    }
    Ok(())
}
