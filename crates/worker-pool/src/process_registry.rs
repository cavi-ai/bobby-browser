//! Self-healing registry for OS processes that a `WorkerFactory` spawns and exclusively
//! owns, such as `ChromiumWorkerFactory`'s `chromiumoxide::Browser` child. Engine-agnostic:
//! an engine that attaches to an externally managed browser (`firefox-companion`) owns no
//! process and registers nothing.

use std::path::{Path, PathBuf};

fn registration_path(registry_dir: &Path, key: &str) -> PathBuf {
    registry_dir.join(format!("{key}.pid"))
}

/// Records `pid` under `key` within `registry_dir`, so a later process can recognize and
/// reap it when this one never runs its clean-shutdown path. Best-effort: returns `None`
/// on any I/O failure rather than blocking a launch. `key` must be unique within
/// `registry_dir`; callers typically use their `WorkerId`.
pub fn register_pid(registry_dir: &Path, key: &str, pid: u32) -> Option<PathBuf> {
    std::fs::create_dir_all(registry_dir).ok()?;
    let path = registration_path(registry_dir, key);
    std::fs::write(&path, pid.to_string()).ok()?;
    Some(path)
}

/// Removes a registration written by `register_pid`. Best-effort: a missing
/// or already-removed file is not an error.
pub fn unregister_pid(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Sweeps `registry_dir` for processes orphaned by an instance that died without running
/// its teardown (SIGKILL, OOM kill, crash). No portable way exists to make children die
/// with the parent (`PR_SET_PDEATHSIG` is Linux-only), so each launch registers its PID
/// and the next start reaps.
///
/// `owns_process` must positively verify a candidate PID belongs to the expected process
/// family before `kill` runs, so a reused PID is never signalled. Every entry is removed
/// regardless of the verification result, or stale entries persist forever.
pub fn reap_orphaned_processes(
    registry_dir: &Path,
    owns_process: impl Fn(u32) -> bool,
    kill: impl Fn(u32),
) {
    let Ok(entries) = std::fs::read_dir(registry_dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("pid") {
            continue;
        }
        if let Ok(contents) = std::fs::read_to_string(&path) {
            if let Ok(pid) = contents.trim().parse::<u32>() {
                if owns_process(pid) {
                    kill(pid);
                }
            }
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// Lowercased `comm` (process name) for `pid` per `ps`, or `None` if the process does
/// not exist or `ps` failed. Match it against a substring needle such as "chrom", not an
/// exact name: browser binaries vary by platform and channel (`google-chrome-stable`,
/// `Google Chrome`, `chromium`).
#[cfg(unix)]
pub fn process_command_name(pid: u32) -> Option<String> {
    let output = std::process::Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", "comm="])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    Some(String::from_utf8_lossy(&output.stdout).to_ascii_lowercase())
}

#[cfg(not(unix))]
pub fn process_command_name(_pid: u32) -> Option<String> {
    None
}

#[cfg(unix)]
pub fn kill_process(pid: u32) {
    if let Ok(pid) = i32::try_from(pid) {
        // SAFETY: `kill` is a plain signal-delivery syscall; a caller-supplied PID can
        // at worst fail with ESRCH/EPERM, ignored here since the process was already
        // verified and the reap is best-effort.
        unsafe {
            libc::kill(pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
pub fn kill_process(_pid: u32) {}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use tempfile::tempdir;

    #[cfg(unix)]
    #[test]
    fn reap_kills_a_verified_live_process_and_clears_stale_entries() {
        use std::process::Command;

        let registry_dir = tempdir().unwrap();
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let live_path = registry_dir.path().join("live.pid");
        std::fs::write(&live_path, pid.to_string()).unwrap();
        // A malformed entry alongside the real one must still be cleared,
        // not just skipped.
        let malformed_path = registry_dir.path().join("malformed.pid");
        std::fs::write(&malformed_path, "not-a-pid").unwrap();
        // Non-`.pid` files in the registry directory are left untouched.
        let unrelated_path = registry_dir.path().join("notes.txt");
        std::fs::write(&unrelated_path, "unrelated").unwrap();

        super::reap_orphaned_processes(registry_dir.path(), |_| true, super::kill_process);

        assert!(!live_path.exists());
        assert!(!malformed_path.exists());
        assert!(unrelated_path.exists());
        let status = child.wait().unwrap();
        assert!(!status.success(), "killed process must exit unsuccessfully");
    }

    #[cfg(unix)]
    #[test]
    fn reap_never_signals_a_process_that_fails_identity_verification() {
        use std::process::Command;

        let registry_dir = tempdir().unwrap();
        let mut child = Command::new("sleep").arg("30").spawn().unwrap();
        let pid = child.id();
        let path = registry_dir.path().join("unverified.pid");
        std::fs::write(&path, pid.to_string()).unwrap();

        super::reap_orphaned_processes(
            registry_dir.path(),
            |_| false,
            |_| {
                panic!("must never signal a process that failed identity verification");
            },
        );

        // The stale registry entry is still cleared even though the process
        // it referenced was left alone.
        assert!(!path.exists());
        assert!(matches!(child.try_wait(), Ok(None)));
        child.kill().unwrap();
        let _ = child.wait();
    }

    #[test]
    fn register_and_unregister_pid_round_trip_through_the_filesystem() {
        let registry_dir = tempdir().unwrap();
        let path = super::register_pid(registry_dir.path(), "worker-a", 4_242)
            .expect("registering a PID under a writable directory must succeed");
        assert_eq!(std::fs::read_to_string(&path).unwrap().trim(), "4242");

        super::unregister_pid(&path);
        assert!(!path.exists());
    }

    #[test]
    fn register_pid_is_best_effort_under_an_unwritable_registry_dir() {
        let unwritable = PathBuf::from("/this/path/does/not/exist/and/cannot/be/created");
        assert!(super::register_pid(&unwritable, "worker-a", 1).is_none());
    }

    #[test]
    fn reap_tolerates_a_missing_registry_directory() {
        // Must not panic when nothing has ever launched a worker into this
        // registry directory yet.
        super::reap_orphaned_processes(
            &PathBuf::from("/this/path/does/not/exist"),
            |_| true,
            |_| panic!("nothing to kill in a missing directory"),
        );
    }

    #[cfg(unix)]
    #[test]
    fn process_command_name_identifies_a_known_running_process() {
        use std::process::Command;

        let mut child = Command::new("sleep").arg("5").spawn().unwrap();
        let name = super::process_command_name(child.id()).expect("process must be running");
        assert!(name.contains("sleep"));
        child.kill().unwrap();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn process_command_name_returns_none_for_an_implausible_pid() {
        assert!(super::process_command_name(i32::MAX as u32).is_none());
    }
}
