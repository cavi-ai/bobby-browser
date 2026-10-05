use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use tokio::sync::RwLock;
use types::{CreateSessionRequest, ExecutionPolicy, PageId, RuntimeError, SessionId, SessionState};
use worker_pool::WorkerPool;

/// Longest a session close waits on browser-side teardown before it unregisters
/// the session anyway.
const DEFAULT_RELEASE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<RwLock<HashMap<SessionId, SessionState>>>,
    workers: Option<Arc<WorkerPool>>,
    release_timeout: Duration,
}

impl Default for SessionManager {
    fn default() -> Self {
        Self {
            inner: Arc::default(),
            workers: None,
            release_timeout: DEFAULT_RELEASE_TIMEOUT,
        }
    }
}

impl SessionManager {
    pub fn new(workers: Arc<WorkerPool>) -> Self {
        Self {
            workers: Some(workers),
            ..Self::default()
        }
    }

    pub fn with_release_timeout(mut self, release_timeout: Duration) -> Self {
        self.release_timeout = release_timeout;
        self
    }

    pub async fn create(&self, req: CreateSessionRequest) -> Result<SessionState, RuntimeError> {
        let now = Utc::now();
        // Godmode: a zigzagzig session forces every capability on — the
        // ladder escalates into vision solving, so the session must be
        // allowed to use what the ladder reaches for.
        let execution_policy = if req.zigzagzig {
            ExecutionPolicy {
                javascript_evaluation: true,
                vision_assist: true,
                fingerprint: true,
                humanize: true,
                ..req.execution_policy
            }
        } else {
            req.execution_policy
        };
        let session = SessionState {
            id: SessionId::default(),
            profile: req.profile,
            proxy: req.proxy,
            page_ids: Vec::new(),
            created_at: now,
            last_used_at: now,
            execution_policy,
            zigzagzig: req.zigzagzig,
        };
        if let Some(workers) = &self.workers {
            workers.lease(session.id.clone()).await.map_err(|error| {
                if error.code == types::ErrorCode::BrowserLaunchFailed
                    || error.code == types::ErrorCode::DeadlineExceeded
                {
                    // Keep the diagnostic prefix leading the message: the MCP
                    // gateway allowlists it by prefix before letting any runtime
                    // detail cross to an external agent. Preserve the concrete
                    // factory/companion/lease error body after the prefix.
                    RuntimeError::EngineUnreachable(format!(
                        "browser launch failed: {}; run `bobby doctor` -- check Firefox BiDi endpoint readiness and companion bind (configured may be ephemeral 127.0.0.1:0; install default is 127.0.0.1:9876)",
                        error.message
                    ))
                } else {
                    RuntimeError::Internal(error.message)
                }
            })?;
        }
        self.inner
            .write()
            .await
            .insert(session.id.clone(), session.clone());
        tracing::info!(session_id = %session.id.0, "session.created");
        Ok(session)
    }

    pub async fn delete(&self, id: &SessionId) -> Result<(), RuntimeError> {
        if !self.inner.read().await.contains_key(id) {
            return Err(RuntimeError::NotFound("session".into()));
        }
        // The session is unregistered whatever the browser does: a dead or hung
        // browser must not keep a closed session listed and holding capacity.
        if let Some(workers) = &self.workers {
            match tokio::time::timeout(self.release_timeout, workers.release_session(id)).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => tracing::warn!(
                    session_id = %id.0,
                    error = %error.message,
                    "session.release_failed"
                ),
                Err(_) => tracing::warn!(session_id = %id.0, "session.release_timed_out"),
            }
        }
        self.inner.write().await.remove(id);
        tracing::info!(session_id = %id.0, "session.deleted");
        Ok(())
    }

    pub async fn list(&self) -> Vec<SessionState> {
        self.inner.read().await.values().cloned().collect()
    }

    pub async fn get(&self, id: &SessionId) -> Result<SessionState, RuntimeError> {
        self.inner
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| RuntimeError::NotFound("session".to_string()))
    }

    pub async fn add_page(&self, id: &SessionId, page_id: PageId) -> Result<(), RuntimeError> {
        let mut guard = self.inner.write().await;
        let session = guard
            .get_mut(id)
            .ok_or_else(|| RuntimeError::NotFound("session".to_string()))?;
        session.page_ids.push(page_id);
        session.last_used_at = Utc::now();
        Ok(())
    }

    pub async fn remove_page(&self, id: &SessionId, page_id: &PageId) -> Result<(), RuntimeError> {
        let mut guard = self.inner.write().await;
        let session = guard
            .get_mut(id)
            .ok_or_else(|| RuntimeError::NotFound("session".to_string()))?;
        session.page_ids.retain(|candidate| candidate != page_id);
        session.last_used_at = Utc::now();
        Ok(())
    }
}
