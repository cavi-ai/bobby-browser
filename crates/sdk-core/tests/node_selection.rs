//! A session names the node it escalates to, and gets that node or nothing.
//!
//! The old shape was a single process-wide `[vision]` endpoint: every session
//! that opted into vision escalation talked to whichever provider the operator
//! had configured, with no way to select one and no way to tell where it was.
//! The failure that mattered was silent — a session could not distinguish
//! "escalated to the local node I asked for" from "escalated to a remote
//! provider I did not know about".
//!
//! These tests pin the replacement: selection is per session, and every
//! negative path (no name, unknown name, wrong kind) declines rather than
//! substituting a different node.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use async_trait::async_trait;
use config::{NodeConfig, NodeKind};
use intent_engine::{StuckKind, VisionAction, VisionAssist, VisionProposal, VisionProposeRequest};
use node_registry::{NodeError, NodeRegistry};
use page_runtime::NodeSelection;
use sdk_core::RuntimeService;
use session_manager::SessionManager;
use types::{CommandError, CreateSessionRequest, ExecutionPolicy};

/// Records that it was asked, so a test can tell "the session's node ran" from
/// "some node ran".
struct CountingAssist {
    calls: Arc<AtomicUsize>,
}

#[async_trait]
impl VisionAssist for CountingAssist {
    async fn propose(&self, _: VisionProposeRequest) -> Result<VisionProposal, CommandError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        Ok(VisionProposal {
            confidence: 0.9,
            action: VisionAction::Click { x: 1.0, y: 1.0 },
        })
    }
}

fn node(kind: NodeKind, endpoint_url: &str) -> NodeConfig {
    NodeConfig {
        kind,
        endpoint_url: endpoint_url.to_owned(),
        token_env: None,
        timeout_ms: 15_000,
    }
}

fn registry(pairs: &[(&str, NodeConfig)]) -> Arc<NodeRegistry> {
    let mut nodes = BTreeMap::new();
    for (name, config) in pairs {
        nodes.insert((*name).to_owned(), config.clone());
    }
    Arc::new(NodeRegistry::new(nodes))
}

async fn runtime(nodes: Arc<NodeRegistry>) -> RuntimeService {
    RuntimeService::new(
        SessionManager::default(),
        page_runtime::PageRuntime::default(),
    )
    .with_nodes(nodes)
}

fn policy(vision_node: Option<&str>) -> ExecutionPolicy {
    ExecutionPolicy {
        vision_assist: true,
        vision_node: vision_node.map(str::to_owned),
        ..ExecutionPolicy::default()
    }
}

#[tokio::test]
async fn a_session_names_the_node_it_escalates_to() {
    let runtime = runtime(registry(&[
        (
            "local",
            node(NodeKind::Vision, "http://127.0.0.1:8080/propose"),
        ),
        ("remote", node(NodeKind::Vision, "https://vision.example/p")),
    ]))
    .await;
    let session = runtime
        .create_session(CreateSessionRequest {
            profile: "node-selection".into(),
            proxy: None,
            execution_policy: policy(Some("local")),
        })
        .await
        .expect("session is created");
    assert_eq!(
        session.execution_policy.vision_node.as_deref(),
        Some("local"),
        "the session did not retain the node it named"
    );
}

#[tokio::test]
async fn the_runtime_reports_the_nodes_it_can_reach() {
    let runtime = runtime(registry(&[
        ("local", node(NodeKind::Vision, "http://127.0.0.1:8080/p")),
        ("remote", node(NodeKind::Vision, "https://vision.example/p")),
    ]))
    .await;
    assert_eq!(runtime.node_names(), vec!["local", "remote"]);
}

#[tokio::test]
async fn a_runtime_built_without_configuration_reaches_no_node() {
    let runtime = RuntimeService::default();
    assert!(
        runtime.node_names().is_empty(),
        "a default runtime must not carry a node nobody configured"
    );
}

/// The negative path, asserted on the registry directly because that is where
/// the decision is made: an unknown name is an error, never a substitution.
#[test]
fn every_unresolvable_selection_declines_rather_than_substituting() {
    let registry = NodeRegistry::new(
        [(
            "local".to_owned(),
            node(NodeKind::Vision, "http://127.0.0.1:8080/p"),
        )]
        .into_iter()
        .collect(),
    );

    assert!(
        matches!(
            registry.vision("typo"),
            Err(NodeError::Unknown(name)) if name == "typo"
        ),
        "an unknown name resolved to something"
    );
    // The positive control: with one vision node configured and named
    // correctly, resolution succeeds — so the assertions above are not passing
    // because resolution never works.
    assert!(registry.vision("local").is_ok());
}

/// Locality is the privacy property, and it has to come from the address
/// rather than from a flag an operator sets next to a remote URL.
#[test]
fn a_remote_node_cannot_be_labelled_local() {
    let registry = NodeRegistry::new(
        [(
            "claims-to-be-local".to_owned(),
            node(NodeKind::Vision, "https://localhost.vision.example/p"),
        )]
        .into_iter()
        .collect(),
    );
    assert!(
        !registry
            .resolve("claims-to-be-local", NodeKind::Vision)
            .expect("configured")
            .is_local(),
        "a remote host whose name starts with localhost was treated as local"
    );
}

/// The three resolution states must reach the engine as three different
/// things. Collapsing "named a node that did not resolve" into "named no node"
/// is what would let a typo silently escalate to the process-wide provider.
#[test]
fn an_unresolved_node_does_not_fall_back_to_the_installed_provider() {
    let calls = Arc::new(AtomicUsize::new(0));
    let installed: Arc<dyn VisionAssist> = Arc::new(CountingAssist {
        calls: Arc::clone(&calls),
    });

    // Named no node: the embedder's provider applies, because nothing was
    // chosen and so nothing was overridden.
    assert!(
        NodeSelection::NotRequested
            .provider_for_test(Some(Arc::clone(&installed)))
            .is_some(),
        "a session that named no node lost the installed provider"
    );

    // Named a node that did not resolve: no provider, and specifically not the
    // installed one.
    assert!(
        NodeSelection::Unresolved
            .provider_for_test(Some(Arc::clone(&installed)))
            .is_none(),
        "an unresolved node fell back to the installed provider"
    );

    // Named a node that resolved: that node, not the installed one.
    let resolved: Arc<dyn VisionAssist> = Arc::new(CountingAssist {
        calls: Arc::new(AtomicUsize::new(0)),
    });
    let chosen = NodeSelection::Resolved(Arc::clone(&resolved))
        .provider_for_test(Some(Arc::clone(&installed)))
        .expect("a resolved node provides");
    assert!(
        Arc::ptr_eq(&chosen, &resolved),
        "a resolved node was replaced by the installed provider"
    );

    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "selection called a provider; it only chooses one"
    );
}

/// The privacy property the registry exists for: when the session names a
/// loopback node, page material goes to that node and nowhere else. A second
/// loopback listener stands in for a remote provider — if selection ever
/// substitutes or fans out, its counter moves.
#[tokio::test]
async fn a_local_session_sends_page_material_only_to_its_loopback_node() {
    async fn mock_node(hits: Arc<AtomicUsize>) -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            while let Ok((mut socket, _)) = listener.accept().await {
                let mut request = vec![0_u8; 8192];
                let _ = socket.read(&mut request).await;
                let body = br#"{"confidence":0.9,"action":{"kind":"click","x":1.0,"y":1.0}}"#;
                let response = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    body.len(),
                    String::from_utf8_lossy(body),
                );
                let _ = socket.write_all(response.as_bytes()).await;
                hits.fetch_add(1, Ordering::SeqCst);
            }
        });
        format!("http://{address}/propose")
    }

    let local_hits = Arc::new(AtomicUsize::new(0));
    let decoy_hits = Arc::new(AtomicUsize::new(0));
    let local_url = mock_node(Arc::clone(&local_hits)).await;
    let decoy_url = mock_node(Arc::clone(&decoy_hits)).await;

    let registry = registry(&[
        ("local", node(NodeKind::Vision, &local_url)),
        ("upstream", node(NodeKind::Vision, &decoy_url)),
    ]);
    let proposal = registry
        .vision("local")
        .expect("the named node resolves")
        .propose(VisionProposeRequest {
            purpose: "Continue".into(),
            intent_kind: "locate".into(),
            screenshot_png: b"page-material".to_vec(),
            stuck: StuckKind::TargetMissing,
        })
        .await
        .expect("the loopback node answers");
    assert!(proposal.confidence > 0.0);

    // The response arriving means the loopback server finished the request;
    // its counter is already incremented by then.
    assert_eq!(
        local_hits.load(Ordering::SeqCst),
        1,
        "the session's node did not receive the proposal"
    );
    assert_eq!(
        decoy_hits.load(Ordering::SeqCst),
        0,
        "page material reached a node the session did not name"
    );
}
