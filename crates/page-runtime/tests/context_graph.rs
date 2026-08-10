//! The context graph's staleness rules, driven through the executor rather
//! than called directly.
//!
//! `crates/page-runtime/src/context.rs` unit-tests the graph in isolation;
//! these cover the wiring, so everything here goes through `execute`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use page_runtime::PageRuntime;
use types::{
    AccessibilityNode, AccessibilitySnapshotCommand, AccessibilityTarget, ClickCommand,
    CommandEnvelope, CommandError, CommandOutcome, Evidence, InspectCommand, NavigateCommand,
    OpenPageRequest, PageId, PrimitiveCommand, RuntimeCommand, SessionId, TypeTextCommand,
    WaitUntil, WorkerId,
};
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};
use workflow_journal::JsonlJournal;

fn node(role: &str, name: &str) -> AccessibilityNode {
    AccessibilityNode {
        role: Some(role.to_owned()),
        name: Some(name.to_owned()),
        target: Some(AccessibilityTarget {
            role: role.to_owned(),
            accessible_name: name.to_owned(),
            ordinal: Some(1),
            frame_path: Vec::new(),
        }),
        ..AccessibilityNode::default()
    }
}

struct SnapshottingWorker {
    id: WorkerId,
    profile: PathBuf,
}

#[async_trait]
impl BrowserWorker for SnapshottingWorker {
    fn worker_id(&self) -> WorkerId {
        self.id.clone()
    }
    fn profile_dir(&self) -> &Path {
        &self.profile
    }
    async fn open_page(&self, _: PageId) -> Result<(), CommandError> {
        Ok(())
    }
    async fn navigate(
        &self,
        _: &PageId,
        command: &NavigateCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Navigation {
            url: command.url.clone(),
            title: "Fixture".into(),
        }])
    }
    async fn inspect(
        &self,
        _: &PageId,
        command: &InspectCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Inspection {
            selector: command.selector.clone(),
            url: "https://example.test/".into(),
            title: "Fixture".into(),
            text: "page".into(),
            html: None,
        }])
    }
    async fn a11y_snapshot(
        &self,
        page_id: &PageId,
        _: &AccessibilitySnapshotCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::AccessibilitySnapshot {
            page_id: page_id.clone(),
            nodes: vec![node("textbox", "Email address")],
            truncated: false,
        }])
    }
    async fn click(&self, _: &PageId, _: &ClickCommand) -> Result<Vec<Evidence>, CommandError> {
        Ok(vec![Evidence::Element {
            selector: "#go".into(),
            text: None,
        }])
    }
    async fn type_text(
        &self,
        _: &PageId,
        _: &TypeTextCommand,
    ) -> Result<Vec<Evidence>, CommandError> {
        Ok(Vec::new())
    }
    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }
}

struct SnapshottingFactory;

#[async_trait]
impl WorkerFactory for SnapshottingFactory {
    async fn launch(&self, _: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(Arc::new(SnapshottingWorker {
            id: WorkerId::new(),
            profile: PathBuf::from("bobby-context-graph"),
        }))
    }
}

async fn runtime() -> (PageRuntime, tempfile::TempDir) {
    let root = tempfile::tempdir().expect("tempdir");
    let journal = Arc::new(
        JsonlJournal::open(root.path().join("journal.jsonl"))
            .await
            .expect("journal opens"),
    );
    let workers = Arc::new(WorkerPool::new(1, Arc::new(SnapshottingFactory)));
    (PageRuntime::new(journal, workers), root)
}

fn envelope(session: &SessionId, page: &PageId, command: PrimitiveCommand) -> CommandEnvelope {
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: types::CommandId::new(),
        workflow_id: types::WorkflowId::new(),
        attempt_id: types::AttemptId::new(),
        session_id: session.clone(),
        page_id: Some(page.clone()),
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Primitive(command),
    }
}

async fn run(runtime: &PageRuntime, envelope: CommandEnvelope) {
    let outcome = runtime.execute(envelope).await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "command did not complete: {outcome:?}"
    );
}

fn snapshot() -> PrimitiveCommand {
    PrimitiveCommand::AccessibilitySnapshot(AccessibilitySnapshotCommand {
        max_nodes: None,
        target: None,
    })
}

fn click() -> PrimitiveCommand {
    PrimitiveCommand::Click(ClickCommand {
        selector: "#go".into(),
        target: None,
        boundary: false,
        expected_url: None,
    })
}

fn navigate() -> PrimitiveCommand {
    PrimitiveCommand::Navigate(NavigateCommand {
        url: "https://example.test/next".into(),
        wait_until: WaitUntil::DomContentLoaded,
        timeout_ms: 30_000,
    })
}

async fn page(runtime: &PageRuntime, session: &SessionId) -> PageId {
    runtime
        .open(OpenPageRequest {
            session_id: session.clone(),
        })
        .await
        .id
}

#[tokio::test]
async fn a_snapshot_command_populates_the_graph() {
    let (runtime, _root) = runtime().await;
    let session = SessionId::new();
    let page_id = page(&runtime, &session).await;
    run(&runtime, envelope(&session, &page_id, snapshot())).await;
    let answer = runtime
        .context()
        .ask(&page_id, "Email address")
        .expect("the snapshot reached the graph");
    assert_eq!(answer.target.role, "textbox");
}

#[tokio::test]
async fn a_click_makes_the_graph_stop_answering() {
    let (runtime, _root) = runtime().await;
    let session = SessionId::new();
    let page_id = page(&runtime, &session).await;
    run(&runtime, envelope(&session, &page_id, snapshot())).await;
    assert!(runtime.context().ask(&page_id, "Email address").is_some());

    run(&runtime, envelope(&session, &page_id, click())).await;
    assert!(
        runtime.context().ask(&page_id, "Email address").is_none(),
        "the graph answered from a snapshot taken before a click"
    );
}

#[tokio::test]
async fn a_navigation_makes_the_graph_stop_answering() {
    let (runtime, _root) = runtime().await;
    let session = SessionId::new();
    let page_id = page(&runtime, &session).await;
    run(&runtime, envelope(&session, &page_id, snapshot())).await;
    run(&runtime, envelope(&session, &page_id, navigate())).await;
    assert!(
        runtime.context().ask(&page_id, "Email address").is_none(),
        "the graph answered from a snapshot taken before a navigation"
    );
}

/// Re-observing after a change makes the graph answerable again.
#[tokio::test]
async fn re_snapshotting_after_a_mutation_restores_answers() {
    let (runtime, _root) = runtime().await;
    let session = SessionId::new();
    let page_id = page(&runtime, &session).await;
    run(&runtime, envelope(&session, &page_id, snapshot())).await;
    run(&runtime, envelope(&session, &page_id, click())).await;
    assert!(runtime.context().ask(&page_id, "Email address").is_none());

    run(&runtime, envelope(&session, &page_id, snapshot())).await;
    assert!(
        runtime.context().ask(&page_id, "Email address").is_some(),
        "the graph never recovered after being invalidated"
    );
}

/// A read-only command must not invalidate the graph; workflows inspect
/// between steps.
#[tokio::test]
async fn a_read_only_command_leaves_the_graph_answerable() {
    let (runtime, _root) = runtime().await;
    let session = SessionId::new();
    let page_id = page(&runtime, &session).await;
    run(&runtime, envelope(&session, &page_id, snapshot())).await;
    run(
        &runtime,
        envelope(
            &session,
            &page_id,
            PrimitiveCommand::Inspect(InspectCommand {
                selector: None,
                target: None,
                include_html: false,
            }),
        ),
    )
    .await;
    assert!(
        runtime.context().ask(&page_id, "Email address").is_some(),
        "a read-only inspect invalidated the graph"
    );
}

/// The answer must be the same target `a11y_snapshot` would hand the agent.
#[tokio::test]
async fn the_graph_answer_matches_the_snapshot_evidence() {
    let (runtime, _root) = runtime().await;
    let session = SessionId::new();
    let page_id = page(&runtime, &session).await;
    let outcome = runtime
        .execute(envelope(&session, &page_id, snapshot()))
        .await;
    let CommandOutcome::Completed { evidence, .. } = outcome else {
        panic!("snapshot did not complete");
    };
    let from_evidence = evidence
        .iter()
        .find_map(|item| match item {
            Evidence::AccessibilitySnapshot { nodes, .. } => nodes
                .iter()
                .find(|node| {
                    node.target
                        .as_ref()
                        .is_some_and(|target| target.accessible_name == "Email address")
                })
                .and_then(|node| node.target.clone()),
            _ => None,
        })
        .expect("the snapshot evidence carries the target");
    let answer = runtime
        .context()
        .ask(&page_id, "Email address")
        .expect("the graph answers");
    assert_eq!(answer.target, from_evidence);
}

/// The runtime calls `forget_all`: retention bounded by session lifetime needs
/// the deletion path to drop the pages before the session record is removed.
#[tokio::test]
async fn deleting_a_session_evicts_its_pages_from_the_graph() {
    let (runtime, _root) = runtime().await;
    let session = SessionId::new();
    let page_id = page(&runtime, &session).await;
    run(&runtime, envelope(&session, &page_id, snapshot())).await;
    assert_eq!(runtime.context().retained_pages(), 1);

    runtime.context().forget_all(std::slice::from_ref(&page_id));
    assert_eq!(
        runtime.context().retained_pages(),
        0,
        "the session's page structure survived eviction"
    );
    assert!(runtime.context().ask(&page_id, "Email address").is_none());
}

/// The context retains command *ids*, not copies of evidence: an agent asking
/// "did this already happen" resolves them through the journal, which stays the
/// one authority.
#[tokio::test]
async fn the_graph_records_which_commands_produced_evidence() {
    let (runtime, _root) = runtime().await;
    let session = SessionId::new();
    let page_id = page(&runtime, &session).await;

    let first = envelope(&session, &page_id, snapshot());
    let first_id = first.command_id.clone();
    assert!(matches!(
        runtime.execute(first).await,
        CommandOutcome::Completed { .. }
    ));

    let recorded = runtime.context().commands_for(&page_id);
    assert!(
        recorded.contains(&first_id),
        "the command that produced evidence was not recorded: {recorded:?}"
    );
}

/// Where a control *is* goes stale when the page changes; what already
/// *happened* does not.
#[tokio::test]
async fn recorded_commands_survive_invalidation() {
    let (runtime, _root) = runtime().await;
    let session = SessionId::new();
    let page_id = page(&runtime, &session).await;

    let first = envelope(&session, &page_id, snapshot());
    let first_id = first.command_id.clone();
    runtime.execute(first).await;
    assert!(runtime.context().ask(&page_id, "Email address").is_some());

    run(&runtime, envelope(&session, &page_id, click())).await;

    assert!(
        runtime.context().ask(&page_id, "Email address").is_none(),
        "targets should be stale after a mutation"
    );
    assert!(
        runtime.context().commands_for(&page_id).contains(&first_id),
        "history was discarded along with the stale targets"
    );
}

/// Session close drops history with everything else.
#[tokio::test]
async fn recorded_commands_are_dropped_on_session_close() {
    let (runtime, _root) = runtime().await;
    let session = SessionId::new();
    let page_id = page(&runtime, &session).await;
    run(&runtime, envelope(&session, &page_id, snapshot())).await;
    assert!(!runtime.context().commands_for(&page_id).is_empty());

    runtime.context().forget_all(std::slice::from_ref(&page_id));
    assert!(
        runtime.context().commands_for(&page_id).is_empty(),
        "a deleted session's command history survived"
    );
}
