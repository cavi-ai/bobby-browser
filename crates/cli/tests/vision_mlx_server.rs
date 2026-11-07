//! The MLX vision server bobby starts: it runs on the configured interpreter,
//! readiness waits for the server itself, and an installed `bobby` finds the
//! server script.

use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};

/// Stands in for vision_server.py: binds `--bind` and serves until stdin closes.
const SERVING: &str = r#"import argparse, http.server, sys, threading
p = argparse.ArgumentParser()
for flag in ("--bind", "--provider", "--model"):
    p.add_argument(flag)
p.add_argument("--exit-with-stdin", action="store_true")
a = p.parse_args()
host, port = a.bind.rsplit(":", 1)
server = http.server.HTTPServer((host, int(port)), http.server.BaseHTTPRequestHandler)
threading.Thread(target=lambda: (sys.stdin.read(), server.shutdown()), daemon=True).start()
server.serve_forever()
"#;

/// Stands in for a vision_server.py whose interpreter lacks its packages.
const BROKEN: &str = "raise SystemExit(\"No module named 'mlx_vlm'\")\n";

fn python3() -> PathBuf {
    let output = Command::new("python3")
        .args(["-c", "import sys; print(sys.executable)"])
        .output()
        .expect("python3 runs the test's stand-in servers");
    PathBuf::from(String::from_utf8(output.stdout).unwrap().trim())
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn model_dir(root: &Path) -> PathBuf {
    let model = root.join("model");
    std::fs::create_dir_all(&model).unwrap();
    std::fs::write(model.join("config.json"), "{}").unwrap();
    std::fs::write(model.join("preprocessor_config.json"), "{}").unwrap();
    model
}

fn activate(root: &Path, model: &str, server: &str, extra: &[&str], path: Option<&Path>) -> Output {
    let script = root.join("vision_server.py");
    std::fs::write(&script, server).unwrap();
    let base_url = format!("http://127.0.0.1:{}", free_port());
    let config = root.join("config.toml");
    let mut command = Command::new(env!("CARGO_BIN_EXE_bobby"));
    command
        .args([
            "vision",
            "connect",
            "--yes",
            "--provider",
            "mlx",
            "--activate",
        ])
        .arg("--config")
        .arg(&config)
        .args(["--model", model, "--base-url", &base_url])
        .args(extra)
        .env("BOBBY_VISION_SERVER_SCRIPT", &script)
        .env("HF_HOME", root.join("hf"))
        .env_remove("BOBBY_VISION_PYTHON")
        .stdin(Stdio::null());
    if let Some(path) = path {
        command.env("PATH", path);
    }
    command.output().unwrap()
}

#[test]
fn mlx_activation_runs_a_local_model_on_the_configured_python() {
    let root = tempfile::tempdir().unwrap();
    let model = model_dir(root.path());
    // A PATH without python3: only the configured interpreter can start the server.
    let no_python = root.path().join("empty-bin");
    std::fs::create_dir_all(&no_python).unwrap();
    let python = python3();

    let output = activate(
        root.path(),
        model.to_str().unwrap(),
        SERVING,
        &["--python", python.to_str().unwrap()],
        Some(&no_python),
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(output.status.success(), "{stdout}\n{stderr}");
    assert!(
        stdout.contains("Loaded and readiness-tested mlx model"),
        "{stdout}"
    );
    let config = std::fs::read_to_string(root.path().join("config.toml")).unwrap();
    assert!(
        config.contains(&format!("python = \"{}\"", python.display())),
        "{config}"
    );
}

#[test]
fn mlx_activation_fails_when_the_vision_server_cannot_start() {
    let root = tempfile::tempdir().unwrap();
    // A cached model, so readiness reaches the server it starts.
    let snapshot = root
        .path()
        .join("hf/hub/models--example--vision-model/snapshots/revision");
    std::fs::create_dir_all(&snapshot).unwrap();
    std::fs::write(snapshot.join("config.json"), "{}").unwrap();
    std::fs::write(snapshot.join("preprocessor_config.json"), "{}").unwrap();

    let output = activate(root.path(), "example/vision-model", BROKEN, &[], None);

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !output.status.success(),
        "a server that never started was reported ready"
    );
    assert!(stderr.contains("exited before loading"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn a_symlinked_bobby_finds_the_vision_server_beside_its_target() {
    let root = tempfile::tempdir().unwrap();
    let link_dir = root.path().join("link-bin");
    std::fs::create_dir_all(&link_dir).unwrap();
    let link = link_dir.join("bobby");
    std::os::unix::fs::symlink(env!("CARGO_BIN_EXE_bobby"), &link).unwrap();

    let mut proxy = Command::new(&link)
        .args([
            "vision-proxy",
            "--managed",
            "--bind",
            "127.0.0.1:0",
            "--upstream",
            "mlx",
            "--spawn-server",
        ])
        .arg("--vision-base-url")
        .arg(format!("http://127.0.0.1:{}", free_port()))
        .arg("--python")
        .arg(python3())
        .env(
            "BOBBY_VISION_TOKEN",
            "vision-proxy-test-token-0123456789abcdef",
        )
        .env_remove("BOBBY_VISION_SERVER_SCRIPT")
        .current_dir(root.path())
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let mut line = String::new();
    std::io::BufRead::read_line(
        &mut std::io::BufReader::new(proxy.stdout.take().unwrap()),
        &mut line,
    )
    .unwrap();
    drop(proxy.stdin.take());
    let output = proxy.wait_with_output().unwrap();

    assert!(
        line.starts_with("listening "),
        "the proxy did not start: {line:?} {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// Stands in for vision_server.py: writes its arguments and model server to
/// `$RECORD`, then serves until stdin closes.
const RECORDING: &str = r#"import argparse, json, os, sys
p = argparse.ArgumentParser()
for flag in ("--bind", "--provider", "--model"):
    p.add_argument(flag)
p.add_argument("--exit-with-stdin", action="store_true")
a = p.parse_args()
record = os.environ["RECORD"]
with open(record + ".part", "w") as out:
    json.dump({"provider": a.provider, "model": a.model, "server": os.environ.get("VISION_MLX_SERVER_URL")}, out)
os.rename(record + ".part", record)
sys.stdin.read()
"#;

#[test]
fn a_model_server_makes_the_mlx_vision_server_send_its_requests_there() {
    let root = tempfile::tempdir().unwrap();
    let script = root.path().join("vision_server.py");
    std::fs::write(&script, RECORDING).unwrap();
    let record = root.path().join("record.json");

    let mut proxy = Command::new(env!("CARGO_BIN_EXE_bobby"))
        .args([
            "vision-proxy",
            "--managed",
            "--bind",
            "127.0.0.1:0",
            "--upstream",
            "mlx",
            "--spawn-server",
            "--model",
            "example/vision-model",
            "--model-server",
            "http://127.0.0.1:8083",
        ])
        .arg("--vision-base-url")
        .arg(format!("http://127.0.0.1:{}", free_port()))
        .arg("--python")
        .arg(python3())
        .env(
            "BOBBY_VISION_TOKEN",
            "vision-proxy-test-token-0123456789abcdef",
        )
        .env("BOBBY_VISION_SERVER_SCRIPT", &script)
        .env("RECORD", &record)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    while !record.exists()
        && proxy.try_wait().unwrap().is_none()
        && std::time::Instant::now() < deadline
    {
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    drop(proxy.stdin.take());
    let output = proxy.wait_with_output().unwrap();

    let recorded = std::fs::read_to_string(&record).unwrap_or_else(|_| {
        panic!(
            "no vision server started: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    let recorded: serde_json::Value = serde_json::from_str(&recorded).unwrap();
    assert_eq!(
        recorded,
        serde_json::json!({
            "provider": "mlx-server",
            "model": "example/vision-model",
            "server": "http://127.0.0.1:8083",
        })
    );
}
