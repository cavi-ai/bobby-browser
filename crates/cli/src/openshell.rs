//! NVIDIA OpenShell host pack: policy + MCP HTTP client config + per-sandbox
//! principal provision. OpenShell owns isolation; bobby stays on the host.

use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use chrono::{Duration as ChronoDuration, Utc};
use clap::ValueEnum;
use serde_json::json;
use types::Capability;
use uuid::Uuid;

use crate::bootstrap_local::{self, AGENT_CAPABILITIES};
use crate::jobs_client::{self, resolve_jobs_auth, resolve_jobs_base_url};
use crate::v1_client::{self, V1Request};

const CLIENT_TOKEN_ENV: &str = "AUTOMATION_RUNTIME_TOKEN";
const DEFAULT_SANDBOX_TTL_HOURS: i64 = 12;
const DEFAULT_MCP_HOST: &str = "host.docker.internal";
const DEFAULT_MCP_PORT: u16 = 7777;
const MCP_PATH: &str = "/v1/mcp";
/// Headroom above bobby's 128 KiB tools/list budget for OpenShell L7 buffering.
const MCP_MAX_BODY_BYTES: u32 = 262_144;

const SKILL_SOURCE: &str = include_str!("../../../skill/SKILL.md");

/// Capability floor minted for an OpenShell sandbox principal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum OpenshellCapabilityPreset {
    /// Narrow tenant floor (default): browse/intent/evidence/recovery only.
    /// No JS eval, vision, jobs, fingerprint, or humanize.
    #[default]
    Openshell,
    /// Full local agent floor (still no `authority:admin`).
    Agent,
}

/// Narrow OpenShell tenant set — least privilege for sandboxed agents.
pub(crate) const OPENSHELL_CAPABILITIES: &[Capability] = &[
    Capability::SessionRead,
    Capability::SessionWrite,
    Capability::PageRead,
    Capability::PageWrite,
    Capability::BrowserMutate,
    Capability::FileUpload,
    Capability::FileDownload,
    Capability::IntentExecute,
    Capability::ContextRead,
    Capability::ArtifactRead,
    Capability::ArtifactCapture,
    Capability::RecoveryRead,
    Capability::RecoveryWrite,
];

pub fn capabilities_for_openshell_preset(
    preset: OpenshellCapabilityPreset,
) -> &'static [Capability] {
    match preset {
        OpenshellCapabilityPreset::Openshell => OPENSHELL_CAPABILITIES,
        OpenshellCapabilityPreset::Agent => AGENT_CAPABILITIES,
    }
}

/// Files written by [`install_pack`] under `<project>/openshell/`.
pub struct OpenshellPack {
    pub dir: PathBuf,
    pub policy: PathBuf,
    /// Network-only fragment for merging into an existing OpenShell policy.
    pub policy_network: PathBuf,
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

fn emit_network_policies_block(options: &PackOptions) -> String {
    format!(
        r#"network_policies:
  bobby_browser_mcp:
    name: bobby-browser-mcp
    endpoints:
      - host: {host}
        port: {port}
        path: {path}
        protocol: mcp
        enforcement: enforce
        mcp:
          max_body_bytes: {max_body}
          allow_all_known_mcp_methods: true
        deny_rules:
          - method: tools/call
            tool: evaluate_javascript
          - method: tools/call
            tool: job_submit
          - method: tools/call
            tool: job_status
          - method: tools/call
            tool: job_cancel
    binaries:
      - path: {binary}
"#,
        host = options.mcp_host,
        port = options.mcp_port,
        path = MCP_PATH,
        max_body = MCP_MAX_BODY_BYTES,
        binary = options.agent_binary,
    )
}

/// Network-only fragment: merge into an existing OpenShell policy (do not
/// `policy set` this file alone — it would wipe FS/process defaults).
pub fn emit_policy_network_yaml(options: &PackOptions) -> String {
    format!(
        r#"# bobby-browser ↔ OpenShell network_policies fragment (merge-only)
# WARNING: `openshell policy set` replaces the entire sandbox policy.
# Merge this block into your existing policy when you already customize FS/process.
# For greenfield sandboxes, prefer the full sample at openshell/policy.yaml.
# Prefer BOBBY_MCP_TOOLSET=explore (or act) inside the sandbox to keep tools/list small.
{block}"#,
        block = emit_network_policies_block(options),
    )
}

/// Full OpenShell policy sample allowing MCP Streamable HTTP to host bobby only.
pub fn emit_policy_yaml(options: &PackOptions) -> String {
    format!(
        r#"# bobby-browser ↔ OpenShell network policy (sample)
# Apply with: openshell policy set <sandbox> --policy openshell/policy.yaml --wait
# WARNING: `policy set` replaces the entire sandbox policy. Prefer merging
# openshell/policy-network.yaml into your existing policy when you already
# customize FS/process.
# Replace binaries.path with the agent binary that will call MCP.
# Reachability uses OpenShell's supervisor proxy — do not expose bobby beyond
# the host gateway address the sandbox can dial (host.docker.internal /
# host.containers.internal / LAN host). Keep bobby bind loopback+gateway only.
# Prefer BOBBY_MCP_TOOLSET=explore (or act) inside the sandbox to keep tools/list small.
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
{block}"#,
        block = emit_network_policies_block(options),
    )
}

fn emit_readme(options: &PackOptions) -> String {
    format!(
        r#"# bobby-browser on OpenShell

Host runs bobby + Firefox companion. The sandbox agent reaches bobby only
through OpenShell's policy proxy (this pack's `policy.yaml`).

## Isolation (read this)

- One OpenShell sandbox ↔ one bobby principal (narrow openshell capability floor by default).
- Sandbox never holds `authority:admin`.
- **Shared Firefox companion is a hard constraint:** cookies, logins, and the
  durable context graph are profile-scoped, not principal-scoped. Two sandboxes
  on the same host companion share site state. For stronger isolation use a
  dedicated companion profile per sandbox, or managed Chromium disposable
  workers (no persistent logins). `bobby doctor` warns when ≥2 local sandboxes
  share one enrolled companion.
- MCP URL defaults to cleartext HTTP across the host gateway — firewall that
  path; do not bind bobby to untrusted networks. Prefer HTTPS or loopback-only
  bind when possible.

## Host (once)

1. `bobby init --preset unrestricted` (mint principals)
2. Pair Firefox: `bobby install --companion`, then Pair in the toolbar
3. `bobby serve` (MCP HTTP at `{url}`)
4. Bind so the sandbox can reach the host gateway (`host.docker.internal` on
   Docker Desktop, `host.containers.internal` on Podman, or the LAN IP).

## Per sandbox

1. Copy this `openshell/` pack into the sandbox image or sync it in.
2. `bobby openshell provision --sandbox <id>` on the host — revokes any prior
   principal for that id, mints a fresh one (unique idempotency key), writes a
   0600 injection env under the OS config dir; pass that bearer into OpenShell
   credential injection as `{token_env}` (never bake it into the image).
   Use `--capabilities-preset agent` only when you need JS/vision/jobs.
3. Apply policy: greenfield → `openshell policy set <sandbox> --policy policy.yaml --wait`
   (full replace). Existing FS/process customizations → merge
   `policy-network.yaml` into your policy, then `policy set`.
4. Point the agent MCP client at `mcp.json`; set `BOBBY_MCP_TOOLSET=explore`.
5. When the sandbox dies: `bobby openshell revoke --sandbox <id>`
"#,
        url = options.mcp_url(),
        token_env = CLIENT_TOKEN_ENV,
    )
}

/// Write the OpenShell pack under `project_root/openshell/`.
pub fn install_pack(project_root: &Path, options: &PackOptions) -> Result<OpenshellPack> {
    let dir = project_root.join("openshell");
    std::fs::create_dir_all(&dir).with_context(|| format!("could not create {}", dir.display()))?;
    let skill_dir = dir.join("skills").join("bobby-browser");
    std::fs::create_dir_all(&skill_dir)
        .with_context(|| format!("could not create {}", skill_dir.display()))?;

    let policy = dir.join("policy.yaml");
    let policy_network = dir.join("policy-network.yaml");
    let mcp = dir.join("mcp.json");
    let readme = dir.join("README.md");
    let skill = skill_dir.join("SKILL.md");

    write_text(&policy, &emit_policy_yaml(options))?;
    write_text(&policy_network, &emit_policy_network_yaml(options))?;
    let mut mcp_body = emit_mcp_config(options);
    mcp_body.push('\n');
    write_text(&mcp, &mcp_body)?;
    write_text(&readme, &emit_readme(options))?;
    write_text(&skill, SKILL_SOURCE)?;

    Ok(OpenshellPack {
        dir,
        policy,
        policy_network,
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
/// Override with `BOBBY_OPENSHELL_SECRETS_DIR` (tests / alternate secret roots).
pub fn openshell_secrets_dir() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("BOBBY_OPENSHELL_SECRETS_DIR") {
        return Ok(PathBuf::from(path));
    }
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

fn sandbox_status_path(sandbox: &str) -> Result<PathBuf> {
    validate_sandbox_id(sandbox)?;
    Ok(openshell_secrets_dir()?.join(format!("{sandbox}.status.json")))
}

fn clear_local_sandbox_files(sandbox: &str) -> Result<()> {
    let _ = std::fs::remove_file(sandbox_env_path(sandbox)?);
    let _ = std::fs::remove_file(sandbox_meta_path(sandbox)?);
    let _ = std::fs::remove_file(sandbox_status_path(sandbox)?);
    Ok(())
}

/// Non-secret sidecar written next to the injection env for `list` / `status`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SandboxStatus {
    pub sandbox: String,
    pub principal_id: String,
    pub expires_at: String,
    pub mcp_url: String,
    pub capabilities_preset: String,
}

/// List locally recorded OpenShell sandboxes (from status sidecars / principal files).
pub fn list_sandboxes() -> Result<Vec<SandboxStatus>> {
    let dir = openshell_secrets_dir()?;
    if !dir.is_dir() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in
        std::fs::read_dir(&dir).with_context(|| format!("could not read {}", dir.display()))?
    {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if let Some(sandbox) = name.strip_suffix(".status.json") {
            if let Ok(status) = read_sandbox_status(sandbox) {
                out.push(status);
            }
            continue;
        }
        if let Some(sandbox) = name.strip_suffix(".principal") {
            if out.iter().any(|s| s.sandbox == sandbox) {
                continue;
            }
            if let Ok(principal) = std::fs::read_to_string(entry.path()) {
                let principal = principal.trim().to_owned();
                if !principal.is_empty() {
                    out.push(SandboxStatus {
                        sandbox: sandbox.to_owned(),
                        principal_id: principal,
                        expires_at: String::new(),
                        mcp_url: String::new(),
                        capabilities_preset: String::new(),
                    });
                }
            }
        }
    }
    out.sort_by(|a, b| a.sandbox.cmp(&b.sandbox));
    Ok(out)
}

/// Read non-secret status for one sandbox.
pub fn read_sandbox_status(sandbox: &str) -> Result<SandboxStatus> {
    validate_sandbox_id(sandbox)?;
    let path = sandbox_status_path(sandbox)?;
    if path.is_file() {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("could not read {}", path.display()))?;
        return serde_json::from_str(&text)
            .with_context(|| format!("{} is not valid status JSON", path.display()));
    }
    let meta = sandbox_meta_path(sandbox)?;
    let principal = std::fs::read_to_string(&meta)
        .with_context(|| {
            format!(
                "no status for sandbox `{sandbox}` (missing {} and {})",
                path.display(),
                meta.display()
            )
        })?
        .trim()
        .to_owned();
    if principal.is_empty() {
        bail!("principal file for sandbox `{sandbox}` is empty");
    }
    Ok(SandboxStatus {
        sandbox: sandbox.to_owned(),
        principal_id: principal,
        expires_at: String::new(),
        mcp_url: String::new(),
        capabilities_preset: String::new(),
    })
}

fn write_sandbox_status(status: &SandboxStatus) -> Result<()> {
    let path = sandbox_status_path(&status.sandbox)?;
    let mut body = serde_json::to_string_pretty(status)?;
    body.push('\n');
    write_secret_file(&path, &body)
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

/// Fresh idempotency key per provision attempt (sandbox id alone is sticky and
/// breaks revoke → re-provision when principalId changes).
pub fn provision_idempotency_key(sandbox: &str, generation: Uuid) -> String {
    format!("openshell-provision-{sandbox}-{generation}")
}

pub struct ProvisionResult {
    pub sandbox: String,
    pub principal_id: Uuid,
    pub expires_at: String,
    pub env_path: PathBuf,
    pub mcp_url: String,
    pub replaced_prior: bool,
}

/// Mint one principal for `sandbox` and write injection env (0600).
///
/// If a prior principal is recorded for this sandbox id, it is revoked first so
/// operators can rotate cleanly. Idempotency keys are unique per attempt.
#[allow(clippy::too_many_arguments)]
pub fn provision_sandbox(
    sandbox: &str,
    base_url: Option<String>,
    config: &config::AppConfig,
    bootstrap_path: &Path,
    token_override: Option<String>,
    ttl_hours: Option<i64>,
    pack: &PackOptions,
    capabilities_preset: OpenshellCapabilityPreset,
) -> Result<ProvisionResult> {
    validate_sandbox_id(sandbox)?;
    let admin = resolve_jobs_auth(token_override.clone(), bootstrap_path)?;
    let base = resolve_jobs_base_url(base_url.clone(), config);

    let replaced_prior = revoke_recorded_principal_if_any(sandbox, &base, &admin)?;

    let mcp_url = pack.mcp_url();
    let ttl = ChronoDuration::hours(ttl_hours.unwrap_or(DEFAULT_SANDBOX_TTL_HOURS));
    let principal_id = Uuid::new_v4();
    let generation = Uuid::new_v4();
    let expires_at = (Utc::now() + ttl).to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let capabilities: Vec<&str> = capabilities_for_openshell_preset(capabilities_preset)
        .iter()
        .map(|cap| cap.as_str())
        .collect();

    let body = json!({
        "principalId": principal_id,
        "capabilities": capabilities,
        "expiresAt": expires_at,
    });
    let url = jobs_client::jobs_url(&base, "/v1/principals")?;
    let response = principals_request(PrincipalsRequest {
        method: reqwest::Method::POST,
        url,
        bearer: admin.clone(),
        body: Some(body),
        idempotency_key: Some(provision_idempotency_key(sandbox, generation)),
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
    if let Err(error) = std::fs::create_dir_all(&secrets) {
        let _ = delete_principal(&base, &admin, &principal.to_string());
        return Err(error).with_context(|| format!("could not create {}", secrets.display()));
    }
    let env_path = sandbox_env_path(sandbox)?;
    let meta_path = sandbox_meta_path(sandbox)?;
    let env_body = format!(
        "# bobby openshell injection for sandbox `{sandbox}` — do not commit\n\
         # Inject into the OpenShell sandbox as process env (never bake into the image).\n\
         {CLIENT_TOKEN_ENV}={bearer}\n\
         BOBBY_OPENSHELL_SANDBOX={sandbox}\n\
         BOBBY_OPENSHELL_PRINCIPAL={principal}\n\
         BOBBY_OPENSHELL_MCP_URL={mcp_url}\n\
         BOBBY_OPENSHELL_CAPABILITIES_PRESET={}\n",
        match capabilities_preset {
            OpenshellCapabilityPreset::Openshell => "openshell",
            OpenshellCapabilityPreset::Agent => "agent",
        }
    );
    if let Err(error) = write_secret_file(&env_path, &env_body) {
        let _ = delete_principal(&base, &admin, &principal.to_string());
        return Err(error);
    }
    if let Err(error) = write_secret_file(&meta_path, &format!("{principal}\n")) {
        let _ = std::fs::remove_file(&env_path);
        let _ = delete_principal(&base, &admin, &principal.to_string());
        return Err(error);
    }
    let status = SandboxStatus {
        sandbox: sandbox.to_owned(),
        principal_id: principal.to_string(),
        expires_at: expires.clone(),
        mcp_url: mcp_url.clone(),
        capabilities_preset: match capabilities_preset {
            OpenshellCapabilityPreset::Openshell => "openshell".to_owned(),
            OpenshellCapabilityPreset::Agent => "agent".to_owned(),
        },
    };
    if let Err(error) = write_sandbox_status(&status) {
        let _ = clear_local_sandbox_files(sandbox);
        let _ = delete_principal(&base, &admin, &principal.to_string());
        return Err(error);
    }

    Ok(ProvisionResult {
        sandbox: sandbox.to_owned(),
        principal_id: principal,
        expires_at: expires,
        env_path,
        mcp_url,
        replaced_prior,
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
    delete_principal(&base, &admin, &principal)?;

    clear_local_sandbox_files(sandbox)?;
    Ok(meta_path)
}

/// If local meta exists for `sandbox`, revoke that principal and clear files.
/// Returns whether a prior principal was revoked.
fn revoke_recorded_principal_if_any(
    sandbox: &str,
    base_url: &str,
    admin_bearer: &str,
) -> Result<bool> {
    let meta_path = sandbox_meta_path(sandbox)?;
    if !meta_path.is_file() {
        return Ok(false);
    }
    let principal = std::fs::read_to_string(&meta_path)
        .with_context(|| format!("could not read {}", meta_path.display()))?
        .trim()
        .to_owned();
    if principal.is_empty() {
        let _ = clear_local_sandbox_files(sandbox);
        return Ok(false);
    }
    // Best-effort: already-revoked principals should not block rotation.
    let _ = delete_principal(base_url, admin_bearer, &principal);
    let _ = clear_local_sandbox_files(sandbox);
    Ok(true)
}

fn delete_principal(base_url: &str, admin_bearer: &str, principal: &str) -> Result<()> {
    let url = jobs_client::jobs_url(base_url, &format!("/v1/principals/{principal}"))?;
    principals_request(PrincipalsRequest {
        method: reqwest::Method::DELETE,
        url,
        bearer: admin_bearer.to_owned(),
        body: None,
        idempotency_key: None,
    })?;
    Ok(())
}

struct PrincipalsRequest {
    method: reqwest::Method,
    url: String,
    bearer: String,
    body: Option<serde_json::Value>,
    idempotency_key: Option<String>,
}

fn principals_request(options: PrincipalsRequest) -> Result<serde_json::Value> {
    let method = options.method.clone();
    let url = options.url.clone();
    let response = v1_client::v1_request(V1Request {
        method: options.method,
        url: options.url,
        bearer: options.bearer,
        body: options.body,
        idempotency_key: options.idempotency_key,
    })?;
    if !response.status.is_success() {
        bail!("{method} {url}: {} {}", response.status, response.body);
    }
    v1_client::parse_json_body(response.status, &response.body)
        .with_context(|| format!("{method} {url} returned non-JSON success body"))
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
    let missing: Vec<&str> = [("policy.yaml", policy.exists()), ("mcp.json", mcp.exists())]
        .into_iter()
        .filter_map(|(name, ok)| if ok { None } else { Some(name) })
        .collect();
    if missing.is_empty() {
        let mut detail = format!(
            "pack at {} (policy.yaml + mcp.json); provision with `bobby openshell provision --sandbox <id>`",
            dir.display()
        );
        if let Ok(policy_text) = std::fs::read_to_string(&policy) {
            if !policy_text.contains("evaluate_javascript") {
                detail.push_str(
                    "; warn: policy lacks tool deny_rules — re-run `bobby openshell install`",
                );
            }
        }
        if !dir.join("policy-network.yaml").is_file() {
            detail.push_str(
                "; warn: missing policy-network.yaml merge fragment — re-run `bobby openshell install`",
            );
        }
        Some((true, detail))
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

fn is_loopback_host(host: &str) -> bool {
    let host = host.trim_matches(|c| c == '[' || c == ']');
    matches!(host, "127.0.0.1" | "localhost" | "::1")
}

/// Parse `scheme` + hostname from an MCP URL like `http://host:7777/v1/mcp`.
fn mcp_url_scheme_host(url: &str) -> Option<(&str, &str)> {
    let (scheme, rest) = url.split_once("://")?;
    let host_port = rest.split('/').next().unwrap_or(rest);
    let host = host_port
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host_port);
    if host.is_empty() {
        return None;
    }
    Some((scheme, host))
}

/// Extra doctor rows when an OpenShell pack is present.
pub struct OpenshellDoctorExtras {
    pub admin: (bool, String),
    pub companion: (bool, String),
    pub mcp_url: Option<(bool, String)>,
    /// Cleartext bearer over a non-loopback host gateway / bind.
    pub cleartext: Option<(bool, String)>,
    pub local_sandboxes: (bool, String),
}

pub fn doctor_openshell_extras(
    project_root: &Path,
    bootstrap_path: Option<&Path>,
    config: Option<&config::AppConfig>,
    firefox_enrolled: bool,
) -> OpenshellDoctorExtras {
    let admin =
        match bootstrap_path {
            Some(path) if path.exists() => {
                match bootstrap_local::read_bootstrap_preset(Some(path)) {
                bootstrap_local::BootstrapPreset::Unrestricted => (
                    true,
                    "bootstrap preset unrestricted (can mint sandbox principals)".to_owned(),
                ),
                bootstrap_local::BootstrapPreset::Agent => (
                    false,
                    "bootstrap preset is agent (no authority:admin); use `bobby init --preset unrestricted` to mint principals"
                        .to_owned(),
                ),
            }
            }
            _ => (
                false,
                "no bootstrap credential; provision needs unrestricted admin bootstrap".to_owned(),
            ),
        };

    let sandbox_count = list_sandboxes().map(|list| list.len()).unwrap_or(0);
    let companion = if firefox_enrolled && sandbox_count >= 2 {
        (
            false,
            format!(
                "{sandbox_count} local OpenShell sandboxes share one Firefox companion profile — cookies/context leak across sandboxes; use a dedicated companion profile per sandbox or managed Chromium disposable workers"
            ),
        )
    } else if firefox_enrolled {
        (
            true,
            "Firefox companion enrolled — cookies/context are profile-scoped across all OpenShell sandboxes on this host"
                .to_owned(),
        )
    } else {
        (
            true,
            "no Firefox companion enrollment — OpenShell sandboxes can use managed Chromium disposable profiles for stronger isolation"
                .to_owned(),
        )
    };

    let mcp_path = project_root.join("openshell").join("mcp.json");
    let mcp_url_text = std::fs::read_to_string(&mcp_path).ok().and_then(|text| {
        serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|json| {
                json["mcpServers"]["bobby-browser"]["url"]
                    .as_str()
                    .map(str::to_owned)
            })
    });

    let mcp_url = {
        if !mcp_path.is_file() {
            None
        } else {
            let detail = match (mcp_url_text.as_deref(), config) {
                (Some(url), Some(cfg)) => {
                    let expected_port = cfg.server.port.to_string();
                    let port_ok = url.contains(&format!(":{expected_port}/"))
                        || url.ends_with(&format!(":{expected_port}"));
                    if port_ok {
                        (
                            true,
                            format!("mcp.json URL {url} matches config port {expected_port}"),
                        )
                    } else {
                        (
                            false,
                            format!(
                                "mcp.json URL {url} may not match config bind {}:{}",
                                cfg.server.host, cfg.server.port
                            ),
                        )
                    }
                }
                (None, _) => match std::fs::read_to_string(&mcp_path) {
                    Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
                        Ok(_) => (false, "mcp.json missing bobby-browser.url".to_owned()),
                        Err(error) => (false, format!("mcp.json is not valid JSON: {error}")),
                    },
                    Err(error) => (false, format!("could not read mcp.json: {error}")),
                },
                (Some(_), None) => (
                    true,
                    "mcp.json present (config unavailable for port check)".to_owned(),
                ),
            };
            Some(detail)
        }
    };

    let cleartext = match mcp_url_text.as_deref() {
        Some(url) => {
            let mut warnings = Vec::new();
            if let Some((scheme, host)) = mcp_url_scheme_host(url) {
                if scheme.eq_ignore_ascii_case("http") && !is_loopback_host(host) {
                    warnings.push(format!(
                        "mcp.json uses cleartext {scheme}://{host} — bearer crosses the host gateway; firewall that path or terminate TLS"
                    ));
                }
            }
            if let Some(cfg) = config {
                if !is_loopback_host(&cfg.server.host) {
                    warnings.push(format!(
                        "config server.host {} is non-loopback without TLS termination in-bobby — scope bind to gateway-only interfaces",
                        cfg.server.host
                    ));
                }
            }
            if warnings.is_empty() {
                Some((
                    true,
                    "MCP URL / serve bind look loopback-safe or HTTPS".to_owned(),
                ))
            } else {
                Some((false, warnings.join("; ")))
            }
        }
        None => None,
    };

    let local_sandboxes = match list_sandboxes() {
        Ok(list) if list.is_empty() => (true, "no local sandbox principals recorded".to_owned()),
        Ok(list) => (
            true,
            format!(
                "{} local sandbox(es): {}",
                list.len(),
                list.iter()
                    .map(|s| s.sandbox.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ),
        Err(error) => (false, format!("could not list local sandboxes: {error:#}")),
    };

    OpenshellDoctorExtras {
        admin,
        companion,
        mcp_url,
        cleartext,
        local_sandboxes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// `BOBBY_OPENSHELL_SECRETS_DIR` is process-global; serialize tests that mutate it.
    static SECRETS_DIR_LOCK: Mutex<()> = Mutex::new(());

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
    fn policy_yaml_denies_js_and_jobs_and_raises_body_budget() {
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
        assert!(yaml.contains("max_body_bytes: 262144"));
        assert!(yaml.contains("tool: evaluate_javascript"));
        assert!(yaml.contains("tool: job_submit"));
        assert!(yaml.contains("policy-network.yaml"));
        let network = emit_policy_network_yaml(&options);
        assert!(network.contains("network_policies:"));
        assert!(network.contains("merge-only") || network.contains("Merge this block"));
        assert!(!network.contains("filesystem_policy:"));
        assert!(network.contains("tool: evaluate_javascript"));
    }

    #[test]
    fn readme_states_shared_companion_isolation() {
        let readme = emit_readme(&PackOptions::default());
        assert!(readme.contains("Shared Firefox companion is a hard constraint"));
        assert!(readme.contains("profile-scoped, not principal-scoped"));
        assert!(readme.contains("unique idempotency key"));
        assert!(readme.contains("policy-network.yaml"));
    }

    #[test]
    fn install_pack_writes_expected_files() {
        let root = tempfile::tempdir().unwrap();
        let pack = install_pack(root.path(), &PackOptions::default()).unwrap();
        assert!(pack.policy.is_file());
        assert!(pack.policy_network.is_file());
        assert!(pack.mcp.is_file());
        assert!(pack.readme.is_file());
        assert!(pack.skill.is_file());
        let (ok, detail) = doctor_pack_detail(root.path()).unwrap();
        assert!(ok, "{detail}");
        assert!(!detail.contains("missing policy-network.yaml"));
    }

    #[test]
    fn doctor_warns_shared_companion_with_multiple_sandboxes() {
        let _guard = SECRETS_DIR_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let secrets = root.path().join("openshell-secrets");
        std::fs::create_dir_all(&secrets).unwrap();
        std::env::set_var("BOBBY_OPENSHELL_SECRETS_DIR", &secrets);
        let project = root.path().join("project");
        std::fs::create_dir_all(project.join("openshell")).unwrap();
        write_text(
            &project.join("openshell").join("mcp.json"),
            &emit_mcp_config(&PackOptions::default()),
        )
        .unwrap();

        let result = (|| {
            for name in ["a", "b"] {
                write_sandbox_status(&SandboxStatus {
                    sandbox: name.into(),
                    principal_id: Uuid::new_v4().to_string(),
                    expires_at: "2099-01-01T00:00:00.000Z".into(),
                    mcp_url: "http://host.docker.internal:7777/v1/mcp".into(),
                    capabilities_preset: "openshell".into(),
                })?;
            }
            let extras = doctor_openshell_extras(&project, None, None, true);
            anyhow::ensure!(!extras.companion.0, "{}", extras.companion.1);
            anyhow::ensure!(extras.companion.1.contains("share one Firefox companion"));
            let cleartext = extras.cleartext.expect("cleartext row");
            anyhow::ensure!(!cleartext.0, "{}", cleartext.1);
            anyhow::ensure!(cleartext.1.contains("cleartext"));
            Ok::<(), anyhow::Error>(())
        })();
        std::env::remove_var("BOBBY_OPENSHELL_SECRETS_DIR");
        result.unwrap();
    }

    #[test]
    fn cleartext_ok_for_loopback_mcp_url() {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("project");
        let openshell = project.join("openshell");
        std::fs::create_dir_all(&openshell).unwrap();
        let options = PackOptions {
            mcp_host: "127.0.0.1".into(),
            mcp_port: 7777,
            agent_binary: "/usr/local/bin/claude".into(),
        };
        write_text(&openshell.join("mcp.json"), &emit_mcp_config(&options)).unwrap();
        let extras = doctor_openshell_extras(&project, None, None, false);
        let cleartext = extras.cleartext.expect("cleartext row");
        assert!(cleartext.0, "{}", cleartext.1);
    }

    #[test]
    fn sandbox_id_rejects_path_traversal() {
        assert!(validate_sandbox_id("../evil").is_err());
        assert!(validate_sandbox_id("good-sandbox_1").is_ok());
    }

    #[test]
    fn openshell_capabilities_are_narrower_than_agent() {
        assert!(!OPENSHELL_CAPABILITIES.contains(&Capability::AuthorityAdmin));
        assert!(!OPENSHELL_CAPABILITIES.contains(&Capability::JavascriptEvaluate));
        assert!(!OPENSHELL_CAPABILITIES.contains(&Capability::VisionAssist));
        assert!(!OPENSHELL_CAPABILITIES.contains(&Capability::JobSubmit));
        assert!(!OPENSHELL_CAPABILITIES.contains(&Capability::BrowserFingerprint));
        assert!(OPENSHELL_CAPABILITIES.contains(&Capability::IntentExecute));
        assert!(OPENSHELL_CAPABILITIES.contains(&Capability::SessionRead));
        assert!(AGENT_CAPABILITIES.len() > OPENSHELL_CAPABILITIES.len());
    }

    #[test]
    fn provision_idempotency_key_includes_generation() {
        let a = Uuid::new_v4();
        let b = Uuid::new_v4();
        let k1 = provision_idempotency_key("demo", a);
        let k2 = provision_idempotency_key("demo", b);
        assert_ne!(k1, k2);
        assert!(k1.starts_with("openshell-provision-demo-"));
        assert!(!k1.ends_with("demo"));
    }

    #[test]
    fn list_and_status_round_trip_via_status_sidecar() {
        let _guard = SECRETS_DIR_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let secrets = root.path().join("openshell-secrets");
        std::fs::create_dir_all(&secrets).unwrap();
        std::env::set_var("BOBBY_OPENSHELL_SECRETS_DIR", &secrets);

        let status = SandboxStatus {
            sandbox: "demo-list".into(),
            principal_id: Uuid::new_v4().to_string(),
            expires_at: "2099-01-01T00:00:00.000Z".into(),
            mcp_url: "http://host.docker.internal:7777/v1/mcp".into(),
            capabilities_preset: "openshell".into(),
        };
        let result = (|| {
            write_sandbox_status(&status)?;
            write_secret_file(
                &sandbox_meta_path("demo-list")?,
                &format!("{}\n", status.principal_id),
            )?;
            let listed = list_sandboxes()?;
            anyhow::ensure!(listed.iter().any(|s| s.sandbox == "demo-list"));
            let read = read_sandbox_status("demo-list")?;
            anyhow::ensure!(read.principal_id == status.principal_id);
            anyhow::ensure!(read.capabilities_preset == "openshell");
            clear_local_sandbox_files("demo-list")?;
            Ok::<(), anyhow::Error>(())
        })();
        std::env::remove_var("BOBBY_OPENSHELL_SECRETS_DIR");
        result.unwrap();
    }
}
