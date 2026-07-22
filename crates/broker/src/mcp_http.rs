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
use mcp_gateway::{ArtifactResources, Server};
use tokio::sync::RwLock;
use types::{CorrelationId, PrincipalId};

use crate::{
    auth::{acquire_principal_permit, ProtocolError},
    AppState,
};

/// One [`mcp_gateway::Server`] per principal, cached for the life of the process.
///
/// This gives each principal its own MCP lifecycle (`initialize` is a once-per-session
/// handshake — see `mcp_gateway::Server`'s `Lifecycle` state machine), matching the
/// fleet model of one principal per team driver agent: a driver initializes once and
/// keeps issuing `tools/call` against the same negotiated session.
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
#[derive(Clone, Default)]
pub struct McpServers {
    entries: Arc<RwLock<HashMap<PrincipalId, (CapabilityHandle, Arc<Server>)>>>,
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
            ArtifactResources::default(),
        ));
        entries.insert(principal, (handle, server.clone()));
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

/// `GET /v1/mcp`: streamable-HTTP servers-sent-event streams are not supported, only
/// one JSON-RPC message per POST.
pub(crate) async fn method_not_allowed() -> Response {
    (
        StatusCode::METHOD_NOT_ALLOWED,
        Json(serde_json::json!({
            "error": "POST one JSON-RPC message per request; GET streams unsupported"
        })),
    )
        .into_response()
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
