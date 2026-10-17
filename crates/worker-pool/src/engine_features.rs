//! Configured implementation support, independent of authority and connection health.
use companion_protocol::BrowserEngine;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EngineFeature {
    ViewportScreenshot,
    FullPageScreenshot,
    ClippedScreenshot,
    HttpOnlyCookieWrite,
    HttpOnlyCookieDelete,
    FocusEmulation,
    EmulatedMedia,
    NetworkQuiet,
    JavaScriptEvaluation,
}

impl EngineFeature {
    pub const ALL: &'static [Self] = &[
        Self::ViewportScreenshot,
        Self::FullPageScreenshot,
        Self::ClippedScreenshot,
        Self::HttpOnlyCookieWrite,
        Self::HttpOnlyCookieDelete,
        Self::FocusEmulation,
        Self::EmulatedMedia,
        Self::NetworkQuiet,
        Self::JavaScriptEvaluation,
    ];

    pub const fn name(self) -> &'static str {
        match self {
            Self::ViewportScreenshot => "captureScreenshot.viewport",
            Self::FullPageScreenshot => "captureScreenshot.fullPage",
            Self::ClippedScreenshot => "captureScreenshot.clip",
            Self::HttpOnlyCookieWrite => "setCookies.httpOnly",
            Self::HttpOnlyCookieDelete => "deleteCookies.httpOnly",
            Self::FocusEmulation => "setFocusEmulation",
            Self::EmulatedMedia => "setEmulatedMedia",
            Self::NetworkQuiet => "waitFor.networkQuiet",
            Self::JavaScriptEvaluation => "evaluateJavaScript",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineFeatureSupport {
    pub engine: BrowserEngine,
    pub features: &'static [EngineFeature],
}

impl EngineFeatureSupport {
    pub const IMPLEMENTATIONS: &'static [Self] = &[
        Self::for_engine(BrowserEngine::Chromium),
        Self::for_engine(BrowserEngine::Firefox),
    ];

    pub const fn for_engine(engine: BrowserEngine) -> Self {
        let features = match engine {
            BrowserEngine::Chromium => EngineFeature::ALL,
            BrowserEngine::Firefox => &[
                EngineFeature::ViewportScreenshot,
                EngineFeature::NetworkQuiet,
                EngineFeature::JavaScriptEvaluation,
            ],
            BrowserEngine::WebKit => &[],
        };
        Self { engine, features }
    }

    pub const fn engine_name(&self) -> &'static str {
        match self.engine {
            BrowserEngine::Chromium => "chromium",
            BrowserEngine::Firefox => "firefox",
            BrowserEngine::WebKit => "webkit",
        }
    }

    pub fn capabilities(&self) -> Vec<String> {
        let engine = self.engine_name();
        self.features
            .iter()
            .map(|feature| format!("engine:{engine}:{}", feature.name()))
            .collect()
    }
}

/// Passive metadata from an existing worker. Missing metadata stays unknown.
#[derive(Debug, Clone, Default)]
pub struct WorkerFeatureObservation {
    pub support: Option<EngineFeatureSupport>,
    pub connection: types::SessionConnectionState,
}
