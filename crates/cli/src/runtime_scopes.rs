//! One local runtime owner per team/project scope, shared by all adapters.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{File, OpenOptions},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::Duration,
};
use uuid::Uuid;

#[derive(Clone, Debug, Default, clap::Args, Serialize, Deserialize)]
pub(crate) struct Scope {
    /// Share Bobby's profile and runtime with this local team.
    #[arg(long, global = true)]
    pub team: Option<String>,
    /// Share a project runtime (optionally within --team).
    #[arg(long, global = true)]
    pub project: Option<String>,
}

#[derive(clap::Subcommand)]
pub(crate) enum RuntimeCommand {
    /// Start or reuse this scope's runtime.
    Start,
    /// Show this scope's owner and connection URL.
    Status,
    /// Gracefully stop this scope's runtime.
    Stop,
    /// List organized local scopes and their runtime status.
    List,
}

#[derive(Serialize, Deserialize)]
struct Owner {
    owner_id: Uuid,
    pid: u32,
    url: String,
    config: PathBuf,
    bootstrap: PathBuf,
    configuration_digest: String,
    vision_policy: String,
    stop_secret: String,
}

fn user_root() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("config directory unavailable")?
        .join("bobby-browser"))
}

impl Scope {
    fn root_at(&self, root: &Path) -> Result<PathBuf> {
        for name in [&self.team, &self.project].into_iter().flatten() {
            if name.is_empty()
                || name.len() > 64
                || !name
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'))
            {
                bail!(
                    "team and project names must be 1–64 letters, digits, hyphens, or underscores"
                );
            }
        }
        let mut path = root.to_path_buf();
        if let Some(team) = &self.team {
            path = path.join("teams").join(team);
        }
        if let Some(project) = &self.project {
            path = path.join("projects").join(project);
        }
        Ok(path)
    }

    pub fn apply(&self) -> Result<()> {
        if self.team.is_none() && self.project.is_none() {
            return Ok(());
        }
        let root = self.root_at(&user_root()?)?;
        private_dir(&root)?;
        write_json(&root.join("scope.json"), self)?;
        ensure_config(&root.join("config.toml"), &root)?;
        // CLI startup is single-threaded with respect to environment mutation,
        // matching the existing bootstrap/config startup contract.
        unsafe {
            std::env::set_var("BOBBY_BROWSER_SCOPE_DIR", &root);
            std::env::set_var("BOBBY_BROWSER_CONFIG", root.join("config.toml"));
            std::env::set_var("BOBBY_BROWSER_BOOTSTRAP_ENV", root.join("bootstrap.env"));
        }
        Ok(())
    }
}

pub(crate) fn gateway_args(subcommand: &str) -> Result<Vec<String>> {
    let mut args = vec![subcommand.to_owned()];
    if let Some(root) = std::env::var_os("BOBBY_BROWSER_SCOPE_DIR") {
        let scope: Scope =
            serde_json::from_slice(&std::fs::read(PathBuf::from(root).join("scope.json"))?)?;
        if let Some(team) = scope.team {
            args.extend(["--team".into(), team]);
        }
        if let Some(project) = scope.project {
            args.extend(["--project".into(), project]);
        }
    }
    Ok(args)
}

pub(crate) fn native_host_name(root: &Path) -> Result<String> {
    if root == user_root()? {
        return Ok("com.bobby_browser.companion".into());
    }
    let hash = hex::encode(Sha256::digest(root.as_os_str().as_encoded_bytes()));
    Ok(format!("com.bobby_browser.companion.scope_{}", &hash[..16]))
}

fn private_dir(path: &Path) -> Result<()> {
    std::fs::create_dir_all(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

fn private_options() -> OpenOptions {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    options
}

fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    use std::io::Write;
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = private_options().open(&temporary)?;
        file.write_all(&serde_json::to_vec_pretty(value)?)?;
        file.sync_all()?;
        std::fs::rename(&temporary, path)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(temporary);
    }
    result
}

fn ensure_config(path: &Path, root: &Path) -> Result<()> {
    if path.exists() {
        return Ok(());
    }
    let mut config = config::AppConfig::default();
    config.browser.profiles_dir = root.join("profiles");
    config.browser.artifacts_dir = root.join("artifacts");
    config.browser.downloads_dir = root.join("downloads");
    config.browser.upload_roots = vec![root.join("uploads")];
    config.storage.journal_path = root.join("storage/commands.jsonl");
    config.storage.checkpoints_dir = root.join("storage/checkpoints");
    config.storage.authority_path = root.join("storage/authority.json");
    config.storage.scheduler_journal_path = root.join("storage/scheduler-jobs.jsonl");
    config.context.dir = Some(root.join("context"));
    use std::io::Write;
    let temporary = path.with_extension(format!("{}.tmp", Uuid::new_v4()));
    let result = (|| {
        let mut file = private_options().open(&temporary)?;
        file.write_all(toml::to_string_pretty(&config)?.as_bytes())?;
        file.sync_all()?;
        // Publish complete contents without replacing another launch's config.
        match std::fs::hard_link(&temporary, path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error.into()),
        }
    })();
    let _ = std::fs::remove_file(temporary);
    result
}

pub(crate) fn runtime_dir() -> Result<PathBuf> {
    let root = config::bobby_config_dir().context("config directory unavailable")?;
    let path = root.join("runtime");
    private_dir(&path)?;
    Ok(path)
}

fn absolute(path: PathBuf) -> Result<PathBuf> {
    Ok(if path.is_absolute() {
        path
    } else {
        std::env::current_dir()?.join(path)
    })
}

pub(crate) fn default_config_path() -> Option<PathBuf> {
    config::bobby_config_dir().map(|root| root.join("config.toml"))
}

/// Resolve a live registry generation before sending any authenticated CLI
/// request to its dynamically assigned port.
pub(crate) fn current_origin() -> Option<String> {
    let dir = config::bobby_config_dir()?.join("runtime");
    let owner: Owner = serde_json::from_slice(&std::fs::read(dir.join("owner.json")).ok()?).ok()?;
    let url = url::Url::parse(&owner.url).ok()?;
    if url.scheme() != "http"
        || !url
            .host_str()?
            .parse::<std::net::IpAddr>()
            .ok()?
            .is_loopback()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return None;
    }
    std::thread::spawn(move || {
        let client = reqwest::blocking::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(Duration::from_secs(2))
            .build()
            .ok()?;
        let value: serde_json::Value = client
            .get(format!("{}/_bobby/runtime", owner.url))
            .send()
            .ok()?
            .json()
            .ok()?;
        (value["ownerId"].as_str() == Some(owner.owner_id.to_string().as_str()))
            .then_some(owner.url)
    })
    .join()
    .ok()
    .flatten()
}

pub(crate) fn use_owner_address(config: &mut config::AppConfig) {
    if let Some(origin) = current_origin().and_then(|url| url::Url::parse(&url).ok()) {
        if let (Some(host), Some(port)) = (origin.host_str(), origin.port()) {
            config.server.host = host.to_owned();
            config.server.port = port;
        }
    }
}

fn lock_file(path: &Path) -> Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true).create(true).truncate(false);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options
            .mode(0o600)
            .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    }
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        bail!("runtime lock must be a regular file");
    }
    Ok(file)
}

fn claim(path: &Path) -> Result<File> {
    let file = lock_file(path)?;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(std::fs::TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20))
            }
            Err(error) => return Err(error.into()),
        }
    }
}

fn digest(config: &Path, bootstrap: &Path) -> Result<String> {
    let mut digest = Sha256::new();
    for path in [config, bootstrap] {
        // Resolve filesystem aliases (e.g. /var and /private/var on macOS)
        // so relative foreground paths identify the same credential/config.
        let identity = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_owned());
        digest.update(identity.as_os_str().as_encoded_bytes());
        digest.update([0]);
        match std::fs::read(path) {
            Ok(bytes) => digest.update(bytes),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound && path == bootstrap => {}
            Err(error) => return Err(error.into()),
        }
        digest.update([0]);
    }
    let file_credential = if bootstrap.exists() {
        crate::bootstrap_local::load_bootstrap_env_map(bootstrap)?
    } else {
        Default::default()
    };
    for name in [
        "AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN",
        "AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL",
        "AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES",
        "AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT",
    ] {
        if let Some(value) = std::env::var(name)
            .ok()
            .or_else(|| file_credential.get(name).cloned())
        {
            digest.update(name);
            digest.update(value);
            digest.update([0]);
        }
    }
    let (selection, _) = crate::resolve_browser_selection()?;
    digest.update(serde_json::to_vec(&selection)?);
    Ok(hex::encode(digest.finalize()))
}

fn client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(2))
        .build()?)
}

fn compatible_owner(
    owner: Owner,
    digest: &str,
    policy: crate::VisionSpawnPolicy,
) -> Result<String> {
    if owner.configuration_digest != digest
        || (policy != crate::VisionSpawnPolicy::Auto
            && owner.vision_policy != format!("{policy:?}"))
    {
        bail!("this scope is running with different configuration; run `bobby runtime stop` with the same team/project flags before restarting it");
    }
    Ok(owner.url)
}

async fn live_owner(dir: &Path) -> Result<Option<Owner>> {
    let path = dir.join("owner.json");
    let owner: Owner = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).context("invalid runtime owner registry")?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let url = url::Url::parse(&owner.url)?;
    if url.scheme() != "http"
        || !url
            .host_str()
            .and_then(|s| s.parse::<std::net::IpAddr>().ok())
            .is_some_and(|ip| ip.is_loopback())
        || url.username() != ""
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("runtime owner registry contains an invalid loopback origin");
    }
    let response = match client()?
        .get(format!("{}/_bobby/runtime", owner.url))
        .send()
        .await
    {
        Ok(response) => response,
        Err(_) => return Ok(None),
    };
    if !response.status().is_success() {
        return Ok(None);
    }
    let value: serde_json::Value = match response.json().await {
        Ok(value) => value,
        Err(_) => return Ok(None),
    };
    Ok((value["ownerId"].as_str() == Some(owner.owner_id.to_string().as_str())).then_some(owner))
}

pub(crate) async fn ensure_owner(
    config: PathBuf,
    bootstrap: PathBuf,
    policy: crate::VisionSpawnPolicy,
) -> Result<String> {
    let dir = runtime_dir()?;
    let launch = dir.join("launch.lock");
    let _launch = tokio::task::spawn_blocking(move || claim(&launch)).await??;
    let root = dir.parent().context("runtime scope root unavailable")?;
    let config = absolute(config)?;
    let bootstrap = absolute(bootstrap)?;
    ensure_config(&config, root)?;
    let loaded = config::AppConfig::load(&config)?;
    let (selection, _) = crate::resolve_browser_selection()?;
    if matches!(
        selection.preference,
        config::EnginePreferenceConfig::Exact {
            engine: config::BrowserEngineConfig::Firefox,
            ..
        }
    ) && selection.firefox.is_empty()
    {
        bail!("this scope has no enrolled Firefox profile; run `bobby install --companion`, then `bobby firefox-start` and Pair from the toolbar, using the same team/project flags");
    }
    crate::bootstrap_local::ensure_unrestricted_bootstrap(&bootstrap)?;
    crate::bootstrap_local::resolve_startup_credential_with(
        "127.0.0.1",
        &bootstrap,
        broker::StartupCredential::from_env,
    )?;
    let expected_digest = digest(&config, &bootstrap)?;
    if let Some(owner) = live_owner(&dir).await? {
        return compatible_owner(owner, &expected_digest, policy);
    }
    if loaded
        .server
        .host
        .parse::<std::net::IpAddr>()
        .is_ok_and(|host| !host.is_loopback())
    {
        bail!("shared local runtimes require a loopback bind");
    }
    let owner_lock = lock_file(&dir.join("owner.lock"))?;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        match owner_lock.try_lock() {
            Ok(()) => break,
            Err(std::fs::TryLockError::WouldBlock) => {}
            Err(error) => return Err(error.into()),
        }
        if let Some(owner) = live_owner(&dir).await? {
            return compatible_owner(owner, &digest(&config, &bootstrap)?, policy);
        }
        if tokio::time::Instant::now() >= deadline {
            bail!(
                "scope owner is busy but unavailable; see {}",
                dir.join("owner.log").display()
            );
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    {
        let mut command = Command::new(std::env::current_exe()?);
        command
            .arg("runtime-owner")
            .arg("--state-dir")
            .arg(&dir)
            .arg("--config")
            .arg(&config)
            .arg("--bootstrap-env")
            .arg(&bootstrap);
        match policy {
            crate::VisionSpawnPolicy::Off => {
                command.arg("--no-vision");
            }
            crate::VisionSpawnPolicy::ForceOn => {
                command.arg("--vision");
            }
            _ => {}
        }
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(dir.join("owner.log"))?;
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::from(log));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            unsafe {
                command.pre_exec(|| {
                    if libc::setsid() == -1 {
                        Err(std::io::Error::last_os_error())
                    } else {
                        Ok(())
                    }
                });
            }
        }
        drop(owner_lock);
        let mut child = command
            .spawn()
            .context("failed to start shared runtime owner")?;
        let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
        loop {
            if let Some(owner) = live_owner(&dir).await? {
                // Explicit vision setup may persist config during startup.
                return compatible_owner(owner, &digest(&config, &bootstrap)?, policy);
            }
            if child.try_wait()?.is_some() {
                bail!(
                    "shared runtime failed to start; see {}",
                    dir.join("owner.log").display()
                );
            }
            if tokio::time::Instant::now() >= deadline {
                bail!(
                    "shared runtime startup timed out; see {}",
                    dir.join("owner.log").display()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

pub(crate) async fn owner(
    state_dir: PathBuf,
    config: PathBuf,
    bootstrap: PathBuf,
    policy: crate::VisionSpawnPolicy,
) -> Result<()> {
    owner_inner(state_dir, config, bootstrap, policy, false, None).await
}

pub(crate) async fn owner_with_cdp(
    state_dir: PathBuf,
    config: PathBuf,
    bootstrap: PathBuf,
    policy: crate::VisionSpawnPolicy,
    port: Option<u16>,
) -> Result<()> {
    owner_inner(state_dir, config, bootstrap, policy, true, port).await
}

async fn owner_inner(
    state_dir: PathBuf,
    config: PathBuf,
    bootstrap: PathBuf,
    policy: crate::VisionSpawnPolicy,
    force_cdp: bool,
    cdp_port: Option<u16>,
) -> Result<()> {
    let config = absolute(config)?;
    let bootstrap = absolute(bootstrap)?;
    let _owner_lock = claim(&state_dir.join("owner.lock"))?;
    let id = Uuid::new_v4();
    let secret = Uuid::new_v4().to_string();
    let mut loaded = config::AppConfig::load(&config)?;
    crate::load_managed_vision_token(&bootstrap, "BOBBY_VISION_TOKEN")?;
    crate::bootstrap_local::ensure_unrestricted_bootstrap(&bootstrap)?;
    loaded.server.host = "127.0.0.1".into();
    loaded.server.port = 0;
    let (mut loaded, _, _vision) = crate::prepare_vision_child(&config, loaded, policy)?;
    if force_cdp {
        loaded.cdp.enabled = true;
        if let Some(port) = cdp_port {
            loaded.cdp.port = port;
        }
    }
    let _telemetry = observability::init(&loaded.observability)?;
    let startup = match crate::bootstrap_local::resolve_startup_credential_with(
        "127.0.0.1",
        &bootstrap,
        broker::StartupCredential::from_env,
    )? {
        crate::bootstrap_local::ResolveOutcome::FromEnv(value)
        | crate::bootstrap_local::ResolveOutcome::FromFile(value)
        | crate::bootstrap_local::ResolveOutcome::Generated {
            credential: value, ..
        } => value,
    };
    let configuration_digest = digest(&config, &bootstrap)?;
    let (selection, _) = crate::resolve_browser_selection()?;
    let profile_id = selection.preference.durable_profile_id().map(str::to_owned);
    if profile_id.is_some() && loaded.context.dir.is_none() {
        loaded.context.dir = Some(crate::default_context_dir()?);
    }
    let factory = firefox_companion::selection::compose_worker_factory_warm(&loaded, selection)?;
    let registry = state_dir.join("owner.json");
    let ready_path = registry.clone();
    let control = broker::SharedRuntimeControl::new(id, secret.clone(), move |address| {
        write_json(
            &ready_path,
            &Owner {
                owner_id: id,
                pid: std::process::id(),
                url: format!("http://{address}"),
                config: config.clone(),
                bootstrap: bootstrap.clone(),
                configuration_digest: configuration_digest.clone(),
                vision_policy: format!("{policy:?}"),
                stop_secret: secret.clone(),
            },
        )
    });
    let result =
        broker::serve_shared_runtime(loaded, startup, factory.clone(), profile_id, control).await;
    factory.shutdown().await;
    if std::fs::read(&registry)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Owner>(&bytes).ok())
        .is_some_and(|owner| owner.owner_id == id)
    {
        std::fs::remove_file(registry)?;
    }
    result
}

pub(crate) async fn run(command: RuntimeCommand) -> Result<()> {
    let dir = runtime_dir()?;
    match command {
        RuntimeCommand::Start => println!(
            "{}",
            ensure_owner(
                crate::resolve_config_path(None),
                crate::resolve_bootstrap_path(None)?,
                crate::VisionSpawnPolicy::Auto
            )
            .await?
        ),
        RuntimeCommand::Status => match live_owner(&dir).await? {
            Some(owner) => println!("running {} pid={} {}", owner.owner_id, owner.pid, owner.url),
            None => println!("stopped"),
        },
        RuntimeCommand::Stop => {
            let launch = dir.join("launch.lock");
            let _launch = tokio::task::spawn_blocking(move || claim(&launch)).await??;
            let owner_lock = lock_file(&dir.join("owner.lock"))?;
            if let Some(owner) = live_owner(&dir).await? {
                client()?
                    .post(format!("{}/_bobby/runtime", owner.url))
                    .header("x-bobby-owner-stop", owner.stop_secret)
                    .send()
                    .await?
                    .error_for_status()?;
                let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
                loop {
                    match owner_lock.try_lock() {
                        Ok(()) => break,
                        Err(std::fs::TryLockError::WouldBlock) => {}
                        Err(error) => return Err(error.into()),
                    }
                    if tokio::time::Instant::now() >= deadline {
                        bail!("runtime shutdown is still pending");
                    }
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                println!("stopped");
            } else {
                owner_lock.try_lock().map_err(|_| {
                    anyhow::anyhow!(
                        "runtime owner is starting or shutting down; retry `runtime stop`"
                    )
                })?;
                println!("stopped");
            }
        }
        RuntimeCommand::List => {
            let mut roots = vec![user_root()?];
            let mut index = 0;
            while index < roots.len() {
                let root = roots[index].clone();
                for kind in ["teams", "projects"] {
                    if let Ok(entries) = std::fs::read_dir(root.join(kind)) {
                        for entry in entries {
                            let entry = entry?;
                            if entry.file_type()?.is_dir() {
                                roots.push(entry.path());
                            }
                        }
                    }
                }
                if root.join("scope.json").exists() || index == 0 {
                    let scope: Scope = std::fs::read(root.join("scope.json"))
                        .ok()
                        .and_then(|v| serde_json::from_slice(&v).ok())
                        .unwrap_or_default();
                    let status = live_owner(&root.join("runtime")).await?;
                    println!(
                        "team={} project={} {} {}",
                        scope.team.as_deref().unwrap_or("personal"),
                        scope.project.as_deref().unwrap_or("default"),
                        if status.is_some() {
                            "running"
                        } else {
                            "stopped"
                        },
                        status.map(|o| o.url).unwrap_or_default()
                    );
                }
                index += 1;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn concurrent_config_initialization_publishes_complete_contents() {
        let root = tempfile::tempdir().unwrap();
        std::thread::scope(|threads| {
            for _ in 0..4 {
                let root = root.path();
                threads.spawn(move || {
                    let path = root.join("config.toml");
                    ensure_config(&path, root).unwrap();
                    let config = config::AppConfig::load(&path).unwrap();
                    assert_eq!(config.browser.profiles_dir, root.join("profiles"));
                });
            }
        });
    }
    #[test]
    fn team_and_project_scopes_are_unambiguous_and_cannot_escape_the_root() {
        let root = Path::new("/scopes");
        let team = Scope {
            team: Some("dev".into()),
            project: None,
        };
        let project = Scope {
            team: None,
            project: Some("dev".into()),
        };
        assert_ne!(team.root_at(root).unwrap(), project.root_at(root).unwrap());
        assert_eq!(
            Scope {
                team: Some("dev".into()),
                project: Some("app".into())
            }
            .root_at(root)
            .unwrap(),
            root.join("teams/dev/projects/app")
        );
        assert!(Scope {
            team: Some("../outside".into()),
            project: None
        }
        .root_at(root)
        .is_err());
    }
}
