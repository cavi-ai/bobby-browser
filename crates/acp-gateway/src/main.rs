//! ACP stdio gateway binary: enrolls the startup bootstrap credential, builds
//! the runtime in-process, and serves ACP on stdin/stdout. Same startup
//! contract as `mcp-gateway` — all four `AUTOMATION_RUNTIME_BOOTSTRAP_*`
//! variables, fail closed.

use std::sync::Arc;

use acp_gateway::AcpServer;
use chrono::{DateTime, Utc};
use config::AppConfig;
use interface_core::AuthorityStore;
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use sha2::{Digest, Sha256};
use types::{Capability, PrincipalId};
use uuid::Uuid;

const MIN_BEARER_BYTES: usize = 32;
const MAX_BEARER_BYTES: usize = 505;

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("acp gateway failed: {error}");
        std::process::exit(1);
    }
}

async fn run() -> anyhow::Result<()> {
    let (handle, capabilities) = explicit_startup_handle().await?;
    let config = AppConfig::default();
    config.validate().map_err(anyhow::Error::msg)?;
    let runtime = RuntimeService::build(&config)
        .await
        .map_err(anyhow::Error::new)?;
    if !handle.is_valid_at(Utc::now()) {
        anyhow::bail!("startup credential expired during runtime construction");
    }
    let authenticated = Arc::new(AuthenticatedRuntime::new(runtime, handle));
    AcpServer::new(authenticated, capabilities).serve().await?;
    Ok(())
}

async fn explicit_startup_handle(
) -> anyhow::Result<(interface_core::CapabilityHandle, Vec<Capability>)> {
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
    let handle = AuthorityStore::with_capacity(1)
        .enroll_hash(token_hash, principal, capabilities.clone(), expires_at)
        .await
        .map_err(|_| anyhow::anyhow!("startup authority enrollment failed"))?;
    Ok((handle, capabilities))
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
