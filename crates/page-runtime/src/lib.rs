use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::RwLock;
use types::{OpenPageRequest, PageId, PageMode, PageState, RuntimeError};

#[derive(Clone, Default)]
pub struct PageRuntime {
    inner: Arc<RwLock<HashMap<PageId, PageState>>>,
}

impl PageRuntime {
    pub async fn open(&self, req: OpenPageRequest) -> PageState {
        let page = PageState {
            id: PageId::default(),
            session_id: req.session_id,
            url: None,
            mode: PageMode::Document,
            ready_state: "created".to_string(),
            pending_requests: 0,
        };
        self.inner.write().await.insert(page.id.clone(), page.clone());
        page
    }

    pub async fn get(&self, id: &PageId) -> Result<PageState, RuntimeError> {
        self.inner
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| RuntimeError::NotFound("page".to_string()))
    }

    pub async fn set_url(&self, id: &PageId, url: String, ready_state: &str) -> Result<PageState, RuntimeError> {
        let mut guard = self.inner.write().await;
        let page = guard
            .get_mut(id)
            .ok_or_else(|| RuntimeError::NotFound("page".to_string()))?;
        page.url = Some(url);
        page.ready_state = ready_state.to_string();
        Ok(page.clone())
    }
}
