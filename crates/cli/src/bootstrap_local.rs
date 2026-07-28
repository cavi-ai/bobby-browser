use std::fmt;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use broker::StartupCredential;
use chrono::{DateTime, Duration, Utc};
use types::{Capability, PrincipalId};
use uuid::Uuid;

pub const DEFAULT_TTL_DAYS: i64 = 30;

const ENV_TOKEN: &str = "AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN";
const ENV_PRINCIPAL: &str = "AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL";
const ENV_CAPABILITIES: &str = "AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES";
const ENV_EXPIRES_AT: &str = "AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT";

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
    Capability::ArtifactRead,
    Capability::ArtifactCapture,
    Capability::RecoveryRead,
    Capability::RecoveryWrite,
    Capability::AuthorityAdmin,
];

pub struct BootstrapMaterial {
    bearer: String,
    principal_id: PrincipalId,
    capabilities_csv: String,
    expires_at: DateTime<Utc>,
}

impl fmt::Debug for BootstrapMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("BootstrapMaterial")
            .field("bearer", &"REDACTED")
            .field("principal_id", &self.principal_id)
            .field("capabilities_csv", &self.capabilities_csv)
            .field("expires_at", &self.expires_at)
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
}

pub fn default_bootstrap_path() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("config directory unavailable")?
        .join("bobby-browser")
        .join("bootstrap.env"))
}

pub fn generate_bootstrap(ttl: Duration) -> Result<BootstrapMaterial> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).context("failed to generate bootstrap bearer entropy")?;
    let bearer = hex::encode(bytes);
    let principal_id = PrincipalId::from_uuid(Uuid::new_v4());
    let capabilities_csv = DEFAULT_CAPABILITIES
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
        "{ENV_TOKEN}={}\n{ENV_PRINCIPAL}={}\n{ENV_CAPABILITIES}={}\n{ENV_EXPIRES_AT}={}\n",
        material.bearer(),
        material.principal_id().as_uuid(),
        material.capabilities_csv(),
        material.expires_at().to_rfc3339(),
    );
    write_private_file(path, contents.as_bytes())
        .with_context(|| format!("failed to write bootstrap env to {}", path.display()))
}

pub fn load_startup_from_env_file(path: &Path) -> Result<StartupCredential> {
    let contents = std::fs::read_to_string(path).with_context(|| {
        format!(
            "failed to read bootstrap env from {}",
            path.display()
        )
    })?;
    let mut token = None;
    let mut principal = None;
    let mut capabilities = None;
    let mut expires_at = None;
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            return Err(anyhow!(
                "invalid bootstrap env line in {}: {line}",
                path.display()
            ));
        };
        match key {
            ENV_TOKEN => token = Some(value.to_owned()),
            ENV_PRINCIPAL => principal = Some(value.to_owned()),
            ENV_CAPABILITIES => capabilities = Some(value.to_owned()),
            ENV_EXPIRES_AT => expires_at = Some(value.to_owned()),
            _ => {}
        }
    }
    let bearer = token.with_context(|| {
        format!(
            "bootstrap env {} missing required key {ENV_TOKEN}",
            path.display()
        )
    })?;
    let principal = principal.with_context(|| {
        format!(
            "bootstrap env {} missing required key {ENV_PRINCIPAL}",
            path.display()
        )
    })?;
    let capabilities = capabilities.with_context(|| {
        format!(
            "bootstrap env {} missing required key {ENV_CAPABILITIES}",
            path.display()
        )
    })?;
    let expires_at = expires_at.with_context(|| {
        format!(
            "bootstrap env {} missing required key {ENV_EXPIRES_AT}",
            path.display()
        )
    })?;
    let principal_id = PrincipalId::from_uuid(
        Uuid::parse_str(&principal).with_context(|| {
            format!(
                "bootstrap env {} has invalid principal {principal}",
                path.display()
            )
        })?,
    );
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
    if let Ok(credential) = from_env() {
        return Ok(ResolveOutcome::FromEnv(credential));
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
    match value {
        "session:read" => Ok(Capability::SessionRead),
        "session:write" => Ok(Capability::SessionWrite),
        "page:read" => Ok(Capability::PageRead),
        "page:write" => Ok(Capability::PageWrite),
        "browser:mutate" => Ok(Capability::BrowserMutate),
        "file:upload" => Ok(Capability::FileUpload),
        "file:download" => Ok(Capability::FileDownload),
        "javascript:evaluate" => Ok(Capability::JavascriptEvaluate),
        "intent:execute" => Ok(Capability::IntentExecute),
        "vision:assist" => Ok(Capability::VisionAssist),
        "artifact:read" => Ok(Capability::ArtifactRead),
        "artifact:capture" => Ok(Capability::ArtifactCapture),
        "recovery:read" => Ok(Capability::RecoveryRead),
        "recovery:write" => Ok(Capability::RecoveryWrite),
        "authority:admin" => Ok(Capability::AuthorityAdmin),
        _ => Err(anyhow!("unknown capability: {value}")),
    }
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
        assert!(err.to_string().contains(path.display().to_string().as_str()));
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
        let outcome =
            resolve_startup_credential_with("127.0.0.1", &path, || Ok(env_cred)).unwrap();
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
}
