use std::fs::OpenOptions;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

const TOKEN_ENV: &str = "BOBBY_VISION_TOKEN";
const TOKEN_FILE_NAME: &str = "vision.env";

pub(crate) fn managed_vision_token_path(bootstrap_path: &Path) -> PathBuf {
    bootstrap_path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join(TOKEN_FILE_NAME)
}

fn read_managed_vision_token(path: &Path) -> Result<Option<String>> {
    let contents = match std::fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("failed to read managed vision token at {}", path.display())
            })
        }
    };
    Ok(contents.lines().find_map(|line| {
        let (name, value) = line
            .trim()
            .strip_prefix("export ")
            .unwrap_or(line.trim())
            .split_once('=')?;
        (name.trim() == TOKEN_ENV)
            .then(|| value.trim().trim_matches(['\'', '"']).to_string())
            .filter(|value| !value.is_empty())
    }))
}

pub(crate) fn resolve_vision_token(bootstrap_path: &Path) -> Result<String> {
    if let Some(value) = std::env::var(TOKEN_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty())
    {
        return Ok(value);
    }
    let path = managed_vision_token_path(bootstrap_path);
    read_managed_vision_token(&path)?.ok_or_else(|| {
        anyhow::anyhow!("vision credential is missing; run `bobby doctor --fix` or `bobby install`")
    })
}

pub(crate) fn ensure_managed_vision_token(bootstrap_path: &Path) -> Result<String> {
    if let Ok(existing) = resolve_vision_token(bootstrap_path) {
        return Ok(existing);
    }
    let path = managed_vision_token_path(bootstrap_path);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut entropy = [0_u8; 32];
    getrandom::fill(&mut entropy).context("failed to generate vision credential entropy")?;
    let token = format!("bobby-vision-{}", hex::encode(entropy));
    let temporary = path.with_extension(format!("env.{}.tmp", uuid::Uuid::new_v4().simple()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&temporary)
        .with_context(|| format!("failed to create {}", temporary.display()))?;
    writeln!(file, "{TOKEN_ENV}={token}")?;
    file.sync_all()?;
    match std::fs::hard_link(&temporary, &path) {
        Ok(()) => {
            std::fs::remove_file(&temporary)?;
            Ok(token)
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            std::fs::remove_file(&temporary)?;
            read_managed_vision_token(&path)?
                .ok_or_else(|| anyhow::anyhow!("managed vision credential exists but is invalid"))
        }
        Err(error) => {
            let _ = std::fs::remove_file(&temporary);
            Err(error).with_context(|| {
                format!(
                    "failed to publish managed vision token at {}",
                    path.display()
                )
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn managed_token_is_private_stable_and_environment_has_precedence() {
        let _guard = ENV_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        let bootstrap = root.path().join("bootstrap.env");
        unsafe { std::env::remove_var("BOBBY_VISION_TOKEN") };

        let first = ensure_managed_vision_token(&bootstrap).unwrap();
        let second = ensure_managed_vision_token(&bootstrap).unwrap();
        assert_eq!(first, second);
        assert!(first.starts_with("bobby-vision-"));
        assert_eq!(
            managed_vision_token_path(&bootstrap),
            root.path().join("vision.env")
        );
        assert_eq!(
            std::fs::metadata(root.path().join("vision.env"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        assert_eq!(resolve_vision_token(&bootstrap).unwrap(), first);

        unsafe { std::env::set_var("BOBBY_VISION_TOKEN", "operator-token") };
        assert_eq!(resolve_vision_token(&bootstrap).unwrap(), "operator-token");
        unsafe { std::env::remove_var("BOBBY_VISION_TOKEN") };
    }
}
