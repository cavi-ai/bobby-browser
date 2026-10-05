//! Connection-local protocol state over a shared authenticated runtime.
use crate::{
    auth::{acquire_principal_permit, ProtocolError},
    AppState,
};

#[derive(Clone)]
pub(crate) struct Lifecycle {
    stop: tokio::sync::watch::Sender<bool>,
    tasks: std::sync::Arc<tokio::sync::Mutex<tokio::task::JoinSet<()>>>,
    live: std::sync::Arc<std::sync::atomic::AtomicUsize>,
}

/// Counts one live connection for as long as it is held; every exit path of the
/// task, including an abort, drops it.
struct LiveConnection(std::sync::Arc<std::sync::atomic::AtomicUsize>);

impl LiveConnection {
    fn enter(live: &std::sync::Arc<std::sync::atomic::AtomicUsize>) -> Self {
        live.fetch_add(1, std::sync::atomic::Ordering::AcqRel);
        Self(live.clone())
    }
}

impl Drop for LiveConnection {
    fn drop(&mut self) {
        self.0.fetch_sub(1, std::sync::atomic::Ordering::AcqRel);
    }
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self {
            stop: tokio::sync::watch::channel(false).0,
            tasks: Default::default(),
            live: Default::default(),
        }
    }
}

impl Lifecycle {
    async fn spawn(&self, task: impl std::future::Future<Output = ()> + Send + 'static) {
        let mut tasks = self.tasks.lock().await;
        while tasks.try_join_next().is_some() {}
        if !*self.stop.borrow() {
            let live = LiveConnection::enter(&self.live);
            tasks.spawn(async move {
                let _live = live;
                task.await
            });
        }
    }
    pub(crate) fn live_connections(&self) -> usize {
        self.live.load(std::sync::atomic::Ordering::Acquire)
    }
    pub(crate) fn stop(&self) {
        self.stop.send_replace(true);
    }
    pub(crate) async fn drain(&self, timeout: std::time::Duration) {
        self.stop();
        let mut tasks = self.tasks.lock().await;
        if tokio::time::timeout(timeout, async {
            while tasks.join_next().await.is_some() {}
        })
        .await
        .is_err()
        {
            tasks.abort_all();
            while tasks.join_next().await.is_some() {}
        }
    }
}
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        Path, State,
    },
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use chrono::Utc;
use futures_util::{SinkExt, StreamExt};

pub(crate) async fn connect(
    State(state): State<AppState>,
    Path(protocol): Path<String>,
    headers: HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    if !matches!(protocol.as_str(), "mcp" | "acp") {
        return StatusCode::NOT_FOUND.into_response();
    }
    let mut values = headers.get_all("authorization").iter();
    let bearer = values
        .next()
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.strip_prefix("Bearer "));
    if values.next().is_some() {
        return ProtocolError::authentication().into_response();
    }
    let Some(bearer) = bearer else {
        return ProtocolError::authentication().into_response();
    };
    let handle = match state.authority.authenticate(bearer, Utc::now()).await {
        Ok(handle) => handle,
        Err(error) => return ProtocolError::from(error).into_response(),
    };
    if protocol == "acp" && state.bind_authenticated.is_none() {
        return StatusCode::NOT_IMPLEMENTED.into_response();
    }
    let toolset = headers
        .get("x-bobby-mcp-toolset")
        .and_then(|value| value.to_str().ok())
        .and_then(mcp_gateway::toolset::Toolset::parse)
        .or(state.mcp_startup_toolset)
        .unwrap_or_default();
    let connection_permit = match state.in_flight_requests.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return ProtocolError::from(crate::auth::interface_error(
                types::InterfaceErrorCode::ResourceExhausted,
                "gateway connection capacity exhausted",
                types::CorrelationId::new(),
                Some(1_000),
            ))
            .into_response()
        }
    };
    let permit =
        match acquire_principal_permit(&state, handle.principal_id(), types::CorrelationId::new())
            .await
        {
            Ok(permit) => permit,
            Err(error) => return error.into_response(),
        };
    let bearer = bearer.to_owned();
    upgrade
        .max_message_size(gateway_transport::MAX_FRAME_BYTES)
        .max_frame_size(gateway_transport::MAX_FRAME_BYTES)
        .on_upgrade(move |socket| async move {
            let lifecycle = state.gateway_lifecycle.clone();
            lifecycle
                .spawn(async move {
                    let _permit = permit;
                    let _connection_permit = connection_permit;
                    let stopped = state.gateway_lifecycle.stop.subscribe();
                    let (server_io, bridge_io) =
                        tokio::io::duplex(gateway_transport::MAX_FRAME_BYTES * 2);
                    let (reader, writer) = tokio::io::split(server_io);
                    let serving = async {
                        if protocol == "mcp" {
                            let server = mcp_gateway::Server::for_interface(
                                (state.bind_runtime)(handle.clone()),
                                handle,
                                state.events.clone(),
                                state.mcp_resources.clone(),
                            )
                            .with_connection_toolset(toolset)
                            .with_jobs(std::sync::Arc::new(mcp_gateway::InProcessJobPort::new(
                                state.scheduler.clone(),
                            )));
                            server
                                .serve(reader, writer)
                                .await
                                .map_err(anyhow::Error::from)
                        } else {
                            let runtime = (state
                                .bind_authenticated
                                .as_ref()
                                .expect("checked ACP binder"))(
                                handle.clone()
                            );
                            acp_gateway::AcpServer::new(
                                runtime,
                                handle.capabilities().iter().collect(),
                            )
                            .serve_io(reader, writer)
                            .await
                            .map_err(anyhow::Error::from)
                        }
                    };
                    let bridge =
                        bridge_socket(socket, bridge_io, state.authority.clone(), bearer, stopped);
                    // Drive disconnect through the protocol server so ACP closes only
                    // its own sessions, while the shared browser owner remains alive.
                    if let Err(error) = run_until_peer_gone(serving, bridge, PEER_GONE_GRACE).await
                    {
                        tracing::warn!(%error, "shared gateway connection ended");
                    }
                })
                .await;
        })
        .into_response()
}

/// How long the protocol server may keep running after its peer is gone. The
/// connection holds a principal permit until it ends, and a server whose
/// requests are stuck on work nobody can receive would never end on its own.
const PEER_GONE_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Drives the protocol server and the socket bridge together. Once the bridge
/// ends the peer is gone and the server gets `grace` to finish its own
/// teardown before it is dropped.
async fn run_until_peer_gone(
    serving: impl std::future::Future<Output = anyhow::Result<()>>,
    bridge: impl std::future::Future<Output = anyhow::Result<()>>,
    grace: std::time::Duration,
) -> anyhow::Result<()> {
    tokio::pin!(serving, bridge);
    let (served, bridged) = tokio::select! {
        served = &mut serving => (served, bridge.await),
        bridged = &mut bridge => {
            let served = tokio::time::timeout(grace, &mut serving)
                .await
                .unwrap_or_else(|_| {
                    Err(anyhow::anyhow!(
                        "protocol server abandoned {grace:?} after the peer left"
                    ))
                });
            (served, bridged)
        }
    };
    served.and(bridged)
}

async fn bridge_socket(
    socket: WebSocket,
    io: tokio::io::DuplexStream,
    authority: std::sync::Arc<dyn interface_core::Authority>,
    bearer: String,
    mut stopped: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    let (sink, stream) = socket.split();
    let incoming = stream.filter_map(move |message| {
        let authority = authority.clone();
        let bearer = bearer.clone();
        async move {
            match message {
                Ok(Message::Text(text)) => {
                    match authority.authenticate(&bearer, Utc::now()).await {
                        Ok(_) => Some(Ok(text.to_string())),
                        Err(_) => Some(Err(axum::Error::new(std::io::Error::new(
                            std::io::ErrorKind::PermissionDenied,
                            "gateway credential expired or was revoked",
                        )))),
                    }
                }
                Ok(Message::Binary(_)) => Some(Err(axum::Error::new(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "binary gateway frame",
                )))),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            }
        }
    });
    let incoming = incoming.take_until(async move {
        if !*stopped.borrow() {
            let _ = stopped.changed().await;
        }
    });
    let outgoing =
        sink.with(async |frame: String| Ok::<_, axum::Error>(Message::Text(frame.into())));
    let (reader, writer) = tokio::io::split(io);
    gateway_transport::bridge(reader, writer, Box::pin(incoming), Box::pin(outgoing)).await
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::run_until_peer_gone;

    #[tokio::test(start_paused = true)]
    async fn a_server_stuck_after_the_peer_left_is_abandoned() {
        let started = tokio::time::Instant::now();
        let result = run_until_peer_gone(
            std::future::pending::<anyhow::Result<()>>(),
            async { Ok(()) },
            Duration::from_secs(5),
        )
        .await;
        assert!(result.is_err());
        assert_eq!(started.elapsed(), Duration::from_secs(5));
    }

    #[tokio::test(start_paused = true)]
    async fn a_server_that_finishes_inside_the_grace_is_not_cut_short() {
        let result = run_until_peer_gone(
            async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                Ok(())
            },
            async { Ok(()) },
            Duration::from_secs(5),
        )
        .await;
        assert!(result.is_ok());
    }

    #[tokio::test(start_paused = true)]
    async fn the_bridge_still_finishes_when_the_server_ends_first() {
        let result = run_until_peer_gone(
            async { Ok(()) },
            async {
                tokio::time::sleep(Duration::from_secs(30)).await;
                Ok(())
            },
            Duration::from_secs(5),
        )
        .await;
        assert!(result.is_ok());
    }
}
