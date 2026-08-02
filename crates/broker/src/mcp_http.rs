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
/// This gives each principal its own MCP lifecycle (see `mcp_gateway::Server`'s
/// `Lifecycle` state machine), matching the fleet model of one principal per team
/// driver agent. A client that reconnects re-sends `initialize`; the server treats
/// that as a session reset rather than an error, so reconnecting drivers recover
/// without a process restart or token rotation.
///
/// Bounded the same way `RuntimeBindingCache` and `AppState::principal_permits` are: an
/// `Authority` only ever hands out live handles for up to `max_principals` distinct
/// principals at once, and revoked principals fail bearer auth upstream (in
/// `post_mcp`, before this cache is ever consulted), so a revoked principal's entry is
/// simply unreachable rather than needing eviction.
///
/// A cached entry is only reused while it still reflects what a fresh authentication
/// would produce — same capability set, still valid (unexpired, unrevoked) at the time
/// of use — mirroring `RuntimeBindingCache::bind` in `crate::lib`. `Server` bakes an
/// `AuthorizationGuard` (and therefore a frozen capability set and expiry) in at
/// construction, so caching by principal id alone would let a rotated token silently
/// keep the stale guard: newly granted capabilities would never be honored, and once
/// the first-seen token's expiry passed the principal would fail closed on `/v1/mcp`
/// forever, even after presenting a fresh, valid bearer.
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
        // Either no entry yet, or the cached handle is stale (rotated capabilities or
        // expired) — (re)build a fresh `Server`. Note this resets the principal's MCP
        // lifecycle: a rotated/renewed bearer must `initialize` again on its next
        // `/v1/mcp` call, exactly as a brand-new principal would. That is correct
        // behavior, not a bug — the alternative (keeping the old `Server`, and thus its
        // frozen `AuthorizationGuard`) is the staleness this cache exists to avoid.
        let runtime = (state.bind_runtime)(handle.clone());
        let server = Arc::new(Server::for_interface(
            runtime,
            handle.clone(),
            // The SAME EventStore AppState hands to HTTP `/v1/events`, so HTTP pollers
            // and this principal's `events_read` MCP tool observe one stream rather
            // than two independent, diverging histories.
            state.events.clone(),
            state.mcp_resources.clone(),
        ));
        let replaced = entries.insert(principal, (handle, server.clone()));
        if let Some((_, replaced)) = replaced {
            // This is the one place in the process where a principal's capability
            // set is observed to change, so it is the one place that can honour the
            // `tools.listChanged: true` the `initialize` result advertises: the tool
            // list the old `Server` served is now stale. Publishing before `replaced`
            // drops leaves the frame buffered in its sink, so a client streaming
            // `GET /v1/mcp` off that `Server` reads `tools/list_changed` and then sees
            // the stream end — reconnect, re-`initialize`, re-`tools/list`.
            replaced.notify_tools_list_changed();
        }
        server
    }
}

/// `POST /v1/mcp`: a transport for the MCP tool surface over streamable-HTTP, mounted
/// outside `protected_router()`'s strict-header middleware. Standard MCP clients can
/// only send a static `Authorization` header — they cannot mint a fresh
/// `x-deadline`/`x-correlation-id`/`x-interface-version` per request the way the
/// broker's other HTTP routes require — so this route does its own thin bearer-only
/// auth instead of running through `auth::authenticate`.
///
/// It still applies the same in-flight protections that middleware provides for every
/// other route: the global in-flight permit and the per-principal quota, both acquired
/// before `handle_message` runs and held across it, so a saturating MCP client cannot
/// starve HTTP callers (or other principals) of interface capacity.
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
        // `bearer` (and the failed authentication attempt) is dropped here without
        // ever being logged or Debug-printed.
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
    // `_global_permit` and `_principal_permit` are both held across `handle_message`
    // below, exactly as the authenticate middleware holds its equivalents across
    // `next.run(request).await`.
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
/// It deliberately holds no `Arc<Server>`. The cached `Server` owns the
/// notification sink, so letting the stream keep it alive would let a client
/// outlive its own capability rotation: `McpServers::get_or_create` replaces the
/// entry, but the stream would go on serving the old, capability-frozen sink.
/// Holding only the subscription means the replacement drops the sink's sender,
/// the buffered `tools/list_changed` is delivered, and the stream ends.
struct McpStream {
    handle: CapabilityHandle,
    notifications: mcp_gateway::NotificationStream,
}

/// `GET /v1/mcp`: the streamable-HTTP SSE channel, carrying the server-initiated
/// JSON-RPC notifications this principal is entitled to — its runtime events as
/// `notifications/bobby/event`, and `notifications/tools/list_changed` when its
/// capability set rotates — one JSON-RPC frame per SSE `data:` line. An idle
/// stream still emits the keep-alive comment it always did. Bearer auth matches
/// `post_mcp`.
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
        // See `McpStream`: the subscription outlives this `Arc`, deliberately.
        drop(server);
        if authorized_for_events(&handle) {
            notifications
        } else {
            // The channel still has to exist — MCP clients open it before they
            // will POST — but a principal that `GET /v1/events` and the
            // `events_read` tool would both refuse must not be handed the same
            // events through the notification stream instead.
            notifications.control_only()
        }
    };

    let stream = futures_util::stream::unfold(
        McpStream {
            handle,
            notifications,
        },
        |mut stream| async move {
            // Re-checked every poll, not just at connect: a stream that now
            // carries event data must not outlive the credential that opened
            // it, whether it expired or was revoked underneath us.
            if !stream.handle.is_valid_at(Utc::now()) {
                return None;
            }
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

/// Whether this principal may be sent runtime events at all — the same
/// `SubscribeEvents` gate `GET /v1/events` and the `events_read` tool apply,
/// evaluated through the same guard so there is only ever one definition of it.
fn authorized_for_events(handle: &CapabilityHandle) -> bool {
    let context = handle.context(Utc::now() + chrono::Duration::minutes(1), None);
    interface_core::AuthorizationGuard::new(handle.clone())
        .authorize(&context, types::InterfaceOperation::SubscribeEvents)
        .is_ok()
}

/// Extracts the bearer token from a single `authorization` header. This is deliberately
/// looser than `auth::bearer` (no length/charset validation) because
/// `Authority::authenticate` already rejects malformed bearers — this route only needs
/// to strip the `Bearer ` prefix before handing the token to the authority.
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
