//! Unix permissions for persisted skill decisions, including legacy files.
use super::{DoctorFixAction, DoctorFixStatus, DoctorReport};
use std::{
    fs::{OpenOptions, Permissions},
    io,
    os::unix::fs::{MetadataExt, OpenOptionsExt, PermissionsExt},
    path::Path,
};

const CHECK: &str = "storage-skill-issuance-permissions";

#[derive(Default)]
struct Scan {
    checked: usize,
    broad: usize,
    repaired: usize,
    failures: usize,
    first_error: Option<String>,
}

impl Scan {
    fn error(&mut self, path: &Path, error: io::Error) {
        self.failures += 1;
        if self.first_error.is_none() {
            self.first_error = Some(format!("{}: {error}", path.display()));
        }
    }

    fn detail(&self) -> String {
        format!(
            "{} skill decisions checked; {} require repair; {} repaired; {} errors{}",
            self.checked,
            self.broad.saturating_sub(self.repaired),
            self.repaired,
            self.failures,
            self.first_error
                .as_ref()
                .map(|e| format!("; {e}"))
                .unwrap_or_default(),
        )
    }
}

pub(super) fn record(report: &mut DoctorReport, root: &Path) {
    let scan = scan(root, false);
    if scan.broad > 0 || scan.failures > 0 {
        report.fail(
            CHECK,
            format!("{}; run `bobby doctor --fix`", scan.detail()),
        );
    } else {
        report.ok(CHECK, scan.detail());
    }
}

pub(super) fn repair(root: &Path) -> DoctorFixAction {
    let scan = scan(root, true);
    DoctorFixAction {
        name: CHECK.into(),
        status: if scan.failures > 0 {
            DoctorFixStatus::Failed
        } else if scan.repaired > 0 {
            DoctorFixStatus::Fixed
        } else {
            DoctorFixStatus::Noop
        },
        detail: scan.detail(),
    }
}

fn scan(root: &Path, repair: bool) -> Scan {
    let mut scan = Scan::default();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return scan,
        Err(error) => {
            scan.error(root, error);
            return scan;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => {
                scan.error(root, error);
                continue;
            }
        };
        let name = entry.file_name();
        let Some(id) = name
            .to_str()
            .and_then(|name| name.strip_suffix(".skill-issuance.json"))
        else {
            continue;
        };
        // Only canonical destinations written by CheckpointStore belong here.
        if !uuid::Uuid::parse_str(id).is_ok_and(|uuid| uuid.to_string() == id) {
            continue;
        }
        let path = entry.path();
        let result = (|| {
            // NOFOLLOW protects against symlink replacement; NONBLOCK prevents
            // a special-file substitution from hanging the doctor. Chmod the
            // descriptor, never re-resolve the path after inspecting it.
            let file = OpenOptions::new()
                .read(true)
                .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
                .open(&path)?;
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.nlink() != 1 {
                return Err(io::Error::other(
                    "refusing a linked or non-regular skill decision; inspect it manually",
                ));
            }
            scan.checked += 1;
            if metadata.permissions().mode() & 0o7777 != 0o600 {
                scan.broad += 1;
                if repair {
                    file.set_permissions(Permissions::from_mode(0o600))?;
                    file.sync_all()?;
                    scan.repaired += 1;
                }
            }
            Ok::<_, io::Error>(())
        })();
        if let Err(error) = result {
            // Concurrent deletion is normal while Bobby consumes a decision.
            if error.kind() != io::ErrorKind::NotFound {
                scan.error(&path, error);
            }
        }
    }
    scan
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doctor::DoctorStatus;
    use std::os::unix::fs::{symlink, PermissionsExt};

    fn legacy_file(root: &Path) -> std::path::PathBuf {
        let path = root.join(format!("{}.skill-issuance.json", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"preserve even corrupt legacy evidence\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        path
    }

    #[test]
    fn legacy_permissions_are_reported_read_only_and_repaired_without_rewriting() {
        let root = tempfile::tempdir().unwrap();
        let path = legacy_file(root.path());
        let bytes = std::fs::read(&path).unwrap();
        let mut report = DoctorReport::default();
        record(&mut report, root.path());
        assert_eq!(report.checks[0].status, DoctorStatus::Fail);
        assert!(report.checks[0].detail.contains("doctor --fix"));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o644
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);

        assert_eq!(repair(root.path()).status, DoctorFixStatus::Fixed);
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o7777,
            0o600
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(repair(root.path()).status, DoctorFixStatus::Noop);
        let mut report = DoctorReport::default();
        record(&mut report, root.path());
        assert_eq!(report.checks[0].status, DoctorStatus::Ok);
    }

    #[test]
    fn repair_refuses_links_and_special_files_but_repairs_other_legacy_decisions() {
        let root = tempfile::tempdir().unwrap();
        let external = tempfile::tempdir().unwrap();
        let target = legacy_file(external.path());
        let symlink_path = root
            .path()
            .join(format!("{}.skill-issuance.json", uuid::Uuid::new_v4()));
        symlink(&target, symlink_path).unwrap();
        let hardlink_path = root
            .path()
            .join(format!("{}.skill-issuance.json", uuid::Uuid::new_v4()));
        std::fs::hard_link(&target, hardlink_path).unwrap();
        let directory = root
            .path()
            .join(format!("{}.skill-issuance.json", uuid::Uuid::new_v4()));
        std::fs::create_dir(&directory).unwrap();
        let fifo = root
            .path()
            .join(format!("{}.skill-issuance.json", uuid::Uuid::new_v4()));
        use std::os::unix::ffi::OsStrExt;
        let fifo_name = std::ffi::CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: fifo_name is a live, NUL-terminated path for this call.
        assert_eq!(unsafe { libc::mkfifo(fifo_name.as_ptr(), 0o600) }, 0);
        let healthy = legacy_file(root.path());
        let unrelated = root.path().join("not-a-workflow.skill-issuance.json");
        std::fs::write(&unrelated, b"unrelated").unwrap();
        std::fs::set_permissions(&unrelated, std::fs::Permissions::from_mode(0o644)).unwrap();

        let scan_root = root.path().to_path_buf();
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || sender.send(repair(&scan_root)).unwrap());
        let action = receiver
            .recv_timeout(std::time::Duration::from_secs(5))
            .expect("doctor repair must not block opening a FIFO");
        assert_eq!(action.status, DoctorFixStatus::Failed);
        assert!(action.detail.contains("1 repaired; 4 errors"));
        assert_eq!(
            std::fs::metadata(target).unwrap().permissions().mode() & 0o7777,
            0o644
        );
        assert_eq!(
            std::fs::metadata(unrelated).unwrap().permissions().mode() & 0o7777,
            0o644
        );
        assert_eq!(
            std::fs::metadata(healthy).unwrap().permissions().mode() & 0o7777,
            0o600
        );
    }

    #[test]
    fn absent_storage_is_a_noop() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(
            repair(&root.path().join("missing")).status,
            DoctorFixStatus::Noop
        );
    }
}
