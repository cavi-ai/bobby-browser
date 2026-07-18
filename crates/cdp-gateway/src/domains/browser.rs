use serde::Deserialize;
use serde_json::Value;

use crate::{CdpError, CdpErrorCode};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DownloadBehavior {
    behavior: String,
    #[serde(default)]
    events_enabled: bool,
    #[serde(default)]
    download_path: Option<String>,
    #[serde(default)]
    browser_context_id: Option<String>,
}

pub(crate) fn validate_download_behavior(params: Value) -> Result<(), CdpError> {
    let value: DownloadBehavior = serde_json::from_value(params)
        .map_err(|_| CdpError::new(CdpErrorCode::InvalidParams, "invalid download behavior"))?;
    if !matches!(value.behavior.as_str(), "deny" | "allow" | "allowAndName")
        || value
            .download_path
            .as_ref()
            .is_some_and(|path| path.len() > 4096)
        || value
            .browser_context_id
            .as_ref()
            .is_some_and(|id| id.is_empty() || id.len() > 256)
    {
        return Err(CdpError::new(
            CdpErrorCode::InvalidParams,
            "invalid download behavior",
        ));
    }
    let _ = value.events_enabled;
    Ok(())
}
