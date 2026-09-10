//! Self-healing registry for OS processes that a `WorkerFactory` spawns and exclusively
//! owns, such as `ChromiumWorkerFactory`'s `chromiumoxide::Browser` child. Engine-agnostic:
//! an engine that attaches to an externally managed browser (`firefox-companion`) owns no
//! process and registers nothing.

use std::path::{Path, PathBuf};

fn registration_path(registry_dir: &Path, key: &str) -> PathBuf {
    registry_dir.join(format!("{key}.pid"))
}

/// Records `pid` under `key` within `registry_dir`, tagged with this process's own PID as
/// the entry's owner, so a later process can recognize and reap it when this one never runs
/// its clean-shutdown path -- but only once that owner is confirmed dead, never while it (or
/// any other live process) still holds the entry. Best-effort: returns `None` on any I/O
/// failure rather than blocking a launch. `key` must be unique within `registry_dir`;
/// callers typically use their `WorkerId`.
pub fn register_pid(registry_dir: &Path, key: &str, pid: u32) -> Option<PathBuf> {
    std::fs::create_dir_all(registry_dir).ok()?;
    let path = registration_path(registry_dir, key);
    let owner_pid = std::process::id();
    std::fs::write(&path, format!("owner={owner_pid}\npid={pid}\n")).ok()?;
    Some(path)
}

/// Removes a registration written by `register_pid`. Best-effort: a missing
/// or already-removed file is not an error.
pub fn unregister_pid(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// The PID `register_pid` recorded at `path`, in either the current owner-tagged format or
/// the pre-ownership bare-PID format a binary from before owner tracking may have written.
/// `None` if the file is missing or its contents are not a recognized registration.
pub fn read_registered_pid(path: &Path) -> Option<u32> {
    let contents = std::fs::read_to_string(path).ok()?;
    parse_entry(&contents).map(|entry| entry.pid)
}

/// A registration parsed from the filesystem: the process it tracks, and -- for anything
/// written by the current `register_pid` -- the PID of the process that wrote it. A
/// pre-ownership entry (a bare PID, no owner line) parses to `owner_pid: None`: nothing
/// recorded which process to compare against, so `reap_orphaned_processes` can never
/// verify it is safe to kill.
struct RegistryEntry {
    owner_pid: Option<u32>,
    pid: u32,
}

/// Parses either the current `"owner=<pid>\npid=<pid>\n"` format or the pre-ownership
/// bare-PID format. `None` for anything else (a malformed or unrelated file).
fn parse_entry(contents: &str) -> Option<RegistryEntry> {
    let trimmed = contents.trim();
    if let Ok(pid) = trimmed.parse::<u32>() {
        return Some(RegistryEntry {
            owner_pid: None,
            pid,
        });
    }
    let mut lines = trimmed.lines();
    let owner_pid = lines.next()?.strip_prefix("owner=")?.trim().parse().ok()?;
    let pid = lines.next()?.strip_prefix("pid=")?.trim().parse().ok()?;
    Some(RegistryEntry {
        owner_pid: Some(owner_pid),
        pid,
    })
}

/// Sweeps `registry_dir` for processes orphaned by an instance that died without running
/// its teardown (SIGKILL, OOM kill, crash). No portable way exists to make children die
/// with the parent (`PR_SET_PDEATHSIG` is Linux-only), so each launch registers its PID
/// and the next start reaps.
///
/// An entry is only ever killed once its recorded owner is confirmed dead -- neither this
/// process nor another still-running one. A live owner's entry, including one this process
/// itself just registered, is left in place untouched: it belongs to a running instance,
/// not an orphan. A pre-ownership entry (no owner recorded) is removed but never killed --
/// nothing recorded which process to compare against, and treating its PID as safe to
/// signal on no evidence is the same unverified guess that let one bobby instance kill a
/// browser another live instance was still driving. `owns_process` must positively verify
/// a candidate PID belongs to the expected process family before `kill` runs, so a reused
/// PID is never signalled. An entry whose owner is confirmed dead is always removed after
/// this check, whether or not it was killed, or stale entries persist forever.
pub fn reap_orphaned_processes(
    registry_dir: &Path,
    owns_process: impl Fn(u32) -> bool,
    kill: impl Fn(u32),
) {
    let Ok(entries) = std::fs::read_dir(registry_dir) else {
        return;
    };
    let this_process = std::process::id();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("pid") {
            continue;
        }
        let contents = std::fs::read_to_string(&path).unwrap_or_default();
        let Some(parsed) = parse_entry(&contents) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        let Some(owner_pid) = parsed.owner_pid else {
            // Pre-ownership entry: clean it up, but never kill on say-so we cannot
            // attribute to a confirmed-dead owner.
            let _ = std::fs::remove_file(&path);
            continue;
        };
        if owner_pid == this_process || owner_is_alive(owner_pid) {
            // A running instance -- possibly this one -- still owns this entry; it is not
            // an orphan, so it is left exactly as it is.
            continue;
        }
        if owns_process(parsed.pid) {
            kill(parsed.pid);
        }
        let _ = std::fs::remove_file(&path);
    }
}

/// Whether `pid` -- an entry's recorded owner -- is still running. Distinguishes a live
/// sibling instance (its entry, and whatever it owns, is left alone) from one that died
/// without unregistering (its entry is reaped). Signal `0` delivers nothing; it only
/// reports, via its return value and `errno`, whether `pid` exists and is visible to us.
#[cfg(unix)]
fn owner_is_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    // SAFETY: signal 0 is a pure existence probe -- it never affects the target process,
    // so any PID, caller-supplied or not, can be passed here without side effects.
    let result = unsafe { libc::kill(pid, 0) };
    if result == 0 {
        return true;
    }
    // ESRCH ("no such process") is the only errno that means gone; EPERM means the
    // process exists but is owned by another user, which still counts as alive.
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(not(unix))]
fn owner_is_alive(_pid: u32) -> bool {
    // No portable liveness probe exists here; treating every owner as alive means this
    // platform never kills on an unverifiable guess (the identity check on the tracked PID
    // would also block it, since `process_command_name` is `None`-only here too).
    true
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

    // An implausibly high PID stands in for a dead owner in these tests: no live
    // process on a real machine holds it (the `process_command_name` test below relies
    // on the same assumption), so `owner_is_alive` reports it gone without any need to
    // actually spawn and kill a second process just to free its PID.
    #[cfg(unix)]
    const DEAD_OWNER_PID: u32 = i32::MAX as u32;

    #[cfg(unix)]
    #[test]
    fn reap_kills_a_verified_live_process_with_a_confirmed_dead_owner() {
        use std::process::Command;

        let registry_dir = tempdir().unwrap();
        let mut child = Command::new("sleep").arg("5").spawn().unwrap();
        let pid = child.id();
        let live_path = registry_dir.path().join("live.pid");
        std::fs::write(&live_path, format!("owner={DEAD_OWNER_PID}\npid={pid}\n")).unwrap();
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
        let mut child = Command::new("sleep").arg("5").spawn().unwrap();
        let pid = child.id();
        let path = registry_dir.path().join("unverified.pid");
        std::fs::write(&path, format!("owner={DEAD_OWNER_PID}\npid={pid}\n")).unwrap();

        super::reap_orphaned_processes(
            registry_dir.path(),
            |_| false,
            |_| {
                panic!("must never signal a process that failed identity verification");
            },
        );

        // The stale registry entry is still cleared, since its owner is confirmed dead,
        // even though the process it referenced was left alone.
        assert!(!path.exists());
        assert!(matches!(child.try_wait(), Ok(None)));
        child.kill().unwrap();
        let _ = child.wait();
    }

    #[cfg(unix)]
    #[test]
    fn reap_leaves_a_live_owners_entry_in_place() {
        use std::process::Command;

        let registry_dir = tempdir().unwrap();
        // A second real process stands in for a live sibling bobby instance that still
        // owns this entry. Its own tracked PID is irrelevant here (4_242 is never a real
        // process) -- the point is that a live owner short-circuits before that PID is
        // ever looked at.
        let mut owner = Command::new("sleep").arg("5").spawn().unwrap();
        let path = registry_dir.path().join("live-owner.pid");
        std::fs::write(&path, format!("owner={}\npid=4242\n", owner.id())).unwrap();

        super::reap_orphaned_processes(
            registry_dir.path(),
            |_| true,
            |_| {
                panic!("must never kill an entry whose owner is still alive");
            },
        );

        assert!(
            path.exists(),
            "a live owner's entry must be left in place, not deleted"
        );
        owner.kill().unwrap();
        let _ = owner.wait();
    }

    #[test]
    fn reap_leaves_this_processs_own_entry_in_place() {
        let registry_dir = tempdir().unwrap();
        let path = registry_dir.path().join("self-owned.pid");
        std::fs::write(&path, format!("owner={}\npid=4242\n", std::process::id())).unwrap();

        super::reap_orphaned_processes(
            registry_dir.path(),
            |_| true,
            |_| {
                panic!("must never kill an entry this process itself owns");
            },
        );

        assert!(
            path.exists(),
            "this process's own entry must be left in place, not deleted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn reap_removes_a_pre_ownership_entry_without_killing_anything() {
        use std::process::Command;

        let registry_dir = tempdir().unwrap();
        let mut child = Command::new("sleep").arg("5").spawn().unwrap();
        let pid = child.id();
        let path = registry_dir.path().join("legacy.pid");
        // Pre-ownership format: a bare PID, no owner line. A binary from before this
        // change could still write this during a rolling upgrade; it must be cleaned up,
        // never killed on unverifiable say-so.
        std::fs::write(&path, pid.to_string()).unwrap();

        super::reap_orphaned_processes(
            registry_dir.path(),
            |_| true,
            |_| {
                panic!("a pre-ownership entry must never be killed: it names no owner to verify");
            },
        );

        assert!(
            !path.exists(),
            "a pre-ownership entry must still be cleaned up"
        );
        assert!(
            matches!(child.try_wait(), Ok(None)),
            "the process itself must be left running"
        );
        child.kill().unwrap();
        let _ = child.wait();
    }

    #[test]
    fn register_and_unregister_pid_round_trip_through_the_filesystem() {
        let registry_dir = tempdir().unwrap();
        let path = super::register_pid(registry_dir.path(), "worker-a", 4_242)
            .expect("registering a PID under a writable directory must succeed");
        assert_eq!(super::read_registered_pid(&path), Some(4_242));
        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(
            contents.contains(&format!("owner={}", std::process::id())),
            "the registration must record this process as the entry's owner: {contents}"
        );

        super::unregister_pid(&path);
        assert!(!path.exists());
    }

    #[test]
    fn read_registered_pid_understands_the_pre_ownership_bare_pid_format() {
        let registry_dir = tempdir().unwrap();
        let path = registry_dir.path().join("legacy.pid");
        std::fs::write(&path, "4242").unwrap();
        assert_eq!(super::read_registered_pid(&path), Some(4_242));
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
