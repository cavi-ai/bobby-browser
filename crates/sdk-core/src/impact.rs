//! What stopping this runtime would affect, read from memory the runtime
//! already holds. Internal to the runtime owner: not part of the per-principal
//! interface.

use serde::Serialize;
use types::{PageId, SessionId, WorkflowId};

use crate::RuntimeService;

const MAX_RECOVERABLE_WORKFLOWS: usize = 16;

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PageImpact {
    pub page_id: PageId,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SessionImpact {
    pub session_id: SessionId,
    pub profile: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub last_used_at: chrono::DateTime<chrono::Utc>,
    pub pages: Vec<PageImpact>,
    pub recoverable_workflows: Vec<WorkflowId>,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RestartImpact {
    pub in_flight_commands: usize,
    pub sessions: Vec<SessionImpact>,
}

impl RuntimeService {
    /// Every session on this runtime regardless of which principal created it.
    /// Reads the session registry, the page registry's last-known URLs and the
    /// checkpoint listing; makes no browser call and writes nothing.
    pub async fn restart_impact(&self) -> RestartImpact {
        let mut sessions = self.sessions.list().await;
        sessions.sort_by_key(|session| session.created_at);
        let mut impact = Vec::with_capacity(sessions.len());
        for session in sessions {
            let mut pages = Vec::with_capacity(session.page_ids.len());
            for page_id in session.page_ids {
                let url = match self.pages.get(&page_id).await {
                    Ok(page) => page.url.as_deref().and_then(redacted_url),
                    Err(_) => None,
                };
                pages.push(PageImpact { page_id, url });
            }
            let recoverable_workflows = self
                .workflows_for_session(&session.id, MAX_RECOVERABLE_WORKFLOWS)
                .await
                .unwrap_or_default();
            impact.push(SessionImpact {
                session_id: session.id,
                profile: session.profile,
                created_at: session.created_at,
                last_used_at: session.last_used_at,
                pages,
                recoverable_workflows,
            });
        }
        RestartImpact {
            in_flight_commands: self.in_flight.load(std::sync::atomic::Ordering::Acquire),
            sessions: impact,
        }
    }
}

/// The URL without userinfo, query or fragment; `None` when it does not parse.
pub(crate) fn redacted_url(raw: &str) -> Option<String> {
    let mut url = url::Url::parse(raw).ok()?;
    let _ = url.set_username("");
    let _ = url.set_password(None);
    url.set_query(None);
    url.set_fragment(None);
    Some(url.into())
}

#[cfg(test)]
mod tests {
    use super::redacted_url;

    #[test]
    fn redacted_url_drops_userinfo_query_and_fragment() {
        assert_eq!(
            redacted_url("https://user:pw@example.test:8443/a/b?token=s#frag").as_deref(),
            Some("https://example.test:8443/a/b")
        );
        assert_eq!(
            redacted_url("https://example.test/?q=1").as_deref(),
            Some("https://example.test/")
        );
    }

    #[test]
    fn redacted_url_omits_an_unparseable_url() {
        assert_eq!(redacted_url("not a url"), None);
        assert_eq!(redacted_url(""), None);
    }
}
