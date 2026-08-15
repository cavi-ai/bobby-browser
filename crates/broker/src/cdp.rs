use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use anyhow::Context;
use config::CdpConfig;
use interface_core::Authority;

pub struct CdpListen {
    pub addr: SocketAddr,
    pub handle: tokio::task::JoinHandle<anyhow::Result<()>>,
}

pub async fn spawn_cdp_listener<A: Authority + 'static>(
    config: &CdpConfig,
    authority: Arc<A>,
    bind_runtime: Arc<crate::RuntimeBinder>,
    artifacts: artifact_store::ArtifactStore,
    upload_staging_root: PathBuf,
) -> anyhow::Result<CdpListen> {
    spawn_cdp_listener_with_shutdown(
        config,
        authority,
        bind_runtime,
        artifacts,
        upload_staging_root,
        std::future::pending(),
    )
    .await
}

pub async fn spawn_cdp_listener_with_shutdown<A: Authority + 'static>(
    config: &CdpConfig,
    authority: Arc<A>,
    bind_runtime: Arc<crate::RuntimeBinder>,
    artifacts: artifact_store::ArtifactStore,
    upload_staging_root: PathBuf,
    shutdown: impl std::future::Future<Output = ()> + Send + 'static,
) -> anyhow::Result<CdpListen> {
    let addr: SocketAddr = format!("{}:{}", config.host, config.port).parse()?;
    // Name the address in the bind error. The default CDP port (9222) is also
    // the default Firefox remote-debugging port, so a collision is the common
    // first-run outcome; bare "Address already in use (os error 48)" does not
    // say which listener failed or on which port.
    let listener = tokio::net::TcpListener::bind(addr).await.with_context(|| {
        format!(
            "CDP listener could not bind {addr}; another process owns the port \
             (Firefox remote debugging also defaults to 9222) -- free it, set \
             [cdp].port, or run `bobby cdp --cdp-port <port>`"
        )
    })?;
    let bound = listener.local_addr()?;
    let ws_base = format!("ws://{bound}");
    tracing::info!(
        endpoint = %format!("http://{bound}"),
        websocket_base = %ws_base,
        "cdp.listener.ready"
    );
    let gateway = Arc::new(
        cdp_gateway::CdpGateway::with_binder(
            authority,
            bind_runtime,
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
