use serde::{Deserialize, Serialize};
use types::{AttachmentId, CommandId, CompanionId, PageId, ProfileId};

pub const PROTOCOL_VERSION: u16 = 1;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum BrowserEngine {
    Firefox,
    Chromium,
    WebKit,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BrowserIdentity {
    pub engine: BrowserEngine,
    pub browser_name: String,
    pub browser_version: String,
    pub os: String,
    pub profile_label: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CompanionCapabilities {
    pub observe: bool,
    pub navigate: bool,
    pub native_input: bool,
    pub tabs: bool,
    pub frames: bool,
    pub native_dialogs: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum InteractionPath {
    EngineNative,
    ExtensionApi,
    HostNative,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairRequest {
    pub protocol_version: u16,
    pub pairing_code: String,
    pub companion_id: CompanionId,
    pub profile_id: ProfileId,
    pub identity: BrowserIdentity,
    pub capabilities: CompanionCapabilities,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionRequest {
    pub protocol_version: u16,
    pub attachment_id: AttachmentId,
    pub command_id: CommandId,
    pub page_id: PageId,
    pub operation: String,
    pub input: serde_json::Value,
    pub deadline_unix_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum TargetKind {
    Page,
    Frame,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BrowserTarget {
    pub target_id: String,
    pub kind: TargetKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TargetDiscovery {
    pub protocol_version: u16,
    pub profile_id: ProfileId,
    pub targets: Vec<BrowserTarget>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct PageBindingDiscovered {
    pub protocol_version: u16,
    pub profile_id: ProfileId,
    pub target_id: String,
    pub binding_nonce: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct GrantedPage {
    pub target_id: String,
    pub page_id: PageId,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct AttachmentGrant {
    pub protocol_version: u16,
    pub attachment_id: AttachmentId,
    pub profile_id: ProfileId,
    pub expires_at_unix_ms: i64,
    pub pages: Vec<GrantedPage>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "input", rename_all = "camelCase")]
pub enum CompanionRequest {
    Pair(PairRequest),
    Grant(AttachmentGrant),
    Action(ActionRequest),
    Ping,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ActionResult {
    pub command_id: CommandId,
    pub interaction_path: InteractionPath,
    pub output: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "output", rename_all = "camelCase")]
pub enum CompanionEvent {
    Paired {
        #[serde(rename = "companionId")]
        companion_id: CompanionId,
        #[serde(rename = "profileId")]
        profile_id: ProfileId,
    },
    ActionCompleted(ActionResult),
    ActionFailed {
        #[serde(rename = "commandId")]
        command_id: CommandId,
        code: String,
        message: String,
        #[serde(rename = "effectUncertain")]
        effect_uncertain: bool,
    },
    TargetsDiscovered(TargetDiscovery),
    PageBindingDiscovered(PageBindingDiscovered),
    Pong,
}
