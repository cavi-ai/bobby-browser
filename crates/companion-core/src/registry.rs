use companion_protocol::{BrowserEngine, BrowserIdentity, CompanionCapabilities};
use sha2::{Digest, Sha256};
use std::{
    collections::HashMap,
    fmt,
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
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
                native_input: false,
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

#[derive(Clone, PartialEq, Eq)]
pub struct CompanionCredential(String);

impl CompanionCredential {
    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for CompanionCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("CompanionCredential([redacted])")
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PairedSession {
    pub companion: PairedCompanion,
    pub credential: CompanionCredential,
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

#[derive(Debug, Clone, Error, PartialEq, Eq)]
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
    #[error("companion credential is invalid")]
    CredentialInvalid,
}

#[derive(Debug)]
struct CompanionRecord {
    profile_id: ProfileId,
    identity: BrowserIdentity,
    capabilities: CompanionCapabilities,
    // Only the digest is stored: a registry memory read must not expose
    // every live reconnect credential.
    credential_sha256: [u8; 32],
    revoked: bool,
}

pub(crate) struct PairingCodeClaim {
    pairing_code: String,
    expires_at: Instant,
}

pub(crate) enum ConnectionAuthentication {
    Pairing(PairingCodeClaim),
    Reconnect(PairedCompanion),
}

impl PairingCodeClaim {
    pub(crate) fn pairing_code(&self) -> &str {
        &self.pairing_code
    }

    pub(crate) fn remaining(&self) -> Duration {
        self.expires_at.saturating_duration_since(Instant::now())
    }
}

fn credential_sha256(credential: &str) -> [u8; 32] {
    Sha256::digest(credential.as_bytes()).into()
}

#[derive(Default)]
struct RegistryState {
    pairing_codes: HashMap<String, Instant>,
    companions: HashMap<CompanionId, CompanionRecord>,
    profiles: HashMap<ProfileId, CompanionId>,
    credentials: HashMap<[u8; 32], CompanionId>,
    attachments: HashMap<AttachmentId, AttachmentLease>,
}

impl fmt::Debug for RegistryState {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RegistryState")
            .field("pairing_code_count", &self.pairing_codes.len())
            .field("companion_count", &self.companions.len())
            .field("profile_count", &self.profiles.len())
            .field("credential_count", &self.credentials.len())
            .field("attachment_count", &self.attachments.len())
            .finish()
    }
}

#[derive(Debug)]
pub struct CompanionRegistry {
    pairing_code_ttl: Duration,
    attachment_ttl: Duration,
    state: RwLock<RegistryState>,
    revocations: tokio::sync::watch::Sender<CompanionId>,
}

impl CompanionRegistry {
    pub fn new(pairing_code_ttl: Duration, attachment_ttl: Duration) -> Self {
        let (revocations, _) = tokio::sync::watch::channel(CompanionId::new());
        Self {
            pairing_code_ttl,
            attachment_ttl,
            state: RwLock::new(RegistryState::default()),
            revocations,
        }
    }

    /// Live-companion teardown signal: a revoked companion's sockets close
    /// instead of serving reads until disconnect.
    pub fn subscribe_revocations(&self) -> tokio::sync::watch::Receiver<CompanionId> {
        self.revocations.subscribe()
    }

    pub async fn issue_pairing_code(&self) -> String {
        let code = Uuid::new_v4().to_string();
        let mut state = self.state.write().await;
        // Amortized pruning: unclaimed codes and expired attachment leases
        // are dead weight; dropping them here keeps both maps bounded for
        // the life of the process.
        let now = Instant::now();
        state
            .pairing_codes
            .retain(|_, expires_at| *expires_at > now);
        state.attachments.retain(|_, lease| lease.expires_at > now);
        state
            .pairing_codes
            .insert(code.clone(), now + self.pairing_code_ttl);
        code
    }

    pub async fn paired_profile_ids(&self) -> Vec<ProfileId> {
        let mut profiles = self
            .state
            .read()
            .await
            .profiles
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        profiles.sort_by_key(|profile| profile.0);
        profiles
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

    pub(crate) async fn authenticate_bearer(
        &self,
        bearer: &str,
    ) -> Result<ConnectionAuthentication, RegistryError> {
        let mut state = self.state.write().await;
        if let Some(expires_at) = state.pairing_codes.remove(bearer) {
            if Instant::now() >= expires_at {
                return Err(RegistryError::PairingCodeInvalid);
            }
            return Ok(ConnectionAuthentication::Pairing(PairingCodeClaim {
                pairing_code: bearer.to_owned(),
                expires_at,
            }));
        }
        let bearer_sha256 = credential_sha256(bearer);
        let companion_id = state
            .credentials
            .get(&bearer_sha256)
            .ok_or(RegistryError::CredentialInvalid)?;
        let record = state
            .companions
            .get(companion_id)
            .ok_or(RegistryError::CredentialInvalid)?;
        if record.revoked {
            return Err(RegistryError::Revoked);
        }
        if record.credential_sha256.ct_eq(&bearer_sha256).unwrap_u8() != 1 {
            return Err(RegistryError::CredentialInvalid);
        }
        Ok(ConnectionAuthentication::Reconnect(PairedCompanion {
            companion_id: companion_id.clone(),
            profile_id: record.profile_id.clone(),
            identity: record.identity.clone(),
            capabilities: record.capabilities.clone(),
        }))
    }

    pub async fn pair(&self, input: PairingInput) -> Result<PairedCompanion, RegistryError> {
        Ok(self.pair_with_credential(input).await?.companion)
    }

    pub async fn pair_with_credential(
        &self,
        input: PairingInput,
    ) -> Result<PairedSession, RegistryError> {
        let claim = self.claim_pairing_code(&input.pairing_code).await?;
        self.pair_claimed(claim, input).await
    }

    pub(crate) async fn pair_claimed(
        &self,
        claim: PairingCodeClaim,
        input: PairingInput,
    ) -> Result<PairedSession, RegistryError> {
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
        if let Some(previous) = state
            .companions
            .get(&input.companion_id)
            .map(|record| record.credential_sha256)
        {
            state.credentials.remove(&previous);
        }
        let credential = CompanionCredential(Uuid::new_v4().to_string());
        let credential_sha256 = credential_sha256(credential.expose_secret());
        state
            .credentials
            .insert(credential_sha256, input.companion_id.clone());
        state.companions.insert(
            input.companion_id,
            CompanionRecord {
                profile_id: input.profile_id,
                identity: input.identity,
                capabilities: input.capabilities,
                credential_sha256,
                revoked: false,
            },
        );
        Ok(PairedSession {
            companion: paired,
            credential,
        })
    }

    pub async fn authenticate_credential(
        &self,
        credential: &str,
    ) -> Result<PairedCompanion, RegistryError> {
        let state = self.state.read().await;
        let credential_sha256 = credential_sha256(credential);
        let companion_id = state
            .credentials
            .get(&credential_sha256)
            .ok_or(RegistryError::CredentialInvalid)?;
        let record = state
            .companions
            .get(companion_id)
            .ok_or(RegistryError::CredentialInvalid)?;
        if record.revoked {
            return Err(RegistryError::Revoked);
        }
        if record
            .credential_sha256
            .ct_eq(&credential_sha256)
            .unwrap_u8()
            != 1
        {
            return Err(RegistryError::CredentialInvalid);
        }
        Ok(PairedCompanion {
            companion_id: companion_id.clone(),
            profile_id: record.profile_id.clone(),
            identity: record.identity.clone(),
            capabilities: record.capabilities.clone(),
        })
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
            .retain(|_, lease| lease.expires_at > Instant::now());
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
        let _ = self.revocations.send(companion_id.clone());
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

    pub async fn renew_attachment(
        &self,
        attachment_id: &AttachmentId,
    ) -> Result<AttachmentLease, RegistryError> {
        let mut state = self.state.write().await;
        let lease = state
            .attachments
            .get(attachment_id)
            .cloned()
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
        let renewed = AttachmentLease {
            expires_at: Instant::now() + self.attachment_ttl,
            ..lease
        };
        state
            .attachments
            .insert(renewed.attachment_id.clone(), renewed.clone());
        Ok(renewed)
    }
}
