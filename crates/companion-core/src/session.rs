use crate::{AttachmentLease, CompanionRegistry, PairedCompanion, RegistryError};
use axum::extract::ws::Message;
use companion_protocol::{
    ActionRequest, AttachmentGrant, BrowserTarget, CompanionEvent, CompanionRequest, GrantedPage,
    TargetDiscovery, PROTOCOL_VERSION,
};
use std::{
    collections::{HashMap, HashSet},
    sync::Arc,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, Mutex, Notify};
use types::{AttachmentId, CommandId, CompanionId, PageId, ProfileId};
use uuid::Uuid;

const MAX_DISCOVERED_TARGETS: usize = 256;
const MAX_TARGET_ID_BYTES: usize = 256;
const MAX_COMMAND_WAIT: Duration = Duration::from_secs(60);

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
    response: oneshot::Sender<Result<CompanionEvent, CompanionSessionError>>,
}

#[derive(Default)]
struct SessionState {
    sessions: HashMap<ProfileId, ActiveSession>,
    discoveries: HashMap<ProfileId, DiscoveryRecord>,
    grants: HashMap<AttachmentId, GrantRecord>,
    pending: HashMap<CommandId, PendingCommand>,
}

pub(crate) struct SessionCoordinator {
    registry: Arc<CompanionRegistry>,
    state: Mutex<SessionState>,
    discovery_changed: Notify,
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
            state: Mutex::new(SessionState::default()),
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
            let mut state = self.state.lock().await;
            if state
                .sessions
                .get(profile_id)
                .is_some_and(|session| session.connection_id == connection_id)
            {
                state.sessions.remove(profile_id);
            }
            Self::remove_connection_state(&mut state, connection_id)
        };
        for response in pending {
            let _ = response.send(Err(CompanionSessionError::ConnectionClosed));
        }
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
                    .map(|pending| pending.response)
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

    async fn complete_command(
        &self,
        connection_id: Uuid,
        command_id: &CommandId,
        event: CompanionEvent,
    ) -> Result<(), CompanionSessionError> {
        let pending = {
            let mut state = self.state.lock().await;
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
        if pending.response.send(Ok(event)).is_err() {
            return Err(CompanionSessionError::InvalidEvent);
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
        send_request(&session.outbound, &CompanionRequest::Grant(grant.clone())).await?;

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
        send_request(&session.outbound, &CompanionRequest::Grant(renewed.clone())).await?;

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
        let (connection_id, outbound) = {
            let mut state = self.state.lock().await;
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
            let (response, receiver) = oneshot::channel();
            state.pending.insert(
                action.command_id.clone(),
                PendingCommand {
                    connection_id: session.connection_id,
                    response,
                },
            );
            (session.connection_id, (session.outbound, receiver))
        };
        let (outbound, receiver) = outbound;
        if let Err(error) = send_request(&outbound, &CompanionRequest::Action(action.clone())).await
        {
            self.remove_pending(&action.command_id, connection_id).await;
            return Err(error);
        }
        let remaining_ms = u64::try_from(action.deadline_unix_ms.saturating_sub(now)).unwrap_or(0);
        let wait = Duration::from_millis(remaining_ms).min(MAX_COMMAND_WAIT);
        match tokio::time::timeout(wait, receiver).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(CompanionSessionError::ConnectionClosed),
            Err(_) => {
                self.remove_pending(&action.command_id, connection_id).await;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PairingInput;
    use companion_protocol::{ActionResult, InteractionPath, TargetKind};

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
}
