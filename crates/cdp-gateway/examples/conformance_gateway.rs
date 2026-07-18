use std::{future::IntoFuture, path::PathBuf, sync::Arc};

use cdp_gateway::{CdpGateway, MethodRegistry};
use chrono::{Duration, Utc};
use interface_core::{AuthorityStore, RuntimeInterface};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use types::{Capability, CreateSessionRequest, OpenPageRequest, PrincipalId};

#[tokio::main]
async fn main() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let site = test_site::spawn().await;
    let root = tempfile::tempdir().unwrap();
    let mut config = config::AppConfig::default();
    config.browser.executable = Some(PathBuf::from(
        "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
    ));
    config.browser.profiles_dir = root.path().join("profiles");
    let upload_staging = root.path().join("uploads");
    std::fs::create_dir_all(&upload_staging).unwrap();
    config.browser.upload_roots = vec![upload_staging.clone()];
    config.browser.downloads_dir = root.path().join("downloads");
    config.browser.artifacts_dir = root.path().join("artifacts");
    config.storage.journal_path = root.path().join("commands.jsonl");
    config.storage.checkpoints_dir = root.path().join("checkpoints");
    config.http.allow_loopback = true;
    let service = RuntimeService::build(&config).await.unwrap();

    let now = Utc::now();
    let authority = Arc::new(AuthorityStore::in_memory());
    let token = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [
                Capability::SessionRead,
                Capability::SessionWrite,
                Capability::PageRead,
                Capability::PageWrite,
                Capability::BrowserMutate,
                Capability::FileUpload,
                Capability::FileDownload,
                Capability::JavascriptEvaluate,
                Capability::ArtifactCapture,
                Capability::RecoveryRead,
                Capability::RecoveryWrite,
            ],
            now + Duration::minutes(5),
        )
        .await
        .unwrap()
        .expose_once();
    let handle = authority.verify(&token).await.unwrap();
    let runtime = Arc::new(AuthenticatedRuntime::new(service, handle.clone()));
    let ctx = handle.context(now + Duration::seconds(30), None);
    let session = runtime
        .create_session(
            ctx.clone(),
            CreateSessionRequest {
                profile: "conformance".into(),
                proxy: None,
            },
        )
        .await
        .unwrap();
    runtime
        .open_page(
            ctx,
            OpenPageRequest {
                session_id: session.id,
            },
        )
        .await
        .unwrap();

    let artifact_store = artifact_store::ArtifactStore::new(
        &config.browser.artifacts_dir,
        config.browser.max_artifact_bytes,
        config.browser.max_screenshot_dimension,
    );
    let gateway = Arc::new(
        CdpGateway::new(
            authority,
            runtime,
            MethodRegistry::compiled(),
            format!("ws://{address}"),
        )
        .with_artifacts(artifact_store)
        .with_upload_staging_root(upload_staging),
    );
    println!(
        "{}",
        serde_json::json!({"endpoint": format!("http://{address}"), "token": token, "site": site.base_url()})
    );
    axum::serve(listener, gateway.router())
        .into_future()
        .await
        .unwrap();
}
