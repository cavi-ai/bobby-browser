use std::sync::Arc;

use artifact_store::ArtifactStore;
use chrono::{Duration, Utc};
use interface_core::{ArtifactReader, ArtifactReference, Authority, AuthorityStore};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use types::{Capability, InterfaceErrorCode, PageId, PrincipalId, RequestContext, SessionId};
use uuid::Uuid;

const PAYLOAD: &[u8] = b"authenticated artifact bytes";

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
    let reader = ArtifactReader::new(store.clone(), 4096);
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

    let recovered = ArtifactReader::new(fixture.store.clone(), 4096);
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
    let bounded = ArtifactReader::new(fixture.store.clone(), PAYLOAD.len() - 1);
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
