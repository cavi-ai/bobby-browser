use std::{
    ffi::{OsStr, OsString},
    fs::{self, File},
    io::{self, Write},
    path::Path,
    sync::atomic::{AtomicU64, Ordering},
};

use rustix::fs::{AtFlags, Mode, OFlags};

pub(crate) const MAX_TEMPORARY_ATTEMPTS: usize = 128;
static TEMPORARY_SEQUENCE: AtomicU64 = AtomicU64::new(0);

pub(crate) trait AtomicPersistenceIo {
    fn create(&self, directory: &File, name: &OsStr) -> io::Result<File>;
    fn metadata(&self, file: &File) -> io::Result<FileIdentity>;
    fn write_all(&self, file: &mut File, bytes: &[u8]) -> io::Result<()>;
    fn sync_file(&self, file: &File) -> io::Result<()>;
    fn rename(&self, directory: &File, source: &OsStr, destination: &OsStr) -> io::Result<()>;
    fn sync_directory(&self, directory: &File) -> io::Result<()>;
}

pub(crate) struct OsAtomicPersistenceIo;

impl AtomicPersistenceIo for OsAtomicPersistenceIo {
    fn create(&self, directory: &File, name: &OsStr) -> io::Result<File> {
        let fd = rustix::fs::openat(
            directory,
            name,
            OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::CLOEXEC | OFlags::NOFOLLOW,
            Mode::RUSR | Mode::WUSR,
        )?;
        Ok(File::from(fd))
    }

    fn metadata(&self, file: &File) -> io::Result<FileIdentity> {
        file_identity(file)
    }

    fn write_all(&self, file: &mut File, bytes: &[u8]) -> io::Result<()> {
        file.write_all(bytes)
    }

    fn sync_file(&self, file: &File) -> io::Result<()> {
        rustix::fs::fsync(file).map_err(Into::into)
    }

    fn rename(&self, directory: &File, source: &OsStr, destination: &OsStr) -> io::Result<()> {
        rustix::fs::renameat(directory, source, directory, destination).map_err(Into::into)
    }

    fn sync_directory(&self, directory: &File) -> io::Result<()> {
        rustix::fs::fsync(directory).map_err(Into::into)
    }
}

pub(crate) struct OutputTarget {
    pub(crate) directory: File,
    pub(crate) file_name: OsString,
}

impl OutputTarget {
    pub(crate) fn open(path: &Path) -> io::Result<Self> {
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let file_name = validated_file_name(path)?;
        let canonical_parent = fs::canonicalize(parent)?;
        Ok(Self {
            directory: open_directory(&canonical_parent)?,
            file_name,
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct FileIdentity {
    pub(crate) device: u64,
    pub(crate) inode: u64,
}

pub(crate) fn persist_bytes(
    target: &OutputTarget,
    bytes: &[u8],
    persistence: &impl AtomicPersistenceIo,
) -> io::Result<()> {
    let mut temporary = OwnedTemporaryFile::create(target, persistence)?;
    persistence.write_all(&mut temporary.file, bytes)?;
    persistence.sync_file(&temporary.file)?;
    temporary.verify_directory_entry()?;
    persistence.rename(&target.directory, &temporary.name, &target.file_name)?;
    temporary.disarm();
    persistence.sync_directory(&target.directory)?;
    Ok(())
}

struct OwnedTemporaryFile<'a> {
    directory: &'a File,
    name: OsString,
    file: File,
    identity: Option<FileIdentity>,
    armed: bool,
}

impl<'a> OwnedTemporaryFile<'a> {
    fn create(
        target: &'a OutputTarget,
        persistence: &impl AtomicPersistenceIo,
    ) -> io::Result<Self> {
        for _ in 0..MAX_TEMPORARY_ATTEMPTS {
            let sequence = TEMPORARY_SEQUENCE.fetch_add(1, Ordering::Relaxed);
            let mut temporary_name = OsString::from(".");
            temporary_name.push(&target.file_name);
            temporary_name.push(format!(
                ".release-gates-{}-{sequence}.tmp",
                std::process::id()
            ));
            match persistence.create(&target.directory, &temporary_name) {
                Ok(file) => {
                    // Ownership begins immediately after openat succeeds. If
                    // the identity probe fails, Drop still compares the open
                    // descriptor to the directory entry before cleanup.
                    let mut temporary = Self {
                        directory: &target.directory,
                        name: temporary_name,
                        file,
                        identity: None,
                        armed: true,
                    };
                    temporary.identity = Some(persistence.metadata(&temporary.file)?);
                    return Ok(temporary);
                }
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error),
            }
        }

        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a unique certification temporary file",
        ))
    }

    fn verify_directory_entry(&self) -> io::Result<()> {
        let actual = relative_identity(self.directory, &self.name, true)?;
        if self.identity == Some(actual) {
            Ok(())
        } else {
            Err(io::Error::other(
                "certification temporary file identity changed before persistence",
            ))
        }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for OwnedTemporaryFile<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let expected = self.identity.or_else(|| file_identity(&self.file).ok());
        let actual = relative_identity(self.directory, &self.name, true).ok();
        if expected.is_some() && expected == actual {
            let _ = rustix::fs::unlinkat(self.directory, &self.name, AtFlags::empty());
        }
    }
}

pub(crate) fn validated_file_name(path: &Path) -> io::Result<OsString> {
    path.file_name()
        .map(OsStr::to_owned)
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "missing output filename"))
}

pub(crate) fn open_directory(path: &Path) -> io::Result<File> {
    let fd = rustix::fs::open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC | OFlags::NOFOLLOW,
        Mode::empty(),
    )?;
    Ok(File::from(fd))
}

pub(crate) fn file_identity(file: &File) -> io::Result<FileIdentity> {
    let stat = rustix::fs::fstat(file)?;
    Ok(FileIdentity {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
    })
}

pub(crate) fn relative_identity(
    directory: &File,
    name: &OsStr,
    no_follow: bool,
) -> io::Result<FileIdentity> {
    let flags = if no_follow {
        AtFlags::SYMLINK_NOFOLLOW
    } else {
        AtFlags::empty()
    };
    let stat = rustix::fs::statat(directory, name, flags)?;
    Ok(FileIdentity {
        device: stat.st_dev as u64,
        inode: stat.st_ino as u64,
    })
}
