use async_trait::async_trait;
use types::{CommandError, PageId, TextMatch, WaitCondition, WaitForCommand};
use worker_pool::wait::{WaitObservation, WaitObserver};
use worker_pool::{PageBehavior, SessionSettings, WaitProvider, WebStateEngine};

struct Defaults;
#[async_trait]
impl SessionSettings for Defaults {}
#[async_trait]
impl WebStateEngine for Defaults {}

#[tokio::test]
async fn absent_interfaces_preserve_existing_defaults() {
    let settings: &dyn SessionSettings = &Defaults;
    settings.set_fingerprint_enabled(true).await.unwrap();
    settings.set_humanization_enabled(true).await.unwrap();
    assert!(!settings.fingerprint_enabled());
    assert!(!settings.humanization_enabled());
    assert!(!WebStateEngine::supports_http_state(&Defaults));
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
