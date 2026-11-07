//! Browser profiles listed in `[vision].assist_profiles` always run with vision
//! assist, so their escalations reach the vision corpus without each caller
//! opting in.

use std::collections::BTreeMap;
use std::sync::Arc;

use config::{AppConfig, NodeConfig, NodeKind};
use node_registry::NodeRegistry;
use sdk_core::RuntimeService;
use session_manager::SessionManager;
use types::{CreateSessionRequest, ExecutionPolicy};

fn vision_nodes() -> Arc<NodeRegistry> {
    let mut nodes = BTreeMap::new();
    nodes.insert(
        "vision".to_owned(),
        NodeConfig {
            kind: NodeKind::Vision,
            endpoint_url: "http://127.0.0.1:9100/vision".to_owned(),
            token_env: None,
            timeout_ms: 15_000,
        },
    );
    Arc::new(NodeRegistry::new(nodes))
}

fn assist_profiles() -> Vec<String> {
    let config = AppConfig::from_toml_str(
        r#"
        [vision]
        assist_profiles = ["corpus-profile"]
        "#,
    )
    .expect("config parses");
    config.vision.assist_profiles
}

fn runtime(nodes: Arc<NodeRegistry>) -> RuntimeService {
    RuntimeService::new(
        SessionManager::default(),
        page_runtime::PageRuntime::default(),
    )
    .with_nodes(nodes)
    .with_vision_assist_profiles(assist_profiles())
}

fn request(profile: &str, execution_policy: ExecutionPolicy) -> CreateSessionRequest {
    CreateSessionRequest {
        profile: profile.into(),
        proxy: None,
        execution_policy,
        zigzagzig: false,
    }
}

#[tokio::test]
async fn a_listed_profile_runs_with_vision_assist_the_caller_did_not_ask_for() {
    let runtime = runtime(vision_nodes());
    let session = runtime
        .create_session(request(
            "corpus-profile",
            ExecutionPolicy {
                javascript_evaluation: true,
                ..ExecutionPolicy::default()
            },
        ))
        .await
        .expect("session is created");
    assert!(session.execution_policy.vision_assist);
    assert!(session.execution_policy.javascript_evaluation);
}

#[tokio::test]
async fn other_profiles_keep_the_policy_the_caller_asked_for() {
    let runtime = runtime(vision_nodes());
    let session = runtime
        .create_session(request("other-profile", ExecutionPolicy::default()))
        .await
        .expect("session is created");
    assert!(!session.execution_policy.vision_assist);
}

#[tokio::test]
async fn a_listed_profile_without_a_vision_provider_still_starts() {
    let runtime = runtime(Arc::new(NodeRegistry::default()));
    let session = runtime
        .create_session(request("corpus-profile", ExecutionPolicy::default()))
        .await
        .expect("session is created");
    assert!(!session.execution_policy.vision_assist);
}
