use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use broker::StartupCredential;
use chrono::{DateTime, Duration, Utc};
use clap::ValueEnum;
use types::{Capability, PrincipalId};
use uuid::Uuid;

pub const DEFAULT_TTL_DAYS: i64 = 30;

const ENV_TOKEN: &str = "AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN";
const ENV_PRINCIPAL: &str = "AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL";
const ENV_CAPABILITIES: &str = "AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES";
const ENV_EXPIRES_AT: &str = "AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT";
/// Optional process-env override for heal targeting (`agent` | `unrestricted`).
const ENV_PRESET: &str = "AUTOMATION_RUNTIME_BOOTSTRAP_PRESET";
const PRESET_MARKER: &str = "bobby-bootstrap-preset:";

/// Which capability floor heal / init use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
pub enum BootstrapPreset {
    /// Full local operator set, including `authority:admin` (default).
    #[default]
    Unrestricted,
    /// Agent host set: no `authority:admin`. Heal never widens past this set.
    Agent,
}

impl BootstrapPreset {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unrestricted => "unrestricted",
            Self::Agent => "agent",
        }
    }

    pub fn parse(raw: &str) -> Option<Self> {
        match raw.trim() {
            "unrestricted" => Some(Self::Unrestricted),
            "agent" => Some(Self::Agent),
            _ => None,
        }
    }
}

/// Unrestricted local defaults. Heal appends any missing entry for the
/// unrestricted preset.
pub(crate) const DEFAULT_CAPABILITIES: &[Capability] = &[
    Capability::SessionRead,
    Capability::SessionWrite,
    Capability::PageRead,
    Capability::PageWrite,
    Capability::BrowserMutate,
    Capability::FileUpload,
    Capability::FileDownload,
    Capability::JavascriptEvaluate,
    Capability::IntentExecute,
    Capability::VisionAssist,
    Capability::ContextRead,
    Capability::ArtifactRead,
    Capability::ArtifactCapture,
    Capability::RecoveryRead,
    Capability::RecoveryWrite,
    Capability::JobSubmit,
    Capability::JobRead,
    Capability::JobCancel,
    Capability::AuthorityAdmin,
    Capability::BrowserFingerprint,
    Capability::BrowserHumanize,
];

/// Agent host defaults: everything in [`DEFAULT_CAPABILITIES`] except
/// `authority:admin`. Heal for the agent preset only appends from this set.
pub(crate) const AGENT_CAPABILITIES: &[Capability] = &[
    Capability::SessionRead,
    Capability::SessionWrite,
    Capability::PageRead,
    Capability::PageWrite,
    Capability::BrowserMutate,
    Capability::FileUpload,
    Capability::FileDownload,
    Capability::JavascriptEvaluate,
    Capability::IntentExecute,
    Capability::VisionAssist,
    Capability::ContextRead,
    Capability::ArtifactRead,
    Capability::ArtifactCapture,
    Capability::RecoveryRead,
    Capability::RecoveryWrite,
    Capability::JobSubmit,
    Capability::JobRead,
    Capability::JobCancel,
    Capability::BrowserFingerprint,
    Capability::BrowserHumanize,
];

pub fn capabilities_for_preset(preset: BootstrapPreset) -> &'static [Capability] {
    match preset {
        BootstrapPreset::Unrestricted => DEFAULT_CAPABILITIES,
        BootstrapPreset::Agent => AGENT_CAPABILITIES,
    }
}

pub struct BootstrapMaterial {
    bearer: String,
    principal_id: PrincipalId,
    capabilities_csv: String,
    expires_at: DateTime<Utc>,
    preset: BootstrapPreset,
}

impl fmt::Debug for BootstrapMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BootstrapMaterial")
            .field("bearer", &"REDACTED")
            .field("principal_id", &self.principal_id)
            .field("capabilities_csv", &self.capabilities_csv)
            .field("expires_at", &self.expires_at)
            .field("preset", &self.preset)
            .finish()
    }
}

impl BootstrapMaterial {
    pub fn bearer(&self) -> &str {
        &self.bearer
    }

    pub fn principal_id(&self) -> &PrincipalId {
        &self.principal_id
    }

    pub fn capabilities_csv(&self) -> &str {
        &self.capabilities_csv
    }

    pub fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }

    pub fn preset(&self) -> BootstrapPreset {
        self.preset
    }
}

pub fn default_bootstrap_path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("config directory unavailable")?
        .join("bobby-browser")
        .join("bootstrap.env"))
}

pub fn generate_bootstrap(ttl: Duration) -> Result<BootstrapMaterial> {
    generate_bootstrap_for_preset(ttl, BootstrapPreset::Unrestricted)
}

pub fn generate_bootstrap_for_preset(
    ttl: Duration,
    preset: BootstrapPreset,
) -> Result<BootstrapMaterial> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).context("failed to generate bootstrap bearer entropy")?;
    let bearer = hex::encode(bytes);
    let principal_id = PrincipalId::from_uuid(Uuid::new_v4());
    let capabilities_csv = capabilities_for_preset(preset)
        .iter()
        .map(|capability| capability.as_str())
        .collect::<Vec<_>>()
        .join(",");
    let expires_at = Utc::now() + ttl;
    Ok(BootstrapMaterial {
        bearer,
        principal_id,
        capabilities_csv,
        expires_at,
        preset,
    })
}

pub fn write_bootstrap_env(path: &Path, material: &BootstrapMaterial, force: bool) -> Result<()> {
    if path.exists() && !force {
        bail!(
            "bootstrap env file {} already exists; pass --force to overwrite",
            path.display()
        );
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create parent dir for {}", parent.display()))?;
    }
    let contents = format!(
        "# {PRESET_MARKER} {}\n{ENV_TOKEN}={}\n{ENV_PRINCIPAL}={}\n{ENV_CAPABILITIES}={}\n{ENV_EXPIRES_AT}={}\n",
        material.preset().as_str(),
        material.bearer(),
        material.principal_id().as_uuid(),
        material.capabilities_csv(),
        material.expires_at().to_rfc3339(),
    );
    write_private_file(path, contents.as_bytes())
        .with_context(|| format!("failed to write bootstrap env to {}", path.display()))
}

/// Read the bootstrap preset from a dotenv comment or process env.
/// Missing marker defaults to [`BootstrapPreset::Unrestricted`] (back-compat).
pub fn read_bootstrap_preset(path: Option<&Path>) -> BootstrapPreset {
    if let Ok(raw) = std::env::var(ENV_PRESET) {
        if let Some(preset) = BootstrapPreset::parse(&raw) {
            return preset;
        }
    }
    path.and_then(|path| read_preset_marker_from_file(path).ok().flatten())
        .unwrap_or(BootstrapPreset::Unrestricted)
}

fn read_preset_marker_from_file(path: &Path) -> Result<Option<BootstrapPreset>> {
    if !path.exists() {
        return Ok(None);
    }
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read bootstrap env from {}", path.display()))?;
    for line in contents.lines() {
        let trimmed = line.trim();
        let Some(rest) = trimmed
            .strip_prefix('#')
            .map(str::trim)
            .and_then(|body| body.strip_prefix(PRESET_MARKER))
            .map(str::trim)
        else {
            continue;
        };
        if let Some(preset) = BootstrapPreset::parse(rest) {
            return Ok(Some(preset));
        }
    }
    Ok(None)
}

pub fn load_startup_from_env_file(path: &Path) -> Result<StartupCredential> {
    let fields = read_bootstrap_env_fields(path)?;
    let bearer = fields.token.with_context(|| {
        format!(
            "bootstrap env {} missing required key {ENV_TOKEN}",
            path.display()
        )
    })?;
    let principal = fields.principal.with_context(|| {
        format!(
            "bootstrap env {} missing required key {ENV_PRINCIPAL}",
            path.display()
        )
    })?;
    let capabilities = fields.capabilities.with_context(|| {
        format!(
            "bootstrap env {} missing required key {ENV_CAPABILITIES}",
            path.display()
        )
    })?;
    let expires_at = fields.expires_at.with_context(|| {
        format!(
            "bootstrap env {} missing required key {ENV_EXPIRES_AT}",
            path.display()
        )
    })?;
    let principal_id = PrincipalId::from_uuid(Uuid::parse_str(&principal).with_context(|| {
        format!(
            "bootstrap env {} has invalid principal {principal}",
            path.display()
        )
    })?);
    let capabilities = capabilities
        .split(',')
        .map(|value| parse_capability(value.trim()))
        .collect::<Result<Vec<_>>>()
        .with_context(|| format!("bootstrap env {} has invalid capabilities", path.display()))?;
    let expires_at = DateTime::parse_from_rfc3339(&expires_at)
        .with_context(|| {
            format!(
                "bootstrap env {} has invalid expiry {expires_at}",
                path.display()
            )
        })?
        .with_timezone(&Utc);
    StartupCredential::new(bearer, principal_id, capabilities, expires_at).with_context(|| {
        format!(
            "bootstrap env {} contains invalid startup credential",
            path.display()
        )
    })
}

pub fn is_loopback_host(host: &str) -> bool {
    host == "127.0.0.1" || host == "::1"
}

#[derive(Debug)]
pub enum ResolveOutcome {
    FromEnv(StartupCredential),
    FromFile(StartupCredential),
    Generated {
        credential: StartupCredential,
        material: BootstrapMaterial,
    },
}

pub fn resolve_startup_credential_with<F>(
    host: &str,
    bootstrap_path: &Path,
    from_env: F,
) -> Result<ResolveOutcome>
where
    F: FnOnce() -> Result<StartupCredential, broker::StartupCredentialError>,
{
    match from_env() {
        Ok(credential) => return Ok(ResolveOutcome::FromEnv(credential)),
        Err(broker::StartupCredentialError::MissingInput) => {}
        Err(error) => return Err(error.into()),
    }
    if bootstrap_path.exists() {
        let credential = load_startup_from_env_file(bootstrap_path)?;
        return Ok(ResolveOutcome::FromFile(credential));
    }
    if is_loopback_host(host) {
        let material = generate_bootstrap(Duration::days(DEFAULT_TTL_DAYS))?;
        write_bootstrap_env(bootstrap_path, &material, false)?;
        let credential = startup_from_material(&material)?;
        return Ok(ResolveOutcome::Generated {
            credential,
            material,
        });
    }
    bail!(
        "startup credentials missing for non-loopback host {host}; run `bobby init` or set AUTOMATION_RUNTIME_BOOTSTRAP_* env vars"
    );
}

fn startup_from_material(material: &BootstrapMaterial) -> Result<StartupCredential> {
    let capabilities = material
        .capabilities_csv()
        .split(',')
        .map(|value| parse_capability(value.trim()))
        .collect::<Result<Vec<_>>>()?;
    StartupCredential::new(
        material.bearer().to_string(),
        material.principal_id().clone(),
        capabilities,
        material.expires_at(),
    )
    .context("generated bootstrap material is not a valid startup credential")
}

fn parse_capability(value: &str) -> Result<Capability> {
    value.parse().map_err(Into::into)
}

/// Read the plaintext bootstrap bearer from a dotenv file (for CLI HTTP clients).
/// Never log or print the returned value.
pub fn load_bootstrap_bearer(path: &Path) -> Result<String> {
    let fields = read_bootstrap_env_fields(path)?;
    fields.token.with_context(|| {
        format!(
            "bootstrap env {} missing required key {ENV_TOKEN}",
            path.display()
        )
    })
}

/// Read the capabilities CSV from a bootstrap dotenv (for doctor checks).
pub fn load_bootstrap_capabilities_csv(path: &Path) -> Result<String> {
    let fields = read_bootstrap_env_fields(path)?;
    fields.capabilities.with_context(|| {
        format!(
            "bootstrap env {} missing required key {ENV_CAPABILITIES}",
            path.display()
        )
    })
}

/// Outcome of an additive capability heal against the preset's floor.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct HealBootstrapReport {
    /// Wire strings appended from the current default set.
    pub added: Vec<&'static str>,
    /// True when the bootstrap.env file was rewritten.
    pub file_rewritten: bool,
    /// True when process env `AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES` was updated.
    pub env_updated: bool,
}

impl HealBootstrapReport {
    pub fn changed(&self) -> bool {
        !self.added.is_empty() || self.file_rewritten || self.env_updated
    }

    pub fn merge(&mut self, other: HealBootstrapReport) {
        for wire in other.added {
            if !self.added.contains(&wire) {
                self.added.push(wire);
            }
        }
        self.file_rewritten |= other.file_rewritten;
        self.env_updated |= other.env_updated;
    }
}

/// Union an existing capabilities CSV with [`DEFAULT_CAPABILITIES`].
pub fn union_capabilities_csv(existing_csv: &str) -> Result<(String, Vec<&'static str>)> {
    union_capabilities_csv_with(existing_csv, DEFAULT_CAPABILITIES)
}

/// Union an existing capabilities CSV with a target floor set.
///
/// Preserves existing order and any non-floor capabilities; appends only
/// missing floor entries. Returns the new CSV and the wire strings that were added.
pub fn union_capabilities_csv_with(
    existing_csv: &str,
    floor: &[Capability],
) -> Result<(String, Vec<&'static str>)> {
    let mut present = BTreeSet::new();
    let mut ordered = Vec::new();
    for part in existing_csv.split(',') {
        let trimmed = part.trim();
        if trimmed.is_empty() {
            continue;
        }
        let capability = parse_capability(trimmed)?;
        if present.insert(capability) {
            ordered.push(capability);
        }
    }
    let mut added = Vec::new();
    for &capability in floor {
        if present.insert(capability) {
            ordered.push(capability);
            added.push(capability.as_str());
        }
    }
    let csv = ordered
        .iter()
        .map(|capability| capability.as_str())
        .collect::<Vec<_>>()
        .join(",");
    Ok((csv, added))
}

/// Rewrite `bootstrap.env` capabilities when the file is missing any entry
/// from the preset's floor. Preserves token, principal, expiry, and the
/// preset marker. No-op when the path does not exist.
pub fn heal_bootstrap_env_file(path: &Path) -> Result<HealBootstrapReport> {
    if !path.exists() {
        return Ok(HealBootstrapReport::default());
    }
    let preset = read_preset_marker_from_file(path)?.unwrap_or(BootstrapPreset::Unrestricted);
    let floor = capabilities_for_preset(preset);
    let fields = read_bootstrap_env_fields(path)?;
    let token = fields.token.with_context(|| {
        format!(
            "bootstrap env {} missing required key {ENV_TOKEN}",
            path.display()
        )
    })?;
    let principal = fields.principal.with_context(|| {
        format!(
            "bootstrap env {} missing required key {ENV_PRINCIPAL}",
            path.display()
        )
    })?;
    let capabilities = fields.capabilities.with_context(|| {
        format!(
            "bootstrap env {} missing required key {ENV_CAPABILITIES}",
            path.display()
        )
    })?;
    let expires_at = fields.expires_at.with_context(|| {
        format!(
            "bootstrap env {} missing required key {ENV_EXPIRES_AT}",
            path.display()
        )
    })?;
    let (healed_csv, added) = union_capabilities_csv_with(&capabilities, floor)?;
    if added.is_empty() {
        return Ok(HealBootstrapReport::default());
    }
    let export = if bootstrap_env_uses_export_prefix(path)? {
        "export "
    } else {
        ""
    };
    let contents = format!(
        "# {PRESET_MARKER} {}\n{export}{ENV_TOKEN}={token}\n{export}{ENV_PRINCIPAL}={principal}\n{export}{ENV_CAPABILITIES}={healed_csv}\n{export}{ENV_EXPIRES_AT}={expires_at}\n",
        preset.as_str(),
    );
    write_private_file(path, contents.as_bytes())
        .with_context(|| format!("failed to heal bootstrap env at {}", path.display()))?;
    Ok(HealBootstrapReport {
        added,
        file_rewritten: true,
        env_updated: false,
    })
}

/// Expand process-env bootstrap capabilities to include the preset's floor.
///
/// No-op when `AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES` is unset.
/// Preset comes from `AUTOMATION_RUNTIME_BOOTSTRAP_PRESET` (default unrestricted).
pub fn heal_process_env_capabilities() -> Result<HealBootstrapReport> {
    let Ok(existing) = std::env::var(ENV_CAPABILITIES) else {
        return Ok(HealBootstrapReport::default());
    };
    let preset = std::env::var(ENV_PRESET)
        .ok()
        .and_then(|raw| BootstrapPreset::parse(&raw))
        .unwrap_or(BootstrapPreset::Unrestricted);
    let (healed_csv, added) = union_capabilities_csv_with(&existing, capabilities_for_preset(preset))?;
    if added.is_empty() {
        return Ok(HealBootstrapReport::default());
    }
    // Caps only enter this process's environment; never printed.
    unsafe { std::env::set_var(ENV_CAPABILITIES, &healed_csv) };
    Ok(HealBootstrapReport {
        added,
        file_rewritten: false,
        env_updated: true,
    })
}

/// Heal bootstrap.env (when present) and process env so local installs stay
/// current as defaults grow. Agent presets never gain `authority:admin`.
/// Call before credential load on serve, mcp-stdio, and doctor.
///
/// Also heals `~/.config/bobby-browser/bootstrap.env` when that file exists and
/// differs from `path`. Launchd wrappers on macOS often `source` the XDG-style
/// path while `dirs::config_dir()` resolves to Application Support.
pub fn ensure_unrestricted_bootstrap(path: &Path) -> Result<HealBootstrapReport> {
    let mut report = heal_bootstrap_env_file(path)?;
    if let Some(legacy) = xdg_config_bootstrap_path() {
        if legacy != *path {
            report.merge(heal_bootstrap_env_file(&legacy)?);
        }
    }
    report.merge(heal_process_env_capabilities()?);
    Ok(report)
}

fn xdg_config_bootstrap_path() -> Option<PathBuf> {
    let home = dirs::home_dir()?;
    Some(
        home.join(".config")
            .join("bobby-browser")
            .join("bootstrap.env"),
    )
}

struct BootstrapEnvFields {
    token: Option<String>,
    principal: Option<String>,
    capabilities: Option<String>,
    expires_at: Option<String>,
}

/// Read all four bootstrap variables from a dotenv file as a map, for handing
/// to a child process (the doctor MCP handshake spawns the gateway with them).
/// Never log or print the returned values.
pub fn load_bootstrap_env_map(path: &Path) -> Result<std::collections::BTreeMap<String, String>> {
    let fields = read_bootstrap_env_fields(path)?;
    let mut map = std::collections::BTreeMap::new();
    let mut insert = |key: &'static str, value: Option<String>| -> Result<()> {
        let value = value.with_context(|| {
            format!(
                "bootstrap env {} missing required key {key}",
                path.display()
            )
        })?;
        map.insert(key.to_owned(), value);
        Ok(())
    };
    insert(ENV_TOKEN, fields.token)?;
    insert(ENV_PRINCIPAL, fields.principal)?;
    insert(ENV_CAPABILITIES, fields.capabilities)?;
    insert(ENV_EXPIRES_AT, fields.expires_at)?;
    Ok(map)
}

fn read_bootstrap_env_fields(path: &Path) -> Result<BootstrapEnvFields> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read bootstrap env from {}", path.display()))?;
    let mut fields = BootstrapEnvFields {
        token: None,
        principal: None,
        capabilities: None,
        expires_at: None,
    };
    for (line_number, line) in contents.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            // Report the line number, never the content: a malformed
            // credential line still carries the bearer.
            return Err(anyhow!(
                "invalid bootstrap env line {} in {}",
                line_number + 1,
                path.display()
            ));
        };
        // Accept both `KEY=value` and shell-sourced `export KEY=value`.
        let key = key.trim().strip_prefix("export ").unwrap_or(key).trim();
        match key {
            ENV_TOKEN => fields.token = Some(value.to_owned()),
            ENV_PRINCIPAL => fields.principal = Some(value.to_owned()),
            ENV_CAPABILITIES => fields.capabilities = Some(value.to_owned()),
            ENV_EXPIRES_AT => fields.expires_at = Some(value.to_owned()),
            _ => {}
        }
    }
    Ok(fields)
}

/// True when the bootstrap dotenv uses shell `export KEY=` lines (common for
/// launchd `source` wrappers). Heal rewrites preserve that style.
fn bootstrap_env_uses_export_prefix(path: &Path) -> Result<bool> {
    let contents = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read bootstrap env from {}", path.display()))?;
    Ok(contents.lines().any(|line| {
        let line = line.trim();
        line.starts_with("export ") && line.contains('=')
    }))
}

fn write_private_file(path: &Path, contents: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(contents)?;
    file.flush()?;
    file.sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn generate_bootstrap_meets_bearer_rules() {
        let material = generate_bootstrap(chrono::Duration::days(DEFAULT_TTL_DAYS)).unwrap();
        assert!(material.bearer().len() >= 32);
        assert!(material.capabilities_csv().contains("authority:admin"));
        assert!(material.capabilities_csv().contains("session:read"));
        assert!(material.capabilities_csv().contains("context:read"));
        assert!(material.capabilities_csv().contains("job:submit"));
        assert!(material.capabilities_csv().contains("job:read"));
        assert!(material.capabilities_csv().contains("job:cancel"));
        assert_eq!(material.preset(), BootstrapPreset::Unrestricted);
    }

    #[test]
    fn agent_preset_omits_authority_admin() {
        let material =
            generate_bootstrap_for_preset(chrono::Duration::days(1), BootstrapPreset::Agent)
                .unwrap();
        assert!(!material.capabilities_csv().contains("authority:admin"));
        assert!(material.capabilities_csv().contains("intent:execute"));
        assert!(material.capabilities_csv().contains("context:read"));
        assert_eq!(material.preset(), BootstrapPreset::Agent);
        let dir = tempdir().unwrap();
        let path = dir.path().join("bootstrap.env");
        write_bootstrap_env(&path, &material, false).unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("# bobby-bootstrap-preset: agent"));
        assert_eq!(
            read_preset_marker_from_file(&path).unwrap(),
            Some(BootstrapPreset::Agent)
        );
    }

    #[test]
    fn agent_heal_never_adds_authority_admin() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bootstrap.env");
        let material =
            generate_bootstrap_for_preset(chrono::Duration::days(1), BootstrapPreset::Agent)
                .unwrap();
        // Stale agent set missing context:read and without authority:admin.
        let stale = "session:read,session:write,page:read,page:write,browser:mutate,intent:execute,vision:assist,artifact:read,artifact:capture,recovery:read,recovery:write";
        let contents = format!(
            "# bobby-bootstrap-preset: agent\n{ENV_TOKEN}={}\n{ENV_PRINCIPAL}={}\n{ENV_CAPABILITIES}={stale}\n{ENV_EXPIRES_AT}={}\n",
            material.bearer(),
            material.principal_id().as_uuid(),
            material.expires_at().to_rfc3339(),
        );
        write_private_file(&path, contents.as_bytes()).unwrap();
        let report = ensure_unrestricted_bootstrap(&path).unwrap();
        assert!(report.file_rewritten);
        assert!(report.added.contains(&"context:read"));
        let caps = load_bootstrap_capabilities_csv(&path).unwrap();
        assert!(caps.contains("context:read"));
        assert!(!caps.split(',').any(|c| c.trim() == "authority:admin"));
    }

    #[test]
    fn default_capabilities_include_job_ops() {
        let caps: Vec<_> = DEFAULT_CAPABILITIES
            .iter()
            .map(|capability| capability.as_str())
            .collect();
        assert!(caps.contains(&"job:submit"));
        assert!(caps.contains(&"job:read"));
        assert!(caps.contains(&"job:cancel"));
        assert_eq!(
            parse_capability("job:submit").unwrap(),
            Capability::JobSubmit
        );
        assert_eq!(parse_capability("job:read").unwrap(), Capability::JobRead);
        assert_eq!(
            parse_capability("job:cancel").unwrap(),
            Capability::JobCancel
        );
    }

    #[test]
    fn write_refuses_existing_without_force() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bootstrap.env");
        let material = generate_bootstrap(chrono::Duration::days(1)).unwrap();
        write_bootstrap_env(&path, &material, false).unwrap();
        let err = write_bootstrap_env(&path, &material, false).unwrap_err();
        assert!(err.to_string().contains("already exists"));
    }

    #[test]
    fn write_force_overwrites_and_load_succeeds() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bootstrap.env");
        let first = generate_bootstrap(chrono::Duration::days(1)).unwrap();
        write_bootstrap_env(&path, &first, false).unwrap();
        let second = generate_bootstrap(chrono::Duration::days(1)).unwrap();
        write_bootstrap_env(&path, &second, true).unwrap();
        load_startup_from_env_file(&path).unwrap();
    }

    #[test]
    #[cfg(unix)]
    fn force_overwrite_restores_private_mode() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let path = dir.path().join("bootstrap.env");
        std::fs::write(&path, b"old content\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o644
        );

        let material = generate_bootstrap(chrono::Duration::days(1)).unwrap();
        write_bootstrap_env(&path, &material, true).unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn loopback_hosts() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("0.0.0.0"));
        assert!(!is_loopback_host("192.168.1.1"));
    }

    #[test]
    fn debug_redacts_bearer() {
        let material = generate_bootstrap(chrono::Duration::days(1)).unwrap();
        let rendered = format!("{material:?}");
        assert!(!rendered.contains(material.bearer()));
        assert!(rendered.contains("REDACTED"));
    }

    #[test]
    fn corrupt_file_errors_with_path() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bootstrap.env");
        std::fs::write(&path, "NOT_A_VALID=file\n").unwrap();
        let err = load_startup_from_env_file(&path).unwrap_err();
        assert!(err
            .to_string()
            .contains(path.display().to_string().as_str()));
    }

    #[test]
    fn resolve_prefers_process_env() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bootstrap.env");
        let file_material = generate_bootstrap(chrono::Duration::days(1)).unwrap();
        write_bootstrap_env(&path, &file_material, false).unwrap();
        let env_material = generate_bootstrap(chrono::Duration::days(1)).unwrap();
        let env_cred = broker::StartupCredential::new(
            env_material.bearer().to_string(),
            env_material.principal_id().clone(),
            DEFAULT_CAPABILITIES.to_vec(),
            env_material.expires_at(),
        )
        .unwrap();
        let outcome = resolve_startup_credential_with("127.0.0.1", &path, || Ok(env_cred)).unwrap();
        assert!(matches!(outcome, ResolveOutcome::FromEnv(_)));
    }

    #[test]
    fn resolve_loads_file_when_env_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bootstrap.env");
        let material = generate_bootstrap(chrono::Duration::days(1)).unwrap();
        write_bootstrap_env(&path, &material, false).unwrap();
        let outcome = resolve_startup_credential_with("127.0.0.1", &path, || {
            Err(broker::StartupCredentialError::MissingInput)
        })
        .unwrap();
        assert!(matches!(outcome, ResolveOutcome::FromFile(_)));
    }

    #[test]
    fn resolve_autogens_on_loopback_when_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bootstrap.env");
        assert!(!path.exists());
        let outcome = resolve_startup_credential_with("127.0.0.1", &path, || {
            Err(broker::StartupCredentialError::MissingInput)
        })
        .unwrap();
        assert!(matches!(outcome, ResolveOutcome::Generated { .. }));
        assert!(path.exists());
    }

    #[test]
    fn resolve_errors_on_non_loopback_when_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bootstrap.env");
        let err = resolve_startup_credential_with("0.0.0.0", &path, || {
            Err(broker::StartupCredentialError::MissingInput)
        })
        .unwrap_err();
        assert!(err.to_string().contains("bobby init"));
    }

    #[test]
    fn resolve_errors_on_invalid_env() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bootstrap.env");
        let material = generate_bootstrap(chrono::Duration::days(1)).unwrap();
        write_bootstrap_env(&path, &material, false).unwrap();
        let err = resolve_startup_credential_with("127.0.0.1", &path, || {
            Err(broker::StartupCredentialError::InvalidPrincipal)
        })
        .unwrap_err();
        assert!(err
            .downcast_ref::<broker::StartupCredentialError>()
            .is_some_and(|error| {
                matches!(error, broker::StartupCredentialError::InvalidPrincipal)
            }));
    }

    #[test]
    fn union_capabilities_csv_is_additive_and_idempotent() {
        let stale = "authority:admin,session:read,session:write,page:read,page:write,browser:mutate,file:upload,file:download,artifact:read,artifact:capture,recovery:read,recovery:write";
        let (healed, added) = union_capabilities_csv(stale).unwrap();
        assert!(added.contains(&"browser:fingerprint"));
        assert!(added.contains(&"browser:humanize"));
        assert!(added.contains(&"javascript:evaluate"));
        assert!(added.contains(&"intent:execute"));
        assert!(added.contains(&"vision:assist"));
        assert!(added.contains(&"context:read"));
        assert!(added.contains(&"job:submit"));
        assert!(healed.contains("browser:fingerprint"));
        assert!(healed.starts_with("authority:admin,session:read"));
        let (again, added_again) = union_capabilities_csv(&healed).unwrap();
        assert!(added_again.is_empty());
        assert_eq!(again, healed);
    }

    #[test]
    fn heal_bootstrap_env_file_rewrites_once() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bootstrap.env");
        let material = generate_bootstrap(chrono::Duration::days(1)).unwrap();
        // Write a deliberately stale capability set while keeping other fields.
        let stale = "authority:admin,session:read,session:write,page:read,page:write,browser:mutate,file:upload,file:download,artifact:read,artifact:capture,recovery:read,recovery:write";
        let contents = format!(
            "{ENV_TOKEN}={}\n{ENV_PRINCIPAL}={}\n{ENV_CAPABILITIES}={stale}\n{ENV_EXPIRES_AT}={}\n",
            material.bearer(),
            material.principal_id().as_uuid(),
            material.expires_at().to_rfc3339(),
        );
        write_private_file(&path, contents.as_bytes()).unwrap();

        let first = heal_bootstrap_env_file(&path).unwrap();
        assert!(first.file_rewritten);
        assert!(first.added.contains(&"browser:fingerprint"));
        let caps = load_bootstrap_capabilities_csv(&path).unwrap();
        assert!(caps.contains("browser:fingerprint"));
        assert!(caps.contains("job:submit"));
        // Token preserved.
        let loaded = load_startup_from_env_file(&path).unwrap();
        assert_eq!(loaded.expires_at(), material.expires_at());

        let second = heal_bootstrap_env_file(&path).unwrap();
        assert!(!second.changed());
        assert_eq!(load_bootstrap_capabilities_csv(&path).unwrap(), caps);
    }

    #[test]
    fn heal_bootstrap_env_file_noop_when_missing() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("missing.env");
        let report = heal_bootstrap_env_file(&path).unwrap();
        assert!(!report.changed());
    }

    #[test]
    fn ensure_unrestricted_bootstrap_heals_file_before_load() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bootstrap.env");
        let material = generate_bootstrap(chrono::Duration::days(1)).unwrap();
        let stale = "session:read,session:write,page:read,page:write,browser:mutate,artifact:read,artifact:capture,recovery:read,recovery:write,authority:admin";
        let contents = format!(
            "{ENV_TOKEN}={}\n{ENV_PRINCIPAL}={}\n{ENV_CAPABILITIES}={stale}\n{ENV_EXPIRES_AT}={}\n",
            material.bearer(),
            material.principal_id().as_uuid(),
            material.expires_at().to_rfc3339(),
        );
        write_private_file(&path, contents.as_bytes()).unwrap();

        let report = ensure_unrestricted_bootstrap(&path).unwrap();
        assert!(report.file_rewritten);
        let caps = load_bootstrap_capabilities_csv(&path).unwrap();
        assert!(caps.split(',').any(|c| c.trim() == "browser:fingerprint"));
        load_startup_from_env_file(&path).unwrap();
    }

    #[test]
    fn heal_preserves_export_prefix_style() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bootstrap.env");
        let material = generate_bootstrap(chrono::Duration::days(1)).unwrap();
        let stale = "session:read,session:write,page:read,page:write,browser:mutate,artifact:read,artifact:capture,recovery:read,recovery:write,authority:admin";
        let contents = format!(
            "export {ENV_TOKEN}={}\nexport {ENV_PRINCIPAL}={}\nexport {ENV_CAPABILITIES}={stale}\nexport {ENV_EXPIRES_AT}={}\n",
            material.bearer(),
            material.principal_id().as_uuid(),
            material.expires_at().to_rfc3339(),
        );
        write_private_file(&path, contents.as_bytes()).unwrap();
        let report = heal_bootstrap_env_file(&path).unwrap();
        assert!(report.file_rewritten);
        let rewritten = std::fs::read_to_string(&path).unwrap();
        assert!(rewritten.lines().all(|line| {
            line.trim().is_empty()
                || line.trim().starts_with("export ")
                || line.trim().starts_with('#')
        }));
        assert!(rewritten.contains("browser:fingerprint"));
        load_startup_from_env_file(&path).unwrap();
    }

    #[test]
    fn read_accepts_export_prefixed_keys() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("bootstrap.env");
        let material = generate_bootstrap(chrono::Duration::days(1)).unwrap();
        let contents = format!(
            "export {ENV_TOKEN}={}\nexport {ENV_PRINCIPAL}={}\nexport {ENV_CAPABILITIES}={}\nexport {ENV_EXPIRES_AT}={}\n",
            material.bearer(),
            material.principal_id().as_uuid(),
            material.capabilities_csv(),
            material.expires_at().to_rfc3339(),
        );
        write_private_file(&path, contents.as_bytes()).unwrap();
        load_startup_from_env_file(&path).unwrap();
    }
}
