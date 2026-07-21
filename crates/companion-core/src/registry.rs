use companion_protocol::{BrowserEngine, BrowserIdentity, CompanionCapabilities};
use std::{
    collections::HashMap,
    time::{Duration, Instant},
};
use thiserror::Error;
use tokio::sync::RwLock;
use types::{AttachmentId, CompanionId, ProfileId};
use uuid::Uuid;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairingInput {
    pub pairing_code: String,
    pub companion_id: CompanionId,
    pub profile_id: ProfileId,
    pub identity: BrowserIdentity,
    pub capabilities: CompanionCapabilities,
}

impl PairingInput {
    pub fn firefox(pairing_code: String) -> Self {
        Self {
            pairing_code,
            companion_id: CompanionId::new(),
            profile_id: ProfileId::new(),
            identity: BrowserIdentity {
                engine: BrowserEngine::Firefox,
                browser_name: "Firefox".into(),
                browser_version: "stable".into(),
                os: std::env::consts::OS.into(),
                profile_label: "default-release".into(),
            },
            capabilities: CompanionCapabilities {
                observe: true,
                navigate: true,
                native_input: true,
                tabs: true,
                frames: true,
                native_dialogs: false,
            },
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedCompanion {
    pub companion_id: CompanionId,
    pub profile_id: ProfileId,
    pub identity: BrowserIdentity,
    pub capabilities: CompanionCapabilities,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AttachmentLease {
    pub attachment_id: AttachmentId,
    pub companion_id: CompanionId,
    pub profile_id: ProfileId,
    pub identity: BrowserIdentity,
    pub capabilities: CompanionCapabilities,
    pub expires_at: Instant,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RegistryError {
    #[error("pairing code is invalid or expired")]
    PairingCodeInvalid,
    #[error("paired profile was not found")]
    ProfileNotFound,
    #[error("requested profile does not match companion")]
    ProfileMismatch,
    #[error("companion is revoked")]
    Revoked,
    #[error("attachment lease is expired")]
    AttachmentExpired,
}

#[derive(Debug)]
struct CompanionRecord {
    profile_id: ProfileId,
    identity: BrowserIdentity,
    capabilities: CompanionCapabilities,
    revoked: bool,
}

pub(crate) struct PairingCodeClaim {
    pairing_code: String,
    expires_at: Instant,
}

impl PairingCodeClaim {
    pub(crate) fn pairing_code(&self) -> &str {
        &self.pairing_code
    }

    pub(crate) fn remaining(&self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }
}

#[derive(Debug, Default)]
struct RegistryState {
    pairing_codes: HashMap<String, Instant>,
    companions: HashMap<CompanionId, CompanionRecord>,
    profiles: HashMap<ProfileId, CompanionId>,
    attachments: HashMap<AttachmentId, AttachmentLease>,
}

#[derive(Debug)]
pub struct CompanionRegistry {
    pairing_code_ttl: Duration,
    attachment_ttl: Duration,
    state: RwLock<RegistryState>,
}

impl CompanionRegistry {
    pub fn new(pairing_code_ttl: Duration, attachment_ttl: Duration) -> Self {
        Self {
            pairing_code_ttl,
            attachment_ttl,
            state: RwLock::new(RegistryState::default()),
        }
    }

    pub async fn issue_pairing_code(&self) -> String {
        let code = Uuid::new_v4().to_string();
        self.state
            .write()
            .await
            .pairing_codes
            .insert(code.clone(), Instant::now() + self.pairing_code_ttl);
        code
    }

    pub(crate) async fn claim_pairing_code(
        &self,
        code: &str,
    ) -> Result<PairingCodeClaim, RegistryError> {
        let expires_at = self
            .state
            .write()
            .await
            .pairing_codes
            .remove(code)
            .ok_or(RegistryError::PairingCodeInvalid)?;
        if Instant::now() >= expires_at {
            return Err(RegistryError::PairingCodeInvalid);
        }
        Ok(PairingCodeClaim {
            pairing_code: code.to_owned(),
            expires_at,
        })
    }

    pub async fn pair(&self, input: PairingInput) -> Result<PairedCompanion, RegistryError> {
        let claim = self.claim_pairing_code(&input.pairing_code).await?;
        self.pair_claimed(claim, input).await
    }

    pub(crate) async fn pair_claimed(
        &self,
        claim: PairingCodeClaim,
        input: PairingInput,
    ) -> Result<PairedCompanion, RegistryError> {
        if claim.pairing_code != input.pairing_code || claim.remaining().is_zero() {
            return Err(RegistryError::PairingCodeInvalid);
        }
        let mut state = self.state.write().await;
        if let Some(record) = state.companions.get(&input.companion_id) {
            if record.revoked {
                return Err(RegistryError::Revoked);
            }
            if record.profile_id != input.profile_id {
                return Err(RegistryError::ProfileMismatch);
            }
        }
        if let Some(companion_id) = state.profiles.get(&input.profile_id) {
            if companion_id != &input.companion_id {
                return Err(RegistryError::ProfileMismatch);
            }
        }

        let paired = PairedCompanion {
            companion_id: input.companion_id.clone(),
            profile_id: input.profile_id.clone(),
            identity: input.identity.clone(),
            capabilities: input.capabilities.clone(),
        };
        state
            .profiles
            .insert(input.profile_id.clone(), input.companion_id.clone());
        state.companions.insert(
            input.companion_id,
            CompanionRecord {
                profile_id: input.profile_id,
                identity: input.identity,
                capabilities: input.capabilities,
                revoked: false,
            },
        );
        Ok(paired)
    }

    pub async fn attach(&self, profile_id: ProfileId) -> Result<AttachmentLease, RegistryError> {
        let mut state = self.state.write().await;
        let companion_id = state
            .profiles
            .get(&profile_id)
            .cloned()
            .ok_or(RegistryError::ProfileNotFound)?;
        let record = state
            .companions
            .get(&companion_id)
            .ok_or(RegistryError::ProfileNotFound)?;
        if record.profile_id != profile_id {
            return Err(RegistryError::ProfileMismatch);
        }
        if record.revoked {
            return Err(RegistryError::Revoked);
        }

        let lease = AttachmentLease {
            attachment_id: AttachmentId::new(),
            companion_id,
            profile_id,
            identity: record.identity.clone(),
            capabilities: record.capabilities.clone(),
            expires_at: Instant::now() + self.attachment_ttl,
        };
        state
            .attachments
            .insert(lease.attachment_id.clone(), lease.clone());
        Ok(lease)
    }

    pub async fn revoke(&self, companion_id: &CompanionId) -> Result<(), RegistryError> {
        let mut state = self.state.write().await;
        let record = state
            .companions
            .get_mut(companion_id)
            .ok_or(RegistryError::ProfileNotFound)?;
        record.revoked = true;
        Ok(())
    }

    pub async fn resolve_attachment(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<AttachmentLease, RegistryError> {
        let state = self.state.read().await;
        let lease = state
            .attachments
            .get(attachment_id)
            .ok_or(RegistryError::ProfileNotFound)?;
        let record = state
            .companions
            .get(&lease.companion_id)
            .ok_or(RegistryError::ProfileNotFound)?;
        if record.revoked {
            return Err(RegistryError::Revoked);
        }
        if Instant::now() >= lease.expires_at {
            return Err(RegistryError::AttachmentExpired);
        }
        Ok(lease.clone())
    }
}
