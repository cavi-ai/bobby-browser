use serde::Deserialize;
use serde_json::Value;

use crate::{CdpError, CdpErrorCode};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AutoAttach {
    pub auto_attach: bool,
    pub wait_for_debugger_on_start: bool,
    pub flatten: bool,
}

pub(crate) fn auto_attach(params: Value) -> Result<AutoAttach, CdpError> {
    serde_json::from_value(params)
        .map_err(|_| {
            CdpError::new(
                CdpErrorCode::InvalidParams,
                "invalid auto-attach parameters",
            )
        })
        .and_then(|value: AutoAttach| {
            if value.auto_attach && !value.flatten {
                Err(CdpError::new(
                    CdpErrorCode::InvalidParams,
                    "flattened sessions are required",
                ))
            } else {
                Ok(value)
            }
        })
}
