use async_trait::async_trait;
use intent_engine::{
    instrument_vision_assist, StuckKind, VisionAction, VisionAssist, VisionProposal,
    VisionProposeRequest,
};
use observability::{test_support::CaptureSink, OperationalMetrics, ProviderMode};
use std::sync::Arc;
use tracing::Instrument;
use types::{CommandError, ErrorCode, ErrorLayer};

struct Provider(bool);
#[async_trait]
impl VisionAssist for Provider {
    async fn propose(&self, _: VisionProposeRequest) -> Result<VisionProposal, CommandError> {
        if self.0 {
            Ok(VisionProposal {
                confidence: 0.1,
                action: VisionAction::ClickCandidate { index: 0 },
            })
        } else {
            Err(CommandError {
                code: ErrorCode::VisionAssistFailed,
                layer: ErrorLayer::Workflow,
                retryable: false,
                message: "PRIVATE_PROVIDER_ERROR".into(),
            })
        }
    }
    fn provider_mode(&self) -> ProviderMode {
        ProviderMode::Http
    }
}

#[tokio::test]
async fn provider_trace_records_return_status_without_request_or_error_payloads() {
    let sink = CaptureSink::install();
    for returns_proposal in [false, true] {
        let assist = instrument_vision_assist(
            Arc::new(Provider(returns_proposal)),
            OperationalMetrics::default(),
        );
        let result = assist
            .propose(VisionProposeRequest {
                purpose: "PRIVATE_PURPOSE".into(),
                intent_kind: "PRIVATE_KIND".into(),
                screenshot_png: Some(b"PRIVATE_SCREENSHOT".to_vec()),
                corpus_screenshot_png: None,
                stuck: StuckKind::TargetMissing,
                context: None,
            })
            .instrument(tracing::info_span!(
                "command",
                workflow_id = "workflow-test"
            ))
            .await;
        assert_eq!(result.is_ok(), returns_proposal);
    }
    let events = sink.events();
    let calls: Vec<_> = events
        .iter()
        .filter(|e| e["fields"]["message"] == "vision.provider.finished")
        .collect();
    assert_eq!(calls.len(), 2);
    for (event, returned) in calls.iter().zip([false, true]) {
        assert_eq!(event["span"]["workflow_id"], "workflow-test");
        assert_eq!(event["fields"]["provider_mode"], "http");
        assert_eq!(event["fields"]["proposal_returned"], returned);
        assert!(event["fields"]["elapsed_ms"].is_u64());
    }
    assert!(!serde_json::to_string(&events).unwrap().contains("PRIVATE_"));
}
