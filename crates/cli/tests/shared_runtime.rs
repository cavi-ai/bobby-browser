//! Production CLI owner and gateway transport regressions; no browser launch.
use serde_json::{json, Value};
use std::{
    path::Path,
    process::{Command, Output},
};
use tokio::io::{AsyncWriteExt, BufReader};

fn command(root: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_bobby"));
    command
        .env("BOBBY_BROWSER_SCOPE_DIR", root)
        .env("BOBBY_BROWSER_CONFIG", root.join("config.toml"))
        .env("BOBBY_BROWSER_BOOTSTRAP_ENV", root.join("bootstrap.env"))
        .env(
            "AUTOMATION_RUNTIME_BROWSER_SELECTION",
            r#"{"preference":{"mode":"managedChromium"},"firefox":[]}"#,
        )
        .env_remove("BOBBY_RUNTIME_URL")
        .env_remove("AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN")
        .env_remove("AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL")
        .env_remove("AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES")
        .env_remove("AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT");
    command.env_remove("AUTOMATION_RUNTIME_BOOTSTRAP_PRESET");
    command
}

fn checked(output: Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct Cleanup(tempfile::TempDir);
impl Drop for Cleanup {
    fn drop(&mut self) {
        let _ = command(self.0.path())
            .args(["runtime", "stop", "--disconnect-agents"])
            .output();
    }
}

async fn connection(
    origin: String,
    bearer: String,
    protocol: &'static str,
) -> (
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
    BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
) {
    let (client, bridge) = tokio::io::duplex(2 * gateway_transport::MAX_FRAME_BYTES);
    let (input, output) = tokio::io::split(bridge);
    let task = tokio::spawn(async move {
        gateway_transport::connect(&origin, protocol, &bearer, input, output).await
    });
    let (reader, writer) = tokio::io::split(client);
    (writer, BufReader::new(reader), task)
}

async fn send(writer: &mut tokio::io::WriteHalf<tokio::io::DuplexStream>, message: Value) {
    writer
        .write_all(format!("{message}\n").as_bytes())
        .await
        .unwrap();
}

async fn response(
    reader: &mut BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    id: u64,
) -> Value {
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            let line = gateway_transport::read_frame(reader)
                .await
                .unwrap()
                .expect("gateway disconnected");
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["id"] == id {
                return value;
            }
        }
    })
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn existing_config_without_context_dir_retains_durable_profile_memory() {
    let scope = Cleanup(tempfile::tempdir().unwrap());
    let path = scope.0.path();
    let mut config = config::AppConfig::default();
    config.browser.profiles_dir = path.join("profiles");
    config.storage.journal_path = path.join("storage/commands.jsonl");
    config.storage.checkpoints_dir = path.join("storage/checkpoints");
    config.storage.authority_path = path.join("storage/authority.json");
    config.storage.scheduler_journal_path = path.join("storage/scheduler-jobs.jsonl");
    assert!(config.context.dir.is_none());
    std::fs::write(path.join("config.toml"), toml::to_string(&config).unwrap()).unwrap();
    checked(
        command(path)
            .env(
                "AUTOMATION_RUNTIME_BROWSER_SELECTION",
                r#"{"preference":{"mode":"exact","engine":"chromium","profileId":"memory-regression"},"firefox":[]}"#,
            )
            .args(["runtime", "start"])
            .output()
            .unwrap(),
    );
    // The production owner must hold the real context store's writer lease.
    let opened = context_store::ContextStore::open(path.join("context"), "memory-regression").await;
    assert!(matches!(
        opened,
        Err(context_store::ContextStoreError::AlreadyLocked)
    ));
    checked(command(path).args(["runtime", "stop"]).output().unwrap());
    assert!(
        context_store::ContextStore::open(path.join("context"), "memory-regression")
            .await
            .is_ok()
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn managed_chromium_owner_holds_the_shared_memory_store() {
    let scope = Cleanup(tempfile::tempdir().unwrap());
    let path = scope.0.path();
    checked(command(path).args(["runtime", "start"]).output().unwrap());
    let opened = context_store::ContextStore::open(
        path.join("context"),
        config::MANAGED_CHROMIUM_CONTEXT_PROFILE,
    )
    .await;
    assert!(matches!(
        opened,
        Err(context_store::ContextStoreError::AlreadyLocked)
    ));
    checked(command(path).args(["runtime", "stop"]).output().unwrap());
    assert!(context_store::ContextStore::open(
        path.join("context"),
        config::MANAGED_CHROMIUM_CONTEXT_PROFILE
    )
    .await
    .is_ok());
}

#[test]
fn install_restart_runtime_stops_the_scope_owner_and_names_its_pid() {
    let scope = Cleanup(tempfile::tempdir().unwrap());
    let path = scope.0.path();
    let cwd = tempfile::tempdir().unwrap();
    checked(command(path).args(["runtime", "start"]).output().unwrap());
    let status = checked(command(path).args(["runtime", "status"]).output().unwrap());
    let pid = status
        .split_whitespace()
        .find_map(|word| word.strip_prefix("pid="))
        .expect("status names the owner pid")
        .to_owned();
    // `--project-skill` selects only the skill written under the cwd; the
    // CLI, companion, host-config and credential items stay off.
    let installed = checked(
        command(path)
            .current_dir(cwd.path())
            .args(["install", "--project-skill", "--yes", "--restart-runtime"])
            .output()
            .unwrap(),
    );
    assert!(
        installed.contains(&format!(
            "stopped runtime owner pid {pid}; the next agent connection starts the new build"
        )),
        "{installed}"
    );
    assert!(cwd.path().join(".agents/skills").is_dir());
    assert_eq!(
        checked(command(path).args(["runtime", "status"]).output().unwrap()),
        "stopped"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn concurrent_cli_starts_share_one_owner_and_keep_connection_lifecycles_independent() {
    let scope = Cleanup(tempfile::tempdir().unwrap());
    let path = scope.0.path().to_owned();
    let first = std::thread::spawn({
        let path = path.clone();
        move || command(&path).args(["runtime", "start"]).output().unwrap()
    });
    let second = std::thread::spawn({
        let path = path.clone();
        move || command(&path).args(["runtime", "start"]).output().unwrap()
    });
    let first = checked(first.join().unwrap());
    let second = checked(second.join().unwrap());
    assert_eq!(first, second, "concurrent starts created different owners");
    let initial: Value =
        serde_json::from_slice(&std::fs::read(path.join("runtime/owner.json")).unwrap()).unwrap();
    assert_ne!(url::Url::parse(&first).unwrap().port().unwrap(), 0);
    let bootstrap = std::fs::read_to_string(path.join("bootstrap.env")).unwrap();
    let mut explicit = command(&path);
    explicit.env("AUTOMATION_RUNTIME_BOOTSTRAP_PRESET", "agent");
    for (name, value) in bootstrap.lines().filter_map(|line| line.split_once('=')) {
        if name.starts_with("AUTOMATION_RUNTIME_BOOTSTRAP_") {
            explicit.env(name, value.trim_matches('\"'));
        }
    }
    assert_eq!(
        checked(explicit.args(["runtime", "start"]).output().unwrap()),
        first
    );
    let bearer = bootstrap
        .lines()
        .find_map(|line| line.strip_prefix("AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN="))
        .unwrap()
        .trim_matches('\"')
        .to_owned();
    let (mut one, mut one_read, one_task) = connection(first.clone(), bearer.clone(), "mcp").await;
    let (mut two, mut two_read, two_task) = connection(first.clone(), bearer.clone(), "mcp").await;
    let (mut acp, mut acp_read, acp_task) = connection(first.clone(), bearer, "acp").await;
    send(&mut acp, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":1,"clientCapabilities":{}}})).await;
    assert!(response(&mut acp_read, 1).await.get("result").is_some());
    acp.shutdown().await.unwrap();
    acp_task.await.unwrap().unwrap();
    let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"scope-regression","version":"1"}}});
    send(&mut one, initialize.clone()).await;
    assert!(response(&mut one_read, 1).await.get("result").is_some());
    send(
        &mut one,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    // A second connection initializes but deliberately has not sent its
    // initialized notification. A principal-cached server would reset one.
    send(&mut two, initialize).await;
    assert!(response(&mut two_read, 1).await.get("result").is_some());
    send(
        &mut one,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;
    let tools = response(&mut one_read, 2).await;
    assert!(tools["result"]["tools"].is_array(), "{tools}");
    send(
        &mut two,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    one.shutdown().await.unwrap();
    one_task.await.unwrap().unwrap();
    send(
        &mut two,
        json!({"jsonrpc":"2.0","id":3,"method":"tools/list"}),
    )
    .await;
    assert!(response(&mut two_read, 3).await["result"]["tools"].is_array());
    assert!(
        checked(command(&path).args(["runtime", "status"]).output().unwrap())
            .contains(initial["owner_id"].as_str().unwrap())
    );
    // A changed config never locks clients out: they keep reaching the
    // running owner, and status says the change applies after a stop.
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(path.join("config.toml"))
        .unwrap()
        .write_all(b"\n# changed after launch\n")
        .unwrap();
    assert_eq!(
        checked(command(&path).args(["runtime", "start"]).output().unwrap()),
        first
    );
    assert!(
        checked(command(&path).args(["runtime", "status"]).output().unwrap())
            .contains("configuration changed since this runtime started")
    );
    // Connection `two` is still attached.
    checked(
        command(&path)
            .args(["runtime", "stop", "--disconnect-agents"])
            .output()
            .unwrap(),
    );
    two_task.await.unwrap().unwrap();
    assert!(!path.join("runtime/owner.json").exists());
    let released = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path.join("runtime/owner.lock"))
        .unwrap();
    released
        .try_lock()
        .expect("stop returned before the owner released its lease");
    drop(released);
    checked(command(&path).args(["runtime", "start"]).output().unwrap());
    let restarted: Value =
        serde_json::from_slice(&std::fs::read(path.join("runtime/owner.json")).unwrap()).unwrap();
    assert_ne!(initial["owner_id"], restarted["owner_id"]);
    checked(command(&path).args(["runtime", "stop"]).output().unwrap());
    // Foreground serve and detached adapters use the same owner identity,
    // including when the foreground command receives relative config paths.
    struct Foreground(std::process::Child);
    impl Drop for Foreground {
        fn drop(&mut self) {
            if self.0.try_wait().ok().flatten().is_none() {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }
    }
    let mut foreground = Foreground(
        command(&path)
            .current_dir(&path)
            .args([
                "serve",
                "--config",
                "config.toml",
                "--bootstrap-env",
                "bootstrap.env",
                "--no-vision",
            ])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .spawn()
            .unwrap(),
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if std::fs::read(path.join("runtime/owner.json"))
                .ok()
                .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                .is_some_and(|owner| owner["pid"] == foreground.0.id())
            {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    checked(command(&path).args(["runtime", "start"]).output().unwrap());
    checked(command(&path).args(["runtime", "stop"]).output().unwrap());
    assert!(foreground.0.wait().unwrap().success());
}

const DISCONNECTED: &str =
    "attached agents were disconnected; each host reconnects when it next starts its bobby server";

fn owner_pid(root: &Path) -> u32 {
    checked(command(root).args(["runtime", "status"]).output().unwrap())
        .split_whitespace()
        .find_map(|word| word.strip_prefix("pid="))
        .expect("status names the owner pid")
        .parse()
        .unwrap()
}

#[cfg(unix)]
fn process_exists(pid: u32) -> bool {
    // SAFETY: signal 0 only probes for existence.
    let result = unsafe { libc::kill(pid as i32, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

#[cfg(unix)]
fn assert_gone(pid: u32) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while process_exists(pid) {
        assert!(
            std::time::Instant::now() < deadline,
            "old owner pid {pid} still exists"
        );
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
}

#[test]
fn restart_replaces_a_running_owner_and_reports_the_swap() {
    let scope = Cleanup(tempfile::tempdir().unwrap());
    let path = scope.0.path();
    checked(command(path).args(["runtime", "start"]).output().unwrap());
    let old = owner_pid(path);
    let out = checked(command(path).args(["runtime", "restart"]).output().unwrap());
    let new = owner_pid(path);
    assert_ne!(old, new);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    assert!(lines[0].starts_with("snapshot: "), "{out}");
    assert!(
        lines[1].starts_with(&format!(
            "restarted: runtime owner pid {new} (was {old}) http://127.0.0.1:"
        )),
        "{out}"
    );
    assert_eq!(lines[2], DISCONNECTED);
    #[cfg(unix)]
    assert_gone(old);
}

#[test]
fn restart_without_an_owner_starts_one() {
    let scope = Cleanup(tempfile::tempdir().unwrap());
    let path = scope.0.path();
    let out = checked(command(path).args(["runtime", "restart"]).output().unwrap());
    let new = owner_pid(path);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 1, "{out}");
    assert!(
        lines[0].starts_with(&format!(
            "started: runtime owner pid {new} http://127.0.0.1:"
        )),
        "{out}"
    );
}

#[cfg(unix)]
struct Frozen(u32, std::path::PathBuf);
#[cfg(unix)]
impl Drop for Frozen {
    fn drop(&mut self) {
        let command = Command::new("ps")
            .args(["-ww", "-p", &self.0.to_string(), "-o", "command="])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        if command.contains("runtime-owner") && command.contains(&*self.1.to_string_lossy()) {
            // SAFETY: the pid is a verified owner this test started in its temp scope.
            unsafe { libc::kill(self.0 as i32, libc::SIGKILL) };
        }
    }
}

/// Freezes the scope's owner, restarts with `args`, and returns the elapsed time.
#[cfg(unix)]
fn restart_hung_owner(args: &[&str]) -> std::time::Duration {
    let scope = Cleanup(tempfile::tempdir().unwrap());
    let path = scope.0.path();
    checked(command(path).args(["runtime", "start"]).output().unwrap());
    let old = owner_pid(path);
    let _frozen = Frozen(old, path.join("runtime"));
    // SAFETY: the owner is a process this test started in its temp scope.
    assert_eq!(unsafe { libc::kill(old as i32, libc::SIGSTOP) }, 0);
    // Without `--disconnect-agents` an owner that does not report its impact
    // is refused, and nothing is signalled: it stays frozen.
    let refused = command(path).args(args).output().unwrap();
    assert!(!refused.status.success());
    let text = text_of(&refused);
    assert!(text.contains("impact unknown"), "{text}");
    assert!(text.contains("refusing to restart"), "{text}");
    let state = Command::new("ps")
        .args(["-p", &old.to_string(), "-o", "stat="])
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&state.stdout)
            .trim()
            .starts_with('T'),
        "the frozen owner was signalled"
    );
    let started = std::time::Instant::now();
    let mut flagged = args.to_vec();
    flagged.push("--disconnect-agents");
    let out = checked(command(path).args(flagged).output().unwrap());
    let elapsed = started.elapsed();
    let new = owner_pid(path);
    assert_ne!(old, new);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 4, "{out}");
    assert_eq!(
        lines[0],
        "snapshot unavailable: the running runtime did not report what is attached"
    );
    assert_eq!(
        lines[1],
        format!("runtime owner pid {old} did not stop gracefully; terminated")
    );
    assert!(
        lines[2].starts_with(&format!(
            "restarted: runtime owner pid {new} (was {old}) http://127.0.0.1:"
        )),
        "{out}"
    );
    assert_eq!(lines[3], DISCONNECTED);
    assert_gone(old);
    elapsed
}

#[cfg(unix)]
#[test]
fn forced_restart_terminates_a_hung_owner() {
    let elapsed = restart_hung_owner(&["runtime", "restart", "--force"]);
    assert!(elapsed < std::time::Duration::from_secs(60), "{elapsed:?}");
}

#[cfg(unix)]
#[test]
fn restart_falls_back_to_termination_when_the_owner_does_not_answer() {
    let elapsed = restart_hung_owner(&["runtime", "restart"]);
    assert!(elapsed < std::time::Duration::from_secs(60), "{elapsed:?}");
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_ends_attached_connections_and_the_new_owner_accepts_new_ones() {
    let scope = Cleanup(tempfile::tempdir().unwrap());
    let path = scope.0.path().to_owned();
    let url = checked(command(&path).args(["runtime", "start"]).output().unwrap());
    let bearer = std::fs::read_to_string(path.join("bootstrap.env"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN="))
        .unwrap()
        .trim_matches('\"')
        .to_owned();
    let initialize = json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"restart-regression","version":"1"}}});
    let (mut writer, mut reader, task) = connection(url, bearer.clone(), "mcp").await;
    send(&mut writer, initialize.clone()).await;
    assert!(response(&mut reader, 1).await.get("result").is_some());
    let restarted = tokio::task::spawn_blocking({
        let path = path.clone();
        move || {
            checked(
                command(&path)
                    .args(["runtime", "restart", "--disconnect-agents"])
                    .output()
                    .unwrap(),
            )
        }
    })
    .await
    .unwrap();
    let closed = tokio::time::timeout(std::time::Duration::from_secs(15), async {
        while let Ok(Some(_)) = gateway_transport::read_frame(&mut reader).await {}
    })
    .await;
    assert!(closed.is_ok(), "the attached connection stayed open");
    let _ = tokio::time::timeout(std::time::Duration::from_secs(15), task)
        .await
        .expect("the gateway task ended");
    let new_url = restarted
        .lines()
        .find(|line| line.starts_with("restarted:"))
        .unwrap()
        .split_whitespace()
        .last()
        .unwrap()
        .to_owned();
    let (mut writer, mut reader, _task) = connection(new_url, bearer, "mcp").await;
    send(&mut writer, initialize).await;
    assert!(response(&mut reader, 1).await.get("result").is_some());
}

const REFUSAL: &str = "refusing to restart: this disconnects every attached agent. Ask the operator to run `bobby runtime restart` in a terminal. Pass --disconnect-agents only when the operator has told you to.";

fn bearer(root: &Path) -> String {
    std::fs::read_to_string(root.join("bootstrap.env"))
        .unwrap()
        .lines()
        .find_map(|line| line.strip_prefix("AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN="))
        .unwrap()
        .trim_matches('\"')
        .to_owned()
}

type Attached = (
    tokio::io::WriteHalf<tokio::io::DuplexStream>,
    BufReader<tokio::io::ReadHalf<tokio::io::DuplexStream>>,
    tokio::task::JoinHandle<anyhow::Result<()>>,
);

/// Starts the scope's owner and attaches one initialized MCP connection.
async fn start_with_connection(root: &Path) -> Attached {
    let url = checked(command(root).args(["runtime", "start"]).output().unwrap());
    let (mut writer, mut reader, task) = connection(url, bearer(root), "mcp").await;
    send(&mut writer, json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-11-25","capabilities":{},"clientInfo":{"name":"guard-regression","version":"1"}}})).await;
    assert!(response(&mut reader, 1).await.get("result").is_some());
    send(
        &mut writer,
        json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    (writer, reader, task)
}

fn snapshots(root: &Path) -> Vec<std::path::PathBuf> {
    std::fs::read_dir(root.join("runtime/restart-snapshots"))
        .map(|entries| entries.map(|entry| entry.unwrap().path()).collect())
        .unwrap_or_default()
}

fn text_of(output: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    )
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_with_an_attached_connection_is_refused_without_the_flag() {
    let scope = Cleanup(tempfile::tempdir().unwrap());
    let path = scope.0.path().to_owned();
    let (mut writer, mut reader, _task) = start_with_connection(&path).await;
    let old = owner_pid(&path);
    let output = tokio::task::spawn_blocking({
        let path = path.clone();
        move || {
            command(&path)
                .args(["runtime", "restart"])
                .output()
                .unwrap()
        }
    })
    .await
    .unwrap();
    assert!(!output.status.success());
    let text = text_of(&output);
    assert!(
        text.contains(&format!("runtime owner pid {old} http://127.0.0.1:")),
        "{text}"
    );
    assert!(text.contains("1 agent connection"), "{text}");
    assert!(text.contains(REFUSAL), "{text}");
    assert_eq!(owner_pid(&path), old);
    assert!(snapshots(&path).is_empty());
    assert!(!path.join("runtime/restart-snapshots").exists());
    send(
        &mut writer,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;
    assert!(response(&mut reader, 2).await["result"]["tools"].is_array());
}

#[tokio::test(flavor = "multi_thread")]
async fn restart_with_the_flag_snapshots_what_was_attached_then_restarts() {
    let scope = Cleanup(tempfile::tempdir().unwrap());
    let path = scope.0.path().to_owned();
    let (_writer, _reader, _task) = start_with_connection(&path).await;
    let old = owner_pid(&path);
    let out = tokio::task::spawn_blocking({
        let path = path.clone();
        move || {
            checked(
                command(&path)
                    .args(["runtime", "restart", "--disconnect-agents"])
                    .output()
                    .unwrap(),
            )
        }
    })
    .await
    .unwrap();
    let new = owner_pid(&path);
    assert_ne!(old, new);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 3, "{out}");
    let file = std::path::PathBuf::from(lines[0].strip_prefix("snapshot: ").expect(&out));
    assert!(file.is_file());
    assert!(file.starts_with(path.join("runtime/restart-snapshots")));
    let snapshot: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(snapshot["connections"], 1);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&file).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(
            std::fs::metadata(file.parent().unwrap())
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o700
        );
    }
    assert!(lines[1].starts_with(&format!(
        "restarted: runtime owner pid {new} (was {old}) http://127.0.0.1:"
    )));
    assert_eq!(lines[2], DISCONNECTED);
}

#[test]
fn restart_with_nothing_attached_needs_no_flag_and_still_snapshots() {
    let scope = Cleanup(tempfile::tempdir().unwrap());
    let path = scope.0.path();
    checked(command(path).args(["runtime", "start"]).output().unwrap());
    checked(command(path).args(["runtime", "restart"]).output().unwrap());
    let files = snapshots(path);
    assert_eq!(files.len(), 1);
    let snapshot: Value = serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
    assert_eq!(snapshot["connections"], 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn install_restart_runtime_is_guarded_like_restart() {
    let scope = Cleanup(tempfile::tempdir().unwrap());
    let path = scope.0.path().to_owned();
    let cwd = tempfile::tempdir().unwrap();
    let (_writer, _reader, _task) = start_with_connection(&path).await;
    let old = owner_pid(&path);
    let install = |extra: &'static [&'static str]| {
        let path = path.clone();
        let cwd = cwd.path().to_owned();
        tokio::task::spawn_blocking(move || {
            command(&path)
                .current_dir(cwd)
                .args(["install", "--project-skill", "--yes", "--restart-runtime"])
                .args(extra)
                .output()
                .unwrap()
        })
    };
    let refused = install(&[]).await.unwrap();
    assert!(refused.status.success(), "{}", text_of(&refused));
    let text = text_of(&refused);
    assert!(text.contains(REFUSAL), "{text}");
    assert!(
        text.contains(&format!(
            "runtime owner pid {old} still runs the previous build"
        )),
        "{text}"
    );
    assert_eq!(owner_pid(&path), old);
    assert!(snapshots(&path).is_empty());
    let allowed = install(&["--disconnect-agents"]).await.unwrap();
    assert!(allowed.status.success(), "{}", text_of(&allowed));
    let text = text_of(&allowed);
    assert!(
        text.contains(&format!("stopped runtime owner pid {old}")),
        "{text}"
    );
    assert_eq!(snapshots(&path).len(), 1);
    assert_eq!(
        checked(command(&path).args(["runtime", "status"]).output().unwrap()),
        "stopped"
    );
}

const STOP_REFUSAL: &str = "refusing to stop: this disconnects every attached agent. Ask the operator to run `bobby runtime stop` in a terminal. Pass --disconnect-agents only when the operator has told you to.";

#[tokio::test(flavor = "multi_thread")]
async fn stop_with_an_attached_connection_is_refused_without_the_flag() {
    let scope = Cleanup(tempfile::tempdir().unwrap());
    let path = scope.0.path().to_owned();
    let (mut writer, mut reader, _task) = start_with_connection(&path).await;
    let old = owner_pid(&path);
    let output = tokio::task::spawn_blocking({
        let path = path.clone();
        move || command(&path).args(["runtime", "stop"]).output().unwrap()
    })
    .await
    .unwrap();
    assert!(!output.status.success());
    let text = text_of(&output);
    assert!(
        text.contains(&format!("runtime owner pid {old} http://127.0.0.1:")),
        "{text}"
    );
    assert!(text.contains("1 agent connection"), "{text}");
    assert!(text.contains(STOP_REFUSAL), "{text}");
    assert!(!text.contains("refusing to restart"), "{text}");
    assert_eq!(owner_pid(&path), old);
    assert!(!path.join("runtime/restart-snapshots").exists());
    send(
        &mut writer,
        json!({"jsonrpc":"2.0","id":2,"method":"tools/list"}),
    )
    .await;
    assert!(response(&mut reader, 2).await["result"]["tools"].is_array());
}

#[tokio::test(flavor = "multi_thread")]
async fn stop_with_the_flag_snapshots_what_was_attached_then_stops() {
    let scope = Cleanup(tempfile::tempdir().unwrap());
    let path = scope.0.path().to_owned();
    let (_writer, _reader, _task) = start_with_connection(&path).await;
    let out = tokio::task::spawn_blocking({
        let path = path.clone();
        move || {
            checked(
                command(&path)
                    .args(["runtime", "stop", "--disconnect-agents"])
                    .output()
                    .unwrap(),
            )
        }
    })
    .await
    .unwrap();
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "{out}");
    let file = std::path::PathBuf::from(lines[0].strip_prefix("snapshot: ").expect(&out));
    let snapshot: Value = serde_json::from_slice(&std::fs::read(&file).unwrap()).unwrap();
    assert_eq!(snapshot["connections"], 1);
    assert_eq!(lines[1], "stopped");
    assert_eq!(
        checked(command(&path).args(["runtime", "status"]).output().unwrap()),
        "stopped"
    );
}

#[test]
fn stop_with_nothing_attached_needs_no_flag_and_snapshots_like_restart() {
    let scope = Cleanup(tempfile::tempdir().unwrap());
    let path = scope.0.path();
    checked(command(path).args(["runtime", "start"]).output().unwrap());
    let out = checked(command(path).args(["runtime", "stop"]).output().unwrap());
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines.len(), 2, "{out}");
    assert!(lines[0].starts_with("snapshot: "), "{out}");
    assert_eq!(lines[1], "stopped");
    let files = snapshots(path);
    assert_eq!(files.len(), 1);
    let snapshot: Value = serde_json::from_slice(&std::fs::read(&files[0]).unwrap()).unwrap();
    assert_eq!(snapshot["connections"], 0);
}

#[test]
fn stop_without_an_owner_prints_stopped_and_writes_nothing() {
    let scope = Cleanup(tempfile::tempdir().unwrap());
    let path = scope.0.path();
    assert_eq!(
        checked(command(path).args(["runtime", "stop"]).output().unwrap()),
        "stopped"
    );
    assert!(!path.join("runtime/restart-snapshots").exists());
}
