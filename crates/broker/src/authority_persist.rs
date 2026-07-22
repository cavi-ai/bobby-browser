//! Persists issued-token records (hashes only, never bearers) to disk so they survive a
//! broker restart. [`PersistentAuthority`] wraps an [`EnrolledAuthority`], delegating
//! authentication and in-memory enrollment to it while additionally mirroring every
//! non-expired, non-revoked record to a JSON file on disk.

use std::path::PathBuf;

use async_trait::async_trait;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use chrono::{DateTime, Utc};
use interface_core::{Authority, CapabilityHandle, IssuedToken};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::{io::AsyncWriteExt, sync::Mutex};
use types::{
    Capability, CorrelationId, ErrorLayer, InterfaceError, InterfaceErrorCode, PrincipalId,
};

use crate::auth::EnrolledAuthority;

/// One persisted record. Deliberately carries only the token hash, never the bearer
/// itself — the bearer is regenerated on `issue()` and never written to disk.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct PersistedRecord {
    token_hash_hex: String,
    principal_id: PrincipalId,
    capabilities: Vec<Capability>,
    expires_at: DateTime<Utc>,
    revoked: bool,
}

pub struct PersistentAuthority {
    inner: EnrolledAuthority,
    path: PathBuf,
    records: Mutex<Vec<PersistedRecord>>,
}

impl PersistentAuthority {
    /// Loads `path` (an empty record set if it does not exist yet), drops any record that
    /// is already revoked or expired (see `compact`), and re-enrolls the rest into
    /// `inner`'s live [`interface_core::AuthorityStore`] via
    /// [`EnrolledAuthority::enroll_restored`]. The startup credential itself is never part
    /// of this file — it is re-supplied by the caller on every boot.
    ///
    /// If restoring hits the store's capacity (e.g. `max_principals` was lowered since the
    /// file was written, or the +1 startup headroom leaves fewer free slots than the file
    /// has live records), restoration stops early: the remaining records stay in
    /// `self.records` (so a later `revoke()` on them still works and a later `persist()`
    /// still compacts them out once they expire) but are not enrolled into the live store,
    /// so they simply fail closed at `authenticate()` until an operator revokes older
    /// principals or raises `max_principals` and reissues. A skipped token failing closed
    /// is the acceptable outcome here — refusing to boot the runtime over it is not.
    pub async fn open(inner: EnrolledAuthority, path: PathBuf) -> anyhow::Result<Self> {
        let loaded = match tokio::fs::read(&path).await {
            Ok(bytes) => serde_json::from_slice::<Vec<PersistedRecord>>(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(error) => return Err(error.into()),
        };

        let mut records = loaded;
        compact(&mut records, Utc::now());

        for (index, record) in records.iter().enumerate() {
            let hash = decode_hash(&record.token_hash_hex)?;
            match inner
                .enroll_restored(
                    hash,
                    record.principal_id.clone(),
                    record.capabilities.clone(),
                    record.expires_at,
                )
                .await
            {
                Ok(()) => {}
                Err(error) if error.code == InterfaceErrorCode::ResourceExhausted => {
                    let skipped = records.len() - index;
                    eprintln!(
                        "authority restore hit capacity; {skipped} records skipped; issued \
                         tokens beyond capacity will not authenticate — reissue after raising \
                         max_principals"
                    );
                    break;
                }
                Err(error) => {
                    return Err(anyhow::anyhow!(
                        "failed to restore authority record: {error:?}"
                    ))
                }
            }
        }

        Ok(Self {
            inner,
            path,
            records: Mutex::new(records),
        })
    }

    /// Compacts `records` in place (drops revoked/expired entries) and writes the result
    /// to `self.path` atomically: a temp file (`<path>.json.tmp`, created with permissions
    /// `0600` on unix from the start, never widened after) followed by a rename over the
    /// target path.
    ///
    /// Callers must hold `self.records`'s lock across this call (not just across the
    /// mutation that precedes it) — `tokio::sync::Mutex` is safe to hold across an
    /// `.await`, and doing so here is what keeps concurrent `issue`/`revoke` calls
    /// strictly serialized. Cloning a snapshot and persisting after releasing the lock
    /// would let two concurrent writers interleave their disk writes in an order that
    /// does not match the order their mutations actually committed, so a later write from
    /// a call that started (and read) before an earlier-committing call's mutation could
    /// still land on disk after it — silently reverting that mutation (e.g. resurrecting a
    /// revoked principal) once the file is next read back after a restart.
    async fn persist(&self, records: &mut Vec<PersistedRecord>) -> Result<(), InterfaceError> {
        compact(records, Utc::now());
        let json = serde_json::to_vec_pretty(records.as_slice()).map_err(|_| internal_error())?;

        let tmp_path = self.path.with_extension("json.tmp");
        if let Some(parent) = tmp_path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|_| internal_error())?;
        }

        let mut open_options = tokio::fs::OpenOptions::new();
        open_options.write(true).create(true).truncate(true);
        // `tokio::fs::OpenOptions` exposes `.mode()` as an inherent method on unix
        // (mirroring `std::os::unix::fs::OpenOptionsExt`), so no extra trait import is
        // needed here. Setting it up front means the file is 0600 from the moment it is
        // created — there is no window where a broader-permission temp file exists on
        // disk waiting to be tightened by a later `set_permissions` call.
        #[cfg(unix)]
        open_options.mode(0o600);
        let mut file = open_options
            .open(&tmp_path)
            .await
            .map_err(|_| internal_error())?;
        file.write_all(&json).await.map_err(|_| internal_error())?;
        file.flush().await.map_err(|_| internal_error())?;
        drop(file);

        tokio::fs::rename(&tmp_path, &self.path)
            .await
            .map_err(|_| internal_error())?;
        Ok(())
    }
}

/// Drops any record that is already revoked or expired as of `now`. A dropped record can
/// never authenticate again (the live `AuthorityStore` — not this file — is the only
/// runtime source of truth), so keeping it on disk serves no purpose and only lets the
/// file grow without bound across the life of a long-running broker.
fn compact(records: &mut Vec<PersistedRecord>, now: DateTime<Utc>) {
    records.retain(|record| !record.revoked && record.expires_at > now);
}

#[async_trait]
impl Authority for PersistentAuthority {
    async fn authenticate(
        &self,
        bearer: &str,
        now: DateTime<Utc>,
    ) -> Result<CapabilityHandle, InterfaceError> {
        self.inner.authenticate(bearer, now).await
    }

    async fn revoke(&self, principal: &PrincipalId) -> Result<(), InterfaceError> {
        self.inner.revoke(principal).await?;
        let mut records = self.records.lock().await;
        for record in records.iter_mut() {
            if &record.principal_id == principal {
                record.revoked = true;
            }
        }
        // Holds `records` across the persist await — see `persist`'s doc comment.
        self.persist(&mut records).await
    }

    async fn issue(
        &self,
        principal: PrincipalId,
        capabilities: Vec<Capability>,
        expires_at: DateTime<Utc>,
    ) -> Result<IssuedToken, InterfaceError> {
        let mut token_bytes = [0_u8; 32];
        getrandom::fill(&mut token_bytes).map_err(|_| internal_error())?;
        let bearer = URL_SAFE_NO_PAD.encode(token_bytes);
        let hash: [u8; 32] = Sha256::digest(bearer.as_bytes()).into();

        self.inner
            .enroll_restored(hash, principal.clone(), capabilities.clone(), expires_at)
            .await?;

        let mut records = self.records.lock().await;
        records.push(PersistedRecord {
            token_hash_hex: hex::encode(hash),
            principal_id: principal,
            capabilities,
            expires_at,
            revoked: false,
        });
        // Holds `records` across the persist await — see `persist`'s doc comment.
        self.persist(&mut records).await?;

        Ok(IssuedToken::from_bearer(bearer))
    }
}

fn decode_hash(hex_str: &str) -> anyhow::Result<[u8; 32]> {
    let bytes = hex::decode(hex_str)?;
    let array: [u8; 32] = bytes.try_into().map_err(|bytes: Vec<u8>| {
        anyhow::anyhow!("token hash has invalid length {}", bytes.len())
    })?;
    Ok(array)
}

fn internal_error() -> InterfaceError {
    InterfaceError {
        code: InterfaceErrorCode::Internal,
        layer: ErrorLayer::Interface,
        message: "authority persistence failed".to_owned(),
        correlation_id: CorrelationId::new(),
        command_id: None,
        retryable: false,
        retry_after_ms: None,
        reconciliation_required: false,
        required_capability: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::StartupCredential;
    use chrono::Duration;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;
    use uuid::Uuid;

    fn startup() -> StartupCredential {
        StartupCredential::new(
            "persist-fixture-bearer-0123456789abcdef01".to_owned(),
            PrincipalId::from_uuid(Uuid::nil()),
            vec![Capability::AuthorityAdmin, Capability::SessionRead],
            Utc::now() + Duration::minutes(30),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn issued_token_survives_reload() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("authority.json");

        let enrolled = EnrolledAuthority::enroll(startup(), 16).await.unwrap();
        let authority = PersistentAuthority::open(enrolled, path.clone())
            .await
            .unwrap();
        let bearer = authority
            .issue(
                PrincipalId::from_uuid(Uuid::from_u128(101)),
                vec![Capability::SessionRead],
                Utc::now() + Duration::minutes(10),
            )
            .await
            .unwrap()
            .expose_once();
        drop(authority);

        let reloaded_enrolled = EnrolledAuthority::enroll(startup(), 16).await.unwrap();
        let reloaded = PersistentAuthority::open(reloaded_enrolled, path)
            .await
            .unwrap();
        let handle = reloaded.authenticate(&bearer, Utc::now()).await.unwrap();
        assert!(handle.is_valid_at(Utc::now()));
    }

    #[tokio::test]
    async fn revocation_survives_reload() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("authority.json");
        let principal = PrincipalId::from_uuid(Uuid::from_u128(102));

        let enrolled = EnrolledAuthority::enroll(startup(), 16).await.unwrap();
        let authority = PersistentAuthority::open(enrolled, path.clone())
            .await
            .unwrap();
        let bearer = authority
            .issue(
                principal.clone(),
                vec![Capability::SessionRead],
                Utc::now() + Duration::minutes(10),
            )
            .await
            .unwrap()
            .expose_once();
        authority.revoke(&principal).await.unwrap();
        drop(authority);

        let reloaded_enrolled = EnrolledAuthority::enroll(startup(), 16).await.unwrap();
        let reloaded = PersistentAuthority::open(reloaded_enrolled, path)
            .await
            .unwrap();
        assert!(reloaded.authenticate(&bearer, Utc::now()).await.is_err());
    }

    #[tokio::test]
    async fn expired_records_are_not_restored() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("authority.json");
        let principal = PrincipalId::from_uuid(Uuid::from_u128(103));

        let enrolled = EnrolledAuthority::enroll(startup(), 16).await.unwrap();
        let authority = PersistentAuthority::open(enrolled, path.clone())
            .await
            .unwrap();
        let bearer = authority
            .issue(
                principal,
                vec![Capability::SessionRead],
                Utc::now() + Duration::milliseconds(50),
            )
            .await
            .unwrap()
            .expose_once();
        drop(authority);

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        let bytes_before = tokio::fs::read(&path).await.unwrap();

        let reloaded_enrolled = EnrolledAuthority::enroll(startup(), 16).await.unwrap();
        let reloaded = PersistentAuthority::open(reloaded_enrolled, path.clone())
            .await
            .unwrap();
        assert!(reloaded.authenticate(&bearer, Utc::now()).await.is_err());

        // Loading must not rewrite the file merely because a record was skipped as
        // expired — persistence only happens on issue()/revoke().
        let bytes_after = tokio::fs::read(&path).await.unwrap();
        assert_eq!(bytes_before, bytes_after);
    }

    #[tokio::test]
    async fn bearer_never_written_to_disk() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("authority.json");

        let enrolled = EnrolledAuthority::enroll(startup(), 16).await.unwrap();
        let authority = PersistentAuthority::open(enrolled, path.clone())
            .await
            .unwrap();
        let bearer = authority
            .issue(
                PrincipalId::from_uuid(Uuid::from_u128(104)),
                vec![Capability::SessionRead],
                Utc::now() + Duration::minutes(10),
            )
            .await
            .unwrap()
            .expose_once();

        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            !raw.contains(&bearer),
            "persisted authority file must never contain a bearer"
        );
        let hash: [u8; 32] = Sha256::digest(bearer.as_bytes()).into();
        assert!(raw.contains(&hex::encode(hash)), "{raw}");
    }

    #[tokio::test]
    async fn concurrent_issue_and_revoke_do_not_lose_updates() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("authority.json");
        let target_principal = PrincipalId::from_uuid(Uuid::from_u128(201));

        let enrolled = EnrolledAuthority::enroll(startup(), 64).await.unwrap();
        let authority = PersistentAuthority::open(enrolled, path.clone())
            .await
            .unwrap();

        // Issue the principal we will race a revoke against, plus a few interleaved
        // "other" issuances, so the concurrent operations really do race on the shared
        // `records` state rather than touching disjoint principals in isolation.
        let target_bearer = authority
            .issue(
                target_principal.clone(),
                vec![Capability::SessionRead],
                Utc::now() + Duration::minutes(10),
            )
            .await
            .unwrap()
            .expose_once();
        let target_hash: [u8; 32] = Sha256::digest(target_bearer.as_bytes()).into();
        let target_hash_hex = hex::encode(target_hash);

        let revoke_target = target_principal.clone();
        let (revoke_result, issue_one, issue_two, issue_three) = tokio::join!(
            authority.revoke(&revoke_target),
            authority.issue(
                PrincipalId::from_uuid(Uuid::from_u128(202)),
                vec![Capability::SessionRead],
                Utc::now() + Duration::minutes(10),
            ),
            authority.issue(
                PrincipalId::from_uuid(Uuid::from_u128(203)),
                vec![Capability::SessionRead],
                Utc::now() + Duration::minutes(10),
            ),
            authority.issue(
                PrincipalId::from_uuid(Uuid::from_u128(204)),
                vec![Capability::SessionRead],
                Utc::now() + Duration::minutes(10),
            ),
        );
        revoke_result.unwrap();
        let other_bearers = [
            issue_one.unwrap().expose_once(),
            issue_two.unwrap().expose_once(),
            issue_three.unwrap().expose_once(),
        ];

        // In-memory truth: the target principal must be unauthenticatable, the three
        // concurrently issued principals must still authenticate.
        assert!(authority
            .authenticate(&target_bearer, Utc::now())
            .await
            .is_err());
        for bearer in &other_bearers {
            assert!(authority.authenticate(bearer, Utc::now()).await.is_ok());
        }

        drop(authority);

        // Disk truth must match: reload from the file the concurrent calls left behind
        // and confirm the revocation was not clobbered by a slower, stale snapshot from
        // one of the concurrent issue() calls.
        let reloaded_enrolled = EnrolledAuthority::enroll(startup(), 64).await.unwrap();
        let reloaded = PersistentAuthority::open(reloaded_enrolled, path.clone())
            .await
            .unwrap();
        for bearer in &other_bearers {
            assert!(
                reloaded.authenticate(bearer, Utc::now()).await.is_ok(),
                "concurrently issued principals must survive reload"
            );
        }
        assert!(
            reloaded
                .authenticate(&target_bearer, Utc::now())
                .await
                .is_err(),
            "revoked principal must not resurrect after reload"
        );
        // The revoked principal's record must not remain live in the file at all —
        // `compact` guarantees a persisted revoke drops the record outright.
        let raw = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            !raw.contains(&target_hash_hex),
            "revoked principal's hash must not remain in the persisted file: {raw}"
        );
    }

    #[tokio::test]
    async fn capacity_exceeded_restore_skips_remaining_records_without_bricking_boot() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("authority.json");

        // Seed a file with more live records than a small store will have room for once
        // opened, simulating `max_principals` having been lowered (or the file having
        // accumulated more live records than fit) since the file was last written.
        let now = Utc::now();
        let mut seed_records = Vec::new();
        let mut bearers = Vec::new();
        for index in 0..3u128 {
            let bearer = format!("seed-restore-capacity-bearer-{index:0>3}");
            let hash: [u8; 32] = Sha256::digest(bearer.as_bytes()).into();
            bearers.push(bearer);
            seed_records.push(PersistedRecord {
                token_hash_hex: hex::encode(hash),
                principal_id: PrincipalId::from_uuid(Uuid::from_u128(600 + index)),
                capabilities: vec![Capability::SessionRead],
                expires_at: now + Duration::minutes(10),
                revoked: false,
            });
        }
        tokio::fs::write(&path, serde_json::to_vec_pretty(&seed_records).unwrap())
            .await
            .unwrap();

        // `max_principals: 1` means store capacity 2 (1 issued slot + the startup
        // credential's own reserved slot): only 1 of the 3 seeded records can be
        // restored. `open()` must still succeed rather than failing the whole boot.
        let enrolled = EnrolledAuthority::enroll(startup(), 1).await.unwrap();
        let authority = PersistentAuthority::open(enrolled, path.clone())
            .await
            .expect("restoring beyond capacity must not brick boot");

        let mut authenticated = 0;
        for bearer in &bearers {
            if authority.authenticate(bearer, Utc::now()).await.is_ok() {
                authenticated += 1;
            }
        }
        assert_eq!(
            authenticated, 1,
            "only the records that fit in the store's remaining capacity may authenticate"
        );
    }

    #[tokio::test]
    async fn revoke_compacts_the_persisted_file() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("authority.json");
        let principal = PrincipalId::from_uuid(Uuid::from_u128(301));

        let enrolled = EnrolledAuthority::enroll(startup(), 16).await.unwrap();
        let authority = PersistentAuthority::open(enrolled, path.clone())
            .await
            .unwrap();
        let bearer = authority
            .issue(
                principal.clone(),
                vec![Capability::SessionRead],
                Utc::now() + Duration::minutes(10),
            )
            .await
            .unwrap()
            .expose_once();
        let hash: [u8; 32] = Sha256::digest(bearer.as_bytes()).into();
        let hash_hex = hex::encode(hash);

        let raw_before_revoke = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(raw_before_revoke.contains(&hash_hex));

        authority.revoke(&principal).await.unwrap();

        let raw_after_revoke = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            !raw_after_revoke.contains(&hash_hex),
            "revoked record's hash must be compacted out of the persisted file: \
             {raw_after_revoke}"
        );
    }

    #[tokio::test]
    async fn expired_records_vanish_from_the_file_on_next_persist() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("authority.json");
        let short_lived = PrincipalId::from_uuid(Uuid::from_u128(401));
        let long_lived = PrincipalId::from_uuid(Uuid::from_u128(402));

        let enrolled = EnrolledAuthority::enroll(startup(), 16).await.unwrap();
        let authority = PersistentAuthority::open(enrolled, path.clone())
            .await
            .unwrap();
        let short_lived_bearer = authority
            .issue(
                short_lived,
                vec![Capability::SessionRead],
                Utc::now() + Duration::milliseconds(50),
            )
            .await
            .unwrap()
            .expose_once();
        let short_lived_hash: [u8; 32] = Sha256::digest(short_lived_bearer.as_bytes()).into();
        let short_lived_hash_hex = hex::encode(short_lived_hash);

        let raw_before_expiry = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(raw_before_expiry.contains(&short_lived_hash_hex));

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Any subsequent persist() (here, a second issue()) must compact out the record
        // that has since expired, even though nothing acted on it directly.
        authority
            .issue(
                long_lived,
                vec![Capability::SessionRead],
                Utc::now() + Duration::minutes(10),
            )
            .await
            .unwrap();

        let raw_after = tokio::fs::read_to_string(&path).await.unwrap();
        assert!(
            !raw_after.contains(&short_lived_hash_hex),
            "expired record must be compacted out of the file on the next persist: {raw_after}"
        );
    }

    #[tokio::test]
    async fn temp_file_is_created_with_owner_only_permissions() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("authority.json");

        let enrolled = EnrolledAuthority::enroll(startup(), 16).await.unwrap();
        let authority = PersistentAuthority::open(enrolled, path.clone())
            .await
            .unwrap();
        authority
            .issue(
                PrincipalId::from_uuid(Uuid::from_u128(501)),
                vec![Capability::SessionRead],
                Utc::now() + Duration::minutes(10),
            )
            .await
            .unwrap();

        let metadata = tokio::fs::metadata(&path).await.unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(
            mode, 0o600,
            "persisted authority file must be owner-read/write only, got {mode:o}"
        );
    }
}
