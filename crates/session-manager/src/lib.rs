use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use types::{
    CreateSessionRequest, ExecutionPolicy, LostSession, PageId, RuntimeError, SessionId,
    SessionState,
};
use worker_pool::{choose_session_engine, WorkerPool};

/// Longest a session close waits on browser-side teardown before it unregisters
/// the session anyway.
const DEFAULT_RELEASE_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct SessionManager {
    inner: Arc<RwLock<HashMap<SessionId, SessionState>>>,
    workers: Option<Arc<WorkerPool>>,
    release_timeout: Duration,
    ledger: Option<Arc<Ledger>>,
}

/// The live sessions on disk, rewritten on every change, so the next runtime
/// can name the sessions this one held when it stopped.
struct Ledger {
    path: PathBuf,
    owners: std::sync::Mutex<HashMap<SessionId, String>>,
    lost: Vec<LedgerEntry>,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct LedgerEntry {
    session_id: SessionId,
    #[serde(default)]
    page_ids: Vec<PageId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    owner: Option<String>,
}

impl Ledger {
    fn write(&self, sessions: &HashMap<SessionId, SessionState>) {
        let owners = self
            .owners
            .lock()
            .unwrap_or_else(|error| error.into_inner());
        let entries: Vec<LedgerEntry> = sessions
            .values()
            .map(|session| LedgerEntry {
                session_id: session.id.clone(),
                page_ids: session.page_ids.clone(),
                owner: owners.get(&session.id).cloned(),
            })
            .collect();
        drop(owners);
        let written = serde_json::to_vec(&entries)
            .map_err(std::io::Error::other)
            .and_then(|bytes| {
                if let Some(parent) = self.path.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                let staged = self.path.with_extension("json.tmp");
                std::fs::write(&staged, bytes)?;
                std::fs::rename(&staged, &self.path)
            });
        if let Err(error) = written {
            tracing::warn!(%error, path = %self.path.display(), "session.ledger_write_failed");
        }
    }
}

impl Default for SessionManager {
    fn default() -> Self {
        Self {
            inner: Arc::default(),
            workers: None,
            release_timeout: DEFAULT_RELEASE_TIMEOUT,
            ledger: None,
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

    /// Records live sessions at `path`. The sessions a previous runtime left
    /// there are reported by [`Self::lost_sessions`]; the file then holds only
    /// this manager's sessions.
    pub fn with_ledger(mut self, path: PathBuf) -> Self {
        let lost: Vec<LedgerEntry> = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|error| {
                tracing::warn!(%error, path = %path.display(), "session.ledger_unreadable");
                Vec::new()
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => {
                tracing::warn!(%error, path = %path.display(), "session.ledger_unreadable");
                Vec::new()
            }
        };
        for entry in &lost {
            tracing::warn!(
                session_id = %entry.session_id.0,
                pages = entry.page_ids.len(),
                "session.lost_on_restart"
            );
        }
        let ledger = Arc::new(Ledger {
            path,
            owners: Default::default(),
            lost,
        });
        let sessions = self
            .inner
            .try_read()
            .map(|sessions| sessions.clone())
            .unwrap_or_default();
        ledger.write(&sessions);
        self.ledger = Some(ledger);
        self
    }

    /// Sessions the previous runtime held when it stopped, with their pages.
    /// `owner` keeps only the sessions recorded for that owner.
    pub fn lost_sessions(&self, owner: Option<&str>) -> Vec<LostSession> {
        self.ledger
            .iter()
            .flat_map(|ledger| ledger.lost.iter())
            .filter(|entry| owner.is_none() || entry.owner.as_deref() == owner)
            .map(|entry| LostSession {
                session_id: entry.session_id.clone(),
                page_ids: entry.page_ids.clone(),
            })
            .collect()
    }

    /// Records who owns `id`, so a later runtime reports it only to that owner.
    pub async fn record_owner(&self, id: &SessionId, owner: String) {
        let Some(ledger) = &self.ledger else {
            return;
        };
        let sessions = self.inner.read().await;
        if !sessions.contains_key(id) {
            return;
        }
        ledger
            .owners
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .insert(id.clone(), owner);
        ledger.write(&sessions);
    }

    fn persist(&self, sessions: &HashMap<SessionId, SessionState>) {
        if let Some(ledger) = &self.ledger {
            ledger.write(sessions);
        }
    }

    pub async fn create(&self, req: CreateSessionRequest) -> Result<SessionState, RuntimeError> {
        let now = types::WireTimestamp::now();
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
            if let Some(engine) = session.execution_policy.engine {
                let chosen =
                    choose_session_engine(config::BrowserSelectionConfig::default(), Some(engine))
                        .map_err(|error| RuntimeError::InvalidRequest(error.to_string()))?;
                workers
                    .pin_session_engine(&session.id, chosen.preference)
                    .await
                    .map_err(|_| {
                        RuntimeError::InvalidRequest(format!(
                            "executionPolicy.engine {engine:?} is not available in this runtime; \
                             run `bobby engine` to see the engines it can use"
                        ))
                    })?;
            }
            if let Err(error) = workers.lease(session.id.clone()).await {
                workers.forget_session_engine(&session.id).await;
                return Err(
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
                    },
                );
            }
        }
        let mut sessions = self.inner.write().await;
        sessions.insert(session.id.clone(), session.clone());
        self.persist(&sessions);
        drop(sessions);
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
            workers.forget_session_engine(id).await;
        }
        let mut sessions = self.inner.write().await;
        sessions.remove(id);
        if let Some(ledger) = &self.ledger {
            ledger
                .owners
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .remove(id);
        }
        self.persist(&sessions);
        drop(sessions);
        tracing::info!(session_id = %id.0, "session.deleted");
        Ok(())
    }

    /// Passive inspection never leases or launches a worker.
    pub async fn feature_observation(
        &self,
        id: &SessionId,
    ) -> Option<worker_pool::WorkerFeatureObservation> {
        match &self.workers {
            Some(workers) => workers.feature_observation(id).await,
            None => None,
        }
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
        let added = !session.page_ids.contains(&page_id);
        if added {
            session.page_ids.push(page_id);
        }
        session.last_used_at = types::WireTimestamp::now();
        if added {
            self.persist(&guard);
        }
        Ok(())
    }

    pub async fn sync_pages(
        &self,
        id: &SessionId,
        live: &[PageId],
        closed: &[PageId],
    ) -> Result<(), RuntimeError> {
        use std::collections::HashSet;
        let mut guard = self.inner.write().await;
        let session = guard
            .get_mut(id)
            .ok_or_else(|| RuntimeError::NotFound("session".into()))?;
        let closed: HashSet<_> = closed.iter().collect();
        let before = session.page_ids.clone();
        session.page_ids.retain(|page| !closed.contains(page));
        let mut known: HashSet<_> = session.page_ids.iter().cloned().collect();
        for page in live {
            if !closed.contains(page) && known.insert(page.clone()) {
                session.page_ids.push(page.clone());
            }
        }
        session.last_used_at = types::WireTimestamp::now();
        if session.page_ids != before {
            self.persist(&guard);
        }
        Ok(())
    }

    pub async fn remove_page(&self, id: &SessionId, page_id: &PageId) -> Result<(), RuntimeError> {
        let mut guard = self.inner.write().await;
        let session = guard
            .get_mut(id)
            .ok_or_else(|| RuntimeError::NotFound("session".to_string()))?;
        let before = session.page_ids.len();
        session.page_ids.retain(|candidate| candidate != page_id);
        session.last_used_at = types::WireTimestamp::now();
        if session.page_ids.len() != before {
            self.persist(&guard);
        }
        Ok(())
    }
}
