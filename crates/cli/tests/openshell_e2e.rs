//! CLI + live broker e2e for `bobby openshell provision|revoke|list|status|rotate`.

use std::{process::Command, sync::Arc, time::Duration};

use broker::{serve_listener_graceful, testing::app_with_unrestricted_admin, RejectionWorkerStats};
use tokio::{net::TcpListener, sync::Notify};

fn bobby() -> Command {
    Command::new(env!("CARGO_BIN_EXE_bobby"))
}

fn run_openshell_sync(
    base_url: &str,
    token: &str,
    secrets_dir: &std::path::Path,
    args: &[&str],
) -> std::process::Output {
    let mut command = bobby();
    command.args(["openshell"]).args(args);
    let needs_auth = args
        .first()
        .is_some_and(|cmd| matches!(*cmd, "provision" | "revoke" | "rotate"));
    if needs_auth {
        command
            .arg("--base-url")
            .arg(base_url)
            .arg("--token")
            .arg(token)
            .arg("--config")
            .arg("/nonexistent/bobby-openshell-e2e-config.toml");
    }
    command.env("BOBBY_OPENSHELL_SECRETS_DIR", secrets_dir);
    command.output().expect("spawn bobby openshell")
}

async fn run_openshell(
    base_url: String,
    token: String,
    secrets_dir: std::path::PathBuf,
    args: Vec<String>,
) -> std::process::Output {
    tokio::task::spawn_blocking(move || {
        let owned: Vec<&str> = args.iter().map(String::as_str).collect();
        run_openshell_sync(&base_url, &token, &secrets_dir, &owned)
    })
    .await
    .expect("openshell CLI thread join")
}

struct LiveBroker {
    base_url: String,
    token: String,
    shutdown: Arc<Notify>,
    server: tokio::task::JoinHandle<std::io::Result<()>>,
}

impl LiveBroker {
    async fn start() -> Self {
        let (app, _authority, token) = app_with_unrestricted_admin(16).await;
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
async fn provision_rotate_revoke_and_list() {
    let broker = LiveBroker::start().await;
    let base_url = broker.base_url.clone();
    let token = broker.token.clone();
    let secrets = tempfile::tempdir().unwrap();
    let sandbox = format!("e2e-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);

    let first = run_openshell(
        base_url.clone(),
        token.clone(),
        secrets.path().to_path_buf(),
        vec![
            "provision".into(),
            "--sandbox".into(),
            sandbox.clone(),
            "--mcp-host".into(),
            "127.0.0.1".into(),
            "--mcp-port".into(),
            "9".into(),
        ],
    )
    .await;
    assert!(
        first.status.success(),
        "provision failed: {}",
        String::from_utf8_lossy(&first.stderr)
    );
    let first_out = String::from_utf8_lossy(&first.stdout);
    assert!(first_out.contains("provisioned sandbox"));

    let status = run_openshell(
        base_url.clone(),
        token.clone(),
        secrets.path().to_path_buf(),
        vec!["status".into(), "--sandbox".into(), sandbox.clone()],
    )
    .await;
    assert!(status.status.success());
    let status_json: serde_json::Value =
        serde_json::from_slice(&status.stdout).expect("status JSON");
    let first_principal = status_json["principalId"].as_str().unwrap().to_owned();
    assert_eq!(status_json["capabilitiesPreset"], "openshell");

    let rotate = run_openshell(
        base_url.clone(),
        token.clone(),
        secrets.path().to_path_buf(),
        vec!["rotate".into(), "--sandbox".into(), sandbox.clone()],
    )
    .await;
    assert!(
        rotate.status.success(),
        "rotate failed: {}",
        String::from_utf8_lossy(&rotate.stderr)
    );
    let rotate_out = String::from_utf8_lossy(&rotate.stdout);
    assert!(rotate_out.contains("replaced prior principal"));

    let status2 = run_openshell(
        base_url.clone(),
        token.clone(),
        secrets.path().to_path_buf(),
        vec!["status".into(), "--sandbox".into(), sandbox.clone()],
    )
    .await;
    let status2_json: serde_json::Value = serde_json::from_slice(&status2.stdout).unwrap();
    let second_principal = status2_json["principalId"].as_str().unwrap();
    assert_ne!(first_principal, second_principal);

    let list = run_openshell(
        base_url.clone(),
        token.clone(),
        secrets.path().to_path_buf(),
        vec!["list".into()],
    )
    .await;
    assert!(list.status.success());
    let list_out = String::from_utf8_lossy(&list.stdout);
    assert!(list_out.contains(&sandbox));

    let revoke = run_openshell(
        base_url.clone(),
        token.clone(),
        secrets.path().to_path_buf(),
        vec!["revoke".into(), "--sandbox".into(), sandbox.clone()],
    )
    .await;
    assert!(
        revoke.status.success(),
        "revoke failed: {}",
        String::from_utf8_lossy(&revoke.stderr)
    );

    let list_after = run_openshell(
        base_url,
        token,
        secrets.path().to_path_buf(),
        vec!["list".into()],
    )
    .await;
    let list_after_out = String::from_utf8_lossy(&list_after.stdout);
    assert!(
        !list_after_out.contains(&sandbox),
        "sandbox still listed after revoke: {list_after_out}"
    );

    broker.stop().await;
}
