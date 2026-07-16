use page_runtime::PageRuntime;
use session_manager::SessionManager;
use types::{
    CreateSessionRequest, NavigationRequest, NavigationResult, OpenPageRequest, PageState, RuntimeError,
    RuntimeInfo, SessionState,
};

#[derive(Clone, Default)]
pub struct RuntimeService {
    pub sessions: SessionManager,
    pub pages: PageRuntime,
}

impl RuntimeService {
    pub async fn runtime_info(&self) -> RuntimeInfo {
        let active_sessions = self.sessions.list().await.len();
        RuntimeInfo {
            version: env!("CARGO_PKG_VERSION").to_string(),
            capabilities: vec![
                "sdk".to_string(),
                "mcp-placeholder".to_string(),
                "cdp-placeholder".to_string(),
            ],
            active_sessions,
            queued_jobs: 0,
            uptime_ms: 0,
        }
    }

    pub async fn create_session(&self, req: CreateSessionRequest) -> SessionState {
        self.sessions.create(req).await
    }

    pub async fn list_sessions(&self) -> Vec<SessionState> {
        self.sessions.list().await
    }

    pub async fn open_page(&self, req: OpenPageRequest) -> Result<PageState, RuntimeError> {
        self.sessions.get(&req.session_id).await?;
        let page = self.pages.open(req).await;
        self.sessions.add_page(&page.session_id, page.id.clone()).await?;
        Ok(page)
    }

    pub async fn navigate(&self, req: NavigationRequest) -> Result<NavigationResult, RuntimeError> {
        let page = self.pages.set_url(&req.page_id, req.url.clone(), "interactive").await?;
        Ok(NavigationResult {
            page_id: page.id,
            url: req.url,
            ready_state: page.ready_state,
        })
    }
}
