use std::path::Path;
use tokio::io::AsyncWriteExt;

#[cfg(windows)]
const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;

pub fn without_following_links(options: &mut std::fs::OpenOptions) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

pub fn without_following_links_async(options: &mut tokio::fs::OpenOptions) {
    #[cfg(unix)]
    {
        options.custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK);
    }
    #[cfg(windows)]
    {
        // Inherent on tokio's Windows OpenOptions; the std trait does not apply.
        options.custom_flags(FILE_FLAG_OPEN_REPARSE_POINT);
    }
}

pub fn require_regular_file(metadata: &std::fs::Metadata, entry: &str) -> std::io::Result<()> {
    if metadata.file_type().is_file() {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("{entry} must be a regular file"),
        ))
    }
}

pub fn require_sole_hard_link(metadata: &std::fs::Metadata, entry: &str) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if metadata.nlink() != 1 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("{entry} must have exactly one hard link"),
            ));
        }
    }
    #[cfg(not(unix))]
    let _ = (metadata, entry);
    Ok(())
}

/// Create `temporary` exclusively, write `bytes`, then rename it onto `destination`.
/// The directory entry is synced. A failed attempt removes `temporary` only
/// after creating it; an existing temporary entry is left intact.
pub async fn replace_exclusive(
    directory: &Path,
    temporary: &Path,
    destination: &Path,
    bytes: &[u8],
) -> std::io::Result<()> {
    let mut options = tokio::fs::OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    options.mode(0o600);
    // Cleanup starts only once this attempt owns a newly created file.
    let mut file = options.open(temporary).await?;
    let result = async move {
        file.write_all(bytes).await?;
        file.sync_all().await?;
        drop(file);
        tokio::fs::rename(temporary, destination).await?;
        tokio::fs::File::open(directory).await?.sync_all().await?;
        Ok(())
    }
    .await;
    if result.is_err() {
        let _ = tokio::fs::remove_file(temporary).await;
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[tokio::test]
    async fn a_temporary_path_collision_preserves_both_existing_files() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("entry.json");
        let temporary = root.path().join(".entry.tmp");
        std::fs::write(&destination, b"previous destination").unwrap();
        std::fs::write(&temporary, b"other writer's temporary bytes").unwrap();

        let error = replace_exclusive(root.path(), &temporary, &destination, b"replacement")
            .await
            .unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            std::fs::read(&destination).unwrap(),
            b"previous destination"
        );
        assert_eq!(
            std::fs::read(&temporary).unwrap(),
            b"other writer's temporary bytes"
        );
    }

    #[tokio::test]
    async fn a_failed_rename_removes_its_own_temporary_file_and_allows_retry() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("occupied");
        let temporary = root.path().join(".entry.tmp");
        std::fs::create_dir(&destination).unwrap();
        let canary = destination.join("canary");
        std::fs::write(&canary, b"keep").unwrap();

        assert!(
            replace_exclusive(root.path(), &temporary, &destination, b"replacement")
                .await
                .is_err()
        );
        assert!(!temporary.exists());
        assert_eq!(std::fs::read(&canary).unwrap(), b"keep");

        let retry = root.path().join("retry.json");
        replace_exclusive(root.path(), &temporary, &retry, b"retry")
            .await
            .unwrap();
        assert_eq!(std::fs::read(retry).unwrap(), b"retry");
        assert!(!temporary.exists());
    }

    #[tokio::test]
    async fn replace_writes_an_owner_file_the_next_read_accepts() {
        let root = tempfile::tempdir().unwrap();
        let destination = root.path().join("entry.json");
        let temporary = root.path().join(".entry.tmp");
        replace_exclusive(root.path(), &temporary, &destination, b"{\"ok\":true}")
            .await
            .unwrap();
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        without_following_links(&mut options);
        let mut file = options.open(&destination).unwrap();
        let metadata = file.metadata().unwrap();
        require_regular_file(&metadata, "storage entry").unwrap();
        require_sole_hard_link(&metadata, "storage entry").unwrap();
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes).unwrap();
        assert_eq!(bytes, b"{\"ok\":true}");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(metadata.permissions().mode() & 0o777, 0o600);
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn a_symlink_is_not_a_regular_entry() {
        let root = tempfile::tempdir().unwrap();
        let target = root.path().join("real.json");
        std::fs::write(&target, b"{}").unwrap();
        let link = root.path().join("link.json");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let mut options = std::fs::OpenOptions::new();
        options.read(true);
        without_following_links(&mut options);
        let error = options.open(&link).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(libc::ELOOP));
    }
}
