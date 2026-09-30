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
        let _ = command(self.0.path()).args(["runtime", "stop"]).output();
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
    // A changed config must never silently reuse or take over the owner.
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(path.join("config.toml"))
        .unwrap()
        .write_all(b"\n# changed after launch\n")
        .unwrap();
    let refused = command(&path).args(["runtime", "start"]).output().unwrap();
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("different configuration"));
    checked(command(&path).args(["runtime", "stop"]).output().unwrap());
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
