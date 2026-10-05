//! The sessions one MCP connection opened and has not closed.
//!
//! A connection that ends must not leave its sessions registered: they hold
//! browser capacity and keep `bobby runtime stop|restart` reporting attached
//! sessions. Sessions live only in memory, so nothing else ever removes them.

use std::sync::Arc;

use chrono::{Duration, Utc};
use futures_util::future::join_all;
use interface_core::{CapabilityHandle, RuntimeInterface};

const DISCONNECT_DELETE_DEADLINE_SECONDS: i64 = 30;

#[derive(Default)]
struct State {
    /// Set when `serve` starts. A `Server` that is never served (the cached
    /// per-principal HTTP server) has no connection end, so it records nothing.
    active: bool,
    closing: bool,
    ids: Vec<types::SessionId>,
}

/// Ids this connection created and has not closed, plus the `closing` flag
/// that makes late creations clean up after themselves. The lock is a plain
/// mutex and is never held across an await.
#[derive(Default)]
pub(super) struct ConnectionSessions {
    state: std::sync::Mutex<State>,
}

impl ConnectionSessions {
    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// `false` when the connection is already closing: nothing is recorded
    /// and the caller must delete the session it just created.
    pub(super) fn record(&self, id: &types::SessionId) -> bool {
        let mut state = self.state();
        if state.closing {
            return false;
        }
        if !state.active {
            return true;
        }
        if !state.ids.contains(id) {
            state.ids.push(id.clone());
        }
        true
    }

    /// Starts recording; called when `serve` begins.
    pub(super) fn activate(&self) {
        self.state().active = true;
    }

    pub(super) fn forget(&self, id: &types::SessionId) {
        self.state().ids.retain(|recorded| recorded != id);
    }

    /// Marks the connection closing and takes every recorded id. Later calls
    /// return nothing, so cleanup is idempotent across the serve return paths
    /// and the drop guard.
    pub(super) fn close(&self) -> Vec<types::SessionId> {
        let mut state = self.state();
        state.closing = true;
        std::mem::take(&mut state.ids)
    }
}

/// Deletes one session with a fresh request context. `NotFound` means it was
/// already closed elsewhere. Any other failure is logged by id and code only.
pub(super) async fn delete_session_quietly(
    runtime: &Arc<dyn RuntimeInterface>,
    handle: &CapabilityHandle,
    session_id: types::SessionId,
) -> bool {
    let context = handle.context(
        Utc::now() + Duration::seconds(DISCONNECT_DELETE_DEADLINE_SECONDS),
        None,
    );
    match runtime.delete_session(context, session_id.clone()).await {
        Ok(()) => true,
        Err(error) if error.code == types::InterfaceErrorCode::NotFound => true,
        Err(error) => {
            tracing::warn!(
                session_id = %session_id.0,
                error_code = ?error.code,
                "mcp connection ended but its session could not be closed"
            );
            false
        }
    }
}

/// Closes the connection's sessions on every way `serve` can end, including
/// the future being dropped.
pub(super) struct DisconnectGuard {
    runtime: Arc<dyn RuntimeInterface>,
    handle: CapabilityHandle,
    sessions: Arc<ConnectionSessions>,
}

impl DisconnectGuard {
    pub(super) fn new(
        runtime: Arc<dyn RuntimeInterface>,
        handle: CapabilityHandle,
        sessions: Arc<ConnectionSessions>,
    ) -> Self {
        Self {
            runtime,
            handle,
            sessions,
        }
    }

    /// Takes the recorded ids and starts deleting them in a task that
    /// outlives the caller. `None` when there is nothing to delete or no
    /// runtime to spawn on.
    fn spawn_cleanup(&self) -> Option<tokio::task::JoinHandle<()>> {
        let ids = self.sessions.close();
        if ids.is_empty() {
            return None;
        }
        let runtime = Arc::clone(&self.runtime);
        let handle = self.handle.clone();
        let tokio_handle = tokio::runtime::Handle::try_current().ok()?;
        Some(tokio_handle.spawn(async move {
            join_all(
                ids.into_iter()
                    .map(|id| delete_session_quietly(&runtime, &handle, id)),
            )
            .await;
        }))
    }

    /// The return paths of `serve`: cleanup finishes before the process may
    /// exit.
    pub(super) async fn finish(self) {
        if let Some(task) = self.spawn_cleanup() {
            let _ = task.await;
        }
    }
}

impl Drop for DisconnectGuard {
    fn drop(&mut self) {
        // Already taken by `finish` leaves nothing to spawn.
        let _ = self.spawn_cleanup();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_record_stores_nothing() {
        let sessions = ConnectionSessions::default();
        let id = types::SessionId::new();
        assert!(sessions.record(&id));
        assert!(sessions.close().is_empty());
    }

    #[test]
    fn active_record_forgets_and_hands_ids_out_once() {
        let sessions = ConnectionSessions::default();
        sessions.activate();
        let kept = types::SessionId::new();
        let closed = types::SessionId::new();
        assert!(sessions.record(&kept));
        assert!(sessions.record(&closed));
        sessions.forget(&closed);
        assert_eq!(sessions.close(), vec![kept.clone()]);
        assert!(sessions.close().is_empty());
        assert!(!sessions.record(&kept));
    }
}
