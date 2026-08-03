//! Agent onboarding: `bobby init --emit` client config fragments, the
//! `bobby doctor` MCP handshake check, `bobby mcp-stdio` (zero-wiring MCP
//! entrypoint), and the `bobby install` interactive installer.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use anyhow::{anyhow, Context, Result};
use clap::ValueEnum;

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
}

const GATEWAY_COMMAND: &str = if cfg!(windows) {
    "mcp-gateway.exe"
} else {
    "mcp-gateway"
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
    if let Ok(current) = std::env::current_exe() {
        if let Some(dir) = current.parent() {
            let sibling = dir.join(GATEWAY_COMMAND);
            if sibling.is_file() {
                return Ok(sibling);
            }
        }
    }
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            let candidate = dir.join(GATEWAY_COMMAND);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    Err(anyhow!(
        "{GATEWAY_COMMAND} not found next to the bobby binary or on PATH"
    ))
}

struct JsonRpcChild {
    child: Child,
    stdin: std::process::ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
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
            if std::time::Instant::now() > deadline {
                return Err(anyhow!("{method}: gateway did not answer within 15s"));
            }
            let mut line = String::new();
            let read = self.stdout.read_line(&mut line)?;
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
    let stdout = BufReader::new(child.stdout.take().expect("stdout piped"));
    let mut rpc = JsonRpcChild {
        child,
        stdin,
        stdout,
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
}

const SKILL_SOURCE: &str = include_str!("../../../skill/SKILL.md");
const SKILL_NAME: &str = "bobby-browser";

/// The MCP server entry an agent host launches: this same binary, absolute,
/// running `mcp-stdio`, which loads the bootstrap credential itself. No env
/// wiring in the host config, no secrets in any file the host reads.
fn static_server_entry() -> Result<(String, Vec<String>)> {
    let exe = std::env::current_exe().context("current executable unknown")?;
    Ok((
        exe.to_str()
            .ok_or_else(|| anyhow!("executable path is not valid UTF-8"))?
            .to_owned(),
        vec!["mcp-stdio".to_owned()],
    ))
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
    }
}

/// Merge the bobby-browser server entry into one host's config file,
/// preserving everything already there. Returns the file written.
pub fn merge_host_config(kind: HostKind, project_root: &Path) -> Result<PathBuf> {
    let path = host_config_path(kind, project_root)?;
    let (command, args) = static_server_entry()?;
    let mut config: serde_json::Value = match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid JSON", path.display()))?,
        _ => serde_json::json!({}),
    };
    let entry = match kind {
        HostKind::Claude => serde_json::json!({"command": command, "args": args}),
        HostKind::Vscode => serde_json::json!({"type": "stdio", "command": command, "args": args}),
        HostKind::Zed => serde_json::json!({"command": {"path": command, "args": args, "env": {}}}),
    };
    let section = match kind {
        HostKind::Claude => "mcpServers",
        HostKind::Vscode => "servers",
        HostKind::Zed => "context_servers",
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

/// Install the agent skill. `project` selects the project `.claude/skills/`
/// tree; otherwise the user-level `~/.claude/skills/` tree. Returns the file
/// written.
pub fn install_skill(project: bool, project_root: &Path) -> Result<PathBuf> {
    let base = if project {
        project_root.join(".claude").join("skills")
    } else {
        dirs::home_dir()
            .context("home directory unavailable")?
            .join(".claude")
            .join("skills")
    };
    let dir = base.join(SKILL_NAME);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("SKILL.md");
    std::fs::write(&path, SKILL_SOURCE)?;
    Ok(path)
}

/// `bobby mcp-stdio`: point an agent host at this and nothing else. Loads the
/// bootstrap credential (process env wins, then the bootstrap.env file) and
/// execs the stdio gateway, replacing this process.
pub fn exec_mcp_stdio(bootstrap_path: &Path) -> Result<()> {
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
            // exec'd gateway inherits. They are never printed or written.
            unsafe { std::env::set_var(key, value) };
        }
    }
    let gateway = resolve_gateway()?;
    exec_gateway(&gateway)
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
    let status = Command::new(gateway)
        .status()
        .with_context(|| format!("failed to run {}", gateway.display()))?;
    std::process::exit(status.code().unwrap_or(1));
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
    pub skill: bool,
    pub project_skill: bool,
    pub companion: bool,
    pub extension: Option<PathBuf>,
    pub force: bool,
    pub yes: bool,
}

/// `bobby install`: the one-command setup. Non-interactive when flags name
/// the work; otherwise a checklist the operator toggles.
pub fn run_install(bootstrap_path: &Path, options: InstallOptions) -> Result<()> {
    let InstallOptions {
        hosts,
        skill,
        project_skill,
        companion,
        extension,
        force,
        yes,
    } = options;
    let hosts = hosts.as_slice();
    let extension = extension.as_deref();
    let project_root = std::env::current_dir()?;
    let mut items: Vec<InstallItem> = Vec::new();

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
    items.push(InstallItem {
        label: credential_label,
        enabled: force || !credential_exists,
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

    for host in [HostKind::Claude, HostKind::Zed, HostKind::Vscode] {
        let selected = hosts.contains(&host);
        let default_on = matches!(host, HostKind::Claude);
        let root = project_root.clone();
        items.push(InstallItem {
            label: format!("{host:?}: merge the MCP server entry into its config"),
            enabled: selected || (hosts.is_empty() && default_on),
            run: Box::new(move || {
                let path = merge_host_config(host, &root)?;
                Ok(format!("merged into {}", path.display()))
            }),
        });
    }

    let companion_default = companion || (hosts.is_empty() && !skill && firefox_present());
    let extension_path = extension.map(Path::to_path_buf);
    items.push(InstallItem {
        label: "Firefox companion: install extension and native host (pair at first use)".to_owned(),
        enabled: companion || companion_default,
        run: Box::new(move || {
            let install = install_firefox_companion(extension_path.as_deref())?;
            Ok(format!(
                "extension at {}, native host manifest at {}. Next: start Firefox with --remote-debugging-port, then `bobby enroll-firefox-profile`",
                install.extension_dir.display(),
                install.manifest_path.display()
            ))
        }),
    });

    items.push(InstallItem {
        label: if project_skill {
            "Agent skill: install into this project's .claude/skills/".to_owned()
        } else {
            "Agent skill: install into ~/.claude/skills/".to_owned()
        },
        enabled: skill,
        run: Box::new({
            let root = project_root.clone();
            move || {
                let path = install_skill(project_skill, &root)?;
                Ok(format!("installed to {}", path.display()))
            }
        }),
    });

    let interactive = !yes && hosts.is_empty() && !skill && !companion && !force;
    if interactive {
        if !std::io::IsTerminal::is_terminal(&std::io::stdin()) {
            anyhow::bail!(
                "bobby install needs a terminal for its checklist, or explicit flags: --host <claude|zed|vscode> --skill --yes"
            );
        }
        println!("bobby install — number toggles, enter runs the checked items, q quits:");
        let stdin = std::io::stdin();
        let mut lines = stdin.lock().lines();
        loop {
            for (index, item) in items.iter().enumerate() {
                println!(
                    "  [{}] {}. {}",
                    if item.enabled { "x" } else { " " },
                    index + 1,
                    item.label
                );
            }
            print!("> ");
            std::io::stdout().flush()?;
            let Some(line) = lines.next() else {
                anyhow::bail!("stdin closed before a selection");
            };
            let line = line?;
            let trimmed = line.trim();
            if trimmed.is_empty() {
                break;
            }
            if trimmed == "q" {
                println!("nothing changed");
                return Ok(());
            }
            match trimmed.parse::<usize>() {
                Ok(number) if (1..=items.len()).contains(&number) => {
                    let item = &mut items[number - 1];
                    item.enabled = !item.enabled;
                }
                _ => println!("  (enter a number, an empty line to run, or q)"),
            }
        }
    }

    let mut ran = 0;
    for item in items.iter().filter(|item| item.enabled) {
        let outcome = (item.run)()?;
        println!("ok: {outcome}");
        ran += 1;
    }
    if ran == 0 {
        println!("nothing selected; nothing changed");
    } else {
        println!("done. `bobby doctor` verifies the whole setup, including the MCP handshake.");
    }
    Ok(())
}

#[cfg(test)]
mod install_tests {
    use super::*;

    #[test]
    fn merging_into_a_fresh_claude_config_creates_the_section() {
        let root = tempfile::tempdir().unwrap();
        let path = merge_host_config(HostKind::Claude, root.path()).unwrap();
        let written: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        let server = &written["mcpServers"]["bobby-browser"];
        assert!(
            server["command"].as_str().unwrap().ends_with("bobby")
                || server["command"].as_str().unwrap().contains("bobby")
        );
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
    fn the_skill_installs_into_the_project_tree() {
        let root = tempfile::tempdir().unwrap();
        let path = install_skill(true, root.path()).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("---\nname: bobby-browser"));
        assert!(path.ends_with(".claude/skills/bobby-browser/SKILL.md"));
    }

    #[test]
    fn companion_install_copies_the_extension_and_installs_the_native_host() {
        let dist = tempfile::tempdir().unwrap();
        std::fs::write(dist.path().join("manifest.json"), "{}").unwrap();
        std::fs::write(dist.path().join("background.js"), "//").unwrap();
        let home = tempfile::tempdir().unwrap();
        // SAFETY: single-threaded test process env; restored after.
        let previous = std::env::var_os("HOME");
        unsafe { std::env::set_var("HOME", home.path()) };
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
/// dir; the manifest goes to Mozilla's per-platform native-messaging dir.
#[derive(Debug)]
pub struct CompanionInstall {
    pub extension_dir: PathBuf,
    pub wrapper_path: PathBuf,
    pub manifest_path: PathBuf,
    pub descriptor_path: PathBuf,
}

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
                "companion extension build not found; run `pnpm --filter @bobby-browser/firefox-companion build` or pass --extension <dir>"
            )
        })
}

/// Install the Firefox companion: copy the built extension into the bobby
/// config dir (so the source tree can move), then install the native-host
/// wrapper and manifest. Pairing against a running Firefox is a later step.
pub fn install_firefox_companion(extension: Option<&Path>) -> Result<CompanionInstall> {
    let dist = find_companion_dist(extension)?;
    let config = bobby_config_dir()?;
    let extension_dir = config.join("firefox-companion");
    copy_dir(&dist, &extension_dir)?;
    let install = CompanionInstall {
        extension_dir,
        wrapper_path: config.join("firefox-native-host"),
        manifest_path: native_messaging_dir()?.join("com.bobby_browser.companion.json"),
        descriptor_path: config.join("firefox-native-host-descriptor.json"),
    };
    let exe = std::env::current_exe().context("current executable unknown")?;
    crate::install_native_host(crate::NativeHostInstallConfig {
        wrapper_path: install.wrapper_path.clone(),
        manifest_path: install.manifest_path.clone(),
        cli_path: exe,
        descriptor_path: install.descriptor_path.clone(),
    })?;
    Ok(install)
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
