use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use types::Capability;

use crate::{CdpError, CdpErrorCode, CdpEvent};

#[derive(Debug, Clone, Copy)]
pub(crate) enum Handler {
    BrowserGetVersion,
    BrowserSetDownloadBehavior,
    EmulationSetFocus,
    EmulationSetMedia,
    LogEnable,
    NetworkEnable,
    PageAddScript,
    PageCreateIsolatedWorld,
    PageCaptureScreenshot,
    PageEnable,
    PageGetFrameTree,
    PageGetLayoutMetrics,
    PageNavigate,
    PageSetLifecycle,
    RuntimeEnable,
    RuntimeCallFunctionOn,
    RuntimeEvaluate,
    RuntimeReleaseObject,
    RuntimeRunIfWaiting,
    TargetGetTargets,
    TargetGetTargetInfo,
    TargetSetAutoAttach,
}

impl Handler {
    const fn translation_function(self) -> &'static str {
        match self {
            Self::BrowserGetVersion => "runtime_info",
            Self::BrowserSetDownloadBehavior => "configure_runtime_downloads",
            Self::EmulationSetFocus => "configure_runtime_focus",
            Self::EmulationSetMedia => "configure_runtime_media",
            Self::LogEnable => "enable_runtime_logs",
            Self::NetworkEnable => "enable_runtime_network_observation",
            Self::PageAddScript => "register_runtime_init_script",
            Self::PageCreateIsolatedWorld => "create_runtime_isolated_context",
            Self::PageCaptureScreenshot => "capture_runtime_screenshot",
            Self::PageEnable => "enable_runtime_page_observation",
            Self::PageGetFrameTree => "runtime_frame_tree",
            Self::PageGetLayoutMetrics => "runtime_layout_metrics",
            Self::PageNavigate => "submit_runtime_navigation",
            Self::PageSetLifecycle => "configure_runtime_lifecycle_events",
            Self::RuntimeEnable => "enable_runtime_observation",
            Self::RuntimeCallFunctionOn => "translate_playwright_semantic_call",
            Self::RuntimeEvaluate => "recognize_playwright_runtime_bootstrap",
            Self::RuntimeReleaseObject => "release_gateway_remote_object",
            Self::RuntimeRunIfWaiting => "resume_runtime_target",
            Self::TargetGetTargets => "list_sessions",
            Self::TargetGetTargetInfo => "runtime_browser_target",
            Self::TargetSetAutoAttach => "attach_runtime_targets",
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum EventTranslator {
    TargetAttached,
    ExecutionContextCreated,
    ExecutionContextsCleared,
    FrameNavigated,
    LifecycleEvent,
    TargetDetached,
    TargetDestroyed,
    ExecutionContextDestroyed,
    FrameDetached,
    BrowserContextDestroyed,
    NetworkLoadingFailed,
    DownloadProgress,
    DownloadWillBegin,
}

impl EventTranslator {
    const fn translation_function(self) -> &'static str {
        match self {
            Self::TargetAttached => "runtime_target_attached",
            Self::ExecutionContextCreated => "runtime_execution_context_created",
            Self::ExecutionContextsCleared => "runtime_execution_contexts_cleared",
            Self::FrameNavigated => "runtime_navigation_committed",
            Self::LifecycleEvent => "runtime_lifecycle_observed",
            Self::TargetDetached => "worker_generation_detached",
            Self::TargetDestroyed => "worker_generation_destroyed",
            Self::ExecutionContextDestroyed => "worker_generation_execution_context_destroyed",
            Self::FrameDetached => "worker_generation_frame_detached",
            Self::BrowserContextDestroyed => "worker_generation_browser_context_destroyed",
            Self::NetworkLoadingFailed => "worker_generation_network_failed",
            Self::DownloadProgress => "worker_generation_download_canceled",
            Self::DownloadWillBegin => "runtime_download_will_begin",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct SupportManifest {
    schema_revision: String,
    methods: Vec<MethodMetadata>,
    events: Vec<EventMetadata>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct MethodMetadata {
    pub name: String,
    pub required_capability: String,
    pub parameter_schema_revision: String,
    pub translation_function: String,
    pub scenarios: Vec<String>,
}

impl MethodMetadata {
    pub fn capability(&self) -> Option<Capability> {
        parse_capability(&self.required_capability)
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EventMetadata {
    pub name: String,
    pub required_capability: String,
    pub parameter_schema_revision: String,
    pub translation_function: String,
    pub scenarios: Vec<String>,
}

impl EventMetadata {
    pub fn capability(&self) -> Option<Capability> {
        parse_capability(&self.required_capability)
    }
}

#[derive(Clone)]
pub struct MethodRegistry {
    schema_revision: String,
    methods: BTreeMap<String, MethodMetadata>,
    events: BTreeMap<String, EventMetadata>,
    handlers: BTreeMap<String, Handler>,
    event_translators: BTreeMap<String, EventTranslator>,
}

impl MethodRegistry {
    pub fn compiled() -> Self {
        let manifest: SupportManifest =
            serde_json::from_str(include_str!("../../../docs/cdp-support.json"))
                .expect("compiled CDP support manifest must be valid");
        let handlers = BTreeMap::from([
            ("Browser.getVersion".to_owned(), Handler::BrowserGetVersion),
            (
                "Browser.setDownloadBehavior".to_owned(),
                Handler::BrowserSetDownloadBehavior,
            ),
            (
                "Emulation.setFocusEmulationEnabled".to_owned(),
                Handler::EmulationSetFocus,
            ),
            (
                "Emulation.setEmulatedMedia".to_owned(),
                Handler::EmulationSetMedia,
            ),
            ("Log.enable".to_owned(), Handler::LogEnable),
            ("Network.enable".to_owned(), Handler::NetworkEnable),
            (
                "Page.addScriptToEvaluateOnNewDocument".to_owned(),
                Handler::PageAddScript,
            ),
            (
                "Page.createIsolatedWorld".to_owned(),
                Handler::PageCreateIsolatedWorld,
            ),
            (
                "Page.captureScreenshot".to_owned(),
                Handler::PageCaptureScreenshot,
            ),
            ("Page.enable".to_owned(), Handler::PageEnable),
            ("Page.getFrameTree".to_owned(), Handler::PageGetFrameTree),
            (
                "Page.getLayoutMetrics".to_owned(),
                Handler::PageGetLayoutMetrics,
            ),
            ("Page.navigate".to_owned(), Handler::PageNavigate),
            (
                "Page.setLifecycleEventsEnabled".to_owned(),
                Handler::PageSetLifecycle,
            ),
            ("Runtime.enable".to_owned(), Handler::RuntimeEnable),
            (
                "Runtime.callFunctionOn".to_owned(),
                Handler::RuntimeCallFunctionOn,
            ),
            ("Runtime.evaluate".to_owned(), Handler::RuntimeEvaluate),
            (
                "Runtime.releaseObject".to_owned(),
                Handler::RuntimeReleaseObject,
            ),
            (
                "Runtime.runIfWaitingForDebugger".to_owned(),
                Handler::RuntimeRunIfWaiting,
            ),
            ("Target.getTargets".to_owned(), Handler::TargetGetTargets),
            (
                "Target.getTargetInfo".to_owned(),
                Handler::TargetGetTargetInfo,
            ),
            (
                "Target.setAutoAttach".to_owned(),
                Handler::TargetSetAutoAttach,
            ),
        ]);
        let event_translators = BTreeMap::from([
            (
                "Target.attachedToTarget".to_owned(),
                EventTranslator::TargetAttached,
            ),
            (
                "Runtime.executionContextCreated".to_owned(),
                EventTranslator::ExecutionContextCreated,
            ),
            (
                "Runtime.executionContextsCleared".to_owned(),
                EventTranslator::ExecutionContextsCleared,
            ),
            (
                "Page.frameNavigated".to_owned(),
                EventTranslator::FrameNavigated,
            ),
            (
                "Page.lifecycleEvent".to_owned(),
                EventTranslator::LifecycleEvent,
            ),
            (
                "Target.detachedFromTarget".to_owned(),
                EventTranslator::TargetDetached,
            ),
            (
                "Target.targetDestroyed".to_owned(),
                EventTranslator::TargetDestroyed,
            ),
            (
                "Runtime.executionContextDestroyed".to_owned(),
                EventTranslator::ExecutionContextDestroyed,
            ),
            (
                "Page.frameDetached".to_owned(),
                EventTranslator::FrameDetached,
            ),
            (
                "Target.browserContextDestroyed".to_owned(),
                EventTranslator::BrowserContextDestroyed,
            ),
            (
                "Network.loadingFailed".to_owned(),
                EventTranslator::NetworkLoadingFailed,
            ),
            (
                "Browser.downloadProgress".to_owned(),
                EventTranslator::DownloadProgress,
            ),
            (
                "Browser.downloadWillBegin".to_owned(),
                EventTranslator::DownloadWillBegin,
            ),
        ]);
        let registry = Self {
            schema_revision: manifest.schema_revision,
            methods: manifest
                .methods
                .into_iter()
                .map(|entry| (entry.name.clone(), entry))
                .collect(),
            events: manifest
                .events
                .into_iter()
                .map(|entry| (entry.name.clone(), entry))
                .collect(),
            handlers,
            event_translators,
        };
        registry
            .validate()
            .expect("compiled CDP registry must be complete and bijective");
        registry
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema_revision.is_empty() {
            return Err("missing manifest schema revision".into());
        }
        if self.methods.keys().any(|name| name.contains('*'))
            || self.events.keys().any(|name| name.contains('*'))
        {
            return Err("wildcard CDP support is forbidden".into());
        }
        if self.methods.keys().cloned().collect::<BTreeSet<_>>()
            != self.handlers.keys().cloned().collect()
        {
            return Err("manifest and handler registry are not bijective".into());
        }
        if self.events.keys().cloned().collect::<BTreeSet<_>>()
            != self.event_translators.keys().cloned().collect()
        {
            return Err("manifest and event translator registry are not bijective".into());
        }
        for method in self.methods.values() {
            if method.capability().is_none()
                || method.parameter_schema_revision.is_empty()
                || method.translation_function.is_empty()
                || method.scenarios.is_empty()
            {
                return Err(format!("incomplete method metadata for {}", method.name));
            }
            if self
                .handlers
                .get(&method.name)
                .is_none_or(|handler| handler.translation_function() != method.translation_function)
            {
                return Err(format!("method translator mismatch for {}", method.name));
            }
        }
        for event in self.events.values() {
            if event.capability().is_none()
                || event.parameter_schema_revision.is_empty()
                || event.translation_function.is_empty()
                || event.scenarios.is_empty()
            {
                return Err(format!("incomplete event metadata for {}", event.name));
            }
            if self
                .event_translators
                .get(&event.name)
                .is_none_or(|translator| {
                    translator.translation_function() != event.translation_function
                })
            {
                return Err(format!("event translator mismatch for {}", event.name));
            }
        }
        Ok(())
    }

    pub fn method(&self, name: &str) -> Option<&MethodMetadata> {
        self.methods.get(name)
    }
    pub fn methods(&self) -> impl Iterator<Item = &MethodMetadata> {
        self.methods.values()
    }
    pub fn has_handler(&self, name: &str) -> bool {
        self.handlers.contains_key(name)
    }
    pub fn method_count(&self) -> usize {
        self.methods.len()
    }
    pub fn handler_count(&self) -> usize {
        self.handlers.len()
    }
    pub(crate) fn handler(&self, name: &str) -> Option<Handler> {
        self.handlers.get(name).copied()
    }
    pub fn events(&self) -> impl Iterator<Item = &EventMetadata> {
        self.events.values()
    }
    pub fn has_event_translator(&self, name: &str) -> bool {
        self.event_translators.contains_key(name)
    }
    pub fn event_count(&self) -> usize {
        self.events.len()
    }
    pub fn event_translator_count(&self) -> usize {
        self.event_translators.len()
    }
    pub(crate) fn event(&self, name: &str) -> Option<&EventMetadata> {
        self.events.get(name)
    }

    pub(crate) fn translate_event(&self, event: CdpEvent) -> Result<CdpEvent, CdpError> {
        let Some(translator) = self.event_translators.get(&event.method) else {
            return Err(CdpError::new(
                CdpErrorCode::MethodNotFound,
                "event is not supported",
            ));
        };
        if matches!(
            translator,
            EventTranslator::ExecutionContextCreated | EventTranslator::FrameNavigated
        ) {
            let container = if matches!(translator, EventTranslator::ExecutionContextCreated) {
                "context"
            } else {
                "frame"
            };
            let id = if matches!(translator, EventTranslator::ExecutionContextCreated) {
                "uniqueId"
            } else {
                "id"
            };
            let valid = event
                .params
                .get(container)
                .and_then(serde_json::Value::as_object)
                .and_then(|value| value.get(id))
                .and_then(serde_json::Value::as_str)
                .is_some_and(|id| !id.is_empty());
            return if valid {
                Ok(event)
            } else {
                Err(CdpError::new(
                    CdpErrorCode::InvalidParams,
                    "invalid event payload",
                ))
            };
        }
        if matches!(translator, EventTranslator::ExecutionContextsCleared) {
            return if event
                .params
                .as_object()
                .is_some_and(serde_json::Map::is_empty)
            {
                Ok(event)
            } else {
                Err(CdpError::new(
                    CdpErrorCode::InvalidParams,
                    "invalid event payload",
                ))
            };
        }
        let required = match translator {
            EventTranslator::TargetAttached => "sessionId",
            EventTranslator::ExecutionContextCreated => unreachable!(),
            EventTranslator::ExecutionContextsCleared => unreachable!(),
            EventTranslator::FrameNavigated => unreachable!(),
            EventTranslator::LifecycleEvent => "frameId",
            EventTranslator::TargetDetached => "sessionId",
            EventTranslator::TargetDestroyed => "targetId",
            EventTranslator::ExecutionContextDestroyed => "executionContextUniqueId",
            EventTranslator::FrameDetached => "frameId",
            EventTranslator::BrowserContextDestroyed => "browserContextId",
            EventTranslator::NetworkLoadingFailed => "requestId",
            EventTranslator::DownloadProgress => "guid",
            EventTranslator::DownloadWillBegin => "guid",
        };
        if event
            .params
            .get(required)
            .and_then(serde_json::Value::as_str)
            .is_none_or(str::is_empty)
        {
            return Err(CdpError::new(
                CdpErrorCode::InvalidParams,
                "invalid event payload",
            ));
        }
        Ok(event)
    }
}

fn parse_capability(value: &str) -> Option<Capability> {
    Some(match value {
        "session:read" => Capability::SessionRead,
        "session:write" => Capability::SessionWrite,
        "page:read" => Capability::PageRead,
        "page:write" => Capability::PageWrite,
        "browser:mutate" => Capability::BrowserMutate,
        "file:upload" => Capability::FileUpload,
        "file:download" => Capability::FileDownload,
        "javascript:evaluate" => Capability::JavascriptEvaluate,
        "artifact:read" => Capability::ArtifactRead,
        "artifact:capture" => Capability::ArtifactCapture,
        "recovery:read" => Capability::RecoveryRead,
        "recovery:write" => Capability::RecoveryWrite,
        _ => return None,
    })
}
