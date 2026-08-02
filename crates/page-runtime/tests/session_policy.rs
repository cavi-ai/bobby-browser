//! `ExecutionPolicy.fingerprint` and `.humanize` are session-scoped, but the
//! things they control live on a *worker*, and workers are pooled and re-leased
//! across sessions. The interesting failure is therefore not "does the flag
//! reach the worker" but "does one session's opt-in survive into the next
//! session's lease" — a session that never asked for fingerprint spoofing
//! silently getting it because the previous tenant did.
//!
//! Both are asserted below against a worker that records every toggle it is
//! handed.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use chrono::{Duration, Utc};
use page_runtime::{PageRuntime, SessionGate, VisionGate};
use tokio::sync::Mutex;
use types::{
    ClickCommand, CommandEnvelope, CommandError, CommandOutcome, Evidence, InspectCommand,
    NavigateCommand, OpenPageRequest, PageId, PrimitiveCommand, RuntimeCommand, SessionId,
    TypeTextCommand, WaitUntil, WorkerId,
};
use worker_pool::{BrowserWorker, WorkerFactory, WorkerPool};
use workflow_journal::JsonlJournal;

#[derive(Default)]
struct Toggles {
    fingerprint: Vec<bool>,
    humanize: Vec<bool>,
}

struct RecordingWorker {
    id: WorkerId,
    profile: PathBuf,
    toggles: Arc<Mutex<Toggles>>,
}

#[async_trait]
impl BrowserWorker for RecordingWorker {
    fn worker_id(&self) -> WorkerId {
        self.id.clone()
    }
    fn profile_dir(&self) -> &Path {
        &self.profile
    }
    async fn set_fingerprint_enabled(&self, enabled: bool) -> Result<(), CommandError> {
        self.toggles.lock().await.fingerprint.push(enabled);
        Ok(())
    }
    async fn set_humanization_enabled(&self, enabled: bool) -> Result<(), CommandError> {
        self.toggles.lock().await.humanize.push(enabled);
        Ok(())
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
    async fn click(&self, _: &PageId, _: &ClickCommand) -> Result<Vec<Evidence>, CommandError> {
        Ok(Vec::new())
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

struct RecordingFactory {
    toggles: Arc<Mutex<Toggles>>,
}

#[async_trait]
impl WorkerFactory for RecordingFactory {
    async fn launch(&self, _: &SessionId) -> Result<Arc<dyn BrowserWorker>, CommandError> {
        Ok(Arc::new(RecordingWorker {
            profile: PathBuf::from("bobby-session-policy"),
            id: WorkerId::new(),
            toggles: Arc::clone(&self.toggles),
        }))
    }
}

async fn runtime(toggles: Arc<Mutex<Toggles>>) -> (PageRuntime, tempfile::TempDir) {
    let root = tempfile::tempdir().expect("tempdir");
    let journal = Arc::new(
        JsonlJournal::open(root.path().join("journal.jsonl"))
            .await
            .expect("journal opens"),
    );
    // One worker in the pool, so the second session is guaranteed to be handed
    // the same worker the first one used. That is the reuse this file is about.
    let workers = Arc::new(WorkerPool::new(1, Arc::new(RecordingFactory { toggles })));
    (PageRuntime::new(journal, workers), root)
}

/// A navigate envelope against a freshly opened page in a fresh session.
async fn envelope(runtime: &PageRuntime) -> CommandEnvelope {
    envelope_for(runtime, SessionId::new()).await
}

async fn envelope_for(runtime: &PageRuntime, session_id: SessionId) -> CommandEnvelope {
    let page_id = runtime
        .open(OpenPageRequest {
            session_id: session_id.clone(),
        })
        .await
        .id;
    CommandEnvelope {
        schema_version: CommandEnvelope::SCHEMA_VERSION,
        command_id: types::CommandId::new(),
        workflow_id: types::WorkflowId::new(),
        attempt_id: types::AttemptId::new(),
        session_id,
        page_id: Some(page_id),
        deadline: Utc::now() + Duration::seconds(30),
        command: RuntimeCommand::Primitive(PrimitiveCommand::Navigate(NavigateCommand {
            url: "https://example.test/".into(),
            wait_until: WaitUntil::DomContentLoaded,
            timeout_ms: 30_000,
        })),
    }
}

async fn navigate_once(runtime: &PageRuntime, session_id: SessionId, gate: SessionGate) {
    let envelope = envelope_for(runtime, session_id).await;
    let outcome = runtime.execute_with_session_gate(envelope, gate).await;
    assert!(
        matches!(outcome, CommandOutcome::Completed { .. }),
        "navigate did not complete: {outcome:?}"
    );
}

#[tokio::test]
async fn the_session_policy_is_written_to_the_leased_worker() {
    let toggles = Arc::new(Mutex::new(Toggles::default()));
    let (runtime, _root) = runtime(Arc::clone(&toggles)).await;
    navigate_once(
        &runtime,
        SessionId::new(),
        SessionGate {
            fingerprint: true,
            humanize: true,
            ..SessionGate::default()
        },
    )
    .await;
    let observed = toggles.lock().await;
    assert_eq!(observed.fingerprint, vec![true], "fingerprint not applied");
    assert_eq!(observed.humanize, vec![true], "humanization not applied");
}

#[tokio::test]
async fn a_session_that_did_not_opt_in_never_sees_the_previous_tenants_settings() {
    let toggles = Arc::new(Mutex::new(Toggles::default()));
    let (runtime, _root) = runtime(Arc::clone(&toggles)).await;

    navigate_once(
        &runtime,
        SessionId::new(),
        SessionGate {
            fingerprint: true,
            humanize: true,
            ..SessionGate::default()
        },
    )
    .await;
    // Second session, same pooled worker, default (all-denied) policy.
    navigate_once(&runtime, SessionId::new(), SessionGate::default()).await;

    let observed = toggles.lock().await;
    assert_eq!(
        observed.fingerprint,
        vec![true, false],
        "the second session inherited fingerprint spoofing it never asked for"
    );
    assert_eq!(
        observed.humanize,
        vec![true, false],
        "the second session inherited humanization it never asked for"
    );
}

/// `execute` and `execute_with_vision_gate` never resolve a session, so neither
/// can be a way to obtain either flag. They are exercised directly rather than
/// through `execute_with_session_gate`, because "the default is denied" is a
/// claim about those two entry points specifically.
#[tokio::test]
async fn the_gateless_execute_paths_deny_both_flags() {
    let toggles = Arc::new(Mutex::new(Toggles::default()));
    let (runtime, _root) = runtime(Arc::clone(&toggles)).await;

    let outcome = runtime.execute(envelope(&runtime).await).await;
    assert!(matches!(outcome, CommandOutcome::Completed { .. }));

    let outcome = runtime
        .execute_with_vision_gate(
            envelope(&runtime).await,
            VisionGate {
                session_ok: true,
                capability_ok: true,
            },
        )
        .await;
    assert!(matches!(outcome, CommandOutcome::Completed { .. }));

    let observed = toggles.lock().await;
    assert_eq!(
        observed.fingerprint,
        vec![false, false],
        "a gateless execute path granted fingerprint spoofing"
    );
    assert_eq!(
        observed.humanize,
        vec![false, false],
        "a gateless execute path granted humanization"
    );
}
