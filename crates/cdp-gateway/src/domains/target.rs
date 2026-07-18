use serde::Deserialize;
use serde_json::Value;

use crate::{CdpError, CdpErrorCode};

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct AutoAttach {
    pub auto_attach: bool,
    pub wait_for_debugger_on_start: bool,
    pub flatten: bool,
    #[serde(default)]
    pub filter: Vec<TargetFilter>,
}

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct TargetFilter {
    #[serde(rename = "type")]
    pub target_type: Option<String>,
    pub exclude: Option<bool>,
}

pub(crate) fn filter_matches(filters: &[TargetFilter], target_type: &str) -> bool {
    if filters.is_empty() {
        return true;
    }
    filters
        .iter()
        .find(|filter| {
            filter
                .target_type
                .as_deref()
                .is_none_or(|kind| kind == target_type)
        })
        .is_some_and(|filter| !filter.exclude.unwrap_or(false))
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
            if value.filter.len() > 32
                || value.filter.iter().any(|item| {
                    item.target_type
                        .as_ref()
                        .is_some_and(|kind| kind.len() > 64)
                        || item.target_type.is_none() && item.exclude.is_some()
                })
            {
                return Err(CdpError::new(
                    CdpErrorCode::InvalidParams,
                    "invalid bounded auto-attach filter",
                ));
            }
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
