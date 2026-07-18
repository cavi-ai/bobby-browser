use std::collections::{BTreeMap, BTreeSet};

use serde::Deserialize;
use types::Capability;

#[derive(Debug, Clone, Copy)]
pub(crate) enum Handler {
    BrowserGetVersion,
    TargetGetTargets,
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
    pub parameter_schema_revision: String,
    pub translation_function: String,
    pub scenarios: Vec<String>,
}

#[derive(Clone)]
pub struct MethodRegistry {
    schema_revision: String,
    methods: BTreeMap<String, MethodMetadata>,
    events: BTreeMap<String, EventMetadata>,
    handlers: BTreeMap<String, Handler>,
}

impl MethodRegistry {
    pub fn compiled() -> Self {
        let manifest: SupportManifest =
            serde_json::from_str(include_str!("../../../docs/cdp-support.json"))
                .expect("compiled CDP support manifest must be valid");
        let handlers = BTreeMap::from([
            ("Browser.getVersion".to_owned(), Handler::BrowserGetVersion),
            ("Target.getTargets".to_owned(), Handler::TargetGetTargets),
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
        for method in self.methods.values() {
            if method.capability().is_none()
                || method.parameter_schema_revision.is_empty()
                || method.translation_function.is_empty()
                || method.scenarios.is_empty()
            {
                return Err(format!("incomplete method metadata for {}", method.name));
            }
        }
        for event in self.events.values() {
            if event.parameter_schema_revision.is_empty()
                || event.translation_function.is_empty()
                || event.scenarios.is_empty()
            {
                return Err(format!("incomplete event metadata for {}", event.name));
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
