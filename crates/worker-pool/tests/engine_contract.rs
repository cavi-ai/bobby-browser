use async_trait::async_trait;
use types::{CommandError, PageId, TextMatch, WaitCondition, WaitForCommand};
use worker_pool::wait::{WaitObservation, WaitObserver};
use worker_pool::{PageBehavior, SessionSettings, WaitProvider, WebStateEngine};

struct Defaults;
#[async_trait]
impl SessionSettings for Defaults {}
#[async_trait]
impl WebStateEngine for Defaults {}

struct LifecycleOnlyWorker;
#[async_trait]
impl worker_pool::BrowserWorker for LifecycleOnlyWorker {
    fn worker_id(&self) -> types::WorkerId {
        types::WorkerId::new()
    }
    fn profile_dir(&self) -> &std::path::Path {
        std::path::Path::new("lifecycle-only")
    }
    async fn close(&self) -> Result<(), CommandError> {
        Ok(())
    }
}
struct LifecycleFactory;
#[async_trait]
impl worker_pool::WorkerFactory for LifecycleFactory {
    async fn launch(
        &self,
        _: &types::SessionId,
    ) -> Result<std::sync::Arc<dyn worker_pool::BrowserWorker>, CommandError> {
        Ok(std::sync::Arc::new(LifecycleOnlyWorker))
    }
}
#[tokio::test]
async fn lifecycle_only_worker_needs_no_page_command_stubs() {
    let pool = worker_pool::WorkerPool::new(1, std::sync::Arc::new(LifecycleFactory));
    let session = types::SessionId::new();
    let lease = pool.lease(session.clone()).await.unwrap();
    assert!(lease.worker().input().is_none());
    assert!(lease.worker().observation().is_none());
    drop(lease);
    pool.release_session(&session).await.unwrap();
}

#[tokio::test]
async fn absent_interfaces_preserve_existing_defaults() {
    let settings: &dyn SessionSettings = worker_pool::session_settings_or_default(None);
    settings.set_fingerprint_enabled(true).await.unwrap();
    settings.set_humanization_enabled(true).await.unwrap();
    assert!(!settings.fingerprint_enabled());
    assert!(!settings.humanization_enabled());
    assert!(!WebStateEngine::supports_http_state(&Defaults));
    let id = PageId::new();
    assert!(worker_pool::navigation_or_default(None)
        .settle_page(&id, std::time::Duration::ZERO, None)
        .await
        .is_none());
    assert!(worker_pool::input_or_default(None)
        .verify_framed_typed_value(
            &id,
            &types::TypeTextCommand {
                selector: "input".into(),
                target: None,
                value: "value".into(),
                clear_first: true,
                expected_url: None,
            },
            None,
            "text"
        )
        .await
        .unwrap()
        .is_none());
    assert!(PageBehavior::wait_for(
        None,
        &PageId::new(),
        &WaitForCommand {
            timeout_ms: 10,
            condition: WaitCondition::Url {
                matcher: TextMatch::Exact("expected".into())
            }
        }
    )
    .await
    .is_err());
}

struct Provider;
struct Observer<'a>(&'a PageId);
#[async_trait]
impl WaitObserver for Observer<'_> {
    async fn observe(&self, _: &WaitCondition) -> Result<WaitObservation, CommandError> {
        Ok(WaitObservation::Url(self.0 .0.to_string()))
    }
}
impl WaitProvider for Provider {
    fn observer<'a>(&'a self, page_id: &'a PageId) -> Box<dyn WaitObserver + 'a> {
        Box::new(Observer(page_id))
    }
}

#[tokio::test]
async fn wait_service_accepts_a_borrowed_object_safe_provider() {
    let id = PageId::new();
    let provider: &dyn WaitProvider = &Provider;
    let evidence = PageBehavior::wait_for(
        Some(provider),
        &id,
        &WaitForCommand {
            timeout_ms: 20,
            condition: WaitCondition::Url {
                matcher: TextMatch::Exact(id.0.to_string()),
            },
        },
    )
    .await
    .unwrap();
    assert_eq!(evidence.len(), 1);
}
