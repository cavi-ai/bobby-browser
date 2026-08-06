use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use config::CdpConfig;
use interface_core::Authority;
use sdk_core::AuthenticatedRuntime;

pub struct CdpListen {
    pub addr: SocketAddr,
    pub handle: tokio::task::JoinHandle<anyhow::Result<()>>,
}

pub async fn spawn_cdp_listener<A: Authority + 'static>(
    config: &CdpConfig,
    authority: Arc<A>,
    runtime: Arc<AuthenticatedRuntime>,
    artifacts: artifact_store::ArtifactStore,
    upload_staging_root: PathBuf,
) -> anyhow::Result<CdpListen> {
    spawn_cdp_listener_with_shutdown(
        config,
        authority,
        runtime,
        artifacts,
        upload_staging_root,
        std::future::pending(),
    )
    .await
}

pub async fn spawn_cdp_listener_with_shutdown<A: Authority + 'static>(
    config: &CdpConfig,
    authority: Arc<A>,
    runtime: Arc<AuthenticatedRuntime>,
    artifacts: artifact_store::ArtifactStore,
    upload_staging_root: PathBuf,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<CdpListen> {
    let addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;
    let bound = listener.local_addr()?;
    let ws_base = format!("ws://{bound}");
    let gateway = Arc::new(
        cdp_gateway::CdpGateway::new(
            authority,
            runtime,
            cdp_gateway::MethodRegistry::compiled(),
            ws_base,
        )
        .with_artifacts(artifacts)
        .with_upload_staging_root(upload_staging_root),
    );
    let handle = tokio::spawn(async move {
        axum::serve(listener, gateway.router())
            .with_graceful_shutdown(shutdown)
            .await
            .map_err(anyhow::Error::new)
    });
    Ok(CdpListen {
        addr: bound,
        handle,
    })
}
