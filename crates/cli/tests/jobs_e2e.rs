//! CLI + live broker e2e for `bobby jobs submit|status|cancel`.

use std::{process::Command, sync::Arc, time::Duration};

use broker::{serve_listener_graceful, testing::app_with_admin, RejectionWorkerStats};
use tokio::{net::TcpListener, sync::Notify};

fn bobby() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bobby"))
}

fn run_jobs_sync(base_url: &str, token: &str, args: &[&str]) -> std::process::Output {
    let mut command = bobby();
    command
        .args(["jobs"])
        .args(args)
        .arg("--base-url")
        .arg(base_url)
        .arg("--token")
        .arg(token)
        .arg("--config")
        .arg("/nonexistent/bobby-jobs-e2e-config.toml");
    command.output().expect("spawn bobby jobs")
}

async fn run_jobs(base_url: String, token: String, args: Vec<String>) -> std::process::Output {
    tokio::task::spawn_blocking(move || {
        let owned: Vec<&str> = args.iter().map(String::as_str).collect();
        run_jobs_sync(&base_url, &token, &owned)
    })
    .await
    .expect("jobs CLI thread join")
}

fn stdout_json(output: &std::process::Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "bobby jobs failed (status {:?})\nstdout:\n{}\nstderr:\n{}",
        output.status.code(),
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr),
    );
    serde_json::from_slice(&output.stdout).unwrap_or_else(|error| {
        panic!(
            "stdout is not JSON ({error}): {}",
            String::from_utf8_lossy(&output.stdout)
        )
    })
}

async fn wait_cli_status(
    base_url: &str,
    token: &str,
    job_id: &str,
    want: &[&str],
) -> serde_json::Value {
    for _ in 0..100 {
        let output = run_jobs(
            base_url.to_owned(),
            token.to_owned(),
            vec!["status".into(), job_id.to_owned()],
        )
        .await;
        let json = stdout_json(&output);
        let status = json["status"].as_str().unwrap_or("");
        if want.iter().any(|value| *value == status) {
            return json;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("job {job_id} did not reach one of {want:?}");
}

struct LiveBroker {
    base_url: String,
    token: String,
    shutdown: Arc<Notify>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl LiveBroker {
    async fn start() -> Self {
        let (app, _authority, token) = app_with_admin(8).await;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}");
        let shutdown = Arc::new(Notify::new());
        let server = {
            let notify = Arc::clone(&shutdown);
            tokio::spawn(serve_listener_graceful(
                listener,
                app,
                8,
                2,
                RejectionWorkerStats::default(),
                async move {
                    notify.notified().await;
                },
                std::future::pending(),
            ))
        };
        // Brief settle so the accept loop is ready before the first CLI request.
        tokio::time::sleep(Duration::from_millis(20)).await;
        Self {
            base_url,
            token,
            shutdown,
            server,
        }
    }

    async fn stop(self) {
        self.shutdown.notify_one();
        let _ = tokio::time::timeout(Duration::from_secs(5), self.server).await;
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_submit_echo_and_status_completes() {
    let broker = LiveBroker::start().await;
    let base_url = broker.base_url.clone();
    let token = broker.token.clone();

    let submit = run_jobs(
        base_url.clone(),
        token.clone(),
        vec![
            "submit".into(),
            "--name".into(),
            "echo".into(),
            "--payload".into(),
            r#"{"hello":"cli-e2e"}"#.into(),
            "--priority".into(),
            "normal".into(),
        ],
    )
    .await;
    let created = stdout_json(&submit);
    let job_id = created["jobId"].as_str().expect("jobId");
    assert_eq!(created["status"], "pending");

    let job = wait_cli_status(&base_url, &token, job_id, &["completed", "failed"]).await;
    assert_eq!(job["status"], "completed");
    assert_eq!(job["name"], "echo");
    assert_eq!(job["result"]["output"]["hello"], "cli-e2e");

    broker.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_submit_payload_file_and_idempotency_key() {
    let broker = LiveBroker::start().await;
    let base_url = broker.base_url.clone();
    let token = broker.token.clone();

    let dir = tempfile::tempdir().unwrap();
    let payload_path = dir.path().join("payload.json");
    std::fs::write(&payload_path, r#"{"from":"file"}"#).unwrap();
    let key = format!("cli-jobs-idem-{}", uuid::Uuid::new_v4());

    let args = vec![
        "submit".into(),
        "--name".into(),
        "echo".into(),
        "--payload-file".into(),
        payload_path.to_string_lossy().into_owned(),
        "--idempotency-key".into(),
        key,
    ];
    let first = stdout_json(&run_jobs(base_url.clone(), token.clone(), args.clone()).await);
    let second = stdout_json(&run_jobs(base_url.clone(), token.clone(), args).await);
    assert_eq!(first["jobId"], second["jobId"]);

    let job_id = first["jobId"].as_str().unwrap();
    let job = wait_cli_status(&base_url, &token, job_id, &["completed", "failed"]).await;
    assert_eq!(job["status"], "completed");
    assert_eq!(job["result"]["output"]["from"], "file");

    broker.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_cancel_running_sleep_job() {
    let broker = LiveBroker::start().await;
    let base_url = broker.base_url.clone();
    let token = broker.token.clone();

    let submit = run_jobs(
        base_url.clone(),
        token.clone(),
        vec![
            "submit".into(),
            "--name".into(),
            "sleep".into(),
            "--payload".into(),
            r#"{"ms":5000}"#.into(),
            "--max-retries".into(),
            "0".into(),
        ],
    )
    .await;
    let created = stdout_json(&submit);
    let job_id = created["jobId"].as_str().expect("jobId").to_string();

    // Give the runner a beat to claim the job (cancel still works while pending).
    tokio::time::sleep(Duration::from_millis(50)).await;

    let cancel = run_jobs(
        base_url.clone(),
        token.clone(),
        vec!["cancel".into(), job_id.clone()],
    )
    .await;
    assert!(
        cancel.status.success(),
        "cancel failed: {}",
        String::from_utf8_lossy(&cancel.stderr)
    );

    let job = wait_cli_status(&base_url, &token, &job_id, &["cancelled"]).await;
    assert_eq!(job["status"], "cancelled");

    broker.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_status_unknown_job_exits_nonzero() {
    let broker = LiveBroker::start().await;
    let output = run_jobs(
        broker.base_url.clone(),
        broker.token.clone(),
        vec![
            "status".into(),
            "00000000-0000-0000-0000-000000000000".into(),
        ],
    )
    .await;
    assert!(!output.status.success());
    let combined = format!(
        "{}{}",
        String::from_utf8_lossy(&output.stderr),
        String::from_utf8_lossy(&output.stdout)
    );
    assert!(
        combined.contains("jobs request failed")
            || combined.contains("404")
            || !combined.is_empty(),
        "expected error output on unknown job, got empty"
    );
    // Must be a fast HTTP failure, not a client timeout.
    assert!(
        !combined.contains("operation timed out"),
        "status probe hung waiting on the broker: {combined}"
    );
    broker.stop().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_jobs_without_token_fails() {
    let broker = LiveBroker::start().await;
    let base_url = broker.base_url.clone();
    let output = tokio::task::spawn_blocking(move || {
        bobby()
            .args([
                "jobs",
                "submit",
                "--name",
                "echo",
                "--base-url",
                &base_url,
                "--config",
                "/nonexistent/bobby-jobs-e2e-config.toml",
                "--bootstrap-env",
                "/nonexistent/bobby-jobs-e2e-bootstrap.env",
            ])
            .env_remove("AUTOMATION_RUNTIME_TOKEN")
            .output()
            .unwrap()
    })
    .await
    .unwrap();
    assert!(!output.status.success());
    broker.stop().await;
}
