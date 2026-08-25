use std::{future::IntoFuture, sync::Arc};

use cdp_gateway::{CdpGateway, MethodRegistry};
use interface_conformance::live::ChromeRuntimeHarness;
use interface_core::RuntimeInterface;
use types::{CreateSessionRequest, OpenPageRequest};

#[tokio::main]
async fn main() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let harness = ChromeRuntimeHarness::start().await;
    let runtime = harness.runtime.clone();
    let ctx = harness.context();
    let session = runtime
        .create_session(
            ctx.clone(),
            CreateSessionRequest {
                profile: "conformance".into(),
                proxy: None,
                execution_policy: Default::default(),
                zigzagzig: false,
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
        &harness.config.browser.artifacts_dir,
        harness.config.browser.max_artifact_bytes,
        harness.config.browser.max_screenshot_dimension,
    );
    let gateway = Arc::new(
        CdpGateway::new(
            harness.authority.clone(),
            runtime,
            MethodRegistry::compiled(),
            format!("ws://{address}"),
        )
        .with_artifacts(artifact_store)
        .with_upload_staging_root(harness.upload_root()),
    );
    println!(
        "{}",
        serde_json::json!({"endpoint": format!("http://{address}"), "token": harness.token, "deniedToken": harness.denied_token, "site": harness.site_url()})
    );
    axum::serve(listener, gateway.router())
        .into_future()
        .await
        .unwrap();
}
