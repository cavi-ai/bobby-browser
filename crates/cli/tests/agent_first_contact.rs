//! What an agent host does on first contact, from the states real machines
//! are in: run `bobby mcp-stdio` (the command `bobby install` writes into
//! every host config), initialize, list tools, and make one call. Every
//! scenario must answer all three with no error and print nothing on stderr.
//!
//! Each scenario gets its own scope directory (`BOBBY_BROWSER_SCOPE_DIR`) and
//! stops the owner it started. Nothing reads or writes the real user scope.

use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde_json::{json, Value};

const ANSWER_TIMEOUT: Duration = Duration::from_secs(60);

fn bobby() -> PathBuf {
    PathBuf::from(env!("CARGO_BIN_EXE_bobby"))
}

/// `mcp-stdio` execs the gateway next to `bobby`; without it the CLI falls
/// back to PATH and would test whatever happens to be installed.
fn sibling_gateway() -> PathBuf {
    let gateway = bobby().with_file_name("mcp-gateway");
    assert!(
        gateway.is_file(),
        "{} is missing; build it with `cargo build -p mcp-gateway`",
        gateway.display()
    );
    gateway
}

struct Scope {
    root: tempfile::TempDir,
}

impl Scope {
    fn new() -> Self {
        sibling_gateway();
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> &Path {
        self.root.path()
    }

    fn command(&self, program: &Path) -> Command {
        let mut command = Command::new(program);
        command
            .env("BOBBY_BROWSER_SCOPE_DIR", self.path())
            .env_remove("BOBBY_BROWSER_CONFIG")
            .env_remove("BOBBY_BROWSER_BOOTSTRAP_ENV")
            .env_remove("BOBBY_RUNTIME_URL")
            .env_remove("AUTOMATION_RUNTIME_BROWSER_SELECTION")
            .env_remove("AUTOMATION_RUNTIME_BOOTSTRAP_TOKEN")
            .env_remove("AUTOMATION_RUNTIME_BOOTSTRAP_PRINCIPAL")
            .env_remove("AUTOMATION_RUNTIME_BOOTSTRAP_CAPABILITIES")
            .env_remove("AUTOMATION_RUNTIME_BOOTSTRAP_EXPIRES_AT")
            .env_remove("AUTOMATION_RUNTIME_BOOTSTRAP_PRESET")
            .env_remove("BOBBY_MCP_TOOLSET");
        command
    }

    /// One agent host session started from `cwd`.
    fn agent(&self, cwd: &Path) -> Agent {
        Agent::spawn(
            self.command(&bobby())
                .arg("mcp-stdio")
                .current_dir(cwd)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap(),
        )
    }

    fn owner(&self) -> Option<Value> {
        serde_json::from_slice(&std::fs::read(self.path().join("runtime/owner.json")).ok()?).ok()
    }

    fn owner_log(&self) -> String {
        std::fs::read_to_string(self.path().join("runtime/owner.log")).unwrap_or_default()
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        let _ = self
            .command(&bobby())
            .args(["runtime", "stop"])
            .current_dir(self.path())
            .output();
    }
}

fn child_stderr(child: &mut Child) -> mpsc::Receiver<String> {
    let stderr = child.stderr.take().unwrap();
    let (sender, receiver) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(Result::ok) {
            let _ = sender.send(line);
        }
    });
    receiver
}

struct Agent {
    child: Child,
    stdin: ChildStdin,
    lines: mpsc::Receiver<String>,
    stderr: mpsc::Receiver<String>,
}

impl Agent {
    fn spawn(mut child: Child) -> Self {
        let stderr = child_stderr(&mut child);
        let stdin = child.stdin.take().unwrap();
        let stdout: ChildStdout = child.stdout.take().unwrap();
        let (sender, lines) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines().map_while(Result::ok) {
                let _ = sender.send(line);
            }
        });
        Self {
            child,
            stdin,
            lines,
            stderr,
        }
    }

    fn send(&mut self, message: Value) {
        writeln!(self.stdin, "{message}").unwrap();
        self.stdin.flush().unwrap();
    }

    fn request(&mut self, id: u64, method: &str, params: Value) -> Value {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}));
        let deadline = std::time::Instant::now() + ANSWER_TIMEOUT;
        loop {
            let remaining = deadline.saturating_duration_since(std::time::Instant::now());
            let line = self.lines.recv_timeout(remaining).unwrap_or_else(|_| {
                panic!(
                    "{method}: no answer; stderr: {:?}",
                    self.stderr.try_iter().collect::<Vec<_>>()
                )
            });
            let value: Value = serde_json::from_str(&line).unwrap();
            if value["id"] == id {
                return value;
            }
        }
    }

    /// initialize, tools/list, and one tool call; every answer a result.
    fn first_contact(mut self) {
        let initialized = self.request(
            1,
            "initialize",
            json!({"protocolVersion":"2025-11-25","capabilities":{},
                   "clientInfo":{"name":"first-contact","version":"1"}}),
        );
        assert!(initialized.get("result").is_some(), "{initialized}");
        self.send(json!({"jsonrpc":"2.0","method":"notifications/initialized"}));
        let tools = self.request(2, "tools/list", json!({}));
        assert!(
            tools["result"]["tools"]
                .as_array()
                .is_some_and(|tools| !tools.is_empty()),
            "{tools}"
        );
        let info = self.request(
            3,
            "tools/call",
            json!({"name":"runtime_info","arguments":{}}),
        );
        assert!(
            info.get("error").is_none() && info["result"]["isError"] != true,
            "{info}"
        );
        drop(self.stdin);
        let status = wait(&mut self.child);
        let stderr = self.stderr.try_iter().collect::<Vec<_>>();
        assert!(stderr.is_empty(), "agent stderr: {stderr:?}");
        assert!(status.success(), "agent exited {status:?}");
    }
}

fn wait(child: &mut Child) -> std::process::ExitStatus {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            return status;
        }
        if std::time::Instant::now() >= deadline {
            let _ = child.kill();
            panic!("agent did not exit after stdin closed");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

#[test]
fn agents_started_in_different_directories_share_one_owner() {
    let scope = Scope::new();
    let plain = tempfile::tempdir().unwrap();
    // A project directory that happens to hold its own config.toml, like a
    // checkout of this repository.
    let project = tempfile::tempdir().unwrap();
    std::fs::write(
        project.path().join("config.toml"),
        "[browser]\nmax_active = 2\n",
    )
    .unwrap();

    scope.agent(plain.path()).first_contact();
    let first = scope.owner().expect("an owner is registered");
    scope.agent(project.path()).first_contact();
    let second = scope.owner().expect("an owner is registered");
    assert_eq!(
        first["owner_id"], second["owner_id"],
        "the second agent replaced the owner"
    );
}

#[test]
fn an_agent_started_in_a_read_only_directory_connects() {
    let scope = Scope::new();
    scope.agent(Path::new("/")).first_contact();
    let log = scope.owner_log();
    assert!(!log.contains("Read-only file system"), "{log}");
}

#[test]
fn an_agent_connects_after_the_scope_config_changes_under_a_running_owner() {
    let scope = Scope::new();
    let cwd = tempfile::tempdir().unwrap();
    scope.agent(cwd.path()).first_contact();
    let config = scope.path().join("config.toml");
    let mut text = std::fs::read_to_string(&config).unwrap();
    text.push_str("\n# edited while the owner runs\n");
    std::fs::write(&config, text).unwrap();
    scope.agent(cwd.path()).first_contact();
}

#[test]
fn an_agent_connects_after_the_owner_was_killed() {
    let scope = Scope::new();
    let cwd = tempfile::tempdir().unwrap();
    scope.agent(cwd.path()).first_contact();
    let pid = scope.owner().unwrap()["pid"].as_u64().unwrap();
    // The owner was started by this test.
    assert!(Command::new("kill")
        .args(["-9", &pid.to_string()])
        .status()
        .unwrap()
        .success());
    scope.agent(cwd.path()).first_contact();
    assert_ne!(scope.owner().unwrap()["pid"].as_u64().unwrap(), pid);
}

#[test]
fn an_agent_connects_when_the_stores_hold_unreadable_data() {
    let scope = Scope::new();
    let cwd = tempfile::tempdir().unwrap();
    scope.agent(cwd.path()).first_contact();
    assert!(scope
        .command(&bobby())
        .args(["runtime", "stop"])
        .status()
        .unwrap()
        .success());
    let storage = scope.path().join("storage");
    // A record an older build wrote, a torn write, and plain damage.
    let mut journal = std::fs::OpenOptions::new()
        .append(true)
        .open(storage.join("commands.jsonl"))
        .unwrap();
    writeln!(
        journal,
        r#"{{"sequence":90,"recordedAt":"2026-09-22T23:23:00Z","commandId":"00000000-0000-4000-8000-000000000000","phase":"failed","envelope":null,"outcome":{{"status":"retired-shape"}}}}"#
    )
    .unwrap();
    writeln!(journal, "{{\"sequence\":91,\"recordedAt\":").unwrap();
    journal.write_all(b"\x00\x00garbage\n").unwrap();
    drop(journal);
    let mut jobs = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(storage.join("scheduler-jobs.jsonl"))
        .unwrap();
    writeln!(jobs, "not a job record").unwrap();
    drop(jobs);
    for ledger in [
        "commands.idempotency.json",
        "commands.lifecycle-idempotency.json",
        "scheduler-jobs.idempotency.json",
    ] {
        std::fs::write(
            storage.join(ledger),
            b"{\"schemaVersion\":99,\"entries\":[]}",
        )
        .unwrap();
    }

    scope.agent(cwd.path()).first_contact();
    let log = scope.owner_log();
    assert!(!log.contains("Error"), "{log}");
}

#[test]
fn a_direct_gateway_entry_attaches_to_the_running_owner() {
    let scope = Scope::new();
    let cwd = tempfile::tempdir().unwrap();
    scope.agent(cwd.path()).first_contact();
    // Older host configs launch the gateway binary with the bootstrap
    // credential in env instead of `bobby mcp-stdio`.
    let mut gateway = scope.command(&sibling_gateway());
    for line in std::fs::read_to_string(scope.path().join("bootstrap.env"))
        .unwrap()
        .lines()
    {
        if let Some((name, value)) = line.split_once('=') {
            if name.starts_with("AUTOMATION_RUNTIME_BOOTSTRAP_") {
                gateway.env(name, value.trim_matches('"'));
            }
        }
    }
    Agent::spawn(
        gateway
            .current_dir(cwd.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap(),
    )
    .first_contact();
}

#[test]
fn a_pinned_occupied_companion_port_and_orphaned_descriptors_do_not_block_agents() {
    let scope = Scope::new();
    let held = TcpListener::bind("127.0.0.1:0").unwrap();
    let pinned = held.local_addr().unwrap();
    let descriptor = scope.path().join("firefox-native-host-descriptor.json");
    for index in 0..40 {
        std::fs::write(
            descriptor.with_extension(format!("pending-{}", uuid::Uuid::new_v4())),
            if index % 2 == 0 { "" } else { "{\"endp" },
        )
        .unwrap();
    }
    let profile = tempfile::tempdir().unwrap();
    let selection = json!({
        "preference":{"mode":"exact","engine":"firefox",
                      "profileId":"5d8a3c1e-2f4b-4c6d-8e9f-0a1b2c3d4e5f"},
        "firefox":[{
            "profileId":"5d8a3c1e-2f4b-4c6d-8e9f-0a1b2c3d4e5f",
            "bidiUrl":"ws://127.0.0.1:9/session",
            "profileDir":profile.path(),
            "companionBind":pinned.to_string(),
            "descriptorPath":descriptor,
        }]
    });
    std::fs::write(
        scope.path().join("browser-selection.json"),
        serde_json::to_vec_pretty(&selection).unwrap(),
    )
    .unwrap();

    let cwd = tempfile::tempdir().unwrap();
    scope.agent(cwd.path()).first_contact();
    let orphans = std::fs::read_dir(scope.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .starts_with("firefox-native-host-descriptor.pending-")
        })
        .count();
    assert_eq!(orphans, 0, "orphaned descriptor temp files survived");
    let published: Value = serde_json::from_slice(&std::fs::read(&descriptor).unwrap()).unwrap();
    let endpoint = published["endpoint"].as_str().unwrap();
    assert!(
        !endpoint.contains(&format!(":{}/", pinned.port())),
        "the companion bound the pinned port: {endpoint}"
    );
    drop(held);
}
