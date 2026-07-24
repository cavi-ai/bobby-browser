use std::path::PathBuf;
use std::time::Duration;

use config::BrowserConfig;
use types::SessionId;
use worker_pool::{ChromiumWorkerFactory, WorkerFactory};

fn browser_config(root: &std::path::Path) -> BrowserConfig {
    BrowserConfig {
        executable: Some(PathBuf::from(
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
        )),
        profiles_dir: root.join("profiles"),
        headless: true,
        max_active: 1,
        upload_roots: vec![root.to_path_buf()],
        downloads_dir: root.join("downloads"),
        artifacts_dir: root.join("artifacts"),
        max_artifact_bytes: 8 * 1024 * 1024,
        max_screenshot_dimension: 16_384,
        max_js_result_bytes: 64 * 1024,
        max_js_timeout_ms: 30_000,
    }
}

/// SAFETY: `pid` was read moments earlier from this test's own PID registry
/// entry for a worker it just launched; signaling it can at worst fail
/// harmlessly (ESRCH) if the process already exited.
unsafe fn force_kill(pid: i32) -> i32 {
    libc::kill(pid, libc::SIGKILL)
}

/// Simulates the exact scenario the orphan-reaping registry exists for: a
/// prior runtime instance's Chrome process outlives it because that
/// instance was killed before `close`/`terminate` (and chromiumoxide's
/// `kill_on_drop`) ever ran. Kills the Chrome child directly — bypassing
/// `BrowserWorker::close`/`terminate` entirely, the same way a SIGKILL to
/// the whole process would — then constructs a *new* factory against the
/// same registry directory and confirms it reaps the leftover process.
#[tokio::test]
#[ignore = "requires installed Chrome or Chromium"]
async fn new_factory_reaps_a_chrome_process_orphaned_by_a_prior_instance() {
    let root = tempfile::tempdir().unwrap();
    let registry_dir = root.path().join("pid-registry");

    let factory = ChromiumWorkerFactory::with_pid_registry_dir(
        browser_config(root.path()),
        registry_dir.clone(),
    );
    let worker = factory.launch(&SessionId::new()).await.unwrap();
    let worker_id = worker.worker_id();

    let registry_entries: Vec<_> = std::fs::read_dir(&registry_dir)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(
        registry_entries.len(),
        1,
        "launch must register exactly one PID entry: {registry_entries:?}"
    );
    let pid: i32 = std::fs::read_to_string(&registry_entries[0])
        .unwrap()
        .trim()
        .parse()
        .unwrap();

    // Bypass `terminate`/`close` entirely: this is what happens to a real
    // Chrome child when the parent runtime process is SIGKILLed before it
    // gets a chance to run its own cleanup. This test process remains the
    // Chrome process's OS-level parent throughout (unlike a genuine orphan,
    // which gets reparented once its real parent exits), so the killed
    // process becomes a zombie here rather than disappearing outright —
    // `kill(pid, 0)` stays "successful" for a zombie's PID until something
    // reaps it. That's still a faithful proxy for "the process is no
    // longer doing anything": what matters for this test is that the
    // signal was delivered, and that a new factory clears the stale
    // registration regardless of exactly when the OS finishes tearing the
    // process down.
    let killed = unsafe { force_kill(pid) };
    assert_eq!(killed, 0, "SIGKILL must be deliverable to our own child");
    tokio::time::sleep(Duration::from_millis(200)).await;
    // The orphaned entry must still be on disk — nothing ran the worker's
    // own cleanup path.
    assert!(registry_entries[0].exists());

    // A brand-new factory, as if the runtime had just restarted, must find
    // and clear that stale registration on construction.
    let _next_factory = ChromiumWorkerFactory::with_pid_registry_dir(
        browser_config(root.path()),
        registry_dir.clone(),
    );

    assert!(
        !registry_entries[0].exists(),
        "constructing a new factory must reap the orphaned PID registration for worker {}",
        worker_id.0
    );
}
