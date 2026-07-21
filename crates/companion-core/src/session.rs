use crate::{AttachmentLease, CompanionRegistry, PairedCompanion, RegistryError};
use axum::extract::ws::Message;
use companion_protocol::{
    ActionRequest, AttachmentGrant, BrowserTarget, CompanionEvent, CompanionRequest, GrantedPage,
    PageBindingDiscovered, TargetDiscovery, TargetKind, PROTOCOL_VERSION,
};
use std::{
    collections::{HashMap, HashSet},
    sync::{Arc, Mutex as TaskMutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch, Mutex, Notify};
use types::{AttachmentId, CommandId, CompanionId, PageId, ProfileId};
use uuid::Uuid;

const MAX_DISCOVERED_TARGETS: usize = 256;
const MAX_TARGET_ID_BYTES: usize = 256;
const MAX_COMMAND_WAIT: Duration = Duration::from_secs(60);
const MAX_PENDING_COMMANDS: usize = 256;
const ABANDONED_COMMAND_RETENTION: Duration = Duration::from_secs(60);
pub(crate) const MAX_PENDING_BINDINGS: usize = 64;
const PAGE_BINDING_TTL: Duration = Duration::from_secs(10);
const GRANT_PUBLICATION_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Debug, Clone, Error, PartialEq, Eq)]
pub enum CompanionSessionError {
    #[error("paired profile has no active companion connection")]
    ProfileUnavailable,
    #[error("paired profile has no browser target discovery")]
    DiscoveryUnavailable,
    #[error("attachment grant is missing or expired")]
    GrantUnavailable,
    #[error("attachment does not match the active grant")]
    AttachmentMismatch,
    #[error("page does not match the active attachment grant")]
    PageMismatch,
    #[error("profile does not match the active companion connection")]
    ProfileMismatch,
    #[error("companion event is invalid for the active connection")]
    InvalidEvent,
    #[error("companion connection closed")]
    ConnectionClosed,
    #[error("companion outbound queue closed")]
    QueueClosed,
    #[error("companion action deadline expired")]
    DeadlineExceeded,
    #[error("companion action response timed out")]
    ResponseTimeout,
    #[error("companion pending command capacity is exhausted")]
    PendingCapacity,
    #[error("companion pending page-binding capacity is exhausted")]
    BindingCapacity,
    #[error("companion page binding expired")]
    BindingExpired,
    #[error(transparent)]
    Registry(#[from] RegistryError),
}

#[derive(Clone)]
struct ActiveSession {
    connection_id: Uuid,
    companion_id: CompanionId,
    outbound: mpsc::Sender<Message>,
}

#[derive(Clone)]
struct DiscoveryRecord {
    connection_id: Uuid,
    companion_id: CompanionId,
    targets: Vec<BrowserTarget>,
}

#[derive(Clone)]
struct GrantRecord {
    connection_id: Uuid,
    companion_id: CompanionId,
    grant: AttachmentGrant,
}

struct PendingCommand {
    connection_id: Uuid,
    response: Option<oneshot::Sender<Result<CompanionEvent, CompanionSessionError>>>,
    expires_at: Instant,
}

struct PendingBinding {
    connection_id: Uuid,
    profile_id: ProfileId,
    attachment_id: AttachmentId,
    expected_page_id: PageId,
    known_targets: HashSet<String>,
    response: oneshot::Sender<Result<AttachmentGrant, CompanionSessionError>>,
    expires_at: Instant,
}

#[derive(Default)]
struct SessionState {
    sessions: HashMap<ProfileId, ActiveSession>,
    discoveries: HashMap<ProfileId, DiscoveryRecord>,
    grants: HashMap<AttachmentId, GrantRecord>,
    pending: HashMap<CommandId, PendingCommand>,
    bindings: HashMap<String, PendingBinding>,
}

pub(crate) struct SessionCoordinator {
    registry: Arc<CompanionRegistry>,
    state: Arc<Mutex<SessionState>>,
    grant_updates: Arc<Mutex<()>>,
    binding_releases: Arc<TaskMutex<HashMap<PageBindingReleaseKey, Arc<PageBindingRelease>>>>,
    discovery_changed: Notify,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct PageBindingReleaseKey {
    attachment_id: AttachmentId,
    page_id: PageId,
}

struct PageBindingRelease {
    result: watch::Sender<Option<Result<(), CompanionSessionError>>>,
    connection_id: TaskMutex<Option<Uuid>>,
}

pub struct PageBindingTicket {
    state: Arc<Mutex<SessionState>>,
    binding_nonce: Option<String>,
    connection_id: Uuid,
    response: Option<oneshot::Receiver<Result<AttachmentGrant, CompanionSessionError>>>,
}

impl std::fmt::Debug for PageBindingTicket {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PageBindingTicket")
            .field("active", &self.binding_nonce.is_some())
            .finish_non_exhaustive()
    }
}

impl PageBindingTicket {
    pub fn binding_nonce(&self) -> &str {
        self.binding_nonce
            .as_deref()
            .expect("page binding ticket is active")
    }

    pub async fn complete(
        mut self,
        timeout: Duration,
    ) -> Result<AttachmentGrant, CompanionSessionError> {
        let response = self
            .response
            .take()
            .expect("page binding ticket response is active");
        let result = tokio::time::timeout(timeout.min(PAGE_BINDING_TTL), response).await;
        match result {
            Ok(Ok(result)) => {
                self.binding_nonce = None;
                result
            }
            Ok(Err(_)) => {
                self.remove().await;
                Err(CompanionSessionError::ConnectionClosed)
            }
            Err(_) => {
                self.remove().await;
                Err(CompanionSessionError::BindingExpired)
            }
        }
    }

    async fn remove(&mut self) {
        let Some(binding_nonce) = self.binding_nonce.take() else {
            return;
        };
        let mut state = self.state.lock().await;
        remove_binding(&mut state, &binding_nonce, self.connection_id);
    }
}

impl Drop for PageBindingTicket {
    fn drop(&mut self) {
        let Some(binding_nonce) = self.binding_nonce.take() else {
            return;
        };
        if let Ok(mut state) = self.state.try_lock() {
            remove_binding(&mut state, &binding_nonce, self.connection_id);
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let state = Arc::clone(&self.state);
        let connection_id = self.connection_id;
        runtime.spawn(async move {
            let mut state = state.lock().await;
            remove_binding(&mut state, &binding_nonce, connection_id);
        });
    }
}

struct PendingGuard {
    state: Arc<Mutex<SessionState>>,
    command_id: Option<CommandId>,
    connection_id: Uuid,
}

impl PendingGuard {
    fn new(state: Arc<Mutex<SessionState>>, command_id: CommandId, connection_id: Uuid) -> Self {
        Self {
            state,
            command_id: Some(command_id),
            connection_id,
        }
    }

    fn disarm(&mut self) {
        self.command_id = None;
    }

    async fn abandon(&mut self) {
        let Some(command_id) = self.command_id.as_ref().cloned() else {
            return;
        };
        let mut state = self.state.lock().await;
        abandon_pending(&mut state, &command_id, self.connection_id);
        self.command_id = None;
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        let Some(command_id) = self.command_id.take() else {
            return;
        };
        if let Ok(mut state) = self.state.try_lock() {
            abandon_pending(&mut state, &command_id, self.connection_id);
            return;
        }
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let state = Arc::clone(&self.state);
        let connection_id = self.connection_id;
        runtime.spawn(async move {
            let mut state = state.lock().await;
            abandon_pending(&mut state, &command_id, connection_id);
        });
    }
}

fn purge_expired_pending(state: &mut SessionState, now: Instant) {
    state.pending.retain(|_, pending| {
        pending.expires_at > now
            || pending
                .response
                .as_ref()
                .is_some_and(|response| !response.is_closed())
    });
}

fn abandon_pending(state: &mut SessionState, command_id: &CommandId, connection_id: Uuid) {
    let now = Instant::now();
    purge_expired_pending(state, now);
    if let Some(pending) = state.pending.get_mut(command_id) {
        if pending.connection_id == connection_id {
            pending.response = None;
            pending.expires_at = now + ABANDONED_COMMAND_RETENTION;
        }
    }
}

fn purge_expired_bindings(state: &mut SessionState, now: Instant) {
    let expired = state
        .bindings
        .iter()
        .filter(|(_, binding)| binding.expires_at <= now)
        .map(|(nonce, _)| nonce.clone())
        .collect::<Vec<_>>();
    for nonce in expired {
        if let Some(binding) = state.bindings.remove(&nonce) {
            let _ = binding
                .response
                .send(Err(CompanionSessionError::BindingExpired));
        }
    }
}

fn expire_page_bindings(state: &mut SessionState, attachment_id: &AttachmentId, page_id: &PageId) {
    purge_expired_bindings(state, Instant::now());
    let pending_nonces = state
        .bindings
        .iter()
        .filter(|(_, binding)| {
            binding.attachment_id == *attachment_id && binding.expected_page_id == *page_id
        })
        .map(|(nonce, _)| nonce.clone())
        .collect::<Vec<_>>();
    for nonce in pending_nonces {
        if let Some(binding) = state.bindings.remove(&nonce) {
            let _ = binding
                .response
                .send(Err(CompanionSessionError::BindingExpired));
        }
    }
}

fn remove_binding(state: &mut SessionState, binding_nonce: &str, connection_id: Uuid) {
    if state
        .bindings
        .get(binding_nonce)
        .is_some_and(|binding| binding.connection_id == connection_id)
    {
        state.bindings.remove(binding_nonce);
    }
}

impl std::fmt::Debug for SessionCoordinator {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("SessionCoordinator")
    }
}

impl SessionCoordinator {
    pub(crate) fn new(registry: Arc<CompanionRegistry>) -> Self {
        Self {
            registry,
            state: Arc::new(Mutex::new(SessionState::default())),
            grant_updates: Arc::new(Mutex::new(())),
            binding_releases: Arc::new(TaskMutex::new(HashMap::new())),
            discovery_changed: Notify::new(),
        }
    }

    pub(crate) async fn register(
        &self,
        paired: PairedCompanion,
        outbound: mpsc::Sender<Message>,
    ) -> Uuid {
        let connection_id = Uuid::new_v4();
        let previous = {
            let _grant_update = self.grant_updates.lock().await;
            let mut state = self.state.lock().await;
            let previous = state.sessions.insert(
                paired.profile_id.clone(),
                ActiveSession {
                    connection_id,
                    companion_id: paired.companion_id.clone(),
                    outbound,
                },
            );
            if let Some(previous) = &previous {
                self.retire_binding_releases(previous.connection_id);
                Self::remove_connection_state(&mut state, previous.connection_id);
            }
            previous
        };
        if let Some(previous) = previous {
            let _ = previous.outbound.send(Message::Close(None)).await;
        }
        connection_id
    }

    pub(crate) async fn unregister(&self, profile_id: &ProfileId, connection_id: Uuid) {
        let pending = {
            let _grant_update = self.grant_updates.lock().await;
            let mut state = self.state.lock().await;
            if state
                .sessions
                .get(profile_id)
                .is_some_and(|session| session.connection_id == connection_id)
            {
                state.sessions.remove(profile_id);
            }
            self.retire_binding_releases(connection_id);
            Self::remove_connection_state(&mut state, connection_id)
        };
        for response in pending {
            let _ = response.send(Err(CompanionSessionError::ConnectionClosed));
        }
    }

    fn retire_binding_releases(&self, connection_id: Uuid) {
        self.binding_releases
            .lock()
            .expect("page-binding release mutex poisoned")
            .retain(|_, release| {
                let retire = *release
                    .connection_id
                    .lock()
                    .expect("page-binding release connection mutex poisoned")
                    == Some(connection_id);
                if retire {
                    release
                        .result
                        .send_replace(Some(Err(CompanionSessionError::ConnectionClosed)));
                }
                !retire
            });
    }

    fn remove_connection_state(
        state: &mut SessionState,
        connection_id: Uuid,
    ) -> Vec<oneshot::Sender<Result<CompanionEvent, CompanionSessionError>>> {
        state
            .discoveries
            .retain(|_, discovery| discovery.connection_id != connection_id);
        state
            .grants
            .retain(|_, grant| grant.connection_id != connection_id);
        let binding_nonces = state
            .bindings
            .iter()
            .filter(|(_, binding)| binding.connection_id == connection_id)
            .map(|(nonce, _)| nonce.clone())
            .collect::<Vec<_>>();
        for nonce in binding_nonces {
            if let Some(binding) = state.bindings.remove(&nonce) {
                let _ = binding
                    .response
                    .send(Err(CompanionSessionError::ConnectionClosed));
            }
        }
        let command_ids: Vec<_> = state
            .pending
            .iter()
            .filter(|(_, pending)| pending.connection_id == connection_id)
            .map(|(command_id, _)| command_id.clone())
            .collect();
        command_ids
            .into_iter()
            .filter_map(|command_id| {
                state
                    .pending
                    .remove(&command_id)
                    .and_then(|pending| pending.response)
            })
            .collect()
    }

    pub(crate) async fn consume_event(
        &self,
        profile_id: &ProfileId,
        connection_id: Uuid,
        event: CompanionEvent,
    ) -> Result<(), CompanionSessionError> {
        match event {
            CompanionEvent::TargetsDiscovered(discovery) => {
                self.record_discovery(profile_id, connection_id, discovery)
                    .await
            }
            CompanionEvent::PageBindingDiscovered(binding) => {
                self.record_page_binding(profile_id, connection_id, binding)
                    .await
            }
            CompanionEvent::ActionCompleted(ref result) => {
                let command_id = result.command_id.clone();
                self.complete_command(connection_id, &command_id, event)
                    .await
            }
            CompanionEvent::ActionFailed { ref command_id, .. } => {
                let command_id = command_id.clone();
                self.complete_command(connection_id, &command_id, event)
                    .await
            }
            CompanionEvent::Pong => Ok(()),
            CompanionEvent::Paired { .. } => Err(CompanionSessionError::InvalidEvent),
        }
    }

    async fn record_discovery(
        &self,
        profile_id: &ProfileId,
        connection_id: Uuid,
        discovery: TargetDiscovery,
    ) -> Result<(), CompanionSessionError> {
        if discovery.protocol_version != PROTOCOL_VERSION || discovery.profile_id != *profile_id {
            return Err(CompanionSessionError::ProfileMismatch);
        }
        if discovery.targets.len() > MAX_DISCOVERED_TARGETS {
            return Err(CompanionSessionError::InvalidEvent);
        }
        let mut target_ids = HashSet::new();
        for target in &discovery.targets {
            if target.target_id.is_empty()
                || target.target_id.len() > MAX_TARGET_ID_BYTES
                || !target_ids.insert(target.target_id.clone())
            {
                return Err(CompanionSessionError::InvalidEvent);
            }
        }
        let mut state = self.state.lock().await;
        let session = state
            .sessions
            .get(profile_id)
            .ok_or(CompanionSessionError::ProfileUnavailable)?;
        if session.connection_id != connection_id {
            return Err(CompanionSessionError::ConnectionClosed);
        }
        let companion_id = session.companion_id.clone();
        state.discoveries.insert(
            profile_id.clone(),
            DiscoveryRecord {
                connection_id,
                companion_id,
                targets: discovery.targets,
            },
        );
        drop(state);
        self.discovery_changed.notify_waiters();
        Ok(())
    }

    pub(crate) async fn begin_page_binding(
        self: &Arc<Self>,
        attachment_id: &AttachmentId,
        expected_page_id: PageId,
    ) -> Result<PageBindingTicket, CompanionSessionError> {
        let lease = self.registry.resolve_attachment(attachment_id).await?;
        let (binding_nonce, connection_id, response) = {
            let mut state = self.state.lock().await;
            purge_expired_bindings(&mut state, Instant::now());
            if state.bindings.len() >= MAX_PENDING_BINDINGS {
                return Err(CompanionSessionError::BindingCapacity);
            }
            let record = state
                .grants
                .get(attachment_id)
                .cloned()
                .ok_or(CompanionSessionError::GrantUnavailable)?;
            if record.grant.profile_id != lease.profile_id
                || record.companion_id != lease.companion_id
                || record.grant.expires_at_unix_ms <= now_unix_ms()
            {
                return Err(CompanionSessionError::AttachmentMismatch);
            }
            if state
                .grants
                .values()
                .flat_map(|grant| &grant.grant.pages)
                .any(|page| page.page_id == expected_page_id)
                || state
                    .bindings
                    .values()
                    .any(|binding| binding.expected_page_id == expected_page_id)
            {
                return Err(CompanionSessionError::InvalidEvent);
            }
            let session = state
                .sessions
                .get(&lease.profile_id)
                .cloned()
                .ok_or(CompanionSessionError::ProfileUnavailable)?;
            let discovery = state
                .discoveries
                .get(&lease.profile_id)
                .cloned()
                .ok_or(CompanionSessionError::DiscoveryUnavailable)?;
            if session.connection_id != record.connection_id
                || discovery.connection_id != record.connection_id
                || session.companion_id != record.companion_id
                || discovery.companion_id != record.companion_id
            {
                return Err(CompanionSessionError::ConnectionClosed);
            }
            let binding_nonce = loop {
                let candidate = Uuid::new_v4().to_string();
                if !state.bindings.contains_key(&candidate) {
                    break candidate;
                }
            };
            let (response, receiver) = oneshot::channel();
            state.bindings.insert(
                binding_nonce.clone(),
                PendingBinding {
                    connection_id: record.connection_id,
                    profile_id: lease.profile_id,
                    attachment_id: attachment_id.clone(),
                    expected_page_id,
                    known_targets: discovery
                        .targets
                        .into_iter()
                        .map(|target| target.target_id)
                        .collect(),
                    response,
                    expires_at: Instant::now() + PAGE_BINDING_TTL,
                },
            );
            (binding_nonce, record.connection_id, receiver)
        };
        Ok(PageBindingTicket {
            state: Arc::clone(&self.state),
            binding_nonce: Some(binding_nonce),
            connection_id,
            response: Some(response),
        })
    }

    pub(crate) async fn release_page_binding(
        &self,
        attachment_id: &AttachmentId,
        page_id: &PageId,
    ) -> Result<(), CompanionSessionError> {
        let key = PageBindingReleaseKey {
            attachment_id: attachment_id.clone(),
            page_id: page_id.clone(),
        };
        let (mut result, owned) = {
            let mut releases = self
                .binding_releases
                .lock()
                .expect("page-binding release mutex poisoned");
            let current = releases.get(&key).cloned();
            if let Some(release) = current {
                let completed = release.result.borrow().clone();
                if matches!(completed, Some(Err(_))) {
                    let connection_id = *release
                        .connection_id
                        .lock()
                        .expect("page-binding release connection mutex poisoned");
                    let (result, receiver) = watch::channel(None);
                    let release = Arc::new(PageBindingRelease {
                        result,
                        connection_id: TaskMutex::new(connection_id),
                    });
                    releases.insert(key.clone(), Arc::clone(&release));
                    (receiver, Some((release, true)))
                } else {
                    (release.result.subscribe(), None)
                }
            } else {
                let (result, receiver) = watch::channel(None);
                let release = Arc::new(PageBindingRelease {
                    result,
                    connection_id: TaskMutex::new(None),
                });
                releases.insert(key.clone(), Arc::clone(&release));
                (receiver, Some((release, false)))
            }
        };
        if let Some((release, republish)) = owned {
            let state = Arc::clone(&self.state);
            let grant_updates = Arc::clone(&self.grant_updates);
            let binding_releases = Arc::clone(&self.binding_releases);
            tokio::spawn(async move {
                let outcome = Self::release_page_binding_owned(
                    &state,
                    &grant_updates,
                    &release,
                    &key.attachment_id,
                    &key.page_id,
                    republish,
                )
                .await;
                if outcome.is_ok() {
                    let mut releases = binding_releases
                        .lock()
                        .expect("page-binding release mutex poisoned");
                    if releases
                        .get(&key)
                        .is_some_and(|current| Arc::ptr_eq(current, &release))
                    {
                        releases.remove(&key);
                    }
                }
            });
        }

        loop {
            if let Some(outcome) = result.borrow().clone() {
                return outcome;
            }
            result
                .changed()
                .await
                .map_err(|_| CompanionSessionError::ConnectionClosed)?;
        }
    }

    async fn release_page_binding_owned(
        state: &Arc<Mutex<SessionState>>,
        grant_updates: &Arc<Mutex<()>>,
        release: &PageBindingRelease,
        attachment_id: &AttachmentId,
        page_id: &PageId,
        republish: bool,
    ) -> Result<(), CompanionSessionError> {
        if release
            .connection_id
            .lock()
            .expect("page-binding release connection mutex poisoned")
            .is_none()
        {
            let connection_id = {
                let mut state = state.lock().await;
                let connection_id = state
                    .grants
                    .get(attachment_id)
                    .map(|record| record.connection_id);
                if connection_id.is_none() {
                    expire_page_bindings(&mut state, attachment_id, page_id);
                }
                connection_id
            };
            let Some(connection_id) = connection_id else {
                release.result.send_replace(Some(Ok(())));
                return Ok(());
            };
            *release
                .connection_id
                .lock()
                .expect("page-binding release connection mutex poisoned") = Some(connection_id);
        }
        let grant_update = match acquire_grant_update(grant_updates).await {
            Ok(grant_update) => grant_update,
            Err(error) => {
                release.result.send_replace(Some(Err(error.clone())));
                return Err(error);
            }
        };
        let outcome = async {
            let update = {
                let mut state = state.lock().await;
                expire_page_bindings(&mut state, attachment_id, page_id);

                let Some(record) = state.grants.get(attachment_id).cloned() else {
                    return if republish {
                        Err(CompanionSessionError::GrantUnavailable)
                    } else {
                        Ok(())
                    };
                };
                *release
                    .connection_id
                    .lock()
                    .expect("page-binding release connection mutex poisoned") =
                    Some(record.connection_id);
                let contains_page = record
                    .grant
                    .pages
                    .iter()
                    .any(|page| page.page_id == *page_id);
                if !contains_page && !republish {
                    return Ok(());
                }
                let mut updated = record.grant;
                if contains_page {
                    updated.pages.retain(|page| page.page_id != *page_id);
                    state
                        .grants
                        .get_mut(attachment_id)
                        .expect("grant exists")
                        .grant = updated.clone();
                }
                let outbound = state
                    .sessions
                    .get(&updated.profile_id)
                    .filter(|session| {
                        session.connection_id == record.connection_id
                            && session.companion_id == record.companion_id
                    })
                    .map(|session| session.outbound.clone())
                    .ok_or(CompanionSessionError::ConnectionClosed)?;
                Ok::<_, CompanionSessionError>((outbound, updated))
            }?;

            send_grant_request(&update.0, &update.1).await
        }
        .await;
        drop(grant_update);
        release.result.send_replace(Some(outcome.clone()));
        outcome
    }

    async fn record_page_binding(
        &self,
        profile_id: &ProfileId,
        connection_id: Uuid,
        binding: PageBindingDiscovered,
    ) -> Result<(), CompanionSessionError> {
        if binding.protocol_version != PROTOCOL_VERSION || binding.profile_id != *profile_id {
            return Err(CompanionSessionError::ProfileMismatch);
        }
        if binding.target_id.is_empty()
            || binding.target_id.len() > MAX_TARGET_ID_BYTES
            || Uuid::parse_str(&binding.binding_nonce).is_err()
        {
            return Ok(());
        }

        let _grant_update = match acquire_grant_update(&self.grant_updates).await {
            Ok(grant_update) => grant_update,
            Err(error) => {
                let mut state = self.state.lock().await;
                if state
                    .bindings
                    .get(&binding.binding_nonce)
                    .is_some_and(|pending| {
                        pending.connection_id == connection_id && pending.profile_id == *profile_id
                    })
                {
                    let pending = state
                        .bindings
                        .remove(&binding.binding_nonce)
                        .expect("pending binding exists");
                    let _ = pending.response.send(Err(error.clone()));
                }
                return Err(error);
            }
        };
        let prepared = {
            let mut state = self.state.lock().await;
            purge_expired_bindings(&mut state, Instant::now());
            let Some(pending) = state.bindings.get(&binding.binding_nonce) else {
                return Ok(());
            };
            if pending.connection_id != connection_id || pending.profile_id != *profile_id {
                return Err(CompanionSessionError::InvalidEvent);
            }
            let Some(discovery) = state.discoveries.get(profile_id) else {
                return Ok(());
            };
            let newly_discovered_page = discovery.connection_id == connection_id
                && discovery.targets.iter().any(|target| {
                    target.target_id == binding.target_id
                        && target.kind == TargetKind::Page
                        && !pending.known_targets.contains(&target.target_id)
                })
                && !state
                    .grants
                    .values()
                    .flat_map(|grant| &grant.grant.pages)
                    .any(|page| page.target_id == binding.target_id);
            if !newly_discovered_page {
                return Ok(());
            }
            let attachment_id = pending.attachment_id.clone();
            let expected_page_id = pending.expected_page_id.clone();
            let Some(record) = state.grants.get(&attachment_id).cloned() else {
                let pending = state
                    .bindings
                    .remove(&binding.binding_nonce)
                    .expect("pending binding exists");
                let _ = pending
                    .response
                    .send(Err(CompanionSessionError::GrantUnavailable));
                return Ok(());
            };
            let Some(session) = state.sessions.get(profile_id).cloned() else {
                return Err(CompanionSessionError::ProfileUnavailable);
            };
            if record.connection_id != connection_id
                || session.connection_id != connection_id
                || record.companion_id != session.companion_id
            {
                return Err(CompanionSessionError::ConnectionClosed);
            }
            let previous = record.grant.clone();
            let mut updated = record.grant;
            updated.pages.retain(|page| {
                page.target_id != binding.target_id && page.page_id != expected_page_id
            });
            if updated.pages.len() >= MAX_DISCOVERED_TARGETS {
                let pending = state
                    .bindings
                    .remove(&binding.binding_nonce)
                    .expect("pending binding exists");
                let _ = pending
                    .response
                    .send(Err(CompanionSessionError::BindingCapacity));
                return Ok(());
            }
            updated.pages.push(GrantedPage {
                target_id: binding.target_id,
                page_id: expected_page_id,
            });
            state
                .grants
                .get_mut(&attachment_id)
                .expect("grant exists")
                .grant = updated.clone();
            let pending = state
                .bindings
                .remove(&binding.binding_nonce)
                .expect("pending binding exists");
            (pending, session.outbound, previous, updated)
        };

        let (pending, outbound, previous, updated) = prepared;
        if let Err(error) = send_grant_request(&outbound, &updated).await {
            let mut state = self.state.lock().await;
            if let Some(current) = state.grants.get_mut(&updated.attachment_id) {
                if current.connection_id == connection_id && current.grant == updated {
                    current.grant = previous;
                }
            }
            drop(state);
            let _ = pending.response.send(Err(error.clone()));
            return Err(error);
        }
        let _ = pending.response.send(Ok(updated));
        Ok(())
    }

    async fn complete_command(
        &self,
        connection_id: Uuid,
        command_id: &CommandId,
        event: CompanionEvent,
    ) -> Result<(), CompanionSessionError> {
        let pending = {
            let mut state = self.state.lock().await;
            purge_expired_pending(&mut state, Instant::now());
            let Some(pending) = state.pending.get(command_id) else {
                return Err(CompanionSessionError::InvalidEvent);
            };
            if pending.connection_id != connection_id {
                return Err(CompanionSessionError::InvalidEvent);
            }
            state
                .pending
                .remove(command_id)
                .expect("pending command exists")
        };
        if let Some(response) = pending.response {
            let _ = response.send(Ok(event));
        }
        Ok(())
    }

    pub(crate) async fn send_request(
        &self,
        profile_id: &ProfileId,
        request: CompanionRequest,
    ) -> Result<(), CompanionSessionError> {
        if !matches!(request, CompanionRequest::Ping) {
            return Err(CompanionSessionError::InvalidEvent);
        }
        let outbound = self.outbound_for(profile_id).await?;
        send_request(&outbound, &request).await
    }

    async fn outbound_for(
        &self,
        profile_id: &ProfileId,
    ) -> Result<mpsc::Sender<Message>, CompanionSessionError> {
        self.state
            .lock()
            .await
            .sessions
            .get(profile_id)
            .map(|session| session.outbound.clone())
            .ok_or(CompanionSessionError::ProfileUnavailable)
    }

    pub(crate) async fn wait_for_discovery(
        &self,
        profile_id: &ProfileId,
        timeout: Duration,
    ) -> Result<Vec<BrowserTarget>, CompanionSessionError> {
        tokio::time::timeout(timeout, async {
            loop {
                let notified = self.discovery_changed.notified();
                if let Some(targets) = self
                    .state
                    .lock()
                    .await
                    .discoveries
                    .get(profile_id)
                    .map(|discovery| discovery.targets.clone())
                {
                    return Ok(targets);
                }
                notified.await;
            }
        })
        .await
        .map_err(|_| CompanionSessionError::DiscoveryUnavailable)?
    }

    pub(crate) async fn active_grant(&self, profile_id: &ProfileId) -> Option<AttachmentGrant> {
        let now = now_unix_ms();
        self.state
            .lock()
            .await
            .grants
            .values()
            .find(|record| {
                record.grant.profile_id == *profile_id && record.grant.expires_at_unix_ms > now
            })
            .map(|record| record.grant.clone())
    }

    pub(crate) async fn grant_discovered_targets(
        &self,
        profile_id: &ProfileId,
    ) -> Result<AttachmentGrant, CompanionSessionError> {
        let _grant_update = acquire_grant_update(&self.grant_updates).await?;
        let (session, discovery) = {
            let state = self.state.lock().await;
            let session = state
                .sessions
                .get(profile_id)
                .cloned()
                .ok_or(CompanionSessionError::ProfileUnavailable)?;
            let discovery = state
                .discoveries
                .get(profile_id)
                .cloned()
                .ok_or(CompanionSessionError::DiscoveryUnavailable)?;
            if session.connection_id != discovery.connection_id
                || session.companion_id != discovery.companion_id
            {
                return Err(CompanionSessionError::ConnectionClosed);
            }
            (session, discovery)
        };
        let lease = self.registry.attach(profile_id.clone()).await?;
        let grant = grant_for_lease(&lease, discovery.targets);
        send_grant_request(&session.outbound, &grant).await?;

        let mut state = self.state.lock().await;
        if !state.sessions.get(profile_id).is_some_and(|current| {
            current.connection_id == session.connection_id
                && current.companion_id == session.companion_id
        }) {
            return Err(CompanionSessionError::ConnectionClosed);
        }
        state.grants.retain(|_, record| {
            record.grant.profile_id != *profile_id || record.connection_id != session.connection_id
        });
        state.grants.insert(
            grant.attachment_id.clone(),
            GrantRecord {
                connection_id: session.connection_id,
                companion_id: session.companion_id,
                grant: grant.clone(),
            },
        );
        Ok(grant)
    }

    pub(crate) async fn renew_grant(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<AttachmentGrant, CompanionSessionError> {
        let _grant_update = acquire_grant_update(&self.grant_updates).await?;
        let (record, session) = {
            let state = self.state.lock().await;
            let record = state
                .grants
                .get(attachment_id)
                .cloned()
                .ok_or(CompanionSessionError::GrantUnavailable)?;
            let session = state
                .sessions
                .get(&record.grant.profile_id)
                .cloned()
                .ok_or(CompanionSessionError::ProfileUnavailable)?;
            if session.connection_id != record.connection_id
                || session.companion_id != record.companion_id
            {
                return Err(CompanionSessionError::ConnectionClosed);
            }
            (record, session)
        };
        let lease = self.registry.renew_attachment(attachment_id).await?;
        if lease.profile_id != record.grant.profile_id || lease.companion_id != record.companion_id
        {
            return Err(CompanionSessionError::AttachmentMismatch);
        }
        let mut renewed = record.grant.clone();
        renewed.expires_at_unix_ms = lease_expiry_unix_ms(&lease);
        send_grant_request(&session.outbound, &renewed).await?;

        let mut state = self.state.lock().await;
        if !state
            .sessions
            .get(&renewed.profile_id)
            .is_some_and(|current| {
                current.connection_id == record.connection_id
                    && current.companion_id == record.companion_id
            })
        {
            return Err(CompanionSessionError::ConnectionClosed);
        }
        let current = state
            .grants
            .get_mut(attachment_id)
            .ok_or(CompanionSessionError::GrantUnavailable)?;
        if current.connection_id != record.connection_id {
            return Err(CompanionSessionError::ConnectionClosed);
        }
        current.grant = renewed.clone();
        Ok(renewed)
    }

    pub(crate) async fn dispatch_action(
        &self,
        action: ActionRequest,
    ) -> Result<CompanionEvent, CompanionSessionError> {
        let now = now_unix_ms();
        if action.protocol_version != PROTOCOL_VERSION || action.deadline_unix_ms <= now {
            return Err(CompanionSessionError::DeadlineExceeded);
        }
        let lease = self
            .registry
            .resolve_attachment(&action.attachment_id)
            .await?;
        let remaining_ms = u64::try_from(action.deadline_unix_ms.saturating_sub(now)).unwrap_or(0);
        let wait = Duration::from_millis(remaining_ms).min(MAX_COMMAND_WAIT);
        let expires_at = Instant::now() + wait;
        let (connection_id, outbound) = {
            let mut state = self.state.lock().await;
            purge_expired_pending(&mut state, Instant::now());
            let record = state
                .grants
                .get(&action.attachment_id)
                .cloned()
                .ok_or(CompanionSessionError::GrantUnavailable)?;
            if record.companion_id != lease.companion_id
                || record.grant.profile_id != lease.profile_id
                || record.grant.attachment_id != action.attachment_id
            {
                return Err(CompanionSessionError::AttachmentMismatch);
            }
            if record.grant.expires_at_unix_ms <= now {
                return Err(CompanionSessionError::GrantUnavailable);
            }
            if !record
                .grant
                .pages
                .iter()
                .any(|page| page.page_id == action.page_id)
            {
                return Err(CompanionSessionError::PageMismatch);
            }
            let session = state
                .sessions
                .get(&lease.profile_id)
                .cloned()
                .ok_or(CompanionSessionError::ProfileUnavailable)?;
            if session.connection_id != record.connection_id
                || session.companion_id != lease.companion_id
            {
                return Err(CompanionSessionError::ConnectionClosed);
            }
            if state.pending.contains_key(&action.command_id) {
                return Err(CompanionSessionError::InvalidEvent);
            }
            if state.pending.len() >= MAX_PENDING_COMMANDS {
                return Err(CompanionSessionError::PendingCapacity);
            }
            let (response, receiver) = oneshot::channel();
            state.pending.insert(
                action.command_id.clone(),
                PendingCommand {
                    connection_id: session.connection_id,
                    response: Some(response),
                    expires_at,
                },
            );
            (session.connection_id, (session.outbound, receiver))
        };
        let (outbound, receiver) = outbound;
        let mut pending_guard = PendingGuard::new(
            Arc::clone(&self.state),
            action.command_id.clone(),
            connection_id,
        );
        if let Err(error) = send_request(&outbound, &CompanionRequest::Action(action.clone())).await
        {
            self.remove_pending(&action.command_id, connection_id).await;
            pending_guard.disarm();
            return Err(error);
        }
        match tokio::time::timeout(wait, receiver).await {
            Ok(Ok(result)) => {
                pending_guard.disarm();
                result
            }
            Ok(Err(_)) => {
                pending_guard.disarm();
                Err(CompanionSessionError::ConnectionClosed)
            }
            Err(_) => {
                pending_guard.abandon().await;
                Err(CompanionSessionError::ResponseTimeout)
            }
        }
    }

    async fn remove_pending(&self, command_id: &CommandId, connection_id: Uuid) {
        let mut state = self.state.lock().await;
        if state
            .pending
            .get(command_id)
            .is_some_and(|pending| pending.connection_id == connection_id)
        {
            state.pending.remove(command_id);
        }
    }
}

fn grant_for_lease(lease: &AttachmentLease, targets: Vec<BrowserTarget>) -> AttachmentGrant {
    AttachmentGrant {
        protocol_version: PROTOCOL_VERSION,
        attachment_id: lease.attachment_id.clone(),
        profile_id: lease.profile_id.clone(),
        expires_at_unix_ms: lease_expiry_unix_ms(lease),
        pages: targets
            .into_iter()
            .map(|target| GrantedPage {
                target_id: target.target_id,
                page_id: PageId::new(),
            })
            .collect(),
    }
}

fn lease_expiry_unix_ms(lease: &AttachmentLease) -> i64 {
    now_unix_ms().saturating_add(
        i64::try_from(
            lease
                .expires_at
                .saturating_duration_since(Instant::now())
                .as_millis(),
        )
        .unwrap_or(i64::MAX),
    )
}

fn now_unix_ms() -> i64 {
    i64::try_from(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis(),
    )
    .unwrap_or(i64::MAX)
}

async fn send_request(
    outbound: &mpsc::Sender<Message>,
    request: &CompanionRequest,
) -> Result<(), CompanionSessionError> {
    let body = serde_json::to_string(request).map_err(|_| CompanionSessionError::InvalidEvent)?;
    outbound
        .send(Message::Text(body.into()))
        .await
        .map_err(|_| CompanionSessionError::QueueClosed)
}

async fn acquire_grant_update(
    grant_updates: &Mutex<()>,
) -> Result<tokio::sync::MutexGuard<'_, ()>, CompanionSessionError> {
    tokio::time::timeout(GRANT_PUBLICATION_TIMEOUT, grant_updates.lock())
        .await
        .map_err(|_| CompanionSessionError::QueueClosed)
}

async fn send_grant_request(
    outbound: &mpsc::Sender<Message>,
    grant: &AttachmentGrant,
) -> Result<(), CompanionSessionError> {
    tokio::time::timeout(
        GRANT_PUBLICATION_TIMEOUT,
        send_request(outbound, &CompanionRequest::Grant(grant.clone())),
    )
    .await
    .map_err(|_| CompanionSessionError::QueueClosed)?
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PairingInput;
    use companion_protocol::{ActionResult, InteractionPath, PageBindingDiscovered, TargetKind};

    async fn session_fixture(
        outbound_capacity: usize,
    ) -> (
        Arc<SessionCoordinator>,
        ProfileId,
        Uuid,
        AttachmentGrant,
        mpsc::Receiver<Message>,
    ) {
        let registry = Arc::new(CompanionRegistry::new(
            Duration::from_secs(60),
            Duration::from_secs(60),
        ));
        let code = registry.issue_pairing_code().await;
        let input = PairingInput::firefox(code);
        let profile_id = input.profile_id.clone();
        let paired = registry.pair(input).await.unwrap();
        let coordinator = Arc::new(SessionCoordinator::new(registry));
        let (outbound, mut requests) = mpsc::channel(outbound_capacity);
        let connection_id = coordinator.register(paired, outbound).await;
        coordinator
            .consume_event(
                &profile_id,
                connection_id,
                CompanionEvent::TargetsDiscovered(TargetDiscovery {
                    protocol_version: PROTOCOL_VERSION,
                    profile_id: profile_id.clone(),
                    targets: vec![BrowserTarget {
                        target_id: "trusted-target".into(),
                        kind: TargetKind::Page,
                    }],
                }),
            )
            .await
            .unwrap();
        let grant = coordinator
            .grant_discovered_targets(&profile_id)
            .await
            .unwrap();
        let _grant_request = requests.recv().await.unwrap();
        (coordinator, profile_id, connection_id, grant, requests)
    }

    async fn bind_page_for_release(
        coordinator: &Arc<SessionCoordinator>,
        profile_id: &ProfileId,
        connection_id: Uuid,
        grant: &AttachmentGrant,
        requests: &mut mpsc::Receiver<Message>,
        target_id: &str,
    ) -> PageId {
        let page_id = PageId::new();
        let ticket = coordinator
            .begin_page_binding(&grant.attachment_id, page_id.clone())
            .await
            .unwrap();
        let binding_nonce = ticket.binding_nonce().to_owned();
        coordinator
            .consume_event(
                profile_id,
                connection_id,
                CompanionEvent::TargetsDiscovered(TargetDiscovery {
                    protocol_version: PROTOCOL_VERSION,
                    profile_id: profile_id.clone(),
                    targets: vec![BrowserTarget {
                        target_id: target_id.into(),
                        kind: TargetKind::Page,
                    }],
                }),
            )
            .await
            .unwrap();
        coordinator
            .consume_event(
                profile_id,
                connection_id,
                CompanionEvent::PageBindingDiscovered(PageBindingDiscovered {
                    protocol_version: PROTOCOL_VERSION,
                    profile_id: profile_id.clone(),
                    target_id: target_id.into(),
                    binding_nonce,
                }),
            )
            .await
            .unwrap();
        ticket.complete(Duration::from_secs(1)).await.unwrap();
        let Message::Text(binding_grant) = requests.recv().await.unwrap() else {
            panic!("expected the page-binding grant")
        };
        let CompanionRequest::Grant(binding_grant): CompanionRequest =
            serde_json::from_str(binding_grant.as_str()).unwrap()
        else {
            panic!("expected a page-binding grant")
        };
        assert!(binding_grant
            .pages
            .iter()
            .any(|page| page.page_id == page_id));
        page_id
    }

    fn action(grant: &AttachmentGrant, command_id: CommandId) -> ActionRequest {
        ActionRequest {
            protocol_version: PROTOCOL_VERSION,
            attachment_id: grant.attachment_id.clone(),
            command_id,
            page_id: grant.pages[0].page_id.clone(),
            operation: "observe".into(),
            input: serde_json::json!({}),
            deadline_unix_ms: now_unix_ms() + 30_000,
        }
    }

    #[tokio::test]
    async fn duplicate_command_id_cannot_replace_pending_response() {
        let registry = Arc::new(CompanionRegistry::new(
            Duration::from_secs(60),
            Duration::from_secs(60),
        ));
        let code = registry.issue_pairing_code().await;
        let input = PairingInput::firefox(code);
        let profile_id = input.profile_id.clone();
        let paired = registry.pair(input).await.unwrap();
        let coordinator = Arc::new(SessionCoordinator::new(registry));
        let (outbound, mut requests) = mpsc::channel(4);
        let connection_id = coordinator.register(paired, outbound).await;
        coordinator
            .consume_event(
                &profile_id,
                connection_id,
                CompanionEvent::TargetsDiscovered(TargetDiscovery {
                    protocol_version: PROTOCOL_VERSION,
                    profile_id: profile_id.clone(),
                    targets: vec![BrowserTarget {
                        target_id: "trusted-target".into(),
                        kind: TargetKind::Page,
                    }],
                }),
            )
            .await
            .unwrap();
        let grant = coordinator
            .grant_discovered_targets(&profile_id)
            .await
            .unwrap();
        let _grant_request = requests.recv().await.unwrap();

        let command_id = CommandId::new();
        let action = ActionRequest {
            protocol_version: PROTOCOL_VERSION,
            attachment_id: grant.attachment_id,
            command_id: command_id.clone(),
            page_id: grant.pages[0].page_id.clone(),
            operation: "observe".into(),
            input: serde_json::json!({}),
            deadline_unix_ms: now_unix_ms() + 5_000,
        };
        let first = tokio::spawn({
            let coordinator = Arc::clone(&coordinator);
            let action = action.clone();
            async move { coordinator.dispatch_action(action).await }
        });
        let _action_request = requests.recv().await.unwrap();

        let duplicate = tokio::time::timeout(
            Duration::from_millis(50),
            coordinator.dispatch_action(action),
        )
        .await;
        assert!(matches!(
            duplicate,
            Ok(Err(CompanionSessionError::InvalidEvent))
        ));

        let completed = CompanionEvent::ActionCompleted(ActionResult {
            command_id,
            interaction_path: InteractionPath::ExtensionApi,
            output: serde_json::json!({"ok": true}),
        });
        assert_eq!(
            coordinator
                .consume_event(&profile_id, Uuid::new_v4(), completed.clone())
                .await,
            Err(CompanionSessionError::InvalidEvent)
        );
        coordinator
            .consume_event(&profile_id, connection_id, completed.clone())
            .await
            .unwrap();
        assert_eq!(first.await.unwrap().unwrap(), completed);
    }

    #[tokio::test]
    async fn aborted_dispatch_keeps_a_bounded_tombstone_and_consumes_the_late_event() {
        let (coordinator, profile_id, connection_id, grant, mut requests) =
            session_fixture(4).await;
        let command_id = CommandId::new();
        let dispatch = tokio::spawn({
            let coordinator = Arc::clone(&coordinator);
            let action = action(&grant, command_id.clone());
            async move { coordinator.dispatch_action(action).await }
        });
        let _action_request = requests.recv().await.unwrap();

        dispatch.abort();
        assert!(dispatch.await.unwrap_err().is_cancelled());
        tokio::task::yield_now().await;
        assert_eq!(coordinator.state.lock().await.pending.len(), 1);

        let completed = CompanionEvent::ActionCompleted(ActionResult {
            command_id,
            interaction_path: InteractionPath::ExtensionApi,
            output: serde_json::json!({"late": true}),
        });
        coordinator
            .consume_event(&profile_id, connection_id, completed)
            .await
            .unwrap();
        assert!(coordinator.state.lock().await.pending.is_empty());
    }

    #[tokio::test]
    async fn abandoned_dispatches_are_bounded_and_unregister_cleans_them() {
        const EXPECTED_PENDING_BOUND: usize = 256;
        let (coordinator, profile_id, connection_id, grant, mut requests) =
            session_fixture(EXPECTED_PENDING_BOUND + 2).await;

        for _ in 0..EXPECTED_PENDING_BOUND {
            let dispatch = tokio::spawn({
                let coordinator = Arc::clone(&coordinator);
                let action = action(&grant, CommandId::new());
                async move { coordinator.dispatch_action(action).await }
            });
            let _action_request = requests.recv().await.unwrap();
            dispatch.abort();
            assert!(dispatch.await.unwrap_err().is_cancelled());
        }

        let mut overflow = tokio::spawn({
            let coordinator = Arc::clone(&coordinator);
            let action = action(&grant, CommandId::new());
            async move { coordinator.dispatch_action(action).await }
        });
        let overflow_result = tokio::time::timeout(Duration::from_millis(50), &mut overflow).await;
        let pending_count = coordinator.state.lock().await.pending.len();
        if overflow_result.is_err() {
            overflow.abort();
            let _ = overflow.await;
        }

        assert!(matches!(
            overflow_result,
            Ok(Ok(Err(CompanionSessionError::PendingCapacity)))
        ));
        assert!(pending_count <= EXPECTED_PENDING_BOUND);
        coordinator.unregister(&profile_id, connection_id).await;
        assert!(coordinator.state.lock().await.pending.is_empty());
    }

    #[test]
    fn pending_purge_removes_expired_abandoned_entries_without_dropping_a_live_waiter() {
        let connection_id = Uuid::new_v4();
        let live_id = CommandId::new();
        let abandoned_id = CommandId::new();
        let (live_response, live_receiver) =
            oneshot::channel::<Result<CompanionEvent, CompanionSessionError>>();
        let expired = Instant::now() - Duration::from_millis(1);
        let mut state = SessionState::default();
        state.pending.insert(
            live_id.clone(),
            PendingCommand {
                connection_id,
                response: Some(live_response),
                expires_at: expired,
            },
        );
        state.pending.insert(
            abandoned_id.clone(),
            PendingCommand {
                connection_id,
                response: None,
                expires_at: expired,
            },
        );

        purge_expired_pending(&mut state, Instant::now());
        assert!(state.pending.contains_key(&live_id));
        assert!(!state.pending.contains_key(&abandoned_id));

        drop(live_receiver);
        purge_expired_pending(&mut state, Instant::now());
        assert!(!state.pending.contains_key(&live_id));
    }

    #[tokio::test]
    async fn one_time_binding_nonce_grants_the_exact_coordinator_page_to_the_reported_target() {
        let (coordinator, profile_id, connection_id, grant, mut requests) =
            session_fixture(8).await;
        let expected_page = PageId::new();
        let ticket = coordinator
            .begin_page_binding(&grant.attachment_id, expected_page.clone())
            .await
            .unwrap();
        let nonce = ticket.binding_nonce().to_owned();
        assert!(!format!("{ticket:?}").contains(&nonce));

        coordinator
            .consume_event(
                &profile_id,
                connection_id,
                CompanionEvent::TargetsDiscovered(TargetDiscovery {
                    protocol_version: PROTOCOL_VERSION,
                    profile_id: profile_id.clone(),
                    targets: vec![
                        BrowserTarget {
                            target_id: "trusted-target".into(),
                            kind: TargetKind::Page,
                        },
                        BrowserTarget {
                            target_id: "new-bidi-context".into(),
                            kind: TargetKind::Page,
                        },
                    ],
                }),
            )
            .await
            .unwrap();

        coordinator
            .consume_event(
                &profile_id,
                connection_id,
                CompanionEvent::PageBindingDiscovered(PageBindingDiscovered {
                    protocol_version: PROTOCOL_VERSION,
                    profile_id: profile_id.clone(),
                    target_id: "new-bidi-context".into(),
                    binding_nonce: nonce.clone(),
                }),
            )
            .await
            .unwrap();
        let updated = ticket.complete(Duration::from_secs(1)).await.unwrap();
        let outbound = requests.recv().await.unwrap();
        let Message::Text(outbound) = outbound else {
            panic!("binding must publish an updated grant")
        };
        let published: CompanionRequest = serde_json::from_str(outbound.as_str()).unwrap();

        assert_eq!(published, CompanionRequest::Grant(updated.clone()));
        assert!(updated
            .pages
            .iter()
            .any(|page| { page.target_id == "new-bidi-context" && page.page_id == expected_page }));

        coordinator
            .consume_event(
                &profile_id,
                connection_id,
                CompanionEvent::PageBindingDiscovered(PageBindingDiscovered {
                    protocol_version: PROTOCOL_VERSION,
                    profile_id: profile_id.clone(),
                    target_id: "trusted-target".into(),
                    binding_nonce: nonce,
                }),
            )
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), requests.recv())
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn releasing_a_page_binding_clears_pending_and_granted_state_idempotently() {
        let (coordinator, profile_id, connection_id, grant, mut requests) =
            session_fixture(8).await;
        let expected_page = PageId::new();
        let ticket = coordinator
            .begin_page_binding(&grant.attachment_id, expected_page.clone())
            .await
            .unwrap();
        let nonce = ticket.binding_nonce().to_owned();
        coordinator
            .consume_event(
                &profile_id,
                connection_id,
                CompanionEvent::TargetsDiscovered(TargetDiscovery {
                    protocol_version: PROTOCOL_VERSION,
                    profile_id: profile_id.clone(),
                    targets: vec![
                        BrowserTarget {
                            target_id: "trusted-target".into(),
                            kind: TargetKind::Page,
                        },
                        BrowserTarget {
                            target_id: "cancelled-context".into(),
                            kind: TargetKind::Page,
                        },
                    ],
                }),
            )
            .await
            .unwrap();
        coordinator
            .consume_event(
                &profile_id,
                connection_id,
                CompanionEvent::PageBindingDiscovered(PageBindingDiscovered {
                    protocol_version: PROTOCOL_VERSION,
                    profile_id: profile_id.clone(),
                    target_id: "cancelled-context".into(),
                    binding_nonce: nonce,
                }),
            )
            .await
            .unwrap();
        ticket.complete(Duration::from_secs(1)).await.unwrap();
        let _grant_with_page = requests.recv().await.unwrap();

        coordinator
            .release_page_binding(&grant.attachment_id, &expected_page)
            .await
            .unwrap();
        let Message::Text(released) = requests.recv().await.unwrap() else {
            panic!("binding release must publish the reduced grant")
        };
        let CompanionRequest::Grant(released): CompanionRequest =
            serde_json::from_str(released.as_str()).unwrap()
        else {
            panic!("binding release must publish a grant")
        };
        assert!(!released
            .pages
            .iter()
            .any(|page| page.page_id == expected_page));

        coordinator
            .release_page_binding(&grant.attachment_id, &expected_page)
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), requests.recv())
                .await
                .is_err()
        );

        let pending_page = PageId::new();
        let pending = coordinator
            .begin_page_binding(&grant.attachment_id, pending_page.clone())
            .await
            .unwrap();
        coordinator
            .release_page_binding(&grant.attachment_id, &pending_page)
            .await
            .unwrap();
        assert!(matches!(
            pending.complete(Duration::from_secs(1)).await,
            Err(CompanionSessionError::BindingExpired)
        ));
        assert!(coordinator.state.lock().await.bindings.is_empty());
    }

    #[tokio::test]
    async fn binding_release_cannot_overtake_the_binding_grant_publication() {
        let (coordinator, profile_id, connection_id, grant, mut requests) =
            session_fixture(1).await;
        coordinator
            .send_request(&profile_id, CompanionRequest::Ping)
            .await
            .unwrap();

        let page_id = PageId::new();
        let ticket = coordinator
            .begin_page_binding(&grant.attachment_id, page_id.clone())
            .await
            .unwrap();
        let nonce = ticket.binding_nonce().to_owned();
        coordinator
            .consume_event(
                &profile_id,
                connection_id,
                CompanionEvent::TargetsDiscovered(TargetDiscovery {
                    protocol_version: PROTOCOL_VERSION,
                    profile_id: profile_id.clone(),
                    targets: vec![
                        BrowserTarget {
                            target_id: "trusted-target".into(),
                            kind: TargetKind::Page,
                        },
                        BrowserTarget {
                            target_id: "new-context".into(),
                            kind: TargetKind::Page,
                        },
                    ],
                }),
            )
            .await
            .unwrap();

        let publish = tokio::spawn({
            let coordinator = Arc::clone(&coordinator);
            let profile_id = profile_id.clone();
            async move {
                coordinator
                    .consume_event(
                        &profile_id,
                        connection_id,
                        CompanionEvent::PageBindingDiscovered(PageBindingDiscovered {
                            protocol_version: PROTOCOL_VERSION,
                            profile_id: profile_id.clone(),
                            target_id: "new-context".into(),
                            binding_nonce: nonce,
                        }),
                    )
                    .await
            }
        });
        loop {
            if coordinator
                .active_grant(&profile_id)
                .await
                .is_some_and(|active| active.pages.iter().any(|page| page.page_id == page_id))
            {
                break;
            }
            tokio::task::yield_now().await;
        }

        let release = coordinator.release_page_binding(&grant.attachment_id, &page_id);
        tokio::pin!(release);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut release)
                .await
                .is_err()
        );
        assert!(
            coordinator
                .active_grant(&profile_id)
                .await
                .unwrap()
                .pages
                .iter()
                .any(|page| page.page_id == page_id),
            "release must not mutate the grant until the in-flight grant publication completes"
        );

        let Message::Text(ping) = requests.recv().await.unwrap() else {
            panic!("expected queued ping")
        };
        assert_eq!(
            serde_json::from_str::<CompanionRequest>(ping.as_str()).unwrap(),
            CompanionRequest::Ping
        );
        let Message::Text(binding_grant) = requests.recv().await.unwrap() else {
            panic!("expected binding grant")
        };
        let CompanionRequest::Grant(binding_grant): CompanionRequest =
            serde_json::from_str(binding_grant.as_str()).unwrap()
        else {
            panic!("expected binding grant")
        };
        assert!(binding_grant
            .pages
            .iter()
            .any(|page| page.page_id == page_id));
        publish.await.unwrap().unwrap();
        ticket.complete(Duration::from_secs(1)).await.unwrap();

        release.await.unwrap();
        let Message::Text(reduced_grant) = requests.recv().await.unwrap() else {
            panic!("expected reduced grant")
        };
        let CompanionRequest::Grant(reduced_grant): CompanionRequest =
            serde_json::from_str(reduced_grant.as_str()).unwrap()
        else {
            panic!("expected reduced grant")
        };
        assert!(!reduced_grant
            .pages
            .iter()
            .any(|page| page.page_id == page_id));
    }

    #[tokio::test]
    async fn stalled_binding_grant_cannot_hold_the_release_gate() {
        let (coordinator, profile_id, connection_id, grant, requests) = session_fixture(1).await;
        coordinator
            .send_request(&profile_id, CompanionRequest::Ping)
            .await
            .unwrap();
        let page_id = PageId::new();
        let ticket = coordinator
            .begin_page_binding(&grant.attachment_id, page_id.clone())
            .await
            .unwrap();
        let binding_nonce = ticket.binding_nonce().to_owned();
        coordinator
            .consume_event(
                &profile_id,
                connection_id,
                CompanionEvent::TargetsDiscovered(TargetDiscovery {
                    protocol_version: PROTOCOL_VERSION,
                    profile_id: profile_id.clone(),
                    targets: vec![
                        BrowserTarget {
                            target_id: "trusted-target".into(),
                            kind: TargetKind::Page,
                        },
                        BrowserTarget {
                            target_id: "stalled-binding-context".into(),
                            kind: TargetKind::Page,
                        },
                    ],
                }),
            )
            .await
            .unwrap();
        let publication = tokio::spawn({
            let coordinator = Arc::clone(&coordinator);
            let profile_id = profile_id.clone();
            async move {
                coordinator
                    .consume_event(
                        &profile_id,
                        connection_id,
                        CompanionEvent::PageBindingDiscovered(PageBindingDiscovered {
                            protocol_version: PROTOCOL_VERSION,
                            profile_id: profile_id.clone(),
                            target_id: "stalled-binding-context".into(),
                            binding_nonce,
                        }),
                    )
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if coordinator
                    .active_grant(&profile_id)
                    .await
                    .is_some_and(|active| active.pages.iter().any(|page| page.page_id == page_id))
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("binding publication must perform its canonical mutation first");
        let release = tokio::spawn({
            let coordinator = Arc::clone(&coordinator);
            let attachment_id = grant.attachment_id.clone();
            let page_id = page_id.clone();
            async move {
                coordinator
                    .release_page_binding(&attachment_id, &page_id)
                    .await
            }
        });

        let (publication, release) = tokio::time::timeout(Duration::from_secs(2), async {
            (publication.await.unwrap(), release.await.unwrap())
        })
        .await
        .expect("a stalled binding publication must not own the release gate forever");
        assert_eq!(publication, Err(CompanionSessionError::QueueClosed));
        assert_eq!(release, Ok(()));
        assert!(!coordinator
            .active_grant(&profile_id)
            .await
            .unwrap()
            .pages
            .iter()
            .any(|page| page.page_id == page_id));
        assert_eq!(
            ticket.complete(Duration::from_secs(1)).await,
            Err(CompanionSessionError::QueueClosed)
        );
        assert!(coordinator.grant_updates.try_lock().is_ok());
        tokio::time::timeout(
            Duration::from_secs(1),
            coordinator.unregister(&profile_id, connection_id),
        )
        .await
        .expect("unregister must remain live after a stalled binding publication");
        drop(requests);
    }

    #[tokio::test]
    async fn stalled_rediscovery_grant_preserves_the_canonical_grant() {
        let (coordinator, profile_id, _connection_id, grant, _requests) = session_fixture(1).await;
        coordinator
            .send_request(&profile_id, CompanionRequest::Ping)
            .await
            .unwrap();

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            coordinator.grant_discovered_targets(&profile_id),
        )
        .await
        .expect("rediscovery grant publication must have a terminal deadline");
        assert_eq!(result, Err(CompanionSessionError::QueueClosed));
        assert_eq!(
            coordinator
                .active_grant(&profile_id)
                .await
                .unwrap()
                .attachment_id,
            grant.attachment_id
        );
        assert!(coordinator.grant_updates.try_lock().is_ok());
    }

    #[tokio::test]
    async fn stalled_renewal_grant_preserves_the_canonical_expiry() {
        let (coordinator, profile_id, _connection_id, grant, _requests) = session_fixture(1).await;
        coordinator
            .send_request(&profile_id, CompanionRequest::Ping)
            .await
            .unwrap();

        let result = tokio::time::timeout(
            Duration::from_secs(2),
            coordinator.renew_grant(&grant.attachment_id),
        )
        .await
        .expect("renewal grant publication must have a terminal deadline");
        assert_eq!(result, Err(CompanionSessionError::QueueClosed));
        assert_eq!(
            coordinator
                .active_grant(&profile_id)
                .await
                .unwrap()
                .expires_at_unix_ms,
            grant.expires_at_unix_ms
        );
        assert!(coordinator.grant_updates.try_lock().is_ok());
    }

    #[tokio::test]
    async fn cancelled_release_waiter_does_not_cancel_publication_and_retry_joins_it() {
        let (coordinator, profile_id, connection_id, grant, mut requests) =
            session_fixture(1).await;
        let page_id = PageId::new();
        let ticket = coordinator
            .begin_page_binding(&grant.attachment_id, page_id.clone())
            .await
            .unwrap();
        let nonce = ticket.binding_nonce().to_owned();
        coordinator
            .consume_event(
                &profile_id,
                connection_id,
                CompanionEvent::TargetsDiscovered(TargetDiscovery {
                    protocol_version: PROTOCOL_VERSION,
                    profile_id: profile_id.clone(),
                    targets: vec![
                        BrowserTarget {
                            target_id: "trusted-target".into(),
                            kind: TargetKind::Page,
                        },
                        BrowserTarget {
                            target_id: "release-owned-context".into(),
                            kind: TargetKind::Page,
                        },
                    ],
                }),
            )
            .await
            .unwrap();
        coordinator
            .consume_event(
                &profile_id,
                connection_id,
                CompanionEvent::PageBindingDiscovered(PageBindingDiscovered {
                    protocol_version: PROTOCOL_VERSION,
                    profile_id: profile_id.clone(),
                    target_id: "release-owned-context".into(),
                    binding_nonce: nonce,
                }),
            )
            .await
            .unwrap();
        ticket.complete(Duration::from_secs(1)).await.unwrap();
        let _binding_grant = requests.recv().await.unwrap();
        coordinator
            .send_request(&profile_id, CompanionRequest::Ping)
            .await
            .unwrap();

        assert!(
            tokio::time::timeout(
                Duration::from_millis(25),
                coordinator.release_page_binding(&grant.attachment_id, &page_id),
            )
            .await
            .is_err(),
            "the first waiter must time out while the outbound queue is saturated"
        );
        assert!(!coordinator
            .active_grant(&profile_id)
            .await
            .unwrap()
            .pages
            .iter()
            .any(|page| page.page_id == page_id));

        let mut retry = tokio::spawn({
            let coordinator = Arc::clone(&coordinator);
            let attachment_id = grant.attachment_id.clone();
            let page_id = page_id.clone();
            async move {
                coordinator
                    .release_page_binding(&attachment_id, &page_id)
                    .await
            }
        });
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut retry)
                .await
                .is_err(),
            "retry must join the unfinished publication instead of returning false success"
        );

        let Message::Text(ping) = requests.recv().await.unwrap() else {
            panic!("expected the queue-saturating ping")
        };
        assert_eq!(
            serde_json::from_str::<CompanionRequest>(ping.as_str()).unwrap(),
            CompanionRequest::Ping
        );
        retry.await.unwrap().unwrap();
        let Message::Text(reduced) = requests.recv().await.unwrap() else {
            panic!("expected the owned reduced-grant publication")
        };
        let CompanionRequest::Grant(reduced): CompanionRequest =
            serde_json::from_str(reduced.as_str()).unwrap()
        else {
            panic!("expected a reduced grant")
        };
        assert!(!reduced.pages.iter().any(|page| page.page_id == page_id));
        assert!(
            tokio::time::timeout(Duration::from_millis(25), requests.recv())
                .await
                .is_err(),
            "deduplicated retries must publish the reduced grant exactly once"
        );
    }

    #[tokio::test]
    async fn failed_owned_release_publication_is_durable_for_retries() {
        let (coordinator, profile_id, connection_id, grant, mut requests) =
            session_fixture(1).await;
        let page_id = PageId::new();
        let ticket = coordinator
            .begin_page_binding(&grant.attachment_id, page_id.clone())
            .await
            .unwrap();
        let nonce = ticket.binding_nonce().to_owned();
        coordinator
            .consume_event(
                &profile_id,
                connection_id,
                CompanionEvent::TargetsDiscovered(TargetDiscovery {
                    protocol_version: PROTOCOL_VERSION,
                    profile_id: profile_id.clone(),
                    targets: vec![
                        BrowserTarget {
                            target_id: "trusted-target".into(),
                            kind: TargetKind::Page,
                        },
                        BrowserTarget {
                            target_id: "release-failure-context".into(),
                            kind: TargetKind::Page,
                        },
                    ],
                }),
            )
            .await
            .unwrap();
        coordinator
            .consume_event(
                &profile_id,
                connection_id,
                CompanionEvent::PageBindingDiscovered(PageBindingDiscovered {
                    protocol_version: PROTOCOL_VERSION,
                    profile_id: profile_id.clone(),
                    target_id: "release-failure-context".into(),
                    binding_nonce: nonce,
                }),
            )
            .await
            .unwrap();
        ticket.complete(Duration::from_secs(1)).await.unwrap();
        let _binding_grant = requests.recv().await.unwrap();
        coordinator
            .send_request(&profile_id, CompanionRequest::Ping)
            .await
            .unwrap();

        assert!(tokio::time::timeout(
            Duration::from_millis(25),
            coordinator.release_page_binding(&grant.attachment_id, &page_id),
        )
        .await
        .is_err());
        drop(requests);

        let first_retry = coordinator
            .release_page_binding(&grant.attachment_id, &page_id)
            .await;
        assert_eq!(first_retry, Err(CompanionSessionError::QueueClosed));
        let second_retry = coordinator
            .release_page_binding(&grant.attachment_id, &page_id)
            .await;
        assert_eq!(second_retry, Err(CompanionSessionError::QueueClosed));
    }

    #[tokio::test]
    async fn full_live_release_queue_bounds_owned_publication_and_notifies_every_joiner() {
        let (coordinator, profile_id, connection_id, grant, mut requests) =
            session_fixture(1).await;
        let page_id = bind_page_for_release(
            &coordinator,
            &profile_id,
            connection_id,
            &grant,
            &mut requests,
            "bounded-release-context",
        )
        .await;
        coordinator
            .send_request(&profile_id, CompanionRequest::Ping)
            .await
            .unwrap();

        let first = tokio::spawn({
            let coordinator = Arc::clone(&coordinator);
            let attachment_id = grant.attachment_id.clone();
            let page_id = page_id.clone();
            async move {
                coordinator
                    .release_page_binding(&attachment_id, &page_id)
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if !coordinator
                    .active_grant(&profile_id)
                    .await
                    .unwrap()
                    .pages
                    .iter()
                    .any(|page| page.page_id == page_id)
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("release must perform its canonical mutation before publication");
        let second = tokio::spawn({
            let coordinator = Arc::clone(&coordinator);
            let attachment_id = grant.attachment_id.clone();
            let page_id = page_id.clone();
            async move {
                coordinator
                    .release_page_binding(&attachment_id, &page_id)
                    .await
            }
        });

        let (first, second) = tokio::time::timeout(Duration::from_secs(2), async {
            (first.await.unwrap(), second.await.unwrap())
        })
        .await
        .expect("an undrained but live outbound queue must not own the grant gate forever");
        assert_eq!(first, Err(CompanionSessionError::QueueClosed));
        assert_eq!(second, first);
        assert!(
            coordinator.grant_updates.try_lock().is_ok(),
            "the terminal publication result must release the global grant gate"
        );
        assert_eq!(
            coordinator
                .binding_releases
                .lock()
                .expect("page-binding release mutex poisoned")
                .len(),
            1,
            "the failed publication remains available for a real retry"
        );
        drop(requests);
    }

    #[tokio::test]
    async fn blocked_release_gate_publishes_a_retryable_terminal_result() {
        let (coordinator, profile_id, connection_id, grant, mut requests) =
            session_fixture(1).await;
        let page_id = bind_page_for_release(
            &coordinator,
            &profile_id,
            connection_id,
            &grant,
            &mut requests,
            "blocked-release-gate-context",
        )
        .await;
        let grant_gate = coordinator.grant_updates.lock().await;
        let first = tokio::spawn({
            let coordinator = Arc::clone(&coordinator);
            let attachment_id = grant.attachment_id.clone();
            let page_id = page_id.clone();
            async move {
                coordinator
                    .release_page_binding(&attachment_id, &page_id)
                    .await
            }
        });
        tokio::task::yield_now().await;
        let second = tokio::spawn({
            let coordinator = Arc::clone(&coordinator);
            let attachment_id = grant.attachment_id.clone();
            let page_id = page_id.clone();
            async move {
                coordinator
                    .release_page_binding(&attachment_id, &page_id)
                    .await
            }
        });

        let (first, second) = tokio::time::timeout(Duration::from_secs(2), async {
            (first.await.unwrap(), second.await.unwrap())
        })
        .await
        .expect("gate acquisition must publish one terminal result to every joiner");
        assert_eq!(first, Err(CompanionSessionError::QueueClosed));
        assert_eq!(second, first);
        assert!(coordinator
            .active_grant(&profile_id)
            .await
            .unwrap()
            .pages
            .iter()
            .any(|page| page.page_id == page_id));

        drop(grant_gate);
        coordinator
            .release_page_binding(&grant.attachment_id, &page_id)
            .await
            .expect("retry must perform the canonical release after the gate becomes available");
        let Message::Text(reduced) = requests.recv().await.unwrap() else {
            panic!("expected the retried reduced grant")
        };
        let CompanionRequest::Grant(reduced): CompanionRequest =
            serde_json::from_str(reduced.as_str()).unwrap()
        else {
            panic!("expected a reduced grant")
        };
        assert!(!reduced.pages.iter().any(|page| page.page_id == page_id));
    }

    #[tokio::test]
    async fn missing_grant_release_does_not_wait_on_the_gate_or_leak_retry_state() {
        let (coordinator, profile_id, connection_id, grant, _requests) = session_fixture(1).await;
        let grant_gate = coordinator.grant_updates.lock().await;
        assert!(coordinator
            .state
            .lock()
            .await
            .grants
            .remove(&grant.attachment_id)
            .is_some());
        let page_id = PageId::new();

        let first = tokio::time::timeout(
            Duration::from_secs(1),
            coordinator.release_page_binding(&grant.attachment_id, &page_id),
        )
        .await
        .expect("an already-missing grant must not wait for the publication gate");
        let retry = tokio::time::timeout(
            Duration::from_secs(1),
            coordinator.release_page_binding(&grant.attachment_id, &page_id),
        )
        .await
        .expect("an idempotent retry must not wait for the publication gate");

        drop(grant_gate);
        tokio::time::timeout(
            Duration::from_secs(1),
            coordinator.unregister(&profile_id, connection_id),
        )
        .await
        .expect("unregister must remain live after an already-missing release");
        assert_eq!(first, Ok(()));
        assert_eq!(retry, Ok(()));
        assert!(
            coordinator
                .binding_releases
                .lock()
                .expect("page-binding release mutex poisoned")
                .is_empty(),
            "an already-missing grant must not leave unassociated retry state"
        );
    }

    #[tokio::test]
    async fn missing_grant_release_cancels_its_pending_binding_before_gate_free_success() {
        let (coordinator, profile_id, connection_id, grant, _requests) = session_fixture(1).await;
        let page_id = PageId::new();
        let ticket = coordinator
            .begin_page_binding(&grant.attachment_id, page_id.clone())
            .await
            .unwrap();
        let replacement = coordinator
            .grant_discovered_targets(&profile_id)
            .await
            .unwrap();
        assert_ne!(replacement.attachment_id, grant.attachment_id);
        assert!(!coordinator
            .state
            .lock()
            .await
            .grants
            .contains_key(&grant.attachment_id));
        let grant_gate = coordinator.grant_updates.lock().await;

        let release = tokio::time::timeout(
            Duration::from_secs(1),
            coordinator.release_page_binding(&grant.attachment_id, &page_id),
        )
        .await
        .expect("an already-missing grant must not wait for the publication gate");
        assert_eq!(release, Ok(()));
        assert!(
            coordinator.state.lock().await.bindings.is_empty(),
            "idempotent release must remove its matching pending binding"
        );
        let ticket_result = tokio::time::timeout(
            Duration::from_millis(100),
            ticket.complete(Duration::from_secs(1)),
        )
        .await
        .expect("the pending ticket must be notified instead of timing out");
        assert_eq!(ticket_result, Err(CompanionSessionError::BindingExpired));
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                if coordinator
                    .binding_releases
                    .lock()
                    .expect("page-binding release mutex poisoned")
                    .is_empty()
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("idempotent release must not leak retry state");

        drop(grant_gate);
        tokio::time::timeout(
            Duration::from_secs(1),
            coordinator.unregister(&profile_id, connection_id),
        )
        .await
        .expect("unregister must remain live after idempotent release cleanup");
    }

    #[tokio::test]
    async fn failed_release_retry_republishes_canonical_grant_before_retirement() {
        let (coordinator, profile_id, connection_id, grant, mut requests) =
            session_fixture(1).await;
        let page_id = bind_page_for_release(
            &coordinator,
            &profile_id,
            connection_id,
            &grant,
            &mut requests,
            "retry-publication-context",
        )
        .await;
        coordinator
            .send_request(&profile_id, CompanionRequest::Ping)
            .await
            .unwrap();

        let failed = tokio::time::timeout(
            Duration::from_secs(2),
            coordinator.release_page_binding(&grant.attachment_id, &page_id),
        )
        .await
        .expect("owned release publication must have a terminal deadline");
        assert_eq!(failed, Err(CompanionSessionError::QueueClosed));
        assert_eq!(
            coordinator
                .binding_releases
                .lock()
                .expect("page-binding release mutex poisoned")
                .len(),
            1
        );

        let Message::Text(ping) = requests.recv().await.unwrap() else {
            panic!("expected the queue-saturating ping")
        };
        assert_eq!(
            serde_json::from_str::<CompanionRequest>(ping.as_str()).unwrap(),
            CompanionRequest::Ping
        );
        coordinator
            .release_page_binding(&grant.attachment_id, &page_id)
            .await
            .expect("retry must republish the unpublished canonical grant");
        let Message::Text(reduced) = requests.recv().await.unwrap() else {
            panic!("expected the retried reduced grant")
        };
        let CompanionRequest::Grant(reduced): CompanionRequest =
            serde_json::from_str(reduced.as_str()).unwrap()
        else {
            panic!("expected a reduced grant")
        };
        assert!(!reduced.pages.iter().any(|page| page.page_id == page_id));
        assert!(
            coordinator
                .binding_releases
                .lock()
                .expect("page-binding release mutex poisoned")
                .is_empty(),
            "successful retry must retire its failed release record"
        );
    }

    #[tokio::test]
    async fn failed_release_records_retire_on_reconnect_and_unregister() {
        let (coordinator, profile_id, connection_id, grant, mut requests) =
            session_fixture(1).await;
        let lease = coordinator
            .registry
            .attach(profile_id.clone())
            .await
            .unwrap();
        let paired = PairedCompanion {
            companion_id: lease.companion_id,
            profile_id: lease.profile_id,
            identity: lease.identity,
            capabilities: lease.capabilities,
        };
        let first_page = bind_page_for_release(
            &coordinator,
            &profile_id,
            connection_id,
            &grant,
            &mut requests,
            "release-before-reconnect",
        )
        .await;
        drop(requests);
        assert_eq!(
            coordinator
                .release_page_binding(&grant.attachment_id, &first_page)
                .await,
            Err(CompanionSessionError::QueueClosed)
        );
        let failed_release = {
            let releases = coordinator
                .binding_releases
                .lock()
                .expect("page-binding release mutex poisoned");
            assert_eq!(releases.len(), 1);
            Arc::clone(releases.values().next().unwrap())
        };

        let (outbound, mut requests) = mpsc::channel(1);
        let grant_gate = coordinator.grant_updates.lock().await;
        let reconnect = tokio::spawn({
            let coordinator = Arc::clone(&coordinator);
            async move { coordinator.register(paired, outbound).await }
        });
        tokio::task::yield_now().await;
        let retry = tokio::spawn({
            let coordinator = Arc::clone(&coordinator);
            let attachment_id = grant.attachment_id.clone();
            let page_id = first_page.clone();
            async move {
                coordinator
                    .release_page_binding(&attachment_id, &page_id)
                    .await
            }
        });
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                let replaced = {
                    let releases = coordinator
                        .binding_releases
                        .lock()
                        .expect("page-binding release mutex poisoned");
                    releases
                        .values()
                        .next()
                        .is_some_and(|release| !Arc::ptr_eq(release, &failed_release))
                };
                if replaced {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("retry must install its joined successor before reconnect proceeds");
        drop(grant_gate);
        let reconnected = reconnect.await.unwrap();
        assert!(retry.await.unwrap().is_err());
        assert!(
            coordinator
                .binding_releases
                .lock()
                .expect("page-binding release mutex poisoned")
                .is_empty(),
            "replacing a connection must retire its failed release records"
        );
        coordinator
            .consume_event(
                &profile_id,
                reconnected,
                CompanionEvent::TargetsDiscovered(TargetDiscovery {
                    protocol_version: PROTOCOL_VERSION,
                    profile_id: profile_id.clone(),
                    targets: vec![BrowserTarget {
                        target_id: "trusted-after-reconnect".into(),
                        kind: TargetKind::Page,
                    }],
                }),
            )
            .await
            .unwrap();
        let reconnected_grant = coordinator
            .grant_discovered_targets(&profile_id)
            .await
            .unwrap();
        let _grant_request = requests.recv().await.unwrap();
        let second_page = bind_page_for_release(
            &coordinator,
            &profile_id,
            reconnected,
            &reconnected_grant,
            &mut requests,
            "release-before-unregister",
        )
        .await;
        drop(requests);
        assert_eq!(
            coordinator
                .release_page_binding(&reconnected_grant.attachment_id, &second_page)
                .await,
            Err(CompanionSessionError::QueueClosed)
        );
        assert_eq!(
            coordinator
                .binding_releases
                .lock()
                .expect("page-binding release mutex poisoned")
                .len(),
            1
        );

        coordinator.unregister(&profile_id, reconnected).await;
        assert!(
            coordinator
                .binding_releases
                .lock()
                .expect("page-binding release mutex poisoned")
                .is_empty(),
            "unregister must retire every failed release record for the connection"
        );
    }

    #[tokio::test]
    async fn pending_page_bindings_are_bounded_and_cancelled_tickets_release_capacity() {
        let (coordinator, _profile_id, _connection_id, grant, _requests) =
            session_fixture(MAX_PENDING_BINDINGS + 2).await;
        let mut tickets = Vec::new();
        for _ in 0..MAX_PENDING_BINDINGS {
            tickets.push(
                coordinator
                    .begin_page_binding(&grant.attachment_id, PageId::new())
                    .await
                    .unwrap(),
            );
        }
        assert!(matches!(
            coordinator
                .begin_page_binding(&grant.attachment_id, PageId::new())
                .await,
            Err(CompanionSessionError::BindingCapacity)
        ));

        drop(tickets.pop());
        tokio::task::yield_now().await;
        coordinator
            .begin_page_binding(&grant.attachment_id, PageId::new())
            .await
            .unwrap();
        assert!(coordinator.state.lock().await.bindings.len() <= MAX_PENDING_BINDINGS);
    }

    #[tokio::test]
    async fn pending_bindings_reject_duplicate_expected_page_ids() {
        let (coordinator, _profile_id, _connection_id, grant, _requests) = session_fixture(4).await;
        let expected_page = PageId::new();
        let _ticket = coordinator
            .begin_page_binding(&grant.attachment_id, expected_page.clone())
            .await
            .unwrap();

        assert!(matches!(
            coordinator
                .begin_page_binding(&grant.attachment_id, expected_page)
                .await,
            Err(CompanionSessionError::InvalidEvent)
        ));
    }

    #[tokio::test]
    async fn newly_discovered_target_can_complete_only_one_pending_binding() {
        let (coordinator, profile_id, connection_id, grant, mut requests) =
            session_fixture(8).await;
        let first = coordinator
            .begin_page_binding(&grant.attachment_id, PageId::new())
            .await
            .unwrap();
        let second = coordinator
            .begin_page_binding(&grant.attachment_id, PageId::new())
            .await
            .unwrap();
        coordinator
            .consume_event(
                &profile_id,
                connection_id,
                CompanionEvent::TargetsDiscovered(TargetDiscovery {
                    protocol_version: PROTOCOL_VERSION,
                    profile_id: profile_id.clone(),
                    targets: vec![
                        BrowserTarget {
                            target_id: "trusted-target".into(),
                            kind: TargetKind::Page,
                        },
                        BrowserTarget {
                            target_id: "new-bidi-context".into(),
                            kind: TargetKind::Page,
                        },
                    ],
                }),
            )
            .await
            .unwrap();

        for nonce in [first.binding_nonce(), second.binding_nonce()] {
            coordinator
                .consume_event(
                    &profile_id,
                    connection_id,
                    CompanionEvent::PageBindingDiscovered(PageBindingDiscovered {
                        protocol_version: PROTOCOL_VERSION,
                        profile_id: profile_id.clone(),
                        target_id: "new-bidi-context".into(),
                        binding_nonce: nonce.to_owned(),
                    }),
                )
                .await
                .unwrap();
        }

        first.complete(Duration::from_secs(1)).await.unwrap();
        assert!(matches!(
            second.complete(Duration::from_millis(25)).await,
            Err(CompanionSessionError::BindingExpired)
        ));
        let _ = requests.recv().await.unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(25), requests.recv())
                .await
                .is_err()
        );
    }
}
