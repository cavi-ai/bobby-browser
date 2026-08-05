use std::{collections::HashMap, sync::Arc};

use axum::{
    body::Bytes,
    extract::State,
    http::{header::AUTHORIZATION, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    Json,
};
use chrono::Utc;
use interface_core::CapabilityHandle;
use mcp_gateway::Server;
use tokio::sync::RwLock;
use types::{CorrelationId, PrincipalId};

use crate::{
    auth::{acquire_principal_permit, ProtocolError},
    AppState,
};

/// One [`mcp_gateway::Server`] per principal, cached for the life of the process.
///
/// Bounded by the `Authority`'s `max_principals`; revoked principals fail bearer auth
/// in `post_mcp` before this cache is consulted, so their entries are unreachable
/// rather than evicted.
///
/// A cached entry may only be reused while it still matches a fresh authentication:
/// same capability set, still valid (unexpired, unrevoked) at time of use. `Server`
/// freezes an `AuthorizationGuard` (capability set and expiry) at construction, so
/// keying on principal id alone would pin a rotated token to the stale guard.
type McpServerEntries = HashMap<PrincipalId, (CapabilityHandle, Arc<Server>)>;

#[derive(Clone, Default)]
pub struct McpServers {
    entries: Arc<RwLock<McpServerEntries>>,
}

impl McpServers {
    async fn get_or_create(&self, state: &AppState, handle: CapabilityHandle) -> Arc<Server> {
        let principal = handle.principal_id().clone();
        if let Some((cached_handle, server)) = self.entries.read().await.get(&principal) {
            if cached_handle.capabilities() == handle.capabilities()
                && cached_handle.is_valid_at(Utc::now())
            {
                return server.clone();
            }
        }
        let mut entries = self.entries.write().await;
        if let Some((cached_handle, server)) = entries.get(&principal) {
            if cached_handle.capabilities() == handle.capabilities()
                && cached_handle.is_valid_at(Utc::now())
            {
                return server.clone();
            }
        }
        // No entry, or the cached handle is stale (rotated capabilities or expired):
        // build a fresh `Server`. This resets the principal's MCP lifecycle, so a
        // rotated bearer must `initialize` again on its next `/v1/mcp` call.
        let runtime = (state.bind_runtime)(handle.clone());
        let server = Arc::new(Server::for_interface(
            runtime,
            handle.clone(),
            // Must be the same EventStore AppState hands to HTTP `/v1/events`, so
            // pollers and the `events_read` MCP tool observe one stream.
            state.events.clone(),
            state.mcp_resources.clone(),
        ));
        let replaced = entries.insert(principal, (handle, server.clone()));
        if let Some((_, replaced)) = replaced {
            // Honours the `tools.listChanged: true` advertised at `initialize`: the
            // old `Server`'s tool list is now stale. Must publish before `replaced`
            // drops, so the frame stays buffered in its sink and a client streaming
            // `GET /v1/mcp` reads `tools/list_changed` before the stream ends.
            replaced.notify_tools_list_changed();
        }
        server
    }
}

/// `POST /v1/mcp`: the MCP tool surface over streamable-HTTP.
///
/// Mounted outside `protected_router()`'s strict-header middleware and does its own
/// bearer-only auth: MCP clients send only a static `Authorization` header, never a
/// per-request `x-deadline`/`x-correlation-id`/`x-interface-version`.
///
/// Still takes the global in-flight permit and the per-principal quota before
/// `handle_message` and holds both across it, as the middleware does elsewhere.
pub(crate) async fn post_mcp(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let bearer = match bearer_token(&headers) {
        Some(bearer) => bearer,
        None => return ProtocolError::authentication().into_response(),
    };
    let handle = match state.authority.authenticate(&bearer, Utc::now()).await {
        Ok(handle) => handle,
        // `bearer` must never be logged or Debug-printed on the failure path.
        Err(error) => return ProtocolError::from(error).into_response(),
    };
    drop(bearer);

    let correlation_id = CorrelationId::new();
    let _global_permit = match state.in_flight_requests.clone().try_acquire_owned() {
        Ok(permit) => permit,
        Err(_) => {
            return ProtocolError::from(crate::auth::interface_error(
                types::InterfaceErrorCode::ResourceExhausted,
                "interface in-flight request capacity exhausted",
                correlation_id.clone(),
                Some(1_000),
            ))
            .into_response()
        }
    };
    let principal_id = handle.principal_id().clone();
    // Both permits must stay held across `handle_message` below.
    let _principal_permit =
        match acquire_principal_permit(&state, &principal_id, correlation_id).await {
            Ok(permit) => permit,
            Err(error) => return error.into_response(),
        };

    let server = state.mcp_servers.get_or_create(&state, handle).await;

    let message: serde_json::Value = match serde_json::from_slice(&body) {
        Ok(message) => message,
        Err(_) => {
            return (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "jsonrpc": "2.0",
                    "id": null,
                    "error": {"code": -32700, "message": "parse error"}
                })),
            )
                .into_response();
        }
    };

    match server.handle_message(message).await {
        Some(response) => (StatusCode::OK, Json(response)).into_response(),
        None => StatusCode::ACCEPTED.into_response(),
    }
}

/// How long the stream may sit idle before it emits a keep-alive comment.
const MCP_STREAM_KEEPALIVE: std::time::Duration = std::time::Duration::from_secs(15);

/// State the SSE stream carries between polls.
///
/// Must not hold an `Arc<Server>`: the `Server` owns the notification sink, so
/// keeping it alive would let a stream outlive its own capability rotation and go
/// on serving the old, capability-frozen sink. Holding only the subscription means
/// replacement drops the sender, the buffered `tools/list_changed` is delivered,
/// and the stream ends.
struct McpStream {
    handle: CapabilityHandle,
    notifications: mcp_gateway::NotificationStream,
}

/// `GET /v1/mcp`: the streamable-HTTP SSE channel, one JSON-RPC frame per SSE
/// `data:` line. Carries this principal's runtime events as
/// `notifications/bobby/event` and `notifications/tools/list_changed` on capability
/// rotation; idle streams emit a keep-alive comment. Bearer auth matches `post_mcp`.
pub(crate) async fn get_mcp(State(state): State<AppState>, headers: HeaderMap) -> Response {
    let Some(bearer) = bearer_token(&headers) else {
        return ProtocolError::authentication().into_response();
    };
    let handle = match state.authority.authenticate(&bearer, Utc::now()).await {
        Ok(handle) => handle,
        Err(error) => return ProtocolError::from(error).into_response(),
    };
    drop(bearer);

    let notifications = {
        let server = state
            .mcp_servers
            .get_or_create(&state, handle.clone())
            .await;
        let notifications = server.notifications().subscribe().await;
        // See `McpStream`: the subscription must outlive this `Arc`.
        drop(server);
        if authorized_for_events(&handle) {
            notifications
        } else {
            // The channel must still exist (MCP clients open it before they POST),
            // but a principal refused by `GET /v1/events` and `events_read` must
            // not receive those events through the notification stream.
            notifications.control_only()
        }
    };

    let stream = futures_util::stream::unfold(
        McpStream {
            handle,
            notifications,
        },
        |mut stream| async move {
            // Must be re-checked every poll, not just at connect: the stream
            // carries event data and cannot outlive the credential that
            // opened it. Capability rotation is re-evaluated per poll too:
            // a stream whose principal loses SubscribeEvents keeps its
            // channel (clients require it before they will POST) but stops
            // receiving event data until the grant returns.
            if !stream.handle.is_valid_at(Utc::now()) {
                return None;
            }
            stream
                .notifications
                .set_events_open(authorized_for_events(&stream.handle));
            match tokio::time::timeout(MCP_STREAM_KEEPALIVE, stream.notifications.recv()).await {
                Ok(Some(frame)) => {
                    let event = axum::response::sse::Event::default()
                        .json_data(&frame)
                        .unwrap_or_else(|_| axum::response::sse::Event::default().comment(""));
                    Some((Ok::<_, std::convert::Infallible>(event), stream))
                }
                // The owning `Server` is gone: the client must reconnect.
                Ok(None) => None,
                Err(_) => Some((
                    Ok(axum::response::sse::Event::default().comment("keep-alive")),
                    stream,
                )),
            }
        },
    );
    axum::response::sse::Sse::new(stream).into_response()
}

/// Whether this principal may be sent runtime events: the `SubscribeEvents` gate,
/// evaluated through the same guard `GET /v1/events` and `events_read` use.
fn authorized_for_events(handle: &CapabilityHandle) -> bool {
    let context = handle.context(Utc::now() + chrono::Duration::minutes(1), None);
    interface_core::AuthorizationGuard::new(handle.clone())
        .authorize(&context, types::InterfaceOperation::SubscribeEvents)
        .is_ok()
}

/// Extracts the bearer token from a single `authorization` header.
///
/// Looser than `auth::bearer` (no length/charset validation): `Authority::authenticate`
/// already rejects malformed bearers.
fn bearer_token(headers: &HeaderMap) -> Option<String> {
    let mut values = headers.get_all(AUTHORIZATION).iter();
    let value = values.next()?;
    if values.next().is_some() {
        return None;
    }
    let value = value.to_str().ok()?;
    let token = value.strip_prefix("Bearer ")?;
    if token.is_empty() {
        return None;
    }
    Some(token.to_owned())
}
