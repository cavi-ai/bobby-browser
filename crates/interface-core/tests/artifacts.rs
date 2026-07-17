#![cfg(unix)]

use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Barrier, RwLock,
    },
};

use artifact_store::{ArtifactRecord, ArtifactStore};
use chrono::{Duration, Utc};
use interface_core::{
    ArtifactBoundaryTestObserver, ArtifactOwnershipLimits, ArtifactPersistenceTestAction,
    ArtifactReader, ArtifactReference, Authority, AuthorityStore, SessionOwnershipAuthority,
};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use types::{Capability, InterfaceErrorCode, PageId, PrincipalId, RequestContext, SessionId};
use uuid::Uuid;

const PAYLOAD: &[u8] = b"authenticated artifact bytes";

#[derive(Default)]
struct FakeSessionOwnership {
    owners: RwLock<HashMap<SessionId, PrincipalId>>,
}

impl FakeSessionOwnership {
    fn grant(&self, principal: PrincipalId, session: SessionId) {
        self.owners.write().unwrap().insert(session, principal);
    }
}

impl SessionOwnershipAuthority for FakeSessionOwnership {
    fn owns_session(&self, principal: &PrincipalId, session: &SessionId) -> bool {
        self.owners
            .read()
            .unwrap()
            .get(session)
            .is_some_and(|owner| owner == principal)
    }
}

fn limits() -> ArtifactOwnershipLimits {
    ArtifactOwnershipLimits {
        max_records: 32,
        max_bytes: 256 * 1024,
    }
}

fn ownership_record_paths(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let directory = root.join(".interface-artifact-ownership");
    if !directory.exists() {
        return Vec::new();
    }
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| {
            path.extension()
                .is_some_and(|extension| extension == "json")
        })
        .collect()
}

fn ownership_temporary_paths(root: &std::path::Path) -> Vec<std::path::PathBuf> {
    let directory = root.join(".interface-artifact-ownership");
    if !directory.exists() {
        return Vec::new();
    }
    std::fs::read_dir(directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|extension| extension == "tmp"))
        .collect()
}

#[derive(Default)]
struct CountingScanObserver {
    scanned: AtomicUsize,
}

impl ArtifactBoundaryTestObserver for CountingScanObserver {
    fn ownership_record_scanned(&self) {
        self.scanned.fetch_add(1, Ordering::SeqCst);
    }
}

struct BlockingCrashObserver {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
    leaving: Arc<Barrier>,
}

impl ArtifactBoundaryTestObserver for BlockingCrashObserver {
    fn after_ownership_temporary_created(&self) -> ArtifactPersistenceTestAction {
        self.entered.wait();
        self.release.wait();
        self.leaving.wait();
        ArtifactPersistenceTestAction::SimulateCrash
    }
}

struct Fixture {
    _root: tempfile::TempDir,
    reader: ArtifactReader,
    store: ArtifactStore,
    owner_handle: interface_core::CapabilityHandle,
    owner_context: RequestContext,
    other_handle: interface_core::CapabilityHandle,
    other_context: RequestContext,
    session: SessionId,
    other_session: SessionId,
    reference: ArtifactReference,
    record: ArtifactRecord,
    ownership: Arc<FakeSessionOwnership>,
}

async fn identity(
    authority: &AuthorityStore,
    principal: PrincipalId,
) -> (interface_core::CapabilityHandle, RequestContext) {
    let issued = authority
        .issue(
            principal,
            [Capability::ArtifactRead, Capability::ArtifactCapture],
            Utc::now() + Duration::hours(1),
        )
        .await
        .unwrap();
    let handle = authority
        .authenticate(&issued.expose_once(), Utc::now())
        .await
        .unwrap();
    let context = handle.context(Utc::now() + Duration::minutes(5), None);
    (handle, context)
}

async fn fixture_reader() -> Fixture {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(root.path(), 4096, 4096);
    let session = SessionId::new();
    let record = store
        .put(
            &session,
            &PageId::new(),
            "application/octet-stream",
            "bin",
            PAYLOAD,
            4096,
        )
        .await
        .unwrap();
    let authority = AuthorityStore::in_memory();
    let (owner_handle, owner_context) =
        identity(&authority, PrincipalId::from_uuid(Uuid::from_u128(1))).await;
    let (other_handle, other_context) =
        identity(&authority, PrincipalId::from_uuid(Uuid::from_u128(2))).await;
    let ownership = Arc::new(FakeSessionOwnership::default());
    ownership.grant(owner_context.principal_id.clone(), session.clone());
    let reader = ArtifactReader::new(store.clone(), ownership.clone(), 4096, limits()).unwrap();
    let reference = reader
        .register(&owner_handle, &owner_context, &session, &record)
        .await
        .unwrap();
    Fixture {
        _root: root,
        reader,
        store,
        owner_handle,
        owner_context,
        other_handle,
        other_context,
        session,
        other_session: SessionId::new(),
        reference,
        record,
        ownership,
    }
}

fn denial_shape(error: &types::InterfaceError) -> (InterfaceErrorCode, &str, bool, bool) {
    (
        error.code,
        error.message.as_str(),
        error.retryable,
        error.reconciliation_required,
    )
}

#[tokio::test]
async fn artifact_reads_require_original_principal_session_and_hash() {
    let fixture = fixture_reader().await;
    let principal_denial = fixture
        .reader
        .read(
            &fixture.other_handle,
            &fixture.other_context,
            &fixture.session,
            &fixture.reference,
        )
        .await
        .unwrap_err();
    let session_denial = fixture
        .reader
        .read(
            &fixture.owner_handle,
            &fixture.owner_context,
            &fixture.other_session,
            &fixture.reference,
        )
        .await
        .unwrap_err();
    assert_eq!(principal_denial.code, InterfaceErrorCode::ArtifactDenied);
    assert_eq!(
        serde_json::to_value(InterfaceErrorCode::ArtifactDenied).unwrap(),
        json!("artifactDenied")
    );
    assert_eq!(
        denial_shape(&principal_denial),
        denial_shape(&session_denial)
    );

    let mut tampered: Value = serde_json::to_value(&fixture.reference).unwrap();
    tampered["sha256"] = json!("0".repeat(64));
    let tampered: ArtifactReference = serde_json::from_value(tampered).unwrap();
    let hash_denial = fixture
        .reader
        .read(
            &fixture.owner_handle,
            &fixture.owner_context,
            &fixture.session,
            &tampered,
        )
        .await
        .unwrap_err();
    assert_eq!(denial_shape(&principal_denial), denial_shape(&hash_denial));
}

#[tokio::test]
async fn committed_ownership_survives_reader_recovery_without_exposing_paths() {
    let fixture = fixture_reader().await;
    let serialized = serde_json::to_value(&fixture.reference).unwrap();
    assert!(serialized.get("path").is_none());
    assert!(serialized.get("committedPath").is_none());
    let mut with_path = serialized;
    with_path["committedPath"] = json!("/etc/passwd");
    assert!(serde_json::from_value::<ArtifactReference>(with_path).is_err());

    let recovered = ArtifactReader::new(
        fixture.store.clone(),
        fixture.ownership.clone(),
        4096,
        limits(),
    )
    .unwrap();
    let content = recovered
        .read(
            &fixture.owner_handle,
            &fixture.owner_context,
            &fixture.session,
            &fixture.reference,
        )
        .await
        .unwrap();
    assert_eq!(content.media_type, "application/octet-stream");
    assert_eq!(content.bytes, PAYLOAD);
}

#[tokio::test]
async fn recovered_reader_enforces_its_streaming_byte_bound() {
    let fixture = fixture_reader().await;
    let bounded = ArtifactReader::new(
        fixture.store.clone(),
        fixture.ownership.clone(),
        PAYLOAD.len() - 1,
        limits(),
    )
    .unwrap();
    let denial = bounded
        .read(
            &fixture.owner_handle,
            &fixture.owner_context,
            &fixture.session,
            &fixture.reference,
        )
        .await
        .unwrap_err();
    assert_eq!(denial.code, InterfaceErrorCode::ArtifactDenied);
}

#[tokio::test]
async fn registration_uses_committed_cas_media_type_after_content_convergence() {
    let fixture = fixture_reader().await;
    let converged = fixture
        .store
        .put(
            &fixture.session,
            &PageId::new(),
            "text/plain",
            "txt",
            PAYLOAD,
            4096,
        )
        .await
        .unwrap();
    assert_eq!(converged.artifact_id, fixture.reference.artifact_id());

    let reference = fixture
        .reader
        .register(
            &fixture.owner_handle,
            &fixture.owner_context,
            &fixture.session,
            &converged,
        )
        .await
        .unwrap();
    let content = fixture
        .reader
        .read(
            &fixture.owner_handle,
            &fixture.owner_context,
            &fixture.session,
            &reference,
        )
        .await
        .unwrap();
    assert_eq!(content.media_type, "application/octet-stream");
    assert_eq!(content.bytes, PAYLOAD);
}

#[tokio::test]
async fn payload_and_committed_manifest_are_verified_before_bytes_are_returned() {
    let fixture = fixture_reader().await;
    let artifact_id = fixture.reference.artifact_id();
    let artifact_dir = fixture
        ._root
        .path()
        .join(fixture.session.0.to_string())
        .join(artifact_id);
    let payload_path = artifact_dir.join(format!("{artifact_id}.bin"));
    std::fs::write(&payload_path, b"authenticated artifact bytez").unwrap();

    let denial = fixture
        .reader
        .read(
            &fixture.owner_handle,
            &fixture.owner_context,
            &fixture.session,
            &fixture.reference,
        )
        .await
        .unwrap_err();
    assert_eq!(denial.code, InterfaceErrorCode::ArtifactDenied);

    std::fs::write(&payload_path, PAYLOAD).unwrap();
    let manifest_path = artifact_dir.join(format!("{artifact_id}.json"));
    let mut manifest: Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["mediaType"] = json!("text/plain");
    std::fs::write(&manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    let denial = fixture
        .reader
        .read(
            &fixture.owner_handle,
            &fixture.owner_context,
            &fixture.session,
            &fixture.reference,
        )
        .await
        .unwrap_err();
    assert_eq!(denial.code, InterfaceErrorCode::ArtifactDenied);
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_artifact_components_are_never_followed() {
    use std::os::unix::fs::symlink;

    let fixture = fixture_reader().await;
    let artifact_id = fixture.reference.artifact_id();
    let session_dir = fixture._root.path().join(fixture.session.0.to_string());
    let artifact_dir = session_dir.join(artifact_id);
    let moved = session_dir.join("moved-artifact");
    std::fs::rename(&artifact_dir, &moved).unwrap();
    symlink(&moved, &artifact_dir).unwrap();

    let denial = fixture
        .reader
        .read(
            &fixture.owner_handle,
            &fixture.owner_context,
            &fixture.session,
            &fixture.reference,
        )
        .await
        .unwrap_err();
    assert_eq!(denial.code, InterfaceErrorCode::ArtifactDenied);
}

#[cfg(unix)]
#[tokio::test]
async fn concurrent_path_replacement_never_returns_uncommitted_bytes() {
    let fixture = Arc::new(fixture_reader().await);
    let artifact_id = fixture.reference.artifact_id().to_owned();
    let artifact_dir = fixture
        ._root
        .path()
        .join(fixture.session.0.to_string())
        .join(&artifact_id);
    let payload_path = artifact_dir.join(format!("{artifact_id}.bin"));
    let replacement_path = artifact_dir.join("replacement.bin");
    let attacker = vec![b'X'; PAYLOAD.len()];
    std::fs::write(&replacement_path, &attacker).unwrap();

    let replacer = std::thread::spawn({
        let payload_path = payload_path.clone();
        let replacement_path = replacement_path.clone();
        move || {
            for _ in 0..128 {
                let _ = std::fs::rename(&replacement_path, &payload_path);
                let _ = std::fs::write(&replacement_path, &attacker);
            }
        }
    });
    for _ in 0..128 {
        match fixture
            .reader
            .read(
                &fixture.owner_handle,
                &fixture.owner_context,
                &fixture.session,
                &fixture.reference,
            )
            .await
        {
            Ok(content) => {
                assert_eq!(content.bytes, PAYLOAD);
                assert_eq!(
                    Sha256::digest(&content.bytes)[..],
                    Sha256::digest(PAYLOAD)[..]
                );
            }
            Err(error) => assert_eq!(error.code, InterfaceErrorCode::ArtifactDenied),
        }
    }
    replacer.join().unwrap();
}

#[tokio::test]
async fn attacker_cannot_bind_known_session_and_record_to_their_principal() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(root.path(), 4096, 4096);
    let session = SessionId::new();
    let record = store
        .put(
            &session,
            &PageId::new(),
            "application/octet-stream",
            "bin",
            PAYLOAD,
            4096,
        )
        .await
        .unwrap();
    let authority = AuthorityStore::in_memory();
    let (owner_handle, owner_context) =
        identity(&authority, PrincipalId::from_uuid(Uuid::from_u128(10))).await;
    let (attacker_handle, attacker_context) =
        identity(&authority, PrincipalId::from_uuid(Uuid::from_u128(11))).await;
    let ownership = Arc::new(FakeSessionOwnership::default());
    ownership.grant(owner_context.principal_id.clone(), session.clone());
    let reader = ArtifactReader::new(store, ownership, 4096, limits()).unwrap();

    let denial = reader
        .register(&attacker_handle, &attacker_context, &session, &record)
        .await
        .unwrap_err();
    assert_eq!(denial.code, InterfaceErrorCode::ArtifactDenied);
    let wrong_session = reader
        .register(&owner_handle, &owner_context, &SessionId::new(), &record)
        .await
        .unwrap_err();
    assert_eq!(denial_shape(&denial), denial_shape(&wrong_session));
    assert!(ownership_record_paths(root.path()).is_empty());
}

#[tokio::test]
async fn repeated_and_concurrent_registration_converges_on_one_reference_and_record() {
    let fixture = Arc::new(fixture_reader().await);
    let recovered_reader = ArtifactReader::new(
        fixture.store.clone(),
        fixture.ownership.clone(),
        4096,
        limits(),
    )
    .unwrap();
    let repeated = fixture
        .reader
        .register(
            &fixture.owner_handle,
            &fixture.owner_context,
            &fixture.session,
            &fixture.record,
        )
        .await
        .unwrap();
    assert_eq!(repeated, fixture.reference);

    let mut tasks = Vec::new();
    for index in 0..16 {
        let fixture = fixture.clone();
        let reader = if index % 2 == 0 {
            fixture.reader.clone()
        } else {
            recovered_reader.clone()
        };
        tasks.push(tokio::spawn(async move {
            reader
                .register(
                    &fixture.owner_handle,
                    &fixture.owner_context,
                    &fixture.session,
                    &fixture.record,
                )
                .await
        }));
    }
    for task in tasks {
        assert_eq!(task.await.unwrap().unwrap(), fixture.reference);
    }
    assert_eq!(ownership_record_paths(fixture._root.path()).len(), 1);
}

#[test]
fn reader_initialization_creates_a_missing_trusted_artifact_root() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("not-created-yet");
    let store = ArtifactStore::new(&root, 4096, 4096);
    let ownership = Arc::new(FakeSessionOwnership::default());

    ArtifactReader::new(store, ownership, 4096, limits()).unwrap();
    assert!(root.join(".interface-artifact-ownership").is_dir());
}

#[tokio::test]
async fn restart_scans_existing_count_and_refuses_new_record_at_global_quota() {
    let fixture = fixture_reader().await;
    let second = fixture
        .store
        .put(
            &fixture.session,
            &PageId::new(),
            "application/octet-stream",
            "bin",
            b"second artifact",
            4096,
        )
        .await
        .unwrap();
    let quota = ArtifactOwnershipLimits {
        max_records: 1,
        max_bytes: 256 * 1024,
    };
    let recovered = ArtifactReader::new(
        fixture.store.clone(),
        fixture.ownership.clone(),
        4096,
        quota,
    )
    .unwrap();

    assert_eq!(
        recovered
            .register(
                &fixture.owner_handle,
                &fixture.owner_context,
                &fixture.session,
                &fixture.record,
            )
            .await
            .unwrap(),
        fixture.reference
    );
    let denial = recovered
        .register(
            &fixture.owner_handle,
            &fixture.owner_context,
            &fixture.session,
            &second,
        )
        .await
        .unwrap_err();
    assert_eq!(denial.code, InterfaceErrorCode::ArtifactDenied);
    assert_eq!(ownership_record_paths(fixture._root.path()).len(), 1);
}

#[tokio::test]
async fn reduced_restart_quota_stops_scanning_at_the_first_over_limit_record() {
    let fixture = fixture_reader().await;
    for index in 0..8 {
        let bytes = format!("quota-record-{index}");
        let record = fixture
            .store
            .put(
                &fixture.session,
                &PageId::new(),
                "application/octet-stream",
                "bin",
                bytes.as_bytes(),
                4096,
            )
            .await
            .unwrap();
        fixture
            .reader
            .register(
                &fixture.owner_handle,
                &fixture.owner_context,
                &fixture.session,
                &record,
            )
            .await
            .unwrap();
    }
    let observer = Arc::new(CountingScanObserver::default());

    let result = ArtifactReader::new_with_test_observer(
        fixture.store.clone(),
        fixture.ownership.clone(),
        4096,
        ArtifactOwnershipLimits {
            max_records: 3,
            max_bytes: 256 * 1024,
        },
        observer.clone(),
    );

    assert!(result.is_err(), "reduced configuration must fail closed");
    assert_eq!(observer.scanned.load(Ordering::SeqCst), 4);
}

#[tokio::test]
async fn restart_counts_exact_existing_bytes_against_aggregate_quota() {
    let fixture = fixture_reader().await;
    let existing_bytes = std::fs::metadata(&ownership_record_paths(fixture._root.path())[0])
        .unwrap()
        .len();
    let second = fixture
        .store
        .put(
            &fixture.session,
            &PageId::new(),
            "application/octet-stream",
            "bin",
            b"aggregate byte quota",
            4096,
        )
        .await
        .unwrap();
    let recovered = ArtifactReader::new(
        fixture.store.clone(),
        fixture.ownership.clone(),
        4096,
        ArtifactOwnershipLimits {
            max_records: 32,
            max_bytes: existing_bytes,
        },
    )
    .unwrap();

    let denial = recovered
        .register(
            &fixture.owner_handle,
            &fixture.owner_context,
            &fixture.session,
            &second,
        )
        .await
        .unwrap_err();
    assert_eq!(denial.code, InterfaceErrorCode::ArtifactDenied);
    assert_eq!(ownership_record_paths(fixture._root.path()).len(), 1);
}

#[tokio::test]
async fn restart_rejects_a_reduced_aggregate_byte_limit_during_initialization() {
    let fixture = fixture_reader().await;
    let existing_bytes = std::fs::metadata(&ownership_record_paths(fixture._root.path())[0])
        .unwrap()
        .len();

    assert!(ArtifactReader::new(
        fixture.store.clone(),
        fixture.ownership.clone(),
        4096,
        ArtifactOwnershipLimits {
            max_records: 32,
            max_bytes: existing_bytes - 1,
        },
    )
    .is_err());
}

#[tokio::test]
async fn cancelled_registration_cannot_leave_an_unreachable_ownership_record() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::new(root.path(), 4096, 4096);
    let session = SessionId::new();
    let record = store
        .put(
            &session,
            &PageId::new(),
            "application/octet-stream",
            "bin",
            PAYLOAD,
            4096,
        )
        .await
        .unwrap();
    let authority = AuthorityStore::in_memory();
    let (handle, context) = identity(&authority, PrincipalId::from_uuid(Uuid::from_u128(12))).await;
    let ownership = Arc::new(FakeSessionOwnership::default());
    ownership.grant(context.principal_id.clone(), session.clone());
    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let leaving = Arc::new(Barrier::new(2));
    let observer = Arc::new(BlockingCrashObserver {
        entered: entered.clone(),
        release: release.clone(),
        leaving: leaving.clone(),
    });
    let reader = ArtifactReader::new_with_test_observer(
        store.clone(),
        ownership.clone(),
        4096,
        limits(),
        observer,
    )
    .unwrap();
    let task = tokio::spawn({
        let reader = reader.clone();
        let handle = handle.clone();
        let context = context.clone();
        let session = session.clone();
        let record = record.clone();
        async move { reader.register(&handle, &context, &session, &record).await }
    });
    tokio::task::spawn_blocking(move || entered.wait())
        .await
        .unwrap();
    task.abort();
    assert!(task.await.unwrap_err().is_cancelled());
    tokio::task::spawn_blocking(move || release.wait())
        .await
        .unwrap();
    tokio::task::spawn_blocking(move || leaving.wait())
        .await
        .unwrap();

    let restarted = ArtifactReader::new(store, ownership, 4096, limits()).unwrap();
    assert!(ownership_temporary_paths(root.path()).is_empty());

    let recovered = restarted
        .register(&handle, &context, &session, &record)
        .await
        .unwrap();
    let again = restarted
        .register(&handle, &context, &session, &record)
        .await
        .unwrap();
    assert_eq!(recovered, again);
    assert_eq!(ownership_record_paths(root.path()).len(), 1);
}
