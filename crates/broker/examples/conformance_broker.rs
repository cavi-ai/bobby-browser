use std::sync::Arc;

use broker::{router, AppState, ArtifactCatalog};
use interface_conformance::live::ChromeRuntimeHarness;
use interface_core::{
    ArtifactOwnershipLimits, ArtifactReader, EventStore, RuntimeInterface, SessionOwnershipRegistry,
};
use sdk_core::AuthenticatedRuntime;

#[tokio::main]
async fn main() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let harness = ChromeRuntimeHarness::start().await;
    let runtime = harness.service.clone();
    let (ownership, recorder) =
        SessionOwnershipRegistry::bounded(harness.config.browser.max_active);
    let reader = ArtifactReader::new(
        artifact_store::ArtifactStore::new(
            &harness.config.browser.artifacts_dir,
            harness.config.browser.max_artifact_bytes,
            harness.config.browser.max_screenshot_dimension,
        ),
        ownership,
        harness.config.browser.max_artifact_bytes,
        ArtifactOwnershipLimits {
            max_records: harness.config.interface.max_event_retention,
            max_bytes: harness.config.browser.max_artifact_bytes as u64,
        },
    )
    .unwrap();
    let app = router(
        AppState::new(
            harness.authority.clone(),
            move |handle| {
                Arc::new(AuthenticatedRuntime::with_session_ownership(
                    runtime.clone(),
                    handle,
                    recorder.clone(),
                )) as Arc<dyn RuntimeInterface>
            },
            config::InterfaceConfig::default(),
        )
        .with_boundaries(
            EventStore::new(harness.config.interface.max_event_retention),
            ArtifactCatalog::new(reader, harness.config.interface.max_event_retention),
        ),
    );
    println!(
        "{}",
        serde_json::json!({"endpoint":format!("http://{address}"),"token":harness.token,"deniedToken":harness.denied_token,"site":harness.site_url(),"uploadRoot":harness.upload_root()})
    );
    axum::serve(listener, app).await.unwrap();
}
