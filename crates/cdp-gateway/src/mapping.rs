use std::collections::HashMap;

use serde_json::json;
use uuid::Uuid;

use crate::CdpEvent;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IdentifierFamily {
    BrowserContext,
    Target,
    CdpSession,
    ExecutionContext,
    Frame,
    NetworkRequest,
    Download,
}

impl IdentifierFamily {
    pub const ALL: [Self; 7] = [
        Self::BrowserContext,
        Self::Target,
        Self::CdpSession,
        Self::ExecutionContext,
        Self::Frame,
        Self::NetworkRequest,
        Self::Download,
    ];
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RuntimeGeneration(pub u64);

#[derive(Clone)]
struct Binding {
    internal: String,
    runtime_session: Option<String>,
    generation: RuntimeGeneration,
}

pub struct IdentifierMap {
    connection_nonce: Uuid,
    browser_contexts: HashMap<String, Binding>,
    targets: HashMap<String, Binding>,
    cdp_sessions: HashMap<String, Binding>,
    execution_contexts: HashMap<String, Binding>,
    frames: HashMap<String, Binding>,
    network_requests: HashMap<String, Binding>,
    downloads: HashMap<String, Binding>,
}

impl Default for IdentifierMap {
    fn default() -> Self {
        Self::new()
    }
}

impl IdentifierMap {
    pub fn new() -> Self {
        Self {
            connection_nonce: Uuid::new_v4(),
            browser_contexts: HashMap::new(),
            targets: HashMap::new(),
            cdp_sessions: HashMap::new(),
            execution_contexts: HashMap::new(),
            frames: HashMap::new(),
            network_requests: HashMap::new(),
            downloads: HashMap::new(),
        }
    }

    fn opaque(&self) -> String {
        format!(
            "{}-{}",
            self.connection_nonce.simple(),
            Uuid::new_v4().simple()
        )
    }
    fn bind(
        map: &mut HashMap<String, Binding>,
        id: String,
        internal: &str,
        session: Option<&str>,
        generation: RuntimeGeneration,
    ) -> String {
        if let Some((existing, _)) = map.iter().find(|(_, binding)| {
            binding.internal == internal
                && binding.runtime_session.as_deref() == session
                && binding.generation == generation
        }) {
            return existing.clone();
        }
        map.insert(
            id.clone(),
            Binding {
                internal: internal.to_owned(),
                runtime_session: session.map(str::to_owned),
                generation,
            },
        );
        id
    }

    pub fn bind_family(
        &mut self,
        family: IdentifierFamily,
        session: &str,
        internal: &str,
        generation: RuntimeGeneration,
    ) -> String {
        match family {
            IdentifierFamily::BrowserContext => {
                self.bind_browser_context(session, internal, generation)
            }
            IdentifierFamily::Target => self.bind_target(session, internal, generation),
            IdentifierFamily::CdpSession => self.bind_cdp_session(session, internal, generation),
            IdentifierFamily::ExecutionContext => {
                self.bind_execution_context(session, internal, generation)
            }
            IdentifierFamily::Frame => self.bind_frame(session, internal, generation),
            IdentifierFamily::NetworkRequest => {
                self.bind_network_request(session, internal, generation)
            }
            IdentifierFamily::Download => self.bind_download(session, internal, generation),
        }
    }

    pub fn adopt_target(
        &mut self,
        opaque: String,
        session: &str,
        page: &str,
        generation: RuntimeGeneration,
    ) {
        self.targets.insert(
            opaque,
            Binding {
                internal: page.to_owned(),
                runtime_session: Some(session.to_owned()),
                generation,
            },
        );
    }
    pub fn bind_browser_context(
        &mut self,
        session: &str,
        internal: &str,
        generation: RuntimeGeneration,
    ) -> String {
        let id = self.opaque();
        Self::bind(
            &mut self.browser_contexts,
            id,
            internal,
            Some(session),
            generation,
        )
    }
    pub fn bind_target(
        &mut self,
        session: &str,
        page: &str,
        generation: RuntimeGeneration,
    ) -> String {
        let id = self.opaque();
        Self::bind(&mut self.targets, id, page, Some(session), generation)
    }
    pub fn bind_cdp_session(
        &mut self,
        session: &str,
        internal: &str,
        generation: RuntimeGeneration,
    ) -> String {
        let id = self.opaque();
        Self::bind(
            &mut self.cdp_sessions,
            id,
            internal,
            Some(session),
            generation,
        )
    }
    pub fn bind_execution_context(
        &mut self,
        session: &str,
        internal: &str,
        generation: RuntimeGeneration,
    ) -> String {
        let id = self.opaque();
        Self::bind(
            &mut self.execution_contexts,
            id,
            internal,
            Some(session),
            generation,
        )
    }
    pub fn bind_frame(
        &mut self,
        session: &str,
        internal: &str,
        generation: RuntimeGeneration,
    ) -> String {
        let id = self.opaque();
        Self::bind(&mut self.frames, id, internal, Some(session), generation)
    }
    pub fn bind_network_request(
        &mut self,
        session: &str,
        internal: &str,
        generation: RuntimeGeneration,
    ) -> String {
        let id = self.opaque();
        Self::bind(
            &mut self.network_requests,
            id,
            internal,
            Some(session),
            generation,
        )
    }
    pub fn bind_download(
        &mut self,
        session: &str,
        internal: &str,
        generation: RuntimeGeneration,
    ) -> String {
        let id = self.opaque();
        Self::bind(&mut self.downloads, id, internal, Some(session), generation)
    }
    pub fn resolve_target(&self, opaque: &str) -> Option<&str> {
        self.targets
            .get(opaque)
            .map(|binding| binding.internal.as_str())
    }
    pub fn resolve_browser_context(&self, opaque: &str) -> Option<&str> {
        resolve(&self.browser_contexts, opaque)
    }
    pub fn resolve_cdp_session(&self, opaque: &str) -> Option<&str> {
        resolve(&self.cdp_sessions, opaque)
    }
    pub fn resolve_execution_context(&self, opaque: &str) -> Option<&str> {
        resolve(&self.execution_contexts, opaque)
    }
    pub fn resolve_frame(&self, opaque: &str) -> Option<&str> {
        resolve(&self.frames, opaque)
    }
    pub fn resolve_network_request(&self, opaque: &str) -> Option<&str> {
        resolve(&self.network_requests, opaque)
    }
    pub fn resolve_download(&self, opaque: &str) -> Option<&str> {
        resolve(&self.downloads, opaque)
    }

    pub fn resolve_family(&self, family: IdentifierFamily, opaque: &str) -> Option<&str> {
        match family {
            IdentifierFamily::BrowserContext => self.resolve_browser_context(opaque),
            IdentifierFamily::Target => self.resolve_target(opaque),
            IdentifierFamily::CdpSession => self.resolve_cdp_session(opaque),
            IdentifierFamily::ExecutionContext => self.resolve_execution_context(opaque),
            IdentifierFamily::Frame => self.resolve_frame(opaque),
            IdentifierFamily::NetworkRequest => self.resolve_network_request(opaque),
            IdentifierFamily::Download => self.resolve_download(opaque),
        }
    }

    pub fn runtime_session_for(&self, family: IdentifierFamily, opaque: &str) -> Option<&str> {
        let map = match family {
            IdentifierFamily::BrowserContext => &self.browser_contexts,
            IdentifierFamily::Target => &self.targets,
            IdentifierFamily::CdpSession => &self.cdp_sessions,
            IdentifierFamily::ExecutionContext => &self.execution_contexts,
            IdentifierFamily::Frame => &self.frames,
            IdentifierFamily::NetworkRequest => &self.network_requests,
            IdentifierFamily::Download => &self.downloads,
        };
        map.get(opaque)?.runtime_session.as_deref()
    }

    pub fn generation_events(
        &self,
        runtime_session: &str,
        current: RuntimeGeneration,
    ) -> Vec<CdpEvent> {
        let mut stale_targets: Vec<_> = self
            .targets
            .iter()
            .filter(|(_, b)| {
                b.runtime_session.as_deref() == Some(runtime_session) && b.generation != current
            })
            .map(|(id, _)| id.clone())
            .collect();
        stale_targets.sort();
        let mut stale_sessions: Vec<_> = self
            .cdp_sessions
            .iter()
            .filter(|(_, b)| {
                b.runtime_session.as_deref() == Some(runtime_session) && b.generation != current
            })
            .map(|(id, _)| id.clone())
            .collect();
        stale_sessions.sort();
        let mut stale_execution_contexts: Vec<_> =
            stale_ids(&self.execution_contexts, runtime_session, current);
        stale_execution_contexts.sort();
        let mut stale_frames: Vec<_> = stale_ids(&self.frames, runtime_session, current);
        stale_frames.sort();
        let mut stale_browser_contexts: Vec<_> =
            stale_ids(&self.browser_contexts, runtime_session, current);
        stale_browser_contexts.sort();
        let mut stale_network_requests: Vec<_> =
            stale_ids(&self.network_requests, runtime_session, current);
        stale_network_requests.sort();
        let mut stale_downloads: Vec<_> = stale_ids(&self.downloads, runtime_session, current);
        stale_downloads.sort();
        let mut events = Vec::new();
        for request_id in &stale_network_requests {
            events.push(CdpEvent {
                method: "Network.loadingFailed".into(),
                params: json!({"requestId": request_id, "errorText": "worker replaced", "canceled": true}),
                session_id: None,
            });
        }
        for guid in &stale_downloads {
            events.push(CdpEvent {
                method: "Browser.downloadProgress".into(),
                params: json!({"guid": guid, "totalBytes": 0, "receivedBytes": 0, "state": "canceled"}),
                session_id: None,
            });
        }
        for execution_context_id in &stale_execution_contexts {
            events.push(CdpEvent {
                method: "Runtime.executionContextDestroyed".into(),
                params: json!({"executionContextUniqueId": execution_context_id}),
                session_id: None,
            });
        }
        for frame_id in &stale_frames {
            events.push(CdpEvent {
                method: "Page.frameDetached".into(),
                params: json!({"frameId": frame_id, "reason": "swap"}),
                session_id: None,
            });
        }
        for session_id in &stale_sessions {
            events.push(CdpEvent {
                method: "Target.detachedFromTarget".into(),
                params: json!({"sessionId": session_id}),
                session_id: None,
            });
        }
        for target_id in &stale_targets {
            events.push(CdpEvent {
                method: "Target.targetDestroyed".into(),
                params: json!({"targetId": target_id}),
                session_id: None,
            });
        }
        for browser_context_id in &stale_browser_contexts {
            events.push(CdpEvent {
                method: "Target.browserContextDestroyed".into(),
                params: json!({"browserContextId": browser_context_id}),
                session_id: None,
            });
        }
        events
    }

    pub fn remove_generation(&mut self, runtime_session: &str, current: RuntimeGeneration) {
        self.cdp_sessions.retain(|_, binding| {
            binding.runtime_session.as_deref() != Some(runtime_session)
                || binding.generation == current
        });
        self.targets.retain(|_, binding| {
            binding.runtime_session.as_deref() != Some(runtime_session)
                || binding.generation == current
        });
        for map in [
            &mut self.browser_contexts,
            &mut self.execution_contexts,
            &mut self.frames,
            &mut self.network_requests,
            &mut self.downloads,
        ] {
            map.retain(|_, binding| {
                binding.runtime_session.as_deref() != Some(runtime_session)
                    || binding.generation == current
            });
        }
    }
}

fn resolve<'a>(map: &'a HashMap<String, Binding>, opaque: &str) -> Option<&'a str> {
    map.get(opaque).map(|binding| binding.internal.as_str())
}

fn stale_ids(
    map: &HashMap<String, Binding>,
    runtime_session: &str,
    current: RuntimeGeneration,
) -> Vec<String> {
    map.iter()
        .filter(|(_, binding)| {
            binding.runtime_session.as_deref() == Some(runtime_session)
                && binding.generation != current
        })
        .map(|(id, _)| id.clone())
        .collect()
}
