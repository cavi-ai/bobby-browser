//! Chromium CDP adapter for [`fingerprinting::FingerprintHost`].

use async_trait::async_trait;
use chromiumoxide::cdp::browser_protocol::emulation::{
    SetDeviceMetricsOverrideParams, SetLocaleOverrideParams, SetTimezoneOverrideParams,
    SetTouchEmulationEnabledParams, UserAgentBrandVersion, UserAgentMetadata,
};
use chromiumoxide::types::MethodId;
use chromiumoxide::{Command, Method, Page};
use fingerprinting::{FingerprintApplyError, FingerprintApplyPlan, FingerprintHost};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

pub struct ChromiumPageHost<'a> {
    pub page: &'a Page,
}

/// CDP setUserAgentOverride with a loose metadata object so we can include
/// deprecated `fullVersion` (required for `navigator.userAgentData` uaFullVersion).
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
struct UserAgentOverrideCmd {
    user_agent: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    accept_language: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    platform: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_agent_metadata: Option<Value>,
    #[serde(skip_serializing)]
    method: &'static str,
}

impl Method for UserAgentOverrideCmd {
    fn identifier(&self) -> MethodId {
        self.method.into()
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct UserAgentOverrideReturns {}

impl Command for UserAgentOverrideCmd {
    type Response = UserAgentOverrideReturns;
}

impl UserAgentOverrideCmd {
    fn for_domain(method: &'static str, plan: &FingerprintApplyPlan, metadata: Value) -> Self {
        Self {
            user_agent: plan.user_agent.clone(),
            accept_language: Some(plan.locale.clone()),
            platform: Some(plan.session.platform.clone()),
            user_agent_metadata: Some(metadata),
            method,
        }
    }
}

fn user_agent_metadata_value(plan: &FingerprintApplyPlan) -> Result<Value, FingerprintApplyError> {
    let hints = &plan.session.client_hints;
    let brands = hints
        .brands
        .iter()
        .map(|b| UserAgentBrandVersion::new(&b.brand, &b.version))
        .collect::<Vec<_>>();
    let full_version_list = hints
        .full_version_list
        .iter()
        .map(|b| UserAgentBrandVersion::new(&b.brand, &b.version))
        .collect::<Vec<_>>();

    let meta = UserAgentMetadata {
        brands: Some(brands),
        full_version_list: Some(full_version_list),
        platform: hints.platform.clone(),
        platform_version: hints.platform_version.clone(),
        architecture: hints.architecture.clone(),
        model: hints.model.clone(),
        mobile: hints.mobile,
        bitness: Some(hints.bitness.clone()),
        wow64: Some(false),
        form_factors: Some(vec!["Desktop".to_string()]),
    };
    let mut value = serde_json::to_value(meta).map_err(|error| {
        FingerprintApplyError::Host(format!("userAgentMetadata serialize failed: {error}"))
    })?;
    if let Some(obj) = value.as_object_mut() {
        // Deprecated CDP field — still what Chromium uses for uaFullVersion.
        obj.insert("fullVersion".into(), json!(hints.full_version));
    }
    Ok(value)
}

#[async_trait]
impl FingerprintHost for ChromiumPageHost<'_> {
    async fn apply_fingerprint(
        &self,
        plan: &FingerprintApplyPlan,
    ) -> Result<(), FingerprintApplyError> {
        let metadata = user_agent_metadata_value(plan)?;
        // Emulation covers navigator.userAgentData; Network covers Sec-CH-UA headers.
        for method in [
            "Emulation.setUserAgentOverride",
            "Network.setUserAgentOverride",
        ] {
            self.page
                .execute(UserAgentOverrideCmd::for_domain(
                    method,
                    plan,
                    metadata.clone(),
                ))
                .await
                .map_err(|error| FingerprintApplyError::Host(error.to_string()))?;
        }

        self.page
            .emulate_locale(SetLocaleOverrideParams {
                locale: Some(plan.locale.clone()),
            })
            .await
            .map_err(|error| FingerprintApplyError::Host(error.to_string()))?;

        self.page
            .emulate_timezone(SetTimezoneOverrideParams {
                timezone_id: plan.timezone_id.clone(),
            })
            .await
            .map_err(|error| FingerprintApplyError::Host(error.to_string()))?;

        let metrics = SetDeviceMetricsOverrideParams::builder()
            .width(plan.device_metrics.width as i64)
            .height(plan.device_metrics.height as i64)
            .device_scale_factor(plan.device_metrics.device_scale_factor)
            .mobile(plan.device_metrics.mobile)
            .build()
            .map_err(FingerprintApplyError::Host)?;
        self.page
            .execute(metrics)
            .await
            .map_err(|error| FingerprintApplyError::Host(error.to_string()))?;

        // Desktop personas must not advertise touch emulation from CDP.
        if plan.session.max_touch_points == 0 {
            let touch = SetTouchEmulationEnabledParams::new(false);
            self.page
                .execute(touch)
                .await
                .map_err(|error| FingerprintApplyError::Host(error.to_string()))?;
        }

        self.page
            .add_init_script(plan.init_script.as_str())
            .await
            .map_err(|error| FingerprintApplyError::Host(error.to_string()))?;

        Ok(())
    }
}
