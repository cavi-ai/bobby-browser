use std::sync::Arc;

use artifact_store::ArtifactStore;
use chrono::{DateTime, Utc};
use config::AppConfig;
use interface_core::{
    ArtifactOwnershipLimits, ArtifactReader, AuthorityStore, EventStore, SessionOwnershipRegistry,
};
use mcp_gateway::{ArtifactResources, Server};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use sha2::{Digest, Sha256};
use types::{Capability, PrincipalId};
use uuid::Uuid;

const MIN_BEARER_BYTES: usize = 32;
const MAX_BEARER_BYTES: usize = 505;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("mcp gateway failed: {error}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let handle = explicit_startup_handle().await?;
    let config = AppConfig::default();
    config.validate().map_err(anyhow::Error::msg)?;
    let runtime = RuntimeService::build(&config)
        .await
        .map_err(anyhow::Error::new)?;
    if !handle.is_valid_at(Utc::now()) {
        anyhow::bail!("startup credential expired during runtime construction");
    }
    let artifact_records = config.interface.max_event_retention;
    let artifact_max_bytes = config
        .browser
        .max_artifact_bytes
        .max(config.http.max_download_bytes);
    let artifact_bytes = u64::try_from(artifact_max_bytes)?
        .checked_mul(u64::try_from(artifact_records)?)
        .ok_or_else(|| anyhow::anyhow!("artifact ownership byte bound overflow"))?;
    let (ownership, recorder) = SessionOwnershipRegistry::bounded(config.browser.max_active);
    let artifact_store = ArtifactStore::new(
        config.browser.artifacts_dir.clone(),
        artifact_max_bytes,
        config.browser.max_screenshot_dimension,
    );
    let artifact_reader = ArtifactReader::new(
        artifact_store.clone(),
        ownership,
        artifact_max_bytes,
        ArtifactOwnershipLimits {
            max_records: artifact_records,
            max_bytes: artifact_bytes,
        },
    )?;
    let resources = ArtifactResources::production(
        artifact_reader,
        artifact_store,
        config.browser.downloads_dir.clone(),
        config.http.max_download_bytes,
        artifact_records,
    );
    let authenticated = Arc::new(AuthenticatedRuntime::with_session_ownership(
        runtime,
        handle.clone(),
        recorder,
    ));
    Server::production(
        authenticated,
        EventStore::new(config.interface.max_event_retention),
        resources,
    )
    .serve(tokio::io::stdin(), tokio::io::stdout())
    .await?;
    Ok(())
}

async fn explicit_startup_handle() -> anyhow::Result<interface_core::CapabilityHandle> {
    let token_hash = {
        let bearer = required_env("AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN")?;
        if !(MIN_BEARER_BYTES..=MAX_BEARER_BYTES).contains(&bearer.len())
            || !bearer.bytes().all(|byte| (0x21..=0x7e).contains(&byte))
        {
            anyhow::bail!("startup bearer is invalid");
        }
        <[u8; 32]>::from(Sha256::digest(bearer.as_bytes()))
    };
    let principal = required_env("AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL")?;
    let principal = PrincipalId::from_uuid(
        Uuid::parse_str(&principal).map_err(|_| anyhow::anyhow!("startup principal is invalid"))?,
    );
    let capabilities = required_env("AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES")?
        .split(',')
        .map(str::trim)
        .map(parse_capability)
        .collect::<anyhow::Result<Vec<_>>>()?;
    if capabilities.is_empty() {
        anyhow::bail!("startup capabilities must be explicit and nonempty");
    }
    let expires_at =
        DateTime::parse_from_rfc3339(&required_env("AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT")?)
            .map_err(|_| anyhow::anyhow!("startup expiry is invalid"))?
            .with_timezone(&Utc);
    if expires_at <= Utc::now() {
        anyhow::bail!("startup credential is expired");
    }
    AuthorityStore::with_capacity(1)
        .enroll_hash(token_hash, principal, capabilities, expires_at)
        .await
        .map_err(|_| anyhow::anyhow!("startup authority enrollment failed"))
}

fn required_env(name: &'static str) -> anyhow::Result<String> {
    std::env::var(name).map_err(|_| anyhow::anyhow!("explicit startup authority input is required"))
}

fn parse_capability(value: &str) -> anyhow::Result<Capability> {
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
        "job:submit" => Ok(Capability::JobSubmit),
        "job:read" => Ok(Capability::JobRead),
        "job:cancel" => Ok(Capability::JobCancel),
        "authority:admin" => Ok(Capability::AuthorityAdmin),
        "browser:fingerprint" => Ok(Capability::BrowserFingerprint),
        "browser:humanize" => Ok(Capability::BrowserHumanize),
        _ => anyhow::bail!("startup capability is invalid"),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::LazyLock;

    use chrono::{Duration, SecondsFormat};
    use tokio::sync::Mutex;

    use super::*;

    static ENVIRONMENT: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));
    const SECRET: &str = "production-bootstrap-secret-000000000000000000000";

    #[tokio::test(flavor = "current_thread")]
    async fn startup_credential_is_immediately_reduced_to_a_redacted_handle() {
        let _guard = ENVIRONMENT.lock().await;
        std::env::set_var("AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN", SECRET);
        std::env::set_var(
            "AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL",
            "10000000-0000-0000-0000-000000000046",
        );
        std::env::set_var(
            "AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES",
            "session:read,artifact:read",
        );
        std::env::set_var(
            "AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT",
            (Utc::now() + Duration::hours(1)).to_rfc3339_opts(SecondsFormat::Secs, true),
        );

        let handle = explicit_startup_handle().await.unwrap();
        let diagnostic = format!("{handle:?}");
        assert!(handle.is_valid_at(Utc::now()));
        assert!(!diagnostic.contains(SECRET));

        for name in [
            "AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN",
            "AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL",
            "AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES",
            "AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT",
        ] {
            std::env::remove_var(name);
        }
    }

    #[test]
    fn startup_capability_parser_accepts_only_the_canonical_wire_vocabulary() {
        assert!(matches!(
            parse_capability("browser:mutate"),
            Ok(Capability::BrowserMutate)
        ));
        assert!(parse_capability("browser:* ").is_err());
    }
}
