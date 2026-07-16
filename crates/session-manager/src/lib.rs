use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::RwLock;
use types::{CreateSessionRequest, PageId, RuntimeError, SessionId, SessionState};

#[derive(Clone, Default)]
pub struct SessionManager {
    inner: Arc<RwLock<HashMap<SessionId, SessionState>>>,
}

impl SessionManager {
    pub async fn create(&self, req: CreateSessionRequest) -> SessionState {
        let now = Utc::now();
        let session = SessionState {
            id: SessionId::default(),
            profile: req.profile,
            proxy: req.proxy,
            page_ids: Vec::new(),
            created_at: now,
            last_used_at: now,
        };
        self.inner.write().await.insert(session.id.clone(), session.clone());
        session
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
}
