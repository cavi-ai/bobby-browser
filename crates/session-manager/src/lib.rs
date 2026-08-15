use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::RwLock;
use types::{CreateSessionRequest, PageId, RuntimeError, SessionId, SessionState};
use worker_pool::WorkerPool;

#[derive(Clone, Default)]
pub struct SessionManager {
    inner: Arc<RwLock<HashMap<SessionId, SessionState>>>,
    workers: Option<Arc<WorkerPool>>,
}

impl SessionManager {
    pub fn new(workers: Arc<WorkerPool>) -> Self {
        Self {
            inner: Arc::default(),
            workers: Some(workers),
        }
    }

    pub async fn create(&self, req: CreateSessionRequest) -> Result<SessionState, RuntimeError> {
        let now = Utc::now();
        let session = SessionState {
            id: SessionId::default(),
            profile: req.profile,
            proxy: req.proxy,
            page_ids: Vec::new(),
            created_at: now,
            last_used_at: now,
            execution_policy: req.execution_policy,
        };
        if let Some(workers) = &self.workers {
            workers.lease(session.id.clone()).await.map_err(|error| {
                if error.code == types::ErrorCode::BrowserLaunchFailed {
                    // Keep the diagnostic prefix leading the message: the MCP
                    // gateway allowlists it by prefix before letting any runtime
                    // detail cross to an external agent.
                    RuntimeError::EngineUnreachable(format!(
                        "browser launch failed: {}; run `bobby doctor` to verify the Firefox BiDi endpoint and detect another service occupying its configured port",
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
        // Release before unregistering: if the release fails, the session
        // stays listed so the caller can retry -- removing first would leak
        // the browser with no handle left to close it.
        if let Some(workers) = &self.workers {
            workers
                .release_session(id)
                .await
                .map_err(|error| RuntimeError::Internal(error.message))?;
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
