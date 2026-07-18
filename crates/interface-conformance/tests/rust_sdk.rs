use interface_conformance::{
    expected_proof, run_canonical_scenario, RustSdkScenarioDriver, CANONICAL_STEPS,
    NEGATIVE_CAPABILITY_MATRIX,
};
use interface_core::{AuthorityStore, RuntimeInterface};
use sdk_core::{AuthenticatedRuntime, RuntimeService};
use std::sync::Arc;
use types::{Capability, CreateSessionRequest, InterfaceErrorCode, PrincipalId};

struct RecordingRustSdk {
    observed: Vec<String>,
}

#[tokio::test]
async fn rust_sdk_observes_live_authenticated_runtime_allow_and_deny_decisions() {
    let authority = AuthorityStore::in_memory();
    let allowed = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [Capability::SessionRead],
            chrono::Utc::now() + chrono::Duration::minutes(5),
        )
        .await
        .unwrap();
    let denied = authority
        .issue(
            PrincipalId::from_uuid(uuid::Uuid::new_v4()),
            [],
            chrono::Utc::now() + chrono::Duration::minutes(5),
        )
        .await
        .unwrap();
    let service = RuntimeService::default();
    let allowed_handle = authority.verify(&allowed.expose_once()).await.unwrap();
    let denied_handle = authority.verify(&denied.expose_once()).await.unwrap();
    let allowed_runtime = Arc::new(AuthenticatedRuntime::new(
        service.clone(),
        allowed_handle.clone(),
    ));
    let denied_runtime = Arc::new(AuthenticatedRuntime::new(service, denied_handle.clone()));
    let info = allowed_runtime
        .runtime_info(
            allowed_handle.context(chrono::Utc::now() + chrono::Duration::seconds(30), None),
        )
        .await
        .unwrap();
    assert!(!info.version.is_empty());
    let error = denied_runtime
        .create_session(
            denied_handle.context(chrono::Utc::now() + chrono::Duration::seconds(30), None),
            CreateSessionRequest {
                profile: "denied".into(),
                proxy: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(error.code, InterfaceErrorCode::MissingCapability);
}
impl RustSdkScenarioDriver for RecordingRustSdk {
    fn execute(&mut self, steps: &[&str]) -> interface_conformance::CanonicalProof {
        self.observed = steps.iter().map(|step| (*step).to_owned()).collect();
        expected_proof()
    }
}

#[test]
fn rust_sdk_consumes_the_same_canonical_scenario() {
    let mut driver = RecordingRustSdk {
        observed: Vec::new(),
    };
    assert_eq!(
        run_canonical_scenario(&mut driver).unwrap(),
        expected_proof()
    );
    assert_eq!(driver.observed, CANONICAL_STEPS);
}

#[test]
fn rust_sdk_negative_capability_matrix_covers_every_step() {
    assert_eq!(NEGATIVE_CAPABILITY_MATRIX.len(), CANONICAL_STEPS.len());
    for step in CANONICAL_STEPS {
        assert!(NEGATIVE_CAPABILITY_MATRIX
            .iter()
            .any(|(candidate, _)| *candidate == step));
    }
}
