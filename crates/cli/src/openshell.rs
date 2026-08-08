//! NVIDIA OpenShell host pack: policy + MCP HTTP client config + per-sandbox
//! principal provision. OpenShell owns isolation; bobby stays on the host.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{Duration as ChronoDuration, Utc};
use serde_json::json;
use types::CURRENT_INTERFACE_VERSION;
use uuid::Uuid;

#[cfg(test)]
use types::Capability;

use crate::bootstrap_local::AGENT_CAPABILITIES;
use crate::jobs_client::{self, resolve_jobs_auth, resolve_jobs_base_url};

const CLIENT_TOKEN_ENV: &str = "AUTOMATION_RUNTIME_TOKEN";
const REQUEST_TIMEOUT_SECS: u64 = 10;
const DEADLINE_MINUTES: i64 = 2;
const DEFAULT_SANDBOX_TTL_HOURS: i64 = 12;
const DEFAULT_MCP_HOST: &str = "host.docker.internal";
const DEFAULT_MCP_PORT: u16 = 7777;
const MCP_PATH: &str = "/v1/mcp";

const SKILL_SOURCE: &str = include_str!("../../../skill/SKILL.md");

/// Files written by [`install_pack`] under `<project>/openshell/`.
pub struct OpenshellPack {
    pub dir: PathBuf,
    pub policy: PathBuf,
    pub mcp: PathBuf,
    pub readme: PathBuf,
    pub skill: PathBuf,
}

/// Options for rendering the pack against a host MCP endpoint.
#[derive(Debug, Clone)]
pub struct PackOptions {
    pub mcp_host: String,
    pub mcp_port: u16,
    /// Agent binary path placeholder inside the OpenShell policy `binaries` list.
    pub agent_binary: String,
}

impl Default for PackOptions {
    fn default() -> Self {
        Self {
            mcp_host: DEFAULT_MCP_HOST.to_owned(),
            mcp_port: DEFAULT_MCP_PORT,
            agent_binary: "/usr/local/bin/claude".to_owned(),
        }
    }
}

impl PackOptions {
    pub fn mcp_url(&self) -> String {
        format!("http://{}:{}{}", self.mcp_host, self.mcp_port, MCP_PATH)
    }
}

/// MCP client JSON fragment for an agent inside an OpenShell sandbox.
/// Bearer comes from OpenShell-injected env — never embedded.
pub fn emit_mcp_config(options: &PackOptions) -> String {
    let fragment = json!({
        "mcpServers": {
            "bobby-browser": {
                "url": options.mcp_url(),
                "transport": "streamable-http",
                "headers": {
                    "Authorization": format!("Bearer ${{{CLIENT_TOKEN_ENV}}}")
                }
            }
        }
    });
    serde_json::to_string_pretty(&fragment).expect("openshell mcp config is serializable")
}

/// OpenShell network policy allowing MCP Streamable HTTP to host bobby only.
pub fn emit_policy_yaml(options: &PackOptions) -> String {
    format!(
        r#"# bobby-browser ↔ OpenShell network policy (sample)
# Apply with: openshell policy set <sandbox> --policy openshell/policy.yaml --wait
# Replace binaries.path with the agent binary that will call MCP.
# Reachability uses OpenShell's supervisor proxy — do not expose bobby beyond
# the host gateway address the sandbox can dial (host.docker.internal /
# host.containers.internal / LAN host). Keep bobby bind loopback+gateway only.
version: 1
filesystem_policy:
  include_workdir: true
  read_only: [/usr, /lib, /proc, /dev/urandom, /app, /etc, /var/log]
  read_write: [/sandbox, /tmp, /dev/null]
landlock:
  compatibility: best_effort
process:
  run_as_user: sandbox
  run_as_group: sandbox
network_policies:
  bobby_browser_mcp:
    name: bobby-browser-mcp
    endpoints:
      - host: {host}
        port: {port}
        path: {path}
        protocol: mcp
        enforcement: enforce
        mcp:
          max_body_bytes: 131072
          allow_all_known_mcp_methods: true
    binaries:
      - path: {binary}
"#,
        host = options.mcp_host,
        port = options.mcp_port,
        path = MCP_PATH,
        binary = options.agent_binary,
    )
}

fn emit_readme(options: &PackOptions) -> String {
    format!(
        r#"# bobby-browser on OpenShell

Host runs bobby + Firefox companion. The sandbox agent reaches bobby only
through OpenShell's policy proxy (this pack's `policy.yaml`).

## Host (once)

1. `bobby init --preset unrestricted` (mint principals)
2. Pair Firefox: `bobby install --companion`, then Pair in the toolbar
3. `bobby serve` (MCP HTTP at `{url}`)
4. Bind so the sandbox can reach the host gateway (`host.docker.internal` on
   Docker Desktop, `host.containers.internal` on Podman, or the LAN IP). Prefer
   not exposing bobby on untrusted networks.

## Per sandbox

1. Copy this `openshell/` pack into the sandbox image or sync it in.
2. `bobby openshell provision --sandbox <id>` on the host — writes a 0600
   injection env under the OS config dir; pass that bearer into OpenShell
   credential injection as `{token_env}` (never bake it into the image).
3. `openshell policy set <sandbox> --policy policy.yaml --wait`
4. Point the agent MCP client at `mcp.json` (or merge its `mcpServers` entry).
5. When the sandbox dies: `bobby openshell revoke --sandbox <id>`

One sandbox ↔ one bobby principal. Sandbox never holds `authority:admin`.
"#,
        url = options.mcp_url(),
        token_env = CLIENT_TOKEN_ENV,
    )
}

/// Write the OpenShell pack under `project_root/openshell/`.
pub fn install_pack(project_root: &Path, options: &PackOptions) -> Result<OpenshellPack> {
    let dir = project_root.join("openshell");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("could not create {}", dir.display()))?;
    let skill_dir = dir.join("skills").join("bobby-browser");
    std::fs::create_dir_all(&skill_dir)
        .with_context(|| format!("could not create {}", skill_dir.display()))?;

    let policy = dir.join("policy.yaml");
    let mcp = dir.join("mcp.json");
    let readme = dir.join("README.md");
    let skill = skill_dir.join("SKILL.md");

    write_text(&policy, &emit_policy_yaml(options))?;
    let mut mcp_body = emit_mcp_config(options);
    mcp_body.push('\n');
    write_text(&mcp, &mcp_body)?;
    write_text(&readme, &emit_readme(options))?;
    write_text(&skill, SKILL_SOURCE)?;

    Ok(OpenshellPack {
        dir,
        policy,
        mcp,
        readme,
        skill,
    })
}

fn write_text(path: &Path, contents: &str) -> Result<()> {
    std::fs::write(path, contents)
        .with_context(|| format!("could not write {}", path.display()))?;
    Ok(())
}

/// OS config dir for per-sandbox injection env files (secrets, never commit).
pub fn openshell_secrets_dir() -> Result<PathBuf> {
    let base = dirs::config_dir().context("config directory unavailable")?;
    Ok(base.join("bobby-browser").join("openshell"))
}

fn sandbox_env_path(sandbox: &str) -> Result<PathBuf> {
    validate_sandbox_id(sandbox)?;
    Ok(openshell_secrets_dir()?.join(format!("{sandbox}.env")))
}

fn sandbox_meta_path(sandbox: &str) -> Result<PathBuf> {
    validate_sandbox_id(sandbox)?;
    Ok(openshell_secrets_dir()?.join(format!("{sandbox}.principal")))
}

fn validate_sandbox_id(sandbox: &str) -> Result<()> {
    if sandbox.is_empty()
        || sandbox.len() > 128
        || !sandbox
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        bail!("sandbox id must be 1–128 chars of [A-Za-z0-9_-]");
    }
    Ok(())
}

pub struct ProvisionResult {
    pub sandbox: String,
    pub principal_id: Uuid,
    pub expires_at: String,
    pub env_path: PathBuf,
    pub mcp_url: String,
}

/// Mint one agent-scoped principal for `sandbox` and write injection env (0600).
pub fn provision_sandbox(
    sandbox: &str,
    base_url: Option<String>,
    config: &config::AppConfig,
    bootstrap_path: &Path,
    token_override: Option<String>,
    ttl_hours: Option<i64>,
    pack: &PackOptions,
) -> Result<ProvisionResult> {
    validate_sandbox_id(sandbox)?;
    let admin = resolve_jobs_auth(token_override, bootstrap_path)?;
    let base = resolve_jobs_base_url(base_url, config);
    let mcp_url = pack.mcp_url();
    let ttl = ChronoDuration::hours(ttl_hours.unwrap_or(DEFAULT_SANDBOX_TTL_HOURS));
    let principal_id = Uuid::new_v4();
    let expires_at = (Utc::now() + ttl).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let capabilities: Vec<&str> = AGENT_CAPABILITIES.iter().map(|cap| cap.as_str()).collect();

    let body = json!({
        "principalId": principal_id,
        "capabilities": capabilities,
        "expiresAt": expires_at,
    });
    let url = jobs_client::jobs_url(&base, "/v1/principals")?;
    let response = principals_request(PrincipalsRequest {
        method: reqwest::Method::POST,
        url,
        bearer: admin,
        body: Some(body),
        idempotency_key: Some(format!("openshell-provision-{sandbox}")),
    })?;

    let bearer = response
        .get("bearer")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("POST /v1/principals response missing bearer"))?
        .to_owned();
    let principal = response
        .get("principalId")
        .and_then(|v| v.as_str())
        .and_then(|s| Uuid::parse_str(s).ok())
        .unwrap_or(principal_id);
    let expires = response
        .get("expiresAt")
        .and_then(|v| v.as_str())
        .unwrap_or(&expires_at)
        .to_owned();

    let secrets = openshell_secrets_dir()?;
    std::fs::create_dir_all(&secrets)
        .with_context(|| format!("could not create {}", secrets.display()))?;
    let env_path = sandbox_env_path(sandbox)?;
    let meta_path = sandbox_meta_path(sandbox)?;
    let env_body = format!(
        "# bobby openshell injection for sandbox `{sandbox}` — do not commit\n\
         # Inject into the OpenShell sandbox as process env (never bake into the image).\n\
         {CLIENT_TOKEN_ENV}={bearer}\n\
         BOBBY_OPENSHELL_SANDBOX={sandbox}\n\
         BOBBY_OPENSHELL_PRINCIPAL={principal}\n\
         BOBBY_OPENSHELL_MCP_URL={mcp_url}\n"
    );
    write_secret_file(&env_path, &env_body)?;
    write_secret_file(&meta_path, &format!("{principal}\n"))?;

    Ok(ProvisionResult {
        sandbox: sandbox.to_owned(),
        principal_id: principal,
        expires_at: expires,
        env_path,
        mcp_url,
    })
}

/// Revoke the principal previously provisioned for `sandbox`.
pub fn revoke_sandbox(
    sandbox: &str,
    base_url: Option<String>,
    config: &config::AppConfig,
    bootstrap_path: &Path,
    token_override: Option<String>,
) -> Result<PathBuf> {
    validate_sandbox_id(sandbox)?;
    let meta_path = sandbox_meta_path(sandbox)?;
    let principal = std::fs::read_to_string(&meta_path)
        .with_context(|| {
            format!(
                "no provisioned principal for sandbox `{sandbox}` at {}",
                meta_path.display()
            )
        })?
        .trim()
        .to_owned();
    if principal.is_empty() {
        bail!("principal file for sandbox `{sandbox}` is empty");
    }

    let admin = resolve_jobs_auth(token_override, bootstrap_path)?;
    let base = resolve_jobs_base_url(base_url, config);
    let url = jobs_client::jobs_url(&base, &format!("/v1/principals/{principal}"))?;
    principals_request(PrincipalsRequest {
        method: reqwest::Method::DELETE,
        url,
        bearer: admin,
        body: None,
        idempotency_key: None,
    })?;

    let env_path = sandbox_env_path(sandbox)?;
    let _ = std::fs::remove_file(&env_path);
    let _ = std::fs::remove_file(&meta_path);
    Ok(meta_path)
}

struct PrincipalsRequest {
    method: reqwest::Method,
    url: String,
    bearer: String,
    body: Option<serde_json::Value>,
    idempotency_key: Option<String>,
}

fn principals_request(options: PrincipalsRequest) -> Result<serde_json::Value> {
    match std::thread::spawn(move || principals_request_blocking(options)).join() {
        Ok(result) => result,
        Err(_) => bail!("openshell principals HTTP thread panicked"),
    }
}

fn principals_request_blocking(options: PrincipalsRequest) -> Result<serde_json::Value> {
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(REQUEST_TIMEOUT_SECS))
        .no_proxy()
        .build()
        .context("failed to build openshell HTTP client")?;

    let correlation_id = Uuid::new_v4();
    let deadline = (Utc::now() + ChronoDuration::minutes(DEADLINE_MINUTES)).to_rfc3339();

    let mut builder = client
        .request(options.method.clone(), &options.url)
        .header(
            reqwest::header::AUTHORIZATION,
            format!("Bearer {}", options.bearer),
        )
        .header("x-interface-version", CURRENT_INTERFACE_VERSION)
        .header("x-correlation-id", correlation_id.to_string())
        .header("x-deadline", deadline);

    if let Some(key) = options.idempotency_key.as_deref() {
        builder = builder.header("idempotency-key", key);
    }

    let response = if let Some(body) = options.body {
        builder
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&body)
            .send()
    } else {
        builder.send()
    }
    .with_context(|| format!("{} {}", options.method, options.url))?;

    let status = response.status();
    let text = response.text().unwrap_or_default();
    if status == reqwest::StatusCode::NO_CONTENT {
        return Ok(json!({}));
    }
    if !status.is_success() {
        bail!("{} {}: {status} {text}", options.method, options.url);
    }
    if text.trim().is_empty() {
        return Ok(json!({}));
    }
    serde_json::from_str(&text).with_context(|| {
        format!(
            "{} {} returned non-JSON success body",
            options.method, options.url
        )
    })
}

fn write_secret_file(path: &Path, contents: &str) -> Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(path)
            .with_context(|| format!("could not open {}", path.display()))?;
        file.write_all(contents.as_bytes())?;
        let mut perms = file.metadata()?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(path, perms)?;
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, contents)
            .with_context(|| format!("could not write {}", path.display()))?;
    }
    Ok(())
}

/// Doctor check when `openshell/` exists in the project (or cwd).
pub fn doctor_pack_detail(project_root: &Path) -> Option<(bool, String)> {
    let dir = project_root.join("openshell");
    if !dir.is_dir() {
        return None;
    }
    let policy = dir.join("policy.yaml");
    let mcp = dir.join("mcp.json");
    let missing: Vec<&str> = [
        ("policy.yaml", policy.exists()),
        ("mcp.json", mcp.exists()),
    ]
    .into_iter()
    .filter_map(|(name, ok)| if ok { None } else { Some(name) })
    .collect();
    if missing.is_empty() {
        Some((
            true,
            format!(
                "pack at {} (policy.yaml + mcp.json); provision with `bobby openshell provision --sandbox <id>`",
                dir.display()
            ),
        ))
    } else {
        Some((
            false,
            format!(
                "openshell/ present but missing {}; re-run `bobby install --host openshell`",
                missing.join(", ")
            ),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_config_uses_env_placeholder_not_a_literal_secret() {
        let options = PackOptions::default();
        let emitted = emit_mcp_config(&options);
        let parsed: serde_json::Value = serde_json::from_str(&emitted).unwrap();
        let auth = parsed["mcpServers"]["bobby-browser"]["headers"]["Authorization"]
            .as_str()
            .unwrap();
        assert_eq!(auth, "Bearer ${AUTOMATION_RUNTIME_TOKEN}");
        assert!(emitted.contains("streamable-http"));
        assert!(emitted.contains("/v1/mcp"));
        assert!(!emitted.contains("authority:admin"));
    }

    #[test]
    fn policy_yaml_targets_mcp_protocol_on_host_gateway() {
        let options = PackOptions {
            mcp_host: "host.containers.internal".into(),
            mcp_port: 7777,
            agent_binary: "/usr/bin/node".into(),
        };
        let yaml = emit_policy_yaml(&options);
        assert!(yaml.contains("protocol: mcp"));
        assert!(yaml.contains("host: host.containers.internal"));
        assert!(yaml.contains("path: /v1/mcp"));
        assert!(yaml.contains("path: /usr/bin/node"));
        assert!(yaml.contains("allow_all_known_mcp_methods: true"));
    }

    #[test]
    fn install_pack_writes_expected_files() {
        let root = tempfile::tempdir().unwrap();
        let pack = install_pack(root.path(), &PackOptions::default()).unwrap();
        assert!(pack.policy.is_file());
        assert!(pack.mcp.is_file());
        assert!(pack.readme.is_file());
        assert!(pack.skill.is_file());
        let (ok, detail) = doctor_pack_detail(root.path()).unwrap();
        assert!(ok, "{detail}");
    }

    #[test]
    fn sandbox_id_rejects_path_traversal() {
        assert!(validate_sandbox_id("../evil").is_err());
        assert!(validate_sandbox_id("good-sandbox_1").is_ok());
    }

    #[test]
    fn agent_capabilities_exclude_admin() {
        assert!(!AGENT_CAPABILITIES.contains(&Capability::AuthorityAdmin));
        let wires: Vec<_> = AGENT_CAPABILITIES.iter().map(|c| c.as_str()).collect();
        assert!(wires.iter().all(|w| *w != "authority:admin"));
        assert!(wires.iter().any(|w| *w == "session:read"));
        assert!(wires.iter().any(|w| *w == "intent:execute"));
    }
}
