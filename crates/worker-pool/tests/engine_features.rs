use companion_protocol::BrowserEngine;
use worker_pool::{ChromiumWorkerFactory, EngineFeature, EngineFeatureSupport, WorkerFactory};

#[test]
fn feature_support_distinguishes_engine_specific_options() {
    let chromium = EngineFeatureSupport::for_engine(BrowserEngine::Chromium);
    let firefox = EngineFeatureSupport::for_engine(BrowserEngine::Firefox);
    assert_eq!(chromium.features, EngineFeature::ALL);
    assert_eq!(
        firefox.features,
        &[
            EngineFeature::ViewportScreenshot,
            EngineFeature::NetworkQuiet,
            EngineFeature::JavaScriptEvaluation,
        ]
    );
    assert!(EngineFeatureSupport::for_engine(BrowserEngine::WebKit)
        .features
        .is_empty());
    assert!(chromium
        .capabilities()
        .contains(&"engine:chromium:captureScreenshot.fullPage".into()));
    assert!(!firefox
        .capabilities()
        .iter()
        .any(|name| name.contains("httpOnly") || name.contains("Emulation")));
}

#[test]
fn chromium_factory_reports_support_without_launching_a_browser() {
    let factory = ChromiumWorkerFactory::new(config::BrowserConfig::default());
    assert_eq!(
        factory.engine_feature_support(),
        vec![EngineFeatureSupport::for_engine(BrowserEngine::Chromium)]
    );
}
